// Cards: everything said about a line of a note, by either side, in one place.
//
// There used to be two of these. A review wrote *diagnostics* into the vault's
// `.claude/diagnostics.json` (severity, message, fix; triaged by apply/dismiss/discuss), and a
// double-click in the viewer opened a *comment card* whose words lived in the log and whose
// answers lived in the engine's `cards.json`. They were the same thing seen from two ends — a
// conversation anchored to a line — so now there is one:
//
//   Card { id, note, line, end, quote, word?, col?, kind, by, thread, fix, closed, applied?, at }
//     note    vault-relative path of the note ("Folder/Note.md")
//     line    1-based first line it is about; `end` the last (a review anchor can span lines)
//     quote   the line as it read when the card was opened (shown dim at the top)
//     kind    comment | info | warn | error — a colour, nothing else
//     by      who opened it: "user" (a double-click) or "server" (the agent, a review)
//     thread  [{by: "user"|"server", text, at}] in order; the composer always follows it
//     fix     null, or the line edits an Apply button makes:
//             [{start_line, end_line, old_text, new_text}] (old_text read from the file, never
//             typed, so the stale-text guard is right by construction)
//     closed  the X: gone from every page, kept on file (`delete` removes it outright)
//
// Stored in `<vault>/.claude/cards.json` as {"cards": [..]}, beside the notes it points into
// (that directory is git-ignored in the vault). Every change takes `cards.lock`, reads the
// file, changes it, writes it back whole and atomically — cards are few — and any failure to
// do so is an error the caller sees: a card is never dropped silently.
//
// Migration is on load and idempotent: anything in the old `diagnostics.json` (which an editor
// or a review skill may still write) is absorbed as server cards and the file left as `{}`; the
// engine's old `cards.json` (state directory) is rebuilt into threads from the log once, then
// renamed `cards.json.migrated`.
use crate::cfg::Cfg;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};


pub fn file(cfg: &Cfg) -> PathBuf { cfg.vault().join(".claude/cards.json") }
fn legacy_diag(cfg: &Cfg) -> PathBuf { cfg.vault().join(".claude/diagnostics.json") }
fn legacy_cards(cfg: &Cfg) -> PathBuf {
    crate::optchat::engine::state_dir(&cfg.store()).join("cards.json")
}

/// Changes whenever any card does: what a page waiting for news compares against.
pub fn version(cfg: &Cfg) -> u64 {
    std::fs::metadata(file(cfg)).ok().and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_micros() as u64).unwrap_or(0)
}

fn now() -> String { crate::optchat::store::now_iso() }

fn kind_of(s: &str) -> String {
    match s.trim() { "error" => "error", "warn" | "warning" => "warn", "info" | "hint" => "info", _ => "comment" }.into()
}

fn read(p: &Path) -> Result<Option<Value>, String> {
    match std::fs::read_to_string(p) {
        Ok(s) if s.trim().is_empty() => Ok(None),
        Ok(s) => serde_json::from_str(&s).map(Some).map_err(|e| format!("{} is not JSON: {}", p.display(), e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {}", p.display(), e)),
    }
}

fn write(p: &Path, text: &str) -> Result<(), String> {
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {}", tmp.display(), e))?;
    std::fs::rename(&tmp, p).map_err(|e| format!("cannot write {}: {}", p.display(), e))
}

/// The one way the file is touched: under the lock, read, `f`, and written back if `f`
/// changed anything. Errors (no vault, a directory that cannot be made, a full disk) come back.
pub fn with<R>(cfg: &Cfg, f: impl FnOnce(&mut Vec<Value>) -> Result<R, String>) -> Result<R, String> {
    let dir = file(cfg).parent().unwrap().to_path_buf();
    if !cfg.vault().is_dir() { return Err(format!("no vault at {}", cfg.vault().display())) }
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot make {}: {}", dir.display(), e))?;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("cards.lock"))
        .map_err(|e| format!("cannot lock {}: {}", dir.display(), e))?;
    lock.lock().map_err(|e| format!("cannot lock {}: {}", dir.display(), e))?;
    let mut cards: Vec<Value> = read(&file(cfg))?
        .and_then(|v| v.get("cards").and_then(|c| c.as_array()).cloned()).unwrap_or_default();
    let before = Value::Array(cards.clone());
    let mut absorbed = migrate_diagnostics(cfg, &mut cards)?;
    absorbed |= migrate_legacy_cards(cfg, &mut cards);
    let r = f(&mut cards)?;
    if absorbed || Value::Array(cards.clone()) != before {
        write(&file(cfg), &(serde_json::to_string_pretty(&json!({"cards": cards})).unwrap() + "\n"))?;
    }
    if absorbed { let _ = write(&legacy_diag(cfg), "{}\n"); }
    Ok(r)
}

/// Every card, closed ones too.
pub fn all(cfg: &Cfg) -> Vec<Value> {
    // a read that cannot take the lock (no vault) still answers: there are no cards
    with(cfg, |c| Ok(c.clone())).unwrap_or_default()
}

/// The open ones, worst first, then by note and line.
pub fn open(cfg: &Cfg) -> Vec<Value> {
    let rank = |c: &Value| match c["kind"].as_str().unwrap_or("") { "error" => 0, "warn" => 1, "info" => 2, _ => 3 };
    let mut v: Vec<Value> = all(cfg).into_iter().filter(|c| c["closed"] != true).collect();
    v.sort_by(|a, b| rank(a).cmp(&rank(b)).then(a["note"].as_str().cmp(&b["note"].as_str()))
        .then(a["line"].as_i64().cmp(&b["line"].as_i64())));
    v
}

pub fn get(cfg: &Cfg, id: &str) -> Option<Value> { all(cfg).into_iter().find(|c| c["id"] == id) }

/// The note's own name, the key a page tags its lines with (`data-note`).
pub fn stem(note: &str) -> String {
    Path::new(note).file_stem().unwrap_or_default().to_string_lossy().to_string()
}

fn find<'a>(cards: &'a mut [Value], id: &str) -> Result<&'a mut Value, String> {
    cards.iter_mut().find(|c| c["id"] == id).ok_or_else(|| format!("no card {}", id))
}

