// Diagnostics: feedback addressed by a code and anchored to a live line of a note.
//
// The format is not ours to invent — it already exists and is shared with Neovim
// (vim.diagnostic), the writing-diagnostics skill and vault-phone:
//
//   <vault>/.claude/diagnostics.json :  "Rel/Path.md" -> [ { code, line, col, severity,
//                                        message, detail?, fix?[{start_line,end_line,
//                                        old_text,new_text}] } ]
//
// Facet joins that contract and adds the two things an editor cannot do: it renders `detail`
// as markdown with real math, and it lets a diagnostic be triaged from a phone. A triage is
// an action: it changes the note or the memory file, and it tells the conversation what it did.
use crate::cfg::Cfg;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub struct Diag {
    pub note: String,            // vault-relative path
    pub code: String,
    pub line: i64,
    pub severity: String,
    pub message: String,
    pub detail: Option<String>,
    pub fixes: usize,
}

impl Diag {
    pub fn rank(&self) -> u8 {
        match self.severity.as_str() { "error" => 0, "warn" => 1, "info" => 2, _ => 3 }
    }
}

pub fn file(cfg: &Cfg) -> PathBuf { cfg.vault().join(".claude/diagnostics.json") }

pub fn load(cfg: &Cfg) -> Value {
    std::fs::read_to_string(file(cfg)).ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Object(Default::default()))
}

fn of(note: &str, v: &Value) -> Diag {
    Diag {
        note: note.to_string(),
        code: v.get("code").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        line: v.get("line").and_then(|x| x.as_i64()).unwrap_or(1),
        severity: v.get("severity").and_then(|x| x.as_str()).unwrap_or("warn").to_string(),
        message: v.get("message").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        detail: v.get("detail").and_then(|x| x.as_str()).map(|s| s.to_string()),
        fixes: v.get("fix").and_then(|x| x.as_array()).map(|a| a.len()).unwrap_or(0),
    }
}

/// Every open diagnostic, worst first.
pub fn all(cfg: &Cfg) -> Vec<Diag> {
    let data = load(cfg);
    let mut out = Vec::new();
    for (note, list) in data.as_object().into_iter().flatten() {
        for d in list.as_array().into_iter().flatten() { out.push(of(note, d)); }
    }
    out.sort_by(|a, b| a.rank().cmp(&b.rank()).then(a.note.cmp(&b.note)).then(a.line.cmp(&b.line)));
    out
}

/// The vault-relative key for an absolute path, which is how the JSON addresses notes.
pub fn rel(cfg: &Cfg, path: &Path) -> String {
    path.strip_prefix(cfg.vault()).unwrap_or(path).to_string_lossy().to_string()
}

pub fn for_note(cfg: &Cfg, path: &Path) -> Vec<Diag> {
    let note = rel(cfg, path);
    let data = load(cfg);
    let mut out: Vec<Diag> = data.get(&note).and_then(|l| l.as_array())
        .map(|a| a.iter().map(|d| of(&note, d)).collect()).unwrap_or_default();
    out.sort_by_key(|d| d.line);
    out
}

pub fn find(cfg: &Cfg, code: &str) -> Option<Diag> {
    all(cfg).into_iter().find(|d| d.code == code)
}

/// A few lines of the note around a diagnostic — what it is actually talking about.
pub fn context(cfg: &Cfg, d: &Diag, span: i64) -> String {
    let text = std::fs::read_to_string(cfg.vault().join(&d.note)).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let lo = (d.line - 1 - span).max(0) as usize;
    let hi = ((d.line - 1 + span) as usize + 1).min(lines.len());
    lines[lo.min(lines.len())..hi].join("\n")
}

