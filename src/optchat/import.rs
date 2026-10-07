// Importing notes (§10): files the user picks, each one message of kind `note`, appended
// through the engine (the one writer); during a turn (the agent importing a file itself) the
// engine holds it until the turn is done. The text is the file's path, then its whole content
// (the compactor's input is never cut, §4.2); the date is the file's modification time.
use super::engine;
use serde_json::json;

/// Larger than this is refused: one message this big would be a single compactor call
/// with the whole file in it, and a poor fit for one 512-byte line anyway.
pub const MAX: usize = 200_000;

/// Import each file; one result line per file.
pub fn files(paths: &[String]) -> Vec<String> {
    let dir = engine::dir();
    let home = crate::cfg::home();
    let mut out = Vec::new();
    for p in paths {
        let path = crate::cfg::tilde(p);
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        let r = (|| -> Result<String, String> {
            let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
            if bytes.len() > MAX { return Err(format!("{} bytes; the limit is {}", bytes.len(), MAX)) }
            let body = String::from_utf8(bytes).map_err(|_| "not a text file".to_string())?;
            if body.trim().is_empty() { return Err("empty".into()) }
            let shown = match path.strip_prefix(&home) { Ok(r) => format!("~/{}", r.display()), Err(_) => path.display().to_string() };
            let date = std::fs::metadata(&path).and_then(|m| m.modified()).ok()
                .map(|t| chrono::DateTime::<chrono::Utc>::from(t).format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string());
            let v = engine::request(&dir, json!({"op": "note", "text": format!("{}\n\n{}", shown, body.trim_end()), "date": date}))?;
            if v["ok"] != true { return Err(v["error"].as_str().unwrap_or("refused").into()) }
            Ok(match v["skipped"].as_str() {
                Some(s) => format!("skipped: {}", s),
                None if v["queued"] == true => "queued: added to the memory when the running turn is done".into(),
                None => format!("message {}", v["i"]),
            })
        })();
        out.push(match r { Ok(m) => format!("{}: {}", p, m), Err(e) => format!("{}: NOT imported: {}", p, e) });
    }
    out
}

/// Split a command line into paths, honouring quotes and backslash-escaped spaces
/// (what a terminal pastes when a file is dragged in).
pub fn split(args: &str) -> Vec<String> {
    let (mut out, mut cur, mut q, mut esc) = (Vec::new(), String::new(), None::<char>, false);
    for c in args.chars() {
        if esc { cur.push(c); esc = false; continue }
        match (c, q) {
            ('\\', None) => esc = true,
            ('"' | '\'', None) => q = Some(c),
            (c, Some(o)) if c == o => q = None,
            (c, None) if c.is_whitespace() => { if !cur.is_empty() { out.push(std::mem::take(&mut cur)) } }
            (c, _) => cur.push(c),
        }
    }
    if !cur.is_empty() { out.push(cur) }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn split_paths() {
        assert_eq!(super::split(r#"a.md "b c.md" d\ e.md 'f g'"#), vec!["a.md", "b c.md", "d e.md", "f g"]);
    }
}