/// The vault-relative path for whatever names a note: a published slug, a path, a relative
/// path, or (what a page and a wikilink carry) its own name.
pub fn resolve(cfg: &Cfg, name: &str) -> Result<String, String> {
    let name = name.trim().trim_start_matches("[[").trim_end_matches("]]");
    if name.is_empty() { return Err("no note given".into()) }
    let rel = |p: &Path| p.strip_prefix(cfg.vault()).unwrap_or(p).to_string_lossy().to_string();
    if cfg.vault().join(name).is_file() { return Ok(name.to_string()) }
    let p = PathBuf::from(name);
    if p.is_absolute() && p.is_file() { return Ok(rel(&p)) }
    if let Some(d) = crate::doc::get(cfg, name) { return Ok(rel(&d.path)) }
    if let Some(p) = crate::doc::find(cfg, name.trim_end_matches(".md")) { return Ok(rel(&p)) }
    Err(format!("no note '{}' in {}", name, cfg.vault().display()))
}

fn lines_of(cfg: &Cfg, note: &str) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(cfg.vault().join(note)).map_err(|e| format!("{}: {}", note, e))?;
    Ok(text.split('\n').map(String::from).collect())
}

fn new_id(taken: &[Value]) -> String {
    let abc = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64).unwrap_or(1) ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    loop {
        let mut c = String::from("s");
        for _ in 0..6 {
            seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17;
            c.push(abc[(seed % abc.len() as u64) as usize] as char);
        }
        if !taken.iter().any(|x| x["id"] == c.as_str()) { return c }
    }
}

fn valid_id(id: &str) -> bool { !id.is_empty() && id.len() <= 40 && id.chars().all(|c| c.is_ascii_alphanumeric()) }

/// Where a card points.
pub enum At { Line(i64), Text(String) }

/// The lines (first, last; 1-based) an anchor names: a line number, or verbatim text that must
/// occur exactly once in the note (whitespace-collapsed, so a wrapped sentence still matches).
fn locate(lines: &[String], at: &At) -> Result<(i64, i64), String> {
    match at {
        At::Line(n) => {
            if *n < 1 || *n as usize > lines.len() { return Err(format!("L{} is outside the note ({} lines)", n, lines.len())) }
            Ok((*n, *n))
        }
        At::Text(a) => {
            let text = lines.join("\n");
            let (norm, pos) = normalize(&text);
            let (a, _) = normalize(a);
            let a = a.trim();
            if a.is_empty() { return Err("the anchor is empty".into()) }
            let hits: Vec<usize> = norm.match_indices(a).map(|(i, _)| i).collect();
            match hits.len() {
                0 => Err(format!("anchor not in the note: {:?}", a)),
                1 => {
                    let first = pos[hits[0].min(pos.len() - 1)];
                    let last = pos[(hits[0] + a.len() - 1).min(pos.len() - 1)].max(first);
                    Ok((first as i64 + 1, last as i64 + 1))
                }
                k => Err(format!("anchor occurs {} times in the note: {:?}", k, a)),
            }
        }
    }
}

/// Whitespace-collapsed text, plus the source line of every *byte* of it.
fn normalize(text: &str) -> (String, Vec<usize>) {
    let (mut out, mut at) = (String::new(), Vec::new());
    let (mut line, mut space) = (0usize, true);
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

fn quote_of(lines: &[String], a: i64, b: i64) -> String {
    let q = lines[(a - 1) as usize..b as usize].join(" ");
    q.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(160).collect()
}

fn edit_for(lines: &[String], a: i64, b: i64, new_text: &str) -> Value {
    json!([{"start_line": a, "end_line": b, "old_text": lines[(a - 1) as usize..b as usize].join("\n"),
            "new_text": new_text.trim_end_matches('\n')}])
}

/// What a new card needs.
pub struct New<'a> {
    pub id: Option<&'a str>,
    pub note: &'a str,
    pub at: At,
    pub text: &'a str,
    pub kind: &'a str,
    pub fix: Option<&'a str>,
    pub by: &'a str,
    /// the quote as the page showed it (a user's card), else read from the note
    pub quote: Option<&'a str>,
    /// the word clicked and its character offset in the line (a user's card; old cards have neither)
    pub word: Option<&'a str>,
    pub col: Option<i64>,
}