/// Apply the fix. Bottom-up, with a stale-text guard, and every surviving diagnostic's line
/// shifted by what the edit did — this bookkeeping is the whole reason the loop stays usable
/// after a fix lands. (Ported from vault-phone, the one piece of it worth keeping verbatim.)
pub fn apply(cfg: &Cfg, code: &str) -> Result<String, String> {
    let mut data = load(cfg);
    let (note, idx) = locate(&data, code).ok_or("that diagnostic is gone")?;
    let entry = data[&note][idx].clone();
    let path = cfg.vault().join(&note);
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();

    let mut fixes: Vec<Value> = entry.get("fix").and_then(|f| f.as_array()).cloned().unwrap_or_default();
    if fixes.is_empty() { return Err("that diagnostic has no fix".into()) }
    fixes.sort_by_key(|f| -(f["start_line"].as_i64().unwrap_or(0)));

    for f in &fixes {
        let (a, b) = (f["start_line"].as_i64().unwrap_or(0), f["end_line"].as_i64().unwrap_or(0));
        if a < 1 || b as usize > lines.len() { return Err(format!("L{}-{} is outside the note now", a, b)) }
        if lines[(a - 1) as usize..b as usize].join("\n") != f["old_text"].as_str().unwrap_or("") {
            return Err(format!("L{}-{} changed since the review; not applying", a, b));
        }
    }

    // drop the applied entry, then edit bottom-up, shifting what remains
    let mut rest: Vec<Value> = data[&note].as_array().cloned().unwrap_or_default();
    rest.remove(idx);
    for f in &fixes {
        let (a, b) = (f["start_line"].as_i64().unwrap_or(0), f["end_line"].as_i64().unwrap_or(0));
        let new: Vec<String> = f["new_text"].as_str().unwrap_or("").split('\n').map(|s| s.to_string()).collect();
        let shift = new.len() as i64 - (b - a + 1);
        lines.splice((a - 1) as usize..b as usize, new);
        for other in rest.iter_mut() {
            if other["line"].as_i64().unwrap_or(0) > b {
                let v = other["line"].as_i64().unwrap_or(0) + shift;
                other["line"] = Value::from(v);
            }
            for g in other.get_mut("fix").and_then(|x| x.as_array_mut()).into_iter().flatten() {
                if g["start_line"].as_i64().unwrap_or(0) > b {
                    let s = g["start_line"].as_i64().unwrap_or(0) + shift;
                    let e = g["end_line"].as_i64().unwrap_or(0) + shift;
                    g["start_line"] = Value::from(s);
                    g["end_line"] = Value::from(e);
                }
            }
        }
    }
    crate::doc::write(&path, &lines.join("\n"))?;
    put(cfg, &mut data, &note, rest);
    Ok(format!("applied {} in {}", code, note))
}

/// A reply that offers a concrete replacement for the line a card was about becomes a fix —
/// the same `fix` a review writes, so applying it gets the stale-text guard and the line-shift
/// bookkeeping `apply` already does, and the same `/x/diag` button triages it. `name` is a
/// note by its own name (what a card's `data-note` carries), resolved the way a wikilink is.
pub fn propose(cfg: &Cfg, name: &str, line: i64, replacement: &str) -> Result<String, String> {
    let d = crate::doc::note(cfg, name).ok_or_else(|| format!("no note '{}'", name))?;
    let note = rel(cfg, &d.path);
    let lines: Vec<&str> = d.text.split('\n').collect();
    if line < 1 || line as usize > lines.len() { return Err(format!("L{} is outside {}", line, note)) }
    let old = lines[(line - 1) as usize].to_string();
    let mut data = load(cfg);
    let taken: Vec<String> = all(cfg).into_iter().map(|x| x.code).collect();
    let code = code_for(&taken, 0);
    let mut list: Vec<Value> = data.get(&note).and_then(|l| l.as_array()).cloned().unwrap_or_default();
    list.push(serde_json::json!({
        "code": code, "line": line, "col": 1, "severity": "info", "message": "a proposed replacement",
        "fix": [{"start_line": line, "end_line": line, "old_text": old, "new_text": replacement}],
    }));
    put(cfg, &mut data, &note, list);
    Ok(code)
}

/// Dismiss: the objection is logged where the next review will read it, so it stops coming back.
pub fn dismiss(cfg: &Cfg, code: &str, reason: &str) -> Result<String, String> {
    let mut data = load(cfg);
    let (note, idx) = locate(&data, code).ok_or("that diagnostic is gone")?;
    let d = of(&note, &data[&note][idx]);
    let mut rest: Vec<Value> = data[&note].as_array().cloned().unwrap_or_default();
    rest.remove(idx);
    put(cfg, &mut data, &note, rest);

    // The same shape the editor's dismissals have, so the next review reads one format.
    let memory = cfg.vault().join(cfg.str("review_memory", "LLM/Review memory.md"));
    let quote = context(cfg, &d, 0).trim().chars().take(160).collect::<String>();
    let line = format!("- [[{}]]: rejected \"{}\"{} at «{}»\n",
        note.trim_end_matches(".md"), d.message,
        if reason.is_empty() { String::new() } else { format!(" — {}", reason) }, quote);
    let old = std::fs::read_to_string(&memory).unwrap_or_default();
    let _ = crate::doc::write(&memory, &(old + &line));
    Ok(format!("dismissed {} in {}", code, note))
}

