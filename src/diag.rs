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