fn make(cfg: &Cfg, cards: &[Value], n: &New) -> Result<Value, String> {
    let note = resolve(cfg, n.note)?;
    let lines = lines_of(cfg, &note)?;
    let (a, b) = locate(&lines, &n.at)?;
    let id = match n.id {
        Some(i) if !valid_id(i) => return Err(format!("bad card id {:?}", i)),
        Some(i) if cards.iter().any(|c| c["id"] == i) => return Err(format!("card {} exists", i)),
        Some(i) => i.to_string(),
        None => new_id(cards),
    };
    let by = if n.by == "user" { "user" } else { "server" };
    let mut c = json!({
        "id": id, "note": note, "line": a, "end": b,
        "quote": n.quote.map(String::from).unwrap_or_else(|| quote_of(&lines, a, b)),
        "kind": kind_of(n.kind), "by": by, "thread": [], "fix": null, "closed": false, "at": now(),
    });
    if let Some(w) = n.word.filter(|w| !w.is_empty()) { c["word"] = json!(w); }
    if let Some(k) = n.col { c["col"] = json!(k); }
    if !n.text.trim().is_empty() {
        c["thread"] = json!([{"by": by, "text": n.text.trim(), "at": now()}]);
    }
    if let Some(f) = n.fix.filter(|f| !f.trim().is_empty()) { c["fix"] = edit_for(&lines, a, b, f); }
    Ok(c)
}

pub fn create(cfg: &Cfg, n: New) -> Result<Value, String> {
    with(cfg, |cards| { let c = make(cfg, cards, &n)?; cards.push(c.clone()); Ok(c) })
}

/// Add to a card's thread; returns the message's index in it. A closed card that gets a new
/// message is open again: someone is still talking about it.
pub fn say(cfg: &Cfg, id: &str, by: &str, text: &str) -> Result<usize, String> {
    let text = text.trim();
    if text.is_empty() { return Err("empty".into()) }
    with(cfg, |cards| {
        let c = find(cards, id)?;
        let t = c["thread"].as_array_mut().ok_or("a card without a thread")?;
        t.push(json!({"by": if by == "user" { "user" } else { "server" }, "text": text, "at": now()}));
        let k = t.len() - 1;
        c["closed"] = json!(false);
        Ok(k)
    })
}

/// Take back message `k` (a send that never reached the conversation), and the card with it if
/// that leaves a user's card empty.
pub fn unsay(cfg: &Cfg, id: &str, k: usize) -> Result<(), String> {
    with(cfg, |cards| {
        let i = cards.iter().position(|c| c["id"] == id).ok_or_else(|| format!("no card {}", id))?;
        if let Some(t) = cards[i]["thread"].as_array_mut() { if k < t.len() { t.remove(k); } }
        if cards[i]["thread"].as_array().is_some_and(|t| t.is_empty()) && cards[i]["by"] == "user" { cards.remove(i); }
        Ok(())
    })
}

/// Set (or with `None`/empty, take off) the replacement for the card's own lines.
pub fn set_fix(cfg: &Cfg, id: &str, text: Option<&str>) -> Result<(), String> {
    with(cfg, |cards| {
        let c = find(cards, id)?;
        match text.filter(|t| !t.trim().is_empty()) {
            None => c["fix"] = Value::Null,
            Some(t) => {
                let note = c["note"].as_str().unwrap_or("").to_string();
                let lines = lines_of(cfg, &note)?;
                let a = c["line"].as_i64().unwrap_or(0);
                let b = c["end"].as_i64().unwrap_or(a).max(a);
                if a < 1 || b as usize > lines.len() { return Err(format!("L{}-{} is outside {}", a, b, note)) }
                c["fix"] = edit_for(&lines, a, b, t);
            }
        }
        Ok(())
    })
}

pub fn set_kind(cfg: &Cfg, id: &str, kind: &str) -> Result<(), String> {
    with(cfg, |cards| { find(cards, id)?["kind"] = json!(kind_of(kind)); Ok(()) })
}

/// The X. A server's card closed without its fix applied is an objection, and is written where
/// the next review reads it (`review_memory`), so the same comment stops coming back.
pub fn close(cfg: &Cfg, id: &str, reason: &str) -> Result<String, String> {
    let c = with(cfg, |cards| { let c = find(cards, id)?; c["closed"] = json!(true); Ok(c.clone()) })?;
    if c["by"] == "server" && c["kind"] != "comment" && c["applied"] != true {
        let memory = cfg.vault().join(cfg.str("review_memory", "LLM/Review memory.md"));
        let first = c["thread"][0]["text"].as_str().unwrap_or("").lines().next().unwrap_or("").to_string();
        let line = format!("- [[{}]]: rejected \"{}\"{} at «{}»\n",
            c["note"].as_str().unwrap_or("").trim_end_matches(".md"), first,
            if reason.trim().is_empty() { String::new() } else { format!(" — {}", reason.trim()) },
            c["quote"].as_str().unwrap_or(""));
        if let Some(d) = memory.parent() { let _ = std::fs::create_dir_all(d); }
        let old = std::fs::read_to_string(&memory).unwrap_or_default();
        let _ = crate::doc::write(&memory, &(old + &line));
    }
    Ok(format!("closed {}", id))
}