fn locate(data: &Value, code: &str) -> Option<(String, usize)> {
    for (note, list) in data.as_object()? {
        for (i, d) in list.as_array()?.iter().enumerate() {
            if d.get("code").and_then(|c| c.as_str()) == Some(code) {
                return Some((note.clone(), i));
            }
        }
    }
    None
}

fn put(cfg: &Cfg, data: &mut Value, note: &str, rest: Vec<Value>) {
    if let Some(m) = data.as_object_mut() {
        if rest.is_empty() { m.remove(note); } else { m.insert(note.into(), Value::Array(rest)); }
    }
    if let Some(d) = file(cfg).parent() { let _ = std::fs::create_dir_all(d); }
    let _ = crate::doc::write(&file(cfg),
        &(serde_json::to_string_pretty(data).unwrap_or_default() + "\n"));
}

/// One line per diagnostic, for the CLI and for Telegram.
pub fn brief(cfg: &Cfg) -> String {
    let ds = all(cfg);
    if ds.is_empty() { return "no open diagnostics".into() }
    let mut out = format!("{} open:\n", ds.len());
    for d in ds.iter().take(20) {
        out.push_str(&format!("{} {} L{} · {}{}\n", d.severity, d.note.trim_end_matches(".md"),
            d.line, d.message, if d.fixes > 0 { " [fix]" } else { "" }));
    }
    out
}

// ---- writing diagnostics ----------------------------------------------------------------
//
// Forming a diagnostic is an output like any other, and it should cost one call, not a
// sequence of them. Hand-writing the JSON means finding a line number, copying the line
// verbatim for the stale guard, inventing a unique code and escaping markdown into JSON —
// four chances to be wrong, all of them mechanical. So the spec is declarative and anchored
// by *text*; the code does the rest, reading the file to get `old_text` exactly right:
//
//     @ the table is what is wired, not what is possible.     <- verbatim, must be unique
//     ! warn  the sentence is doing two jobs                  <- severity + message
//     ? Keep the claim, drop the excuse. Math is fine: $a \preceq b$.
//     + artifact knows about neither renderer nor form.       <- the replacement line(s)
//
// `?` and `+` may run over several lines. Repeat the block for each comment.

pub struct Draft {
    pub anchor: String,
    pub severity: String,
    pub message: String,
    pub detail: String,
    pub fix: String,
}

pub fn parse_spec(spec: &str) -> Result<Vec<Draft>, String> {
    let mut out: Vec<Draft> = Vec::new();
    let mut mode = ' ';
    for raw in spec.lines() {
        let (m, rest) = match raw.chars().next() {
            Some(c @ ('@' | '!' | '?' | '+')) => (c, raw[1..].trim_start().to_string()),
            _ => (' ', raw.to_string()),
        };
        if m == '@' {
            out.push(Draft { anchor: rest, severity: "warn".into(), message: String::new(),
                             detail: String::new(), fix: String::new() });
            mode = '@';
            continue;
        }
        let d = out.last_mut().ok_or("the spec must start with an @anchor line")?;
        match m {
            '!' => {
                let (first, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest.as_str(), ""));
                if ["error", "warn", "info", "hint"].contains(&first) {
                    d.severity = first.to_string();
                    d.message = tail.trim().to_string();
                } else { d.message = rest.clone(); }
                mode = '!';
            }
            '?' => { d.detail = rest; mode = '?' }
            '+' => { d.fix = rest; mode = '+' }
            _ => match mode {   // a continuation line belongs to whichever block is open
                '?' => { d.detail.push('\n'); d.detail.push_str(&rest) }
                '+' => { d.fix.push('\n'); d.fix.push_str(&rest) }
                '!' => { d.message.push(' '); d.message.push_str(rest.trim()) }
                _ => {}
            },
        }
    }
    if out.is_empty() { return Err("empty spec".into()) }
    Ok(out)
}

/// Resolve what a note was called on the command line: a slug, a vault-relative path, or a path.
pub fn resolve(cfg: &Cfg, name: &str) -> Result<String, String> {
    if let Some(d) = crate::doc::get(cfg, name) { return Ok(rel(cfg, &d.path)) }
    let p = PathBuf::from(name);
    if p.is_file() {
        let abs = p.canonicalize().map_err(|e| e.to_string())?;
        return Ok(rel(cfg, &abs));
    }
    if cfg.vault().join(name).is_file() { return Ok(name.to_string()) }
    Err(format!("no note '{}'", name))
}

