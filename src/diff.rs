//! What the agent just changed in a note, as a word diff to draw on the note's page.
//!
//! A thread watches the vault (see `watch`). While an engine turn is running, a changed note
//! is compared with the text it had before the turn began, so a turn's several writes make one
//! diff. Edits outside a turn (Obsidian, git) only move the baseline. The note is never
//! touched: the diff is kept here, per note, and the page draws it. A new diff replaces the
//! old one; it stays until then, or until the reader presses Escape (`clear`).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// 0 unchanged, 1 inserted, 2 deleted; the token is as the line has it.
pub type Op = (u8, String);

#[derive(Clone, Default)]
struct Diff { stamp: u64, lines: BTreeMap<usize, Vec<Op>> }

#[derive(Default)]
struct Store { diffs: HashMap<String, Diff>, stamps: HashMap<String, u64>, loaded: bool }

fn store() -> &'static Mutex<Store> {
    static S: OnceLock<Mutex<Store>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Store::default()))
}

fn file() -> PathBuf { crate::cfg::dir().join("diffs.json") }

fn now_us() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0)
}

fn load(s: &mut Store) {
    if s.loaded { return }
    s.loaded = true;
    let Some(v) = std::fs::read_to_string(file()).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()) else { return };
    for (note, d) in v.as_object().into_iter().flatten() {
        let mut df = Diff { stamp: d["stamp"].as_u64().unwrap_or(0), lines: BTreeMap::new() };
        for (n, ops) in d["lines"].as_object().into_iter().flatten() {
            let Ok(n) = n.parse::<usize>() else { continue };
            df.lines.insert(n, ops.as_array().into_iter().flatten().filter_map(|o|
                Some((o[0].as_u64()? as u8, o[1].as_str()?.to_string()))).collect());
        }
        s.stamps.insert(note.clone(), df.stamp);
        s.diffs.insert(note.clone(), df);
    }
}

fn to_json(d: &Diff) -> serde_json::Value {
    let lines: serde_json::Map<String, serde_json::Value> = d.lines.iter()
        .map(|(n, ops)| (n.to_string(), serde_json::json!(ops))).collect();
    serde_json::json!({"stamp": d.stamp, "lines": lines})
}

fn save(s: &Store) {
    let m: serde_json::Map<String, serde_json::Value> = s.diffs.iter().map(|(k, d)| (k.clone(), to_json(d))).collect();
    let _ = std::fs::create_dir_all(crate::cfg::dir());
    let _ = std::fs::write(file(), serde_json::Value::Object(m).to_string());
}

/// Changes whenever the note's diff is drawn, replaced or cleared; a page's version folds it in.
pub fn stamp(note: &str) -> u64 {
    let mut s = store().lock().unwrap(); load(&mut s);
    s.stamps.get(note).copied().unwrap_or(0)
}

/// The note's diff as the page's `data-diff`: `{"note":..,"lines":{"22":[[1,"word"],..]}}`.
pub fn json(note: &str) -> Option<String> {
    let mut s = store().lock().unwrap(); load(&mut s);
    let d = s.diffs.get(note)?;
    let mut v = to_json(d);
    v["note"] = note.into();
    v.as_object_mut()?.remove("stamp");
    Some(v.to_string())
}

/// Forget the note's diff (Escape on the page).
pub fn clear(note: &str) {
    let mut s = store().lock().unwrap(); load(&mut s);
    if s.diffs.remove(note).is_some() { s.stamps.insert(note.to_string(), now_us()); save(&s); }
}

fn set(note: &str, lines: BTreeMap<usize, Vec<Op>>) {
    let mut s = store().lock().unwrap(); load(&mut s);
    if lines.is_empty() {
        if s.diffs.remove(note).is_none() { return }
        s.stamps.insert(note.to_string(), now_us());
    } else {
        let stamp = now_us().max(s.stamps.get(note).copied().unwrap_or(0) + 1);
        s.stamps.insert(note.to_string(), stamp);
        s.diffs.insert(note.to_string(), Diff { stamp, lines });
    }
    save(&s);
}

// ---- the diff itself ----------------------------------------------------------------------