pub fn delete(cfg: &Cfg, id: &str) -> Result<String, String> {
    with(cfg, |cards| {
        let n = cards.len();
        cards.retain(|c| c["id"] != id);
        if cards.len() == n { Err(format!("no card {}", id)) } else { Ok(format!("deleted {}", id)) }
    })
}

/// The Apply button: the fix goes into the note bottom-up, behind a stale-text guard, and every
/// other card on the note has its lines shifted by what the edit did (ported from vault-phone).
/// The card is then closed, as applied.
pub fn apply(cfg: &Cfg, id: &str) -> Result<String, String> {
    with(cfg, |cards| {
        let c = find(cards, id)?.clone();
        let note = c["note"].as_str().unwrap_or("").to_string();
        let mut fixes: Vec<Value> = c["fix"].as_array().cloned().unwrap_or_default();
        if fixes.is_empty() { return Err(format!("card {} has no fix", id)) }
        let path = cfg.vault().join(&note);
        let mut lines = lines_of(cfg, &note)?;
        fixes.sort_by_key(|f| -(f["start_line"].as_i64().unwrap_or(0)));
        for f in &fixes {
            let (a, b) = (f["start_line"].as_i64().unwrap_or(0), f["end_line"].as_i64().unwrap_or(0));
            if a < 1 || b < a || b as usize > lines.len() { return Err(format!("L{}-{} is outside the note now", a, b)) }
            if lines[(a - 1) as usize..b as usize].join("\n") != f["old_text"].as_str().unwrap_or("") {
                return Err(format!("L{}-{} changed since the fix was written; not applying", a, b));
            }
        }
        for f in &fixes {
            let (a, b) = (f["start_line"].as_i64().unwrap_or(0), f["end_line"].as_i64().unwrap_or(0));
            let new: Vec<String> = f["new_text"].as_str().unwrap_or("").split('\n').map(String::from).collect();
            let shift = new.len() as i64 - (b - a + 1);
            lines.splice((a - 1) as usize..b as usize, new);
            for o in cards.iter_mut().filter(|o| o["note"] == note.as_str() && o["id"] != id) {
                for k in ["line", "end"] {
                    if let Some(v) = o[k].as_i64() { if v > b { o[k] = json!(v + shift) } }
                }
                for g in o.get_mut("fix").and_then(|x| x.as_array_mut()).into_iter().flatten() {
                    if g["start_line"].as_i64().unwrap_or(0) > b {
                        let (s, e) = (g["start_line"].as_i64().unwrap_or(0), g["end_line"].as_i64().unwrap_or(0));
                        g["start_line"] = json!(s + shift);
                        g["end_line"] = json!(e + shift);
                    }
                }
            }
        }
        crate::doc::write(&path, &lines.join("\n"))?;
        let c = find(cards, id)?;
        c["closed"] = json!(true);
        c["applied"] = json!(true);
        Ok(format!("applied {} in {}", id, note))
    })
}