/// Whitespace-collapsed text, plus the source line of every *byte* of it — byte offsets,
/// because that is what `match_indices` reports and the notes are full of em dashes.
fn normalize(text: &str) -> (String, Vec<usize>) {
    let (mut out, mut at) = (String::new(), Vec::new());
    let mut line = 0usize;
    let mut space = true;        // leading whitespace is dropped
    for c in text.chars() {
        if c.is_whitespace() {
            if !space { out.push(' '); at.push(line) }
            space = true;
        } else {
            out.push(c);
            for _ in 0..c.len_utf8() { at.push(line) }
            space = false;
        }
        if c == '\n' { line += 1 }
    }
    at.push(line);
    (out, at)
}

fn code_for(taken: &[String], n: usize) -> String {
    let abc = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64).unwrap_or(1) ^ ((n as u64 + 1) * 0x9E37_79B9_7F4A_7C15);
    loop {
        let mut c = String::new();
        for _ in 0..4 {
            seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17;
            c.push(abc[(seed % abc.len() as u64) as usize] as char);
        }
        if !taken.contains(&c) { return c }
    }
}

/// Write a batch of comments onto one note. One call, one event.
pub fn review(cfg: &Cfg, name: &str, spec: &str, replace: bool) -> Result<String, String> {
    let note = resolve(cfg, name)?;
    let drafts = parse_spec(spec)?;
    let text = std::fs::read_to_string(cfg.vault().join(&note)).map_err(|e| e.to_string())?;
    let lines: Vec<&str> = text.split('\n').collect();

    let mut data = load(cfg);
    let mut list: Vec<Value> = if replace { Vec::new() }
        else { data.get(&note).and_then(|l| l.as_array()).cloned().unwrap_or_default() };
    let mut taken: Vec<String> = all(cfg).into_iter().map(|d| d.code).collect();

    let (norm, at) = normalize(&text);
    for (n, d) in drafts.iter().enumerate() {
        // Anchors are matched on whitespace-collapsed text, so a sentence that happens to wrap
        // still matches: where the note breaks its lines is not something I should have to know.
        let (a, _) = normalize(&d.anchor);
        let a = a.trim();
        if a.is_empty() { return Err("an @anchor is empty".into()) }
        let hits: Vec<usize> = norm.match_indices(a).map(|(i, _)| i).collect();
        match hits.len() {
            0 => return Err(format!("anchor not in {}: {:?}", note, d.anchor.trim())),
            1 => {}
            k => return Err(format!("anchor occurs {} times in {}: {:?}", k, note, d.anchor.trim())),
        }
        let first = at[hits[0].min(at.len() - 1)];
        let last = at[(hits[0] + a.len()).min(at.len() - 1)].max(first);
        let code = code_for(&taken, n);
        taken.push(code.clone());
        let mut e = serde_json::json!({
            "code": code, "line": first as i64 + 1, "col": 1,
            "severity": d.severity, "message": d.message,
        });
        if !d.detail.trim().is_empty() { e["detail"] = Value::from(d.detail.trim()); }
        if !d.fix.trim().is_empty() {
            // the fix replaces whole lines, and old_text comes from the file, never from the
            // model: the stale guard is right by construction instead of by transcription
            e["fix"] = serde_json::json!([{ "start_line": first as i64 + 1, "end_line": last as i64 + 1,
                "old_text": lines[first..=last].join("\n"), "new_text": d.fix.trim_end() }]);
        }
        list.push(e);
    }
    let n = list.len();
    put(cfg, &mut data, &note, list);
    Ok(format!("{} comment(s) on {}", n, note))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault(name: &str, text: &str) -> Cfg {
        let d = std::env::temp_dir().join(format!("facet-diag-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".claude")).unwrap();
        std::fs::write(d.join("Note.md"), text).unwrap();
        Cfg(serde_json::json!({"vault": d.to_string_lossy()}))
    }

    #[test]
    fn a_proposed_replacement_is_a_fix_apply_can_take() {
        let cfg = vault("propose", "one\ntwo\nthree\n");
        let code = propose(&cfg, "Note", 2, "replaced").unwrap();
        let d = for_note(&cfg, &cfg.vault().join("Note.md"));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].code, code);
        assert_eq!(d[0].severity, "info");
        assert_eq!(apply(&cfg, &code).unwrap(), format!("applied {} in Note.md", code));
        assert_eq!(std::fs::read_to_string(cfg.vault().join("Note.md")).unwrap(), "one\nreplaced\nthree\n");
        assert!(propose(&cfg, "Note", 99, "x").is_err());
        assert!(propose(&cfg, "No such note", 1, "x").is_err());
    }
}