/// A line as words: whitespace-separated, except that a `$..$` or `$$..$$` formula is one token,
/// and a wikilink reads as the text it shows.
pub fn tokens(line: &str) -> Vec<String> {
    let mut plain = String::new();
    let mut rest = line;
    while let Some(i) = rest.find("[[") {
        plain.push_str(&rest[..i]);
        let Some(j) = rest[i + 2..].find("]]") else { rest = &rest[i..]; break };
        let inner = &rest[i + 2..i + 2 + j];
        if inner.starts_with('@') { plain.push_str(&rest[i..i + 2 + j + 2]) }
        else { plain.push_str(inner.rsplit('|').next().unwrap_or(inner)) }
        rest = &rest[i + 2 + j + 2..];
    }
    plain.push_str(rest);
    let b: Vec<char> = plain.chars().collect();
    let (mut out, mut cur, mut i) = (Vec::new(), String::new(), 0);
    while i < b.len() {
        if b[i] == '$' {
            let d = if b.get(i + 1) == Some(&'$') { 2 } else { 1 };
            let close = (i + d..b.len()).find(|&k| b[k] == '$' && (d == 1 || b.get(k + 1) == Some(&'$')));
            if let Some(k) = close {
                let end = k + d;
                cur.extend(&b[i..end]);          // glued to a word before or after it, as in "$x$,"
                i = end;
                continue;
            }
        }
        if b[i].is_whitespace() { if !cur.is_empty() { out.push(std::mem::take(&mut cur)) } }
        else { cur.push(b[i]) }
        i += 1;
    }
    if !cur.is_empty() { out.push(cur) }
    out
}

/// The new line's words in order, each unchanged or inserted, with the old line's deleted
/// words in the place they were.
pub fn word_diff(old: &str, new: &str) -> Vec<Op> {
    let (a, b) = (tokens(old), tokens(new));
    let (n, m) = (a.len(), b.len());
    let mut l = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() { for j in (0..m).rev() {
        l[i][j] = if a[i] == b[j] { l[i + 1][j + 1] + 1 } else { l[i + 1][j].max(l[i][j + 1]) };
    } }
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] { out.push((0, b[j].clone())); i += 1; j += 1 }
        else if i < n && (j >= m || l[i + 1][j] >= l[i][j + 1]) { out.push((2, a[i].clone())); i += 1 }
        else { out.push((1, b[j].clone())); j += 1 }
    }
    out
}

/// Per changed line of `new` (1-based), its word diff. Lines are matched by their common head
/// and tail and an LCS between; what is left between pairs up in order.
pub fn text_diff(old: &str, new: &str) -> BTreeMap<usize, Vec<Op>> {
    let (a, b): (Vec<&str>, Vec<&str>) = (old.split('\n').collect(), new.split('\n').collect());
    let mut p = 0;
    while p < a.len() && p < b.len() && a[p] == b[p] { p += 1 }
    let mut s = 0;
    while s < a.len() - p && s < b.len() - p && a[a.len() - 1 - s] == b[b.len() - 1 - s] { s += 1 }
    let (am, bm) = (&a[p..a.len() - s], &b[p..b.len() - s]);
    let mut out = BTreeMap::new();
    let mut hunk = |o: &[&str], n: &[&str], at: usize, out: &mut BTreeMap<usize, Vec<Op>>| {
        for (k, nl) in n.iter().enumerate() {
            if nl.trim().is_empty() { continue }
            let ops = word_diff(o.get(k).copied().unwrap_or(""), nl);
            if ops.iter().any(|(op, _)| *op != 0) { out.insert(at + k + 1, ops); }
        }
    };
    if am.len() * bm.len() > 4_000_000 { hunk(am, bm, p, &mut out); return out }
    let (n, m) = (am.len(), bm.len());
    let mut l = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() { for j in (0..m).rev() {
        l[i][j] = if am[i] == bm[j] { l[i + 1][j + 1] + 1 } else { l[i + 1][j].max(l[i][j + 1]) };
    } }
    let (mut i, mut j) = (0, 0);
    let (mut oi, mut nj) = (0, 0);      // start of the open hunk in each
    while i < n || j < m {
        if i < n && j < m && am[i] == bm[j] {
            hunk(&am[oi..i], &bm[nj..j], p + nj, &mut out);
            i += 1; j += 1; oi = i; nj = j;
        } else if i < n && (j >= m || l[i + 1][j] >= l[i][j + 1]) { i += 1 } else { j += 1 }
    }
    hunk(&am[oi..], &bm[nj..], p + nj, &mut out);
    out
}