/// One line per open card, for the CLI, Telegram and the agent's `list_cards`.
pub fn brief(cfg: &Cfg, note: Option<&str>) -> String {
    let want = note.map(|n| resolve(cfg, n).unwrap_or_else(|_| n.to_string()));
    let cs: Vec<Value> = open(cfg).into_iter().filter(|c| want.as_deref().is_none_or(|w| c["note"] == w)).collect();
    if cs.is_empty() { return "no open cards".into() }
    let mut out = format!("{} open:\n", cs.len());
    for c in cs.iter().take(40) {
        let t = c["thread"].as_array().cloned().unwrap_or_default();
        let last = t.last().map(|m| format!("{}: {}", m["by"].as_str().unwrap_or(""),
            m["text"].as_str().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ").chars().take(90).collect::<String>()))
            .unwrap_or_default();
        out.push_str(&format!("#{} {} [[{}]] L{} ({} msg{}) {}{}\n", c["id"].as_str().unwrap_or(""),
            c["kind"].as_str().unwrap_or(""), stem(c["note"].as_str().unwrap_or("")), c["line"],
            t.len(), if t.len() == 1 { "" } else { "s" }, last, if c["fix"].is_array() { " [fix]" } else { "" }));
    }
    out
}

/// What the conversation is told when the user writes on a card: the card's address (the shape
/// `from_card` reads, so the reply goes to the card and not the chat) and, on a card the agent
/// opened, what it had said — the user is answering that, and the view may not show it.
pub fn message(c: &Value, text: &str) -> String {
    let mut s = format!("[[{}]] L{} #{}: \"{}\"", stem(c["note"].as_str().unwrap_or("")), c["line"],
        c["id"].as_str().unwrap_or(""), c["quote"].as_str().unwrap_or(""));
    if c["by"] == "server" {
        if let Some(first) = c["thread"][0]["text"].as_str() {
            let f: String = first.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(300).collect();
            s.push_str(&format!("\n(on your {} card: {})", c["kind"].as_str().unwrap_or(""), f));
        }
    }
    s.push('\n');
    s.push_str(text.trim());
    s
}

/// The card a message came from, if it came from one: the id in `[[Note]] L<n> #<id>...`
/// (after a Telegram prelude, if one rode along). Such a message belongs to the card venue: it
/// is answered on the card, not shown in the chat, and a turn it starts gets no fallback reply.
pub fn from_card(text: &str) -> Option<&str> {
    let t = match text.find("[end prior context]\n\n") { Some(k) => &text[k + 21..], None => text };
    let t = t.trim_start().strip_prefix("[[")?;
    let t = &t[t.find("]] L")? + 4..];
    let t = t.trim_start_matches(|c: char| c.is_ascii_digit());
    let t = t.strip_prefix(" #")?;
    let n = t.find(|c: char| !c.is_ascii_alphanumeric()).unwrap_or(t.len());
    if n == 0 { None } else { Some(&t[..n]) }
}

// ---- one operation, from anywhere --------------------------------------------------------
//
// The engine (MCP tools, socket op `card`) and the CLI (when no engine is up) run the same
// function. It returns what to answer and, for whatever the agent did on a card, the line the
// stream records (kind `answer`), since an output tool's call is not itself logged.

pub fn op(cfg: &Cfg, v: &Value) -> Result<(Value, Option<String>), String> {
    let s = |k: &str| v[k].as_str().map(|x| x.to_string())
        .or_else(|| v[k].as_i64().map(|x| x.to_string())).unwrap_or_default();
    let id = s("id").trim().trim_start_matches('#').to_string();
    let fix = v["fix"].as_str().or(v["apply"].as_str()).map(str::trim).filter(|x| !x.is_empty());
    let line_of = |c: &Value, text: &str| format!("#{} on [[{}]] L{}: {}", c["id"].as_str().unwrap_or(""),
        stem(c["note"].as_str().unwrap_or("")), c["line"], text);
    match s("do").as_str() {
        "new" => {
            let at = match (v["line"].as_i64().or_else(|| s("line").parse().ok()), s("anchor")) {
                (_, a) if !a.trim().is_empty() => At::Text(a),
                (Some(n), _) => At::Line(n),
                _ => return Err("a new card needs a line or an anchor".into()),
            };
            let text = s("text");
            if text.trim().is_empty() { return Err("a new card needs text".into()) }
            let kind = s("kind");
            let c = create(cfg, New { id: None, note: &s("note"), at, text: &text, kind: &kind, fix, by: "server", quote: None, word: None, col: None })?;
            let mut l = format!("new {} card {}", c["kind"].as_str().unwrap_or(""), line_of(&c, text.trim()));
            if let Some(f) = fix { l.push_str(&format!("\n(fix offered: {})", f)); }
            Ok((json!({"ok": true, "id": c["id"], "line": c["line"]}), Some(l)))
        }
        "say" | "reply" => {
            let text = s("text");
            if id.is_empty() || (text.trim().is_empty() && fix.is_none()) { return Err("id and text required".into()) }
            let c = get(cfg, &id).ok_or_else(|| format!("no card {}", id))?;
            if let Some(f) = fix { set_fix(cfg, &id, Some(f))?; }
            if !text.trim().is_empty() { say(cfg, &id, "server", &text)?; }
            let mut l = line_of(&c, text.trim());
            if let Some(f) = fix { l.push_str(&format!("\n(fix offered, replacing the line with: {})", f)); }
            Ok((json!({"ok": true, "id": id, "fix": fix.is_some()}), Some(l)))
        }
        "fix" => {
            let c = get(cfg, &id).ok_or_else(|| format!("no card {}", id))?;
            set_fix(cfg, &id, fix.or(v["text"].as_str()))?;
            let f = fix.or(v["text"].as_str()).unwrap_or("").trim().to_string();
            Ok((json!({"ok": true, "id": id}), Some(line_of(&c, &if f.is_empty() { "(fix removed)".into() } else { format!("(fix set: {})", f) }))))
        }
        "kind" => { set_kind(cfg, &id, &s("kind"))?; Ok((json!({"ok": true}), None)) }
        "close" => {
            let c = get(cfg, &id).ok_or_else(|| format!("no card {}", id))?;
            close(cfg, &id, &s("reason"))?;
            Ok((json!({"ok": true}), Some(line_of(&c, "(closed)"))))
        }
        "delete" => {
            let c = get(cfg, &id).ok_or_else(|| format!("no card {}", id))?;
            delete(cfg, &id)?;
            Ok((json!({"ok": true}), Some(line_of(&c, "(deleted)"))))
        }
        "apply" => { let m = apply(cfg, &id)?; Ok((json!({"ok": true, "text": m}), None)) }
        "list" => {
            let n = s("note");
            Ok((json!({"ok": true, "text": brief(cfg, if n.is_empty() { None } else { Some(&n) })}), None))
        }
        other => Err(format!("unknown card action {:?} (new, reply, fix, kind, close, delete, apply, list)", other)),
    }
}

// ---- reviews: many server cards on one note, in one call ----------------------------------
//
//     @ the table is what is wired, not what is possible.     <- verbatim, must be unique
//     ! warn  the sentence is doing two jobs                  <- kind + message
//     ? Keep the claim, drop the excuse. Math is fine: $a \preceq b$.
//     + artifact knows about neither renderer nor form.       <- the replacement line(s)
//
// `?` and `+` may run over several lines. Repeat the block for each card.

pub struct Draft { pub anchor: String, pub kind: String, pub message: String, pub detail: String, pub fix: String }

pub fn parse_spec(spec: &str) -> Result<Vec<Draft>, String> {
    let mut out: Vec<Draft> = Vec::new();
    let mut mode = ' ';
    for raw in spec.lines() {
        let (m, rest) = match raw.chars().next() {
            Some(c @ ('@' | '!' | '?' | '+')) => (c, raw[1..].trim_start().to_string()),
            _ => (' ', raw.to_string()),
        };
        if m == '@' {
            out.push(Draft { anchor: rest, kind: "warn".into(), message: String::new(), detail: String::new(), fix: String::new() });
            mode = '@';
            continue;
        }
        let d = out.last_mut().ok_or("the spec must start with an @anchor line")?;
        match m {
            '!' => {
                let (first, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest.as_str(), ""));
                if ["error", "warn", "warning", "info", "hint", "comment"].contains(&first) {
                    d.kind = kind_of(first);
                    d.message = tail.trim().to_string();
                } else { d.message = rest.clone(); }
                mode = '!';
            }
            '?' => { d.detail = rest; mode = '?' }
            '+' => { d.fix = rest; mode = '+' }
            _ => match mode {
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

/// A batch of server cards on one note; all or nothing. `replace` first deletes the open cards
/// a previous review (the server) opened on that note.
pub fn review(cfg: &Cfg, name: &str, spec: &str, replace: bool) -> Result<String, String> {
    let note = resolve(cfg, name)?;
    let drafts = parse_spec(spec)?;
    with(cfg, |cards| {
        if replace { cards.retain(|c| !(c["note"] == note.as_str() && c["by"] == "server" && c["closed"] != true)); }
        let mut made = Vec::new();
        for d in &drafts {
            let text = if d.detail.trim().is_empty() { d.message.clone() } else { format!("{}\n\n{}", d.message, d.detail.trim()) };
            let fix = if d.fix.trim().is_empty() { None } else { Some(d.fix.trim_end()) };
            let mut all = cards.clone(); all.extend(made.iter().cloned());
            let c = make(cfg, &all, &New { id: None, note: &note, at: At::Text(d.anchor.clone()), text: &text,
                kind: &d.kind, fix, by: "server", quote: None, word: None, col: None })?;
            made.push(c);
        }
        let n = made.len();
        cards.extend(made);
        Ok(format!("{} card(s) on {}", n, note))
    })
}

// ---- migration ----------------------------------------------------------------------------

/// The old diagnostics file, absorbed: each item a server card (its message, then its detail).
fn migrate_diagnostics(cfg: &Cfg, cards: &mut Vec<Value>) -> Result<bool, String> {
    let Some(data) = read(&legacy_diag(cfg)).unwrap_or(None) else { return Ok(false) };
    let mut any = false;
    for (note, list) in data.as_object().into_iter().flatten() {
        for d in list.as_array().into_iter().flatten() {
            any = true;
            let code = d["code"].as_str().filter(|c| valid_id(c) && !cards.iter().any(|x| x["id"] == *c))
                .map(String::from).unwrap_or_else(|| new_id(cards));
            let line = d["line"].as_i64().unwrap_or(1);
            let end = d["fix"][0]["end_line"].as_i64().unwrap_or(line).max(line);
            let quote = std::fs::read_to_string(cfg.vault().join(note)).ok().map(|t| {
                let ls: Vec<String> = t.split('\n').map(String::from).collect();
                if line >= 1 && end as usize <= ls.len() { quote_of(&ls, line, end) } else { String::new() }
            }).unwrap_or_default();
            let mut text = d["message"].as_str().unwrap_or("").to_string();
            if let Some(det) = d["detail"].as_str().filter(|s| !s.trim().is_empty()) { text = format!("{}\n\n{}", text, det.trim()); }
            cards.push(json!({
                "id": code, "note": note, "line": line, "end": end, "quote": quote,
                "kind": kind_of(d["severity"].as_str().unwrap_or("warn")), "by": "server",
                "thread": if text.trim().is_empty() { json!([]) } else { json!([{"by": "server", "text": text, "at": now()}]) },
                "fix": d["fix"].as_array().filter(|a| !a.is_empty()).map(|a| Value::Array(a.clone())).unwrap_or(Value::Null),
                "closed": false, "at": now(),
            }));
        }
    }
    Ok(any)
}

/// The engine's old per-card file: words from the log (each a user message carrying the card's
/// id), answers from the file, put in order by how many sends each answer came after.
fn migrate_legacy_cards(cfg: &Cfg, cards: &mut Vec<Value>) -> bool {
    let p = legacy_cards(cfg);
    let Ok(Some(old)) = read(&p) else { return false };
    let msgs = crate::log::since_by(cfg, -1, |m| m.kind == "user" && from_card(&m.text).is_some());
    for (id, c) in old.as_object().into_iter().flatten() {
        if cards.iter().any(|x| x["id"] == id.as_str()) { continue }
        let name = c["note"].as_str().unwrap_or("");
        let note = resolve(cfg, name).unwrap_or_else(|_| format!("{}.md", name));
        let mut said: Vec<(String, String)> = Vec::new(); // (quote, words)
        for m in msgs.iter().filter(|m| from_card(&m.text) == Some(id.as_str())) {
            let t = m.text.find("[end prior context]\n\n").map(|k| &m.text[k + 21..]).unwrap_or(&m.text).trim_start();
            let (w, r) = t.split_once('\n').unwrap_or((t, ""));
            let q = w.split_once(": \"").map(|(_, q)| q.trim_end_matches('"').to_string()).unwrap_or_default();
            said.push((q, r.trim().to_string()));
        }
        let mut thread = Vec::new();
        let answers = c["answers"].as_array().cloned().unwrap_or_default();
        let mut ai = 0;
        for (k, (_, w)) in said.iter().enumerate() {
            if !w.is_empty() { thread.push(json!({"by": "user", "text": w, "at": ""})) }
            while ai < answers.len() && answers[ai]["after"].as_u64().is_some_and(|a| a as usize <= k + 1) {
                thread.push(json!({"by": "server", "text": answers[ai]["text"], "at": answers[ai]["at"]})); ai += 1;
            }
        }
        for a in &answers[ai..] { thread.push(json!({"by": "server", "text": a["text"], "at": a["at"]})) }
        let line = c["line"].as_i64().unwrap_or(1);
        cards.push(json!({
            "id": id, "note": note, "line": line, "end": line,
            "quote": said.first().map(|s| s.0.clone()).unwrap_or_default(),
            "kind": "comment", "by": "user", "thread": thread, "fix": null,
            "closed": c["hidden"] == true, "at": "",
        }));
    }
    let _ = std::fs::rename(&p, p.with_extension("json.migrated"));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault(name: &str, text: &str) -> Cfg {
        let d = std::env::temp_dir().join(format!("facet-cards-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();   // no .claude: creating a card must make it
        std::fs::write(d.join("Note.md"), text).unwrap();
        Cfg(json!({"vault": d.to_string_lossy(), "store": d.join("store").to_string_lossy()}))
    }

    #[test]
    fn a_card_comment_is_told_from_a_chat_message() {
        assert_eq!(from_card("[[Some Note]] L16 #cmuyx4le1x81: \"$x$\" fix this"), Some("cmuyx4le1x81"));
        assert_eq!(from_card("[[Some Note]] L3 #c1 hi"), Some("c1"));
        assert_eq!(from_card("[2 command(s) answered on Telegram]\nx\n[end prior context]\n\n[[N]] L1 #ab: q"), Some("ab"));
        assert_eq!(from_card("look at [[Some Note]] L3 #c1"), None);
        assert_eq!(from_card("[[Some Note]] is wrong"), None);
        assert_eq!(from_card("hello"), None);
    }

    #[test]
    fn a_card_is_created_even_without_a_claude_dir_and_holds_a_thread() {
        let cfg = vault("thread", "one\ntwo\nthree\n");
        let c = create(&cfg, New { id: Some("c1"), note: "Note", at: At::Line(2), text: "hm", kind: "comment",
            fix: None, by: "user", quote: Some("two"), word: Some("two"), col: Some(4) }).unwrap();
        assert_eq!(c["note"], "Note.md");
        assert_eq!((c["word"].as_str(), c["col"].as_i64()), (Some("two"), Some(4)));
        assert!(file(&cfg).is_file());
        assert_eq!(say(&cfg, "c1", "server", "looks right").unwrap(), 1);
        assert_eq!(say(&cfg, "c1", "user", "and?").unwrap(), 2);
        assert!(say(&cfg, "nope", "server", "x").is_err());
        let c = get(&cfg, "c1").unwrap();
        let by: Vec<&str> = c["thread"].as_array().unwrap().iter().map(|m| m["by"].as_str().unwrap()).collect();
        assert_eq!(by, ["user", "server", "user"]);
        assert!(message(&c, "x").starts_with("[[Note]] L2 #c1: \"two\"\nx"));
        assert_eq!(from_card(&message(&c, "x")), Some("c1"));
        unsay(&cfg, "c1", 2).unwrap();
        assert_eq!(get(&cfg, "c1").unwrap()["thread"].as_array().unwrap().len(), 2);
        assert!(create(&cfg, New { id: Some("c1"), note: "Note", at: At::Line(1), text: "", kind: "", fix: None, by: "user", quote: None, word: None, col: None }).is_err());
        assert!(create(&cfg, New { id: None, note: "Nope", at: At::Line(1), text: "x", kind: "", fix: None, by: "server", quote: None, word: None, col: None }).is_err());
    }

    #[test]
    fn creating_where_no_card_can_be_kept_fails_out_loud() {
        let cfg = Cfg(json!({"vault": "/nonexistent/facet-vault"}));
        assert!(create(&cfg, New { id: None, note: "Note", at: At::Line(1), text: "x", kind: "warn", fix: None, by: "server", quote: None, word: None, col: None }).is_err());
    }

    #[test]
    fn a_fix_applies_behind_its_guard_and_shifts_the_others() {
        let cfg = vault("apply", "one\ntwo\nthree\nfour\n");
        let a = create(&cfg, New { id: None, note: "Note", at: At::Text("two".into()), text: "split it", kind: "warn",
            fix: Some("2a\n2b"), by: "server", quote: None, word: None, col: None }).unwrap();
        let b = create(&cfg, New { id: None, note: "Note", at: At::Line(4), text: "fine", kind: "info", fix: None, by: "server", quote: None, word: None, col: None }).unwrap();
        let (a, b) = (a["id"].as_str().unwrap().to_string(), b["id"].as_str().unwrap().to_string());
        set_fix(&cfg, &b, Some("FOUR")).unwrap();
        assert!(apply(&cfg, "missing").is_err());
        apply(&cfg, &a).unwrap();
        assert_eq!(std::fs::read_to_string(cfg.vault().join("Note.md")).unwrap(), "one\n2a\n2b\nthree\nfour\n");
        assert_eq!(get(&cfg, &a).unwrap()["closed"], true);
        let bc = get(&cfg, &b).unwrap();
        assert_eq!((bc["line"].as_i64(), bc["fix"][0]["start_line"].as_i64()), (Some(5), Some(5)));
        apply(&cfg, &b).unwrap();
        assert!(std::fs::read_to_string(cfg.vault().join("Note.md")).unwrap().contains("FOUR"));
        // a closed review card leaves no objection when it was applied
        assert!(!cfg.vault().join("LLM/Review memory.md").exists());
    }

    #[test]
    fn closing_a_review_card_records_the_objection() {
        let cfg = vault("close", "alpha beta\ngamma\n");
        let r = review(&cfg, "Note", "@ alpha beta\n! warn too short\n? because\n", false).unwrap();
        assert_eq!(r, "1 card(s) on Note.md");
        let c = open(&cfg).pop().unwrap();
        assert_eq!(c["thread"][0]["text"], "too short\n\nbecause");
        close(&cfg, c["id"].as_str().unwrap(), "it is fine").unwrap();
        assert!(open(&cfg).is_empty());
        let mem = std::fs::read_to_string(cfg.vault().join("LLM/Review memory.md")).unwrap();
        assert!(mem.contains("rejected \"too short\" — it is fine"), "{}", mem);
        assert!(review(&cfg, "Note", "@ nowhere\n! x\n", false).is_err());
    }

    #[test]
    fn old_diagnostics_are_absorbed_once() {
        let cfg = vault("migrate", "one\ntwo\n");
        std::fs::create_dir_all(cfg.vault().join(".claude")).unwrap();
        std::fs::write(legacy_diag(&cfg), r#"{"Note.md":[{"code":"ab12","line":2,"severity":"error","message":"m",
            "detail":"d","fix":[{"start_line":2,"end_line":2,"old_text":"two","new_text":"TWO"}]}]}"#).unwrap();
        let cs = open(&cfg);
        assert_eq!(cs.len(), 1);
        assert_eq!((cs[0]["id"].as_str(), cs[0]["kind"].as_str(), cs[0]["quote"].as_str()), (Some("ab12"), Some("error"), Some("two")));
        assert_eq!(cs[0]["thread"][0]["text"], "m\n\nd");
        assert_eq!(std::fs::read_to_string(legacy_diag(&cfg)).unwrap().trim(), "{}");
        assert_eq!(open(&cfg).len(), 1, "not absorbed twice");
        apply(&cfg, "ab12").unwrap();
        assert_eq!(std::fs::read_to_string(cfg.vault().join("Note.md")).unwrap(), "one\nTWO\n");
    }

    #[test]
    fn the_op_is_what_the_tools_call() {
        let cfg = vault("op", "a\nb\nc\n");
        let (r, l) = op(&cfg, &json!({"do": "new", "note": "Note", "line": 2, "text": "look", "kind": "warn", "fix": "B"})).unwrap();
        let id = r["id"].as_str().unwrap().to_string();
        assert!(l.unwrap().starts_with(&format!("new warn card #{} on [[Note]] L2: look", id)));
        let (_, l) = op(&cfg, &json!({"do": "reply", "id": id, "text": "more"})).unwrap();
        assert_eq!(l.unwrap(), format!("#{} on [[Note]] L2: more", id));
        op(&cfg, &json!({"do": "fix", "id": id, "fix": ""})).unwrap();
        assert!(get(&cfg, &id).unwrap()["fix"].is_null());
        assert!(op(&cfg, &json!({"do": "list"})).unwrap().0["text"].as_str().unwrap().contains(&id));
        op(&cfg, &json!({"do": "delete", "id": id})).unwrap();
        assert!(get(&cfg, &id).is_none());
        assert!(op(&cfg, &json!({"do": "reply", "id": "zz", "text": "x"})).is_err());
    }
}