// ---- watching ------------------------------------------------------------------------------

fn notes(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if e.file_type().map_or(false, |t| t.is_dir()) {
            if !matches!(name.as_str(), ".git" | ".trash" | "node_modules" | ".obsidian" | ".claude") { notes(&p, out) }
        } else if name.ends_with(".md") { out.push(p) }
    }
}

fn mtime_ns(p: &Path) -> u128 {
    std::fs::metadata(p).ok().and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0)
}

fn busy() -> bool {
    crate::optchat::engine::request(&crate::optchat::engine::dir(), serde_json::json!({"op": "status"}))
        .map(|v| v["busy"] == true).unwrap_or(false)
}

/// Start the watcher (once per process).
pub fn spawn(vault: PathBuf) {
    static ONCE: OnceLock<()> = OnceLock::new();
    if ONCE.set(()).is_err() { return }
    crate::watch::ensure(&vault);
    std::thread::spawn(move || {
        let mut base: HashMap<PathBuf, (u128, String)> = HashMap::new();
        let mut pre: HashMap<PathBuf, String> = HashMap::new();   // text before this turn, per note touched
        let mut was_busy = false;
        let mut seen = crate::watch::now();
        let mut first = true;
        loop {
            let rang = first || { let n = crate::watch::wait(seen, Duration::from_secs(2)); let r = n != seen; seen = n; r };
            if !first && !rang && !was_busy { continue }
            if rang && !first { std::thread::sleep(Duration::from_millis(300)); }  // let a burst of writes settle
            // a write just before the turn's end is still the turn's: look once more after it
            let b = if first { false } else { busy() };
            let in_turn = b || was_busy;
            let mut files = Vec::new();
            notes(&vault, &mut files);
            for p in files {
                let t = mtime_ns(&p);
                let old = base.get(&p).map(|(t, _)| *t);
                if old == Some(t) { continue }
                if std::fs::metadata(&p).map_or(true, |m| m.len() > 2_000_000) { continue }
                let Ok(text) = std::fs::read_to_string(&p) else { continue };
                if first { base.insert(p, (t, text)); continue }
                let before = base.get(&p).map(|(_, s)| s.clone());
                if in_turn {
                    if let Some(prev) = before.clone() {
                        let start = pre.entry(p.clone()).or_insert(prev).clone();
                        let stem = p.file_stem().unwrap_or_default().to_string_lossy().to_string();
                        set(&stem, text_diff(&start, &text));
                    }
                }
                base.insert(p, (t, text));
            }
            if !in_turn { pre.clear() }
            was_busy = b;
            first = false;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(ops: &[Op]) -> String {
        ops.iter().map(|(o, t)| match o { 1 => format!("+{}", t), 2 => format!("-{}", t), _ => t.clone() })
            .collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn a_word_diff_marks_only_the_changed_words() {
        let d = word_diff("the range is a union", "the range is mostly a union of pieces");
        assert_eq!(show(&d), "the range is +mostly a union +of +pieces");
        let d = word_diff("uses sparsity", "mostly uses structure");
        assert_eq!(show(&d), "+mostly uses -sparsity +structure");
    }

    #[test]
    fn a_formula_is_one_token_and_a_wikilink_is_its_text() {
        assert_eq!(tokens("so $x + y$, and [[A|shown]] [[B]]"), vec!["so", "$x + y$,", "and", "shown", "B"]);
        let d = word_diff("take $x + y$ here", "take $x - y$ here");
        assert_eq!(show(&d), "take -$x + y$ +$x - y$ here");
    }

    #[test]
    fn a_text_diff_names_the_changed_lines_of_the_new_text() {
        let old = "# T\n\nfirst line\nsecond line\nthird\n";
        let new = "# T\n\nfirst line\nsecond new line\ninserted para\nthird\n";
        let d = text_diff(old, new);
        assert_eq!(d.keys().copied().collect::<Vec<_>>(), vec![4, 5]);
        assert_eq!(show(&d[&4]), "second +new line");
        assert_eq!(show(&d[&5]), "+inserted +para");
        assert!(text_diff(new, new).is_empty());
    }
}
