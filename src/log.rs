// The log is the one source of truth about the conversation and it is read-only to us:
// ~/.optchat/chat/main/*.jsonl, one JSON object per line, appended by the engine.
// Nothing here writes. Every view in this program is a fold over this sequence.
use crate::cfg::Cfg;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Debug)]
pub struct Msg {
    pub i: i64,
    pub kind: String,
    pub text: String,
}

impl Msg {
    pub fn is_prose(&self) -> bool { matches!(self.kind.as_str(), "user" | "talk" | "chat" | "answer" | "note" | "work") }
    /// The chat venue (Telegram and the web chat page; README, "Stream and venues"): the user's
    /// own chat messages and what the agent sent there with `send_chat`. Not its plain text
    /// (`talk`), its steps, card comments and card answers, or subagent reports.
    pub fn in_chat(&self) -> bool {
        match self.kind.as_str() {
            "chat" => true,
            "user" => wants_chat(&self.text),
            _ => false,
        }
    }
}

/// A subagent's report, logged as a user message starting "[id] " (no space inside the
/// brackets, which tells it from the Telegram prelude's "[2 command(s) ...]").
pub fn is_report(text: &str) -> bool {
    text.strip_prefix('[').and_then(|t| t.find("] ").map(|k| &t[..k]))
        .is_some_and(|id| !id.is_empty() && !id.contains(char::is_whitespace) && !id.starts_with('['))
}

/// A user message that came through the chat venue, and so is owed an answer there:
/// neither a card's line-comment nor a subagent's report.
pub fn wants_chat(text: &str) -> bool { crate::cards::from_card(text).is_none() && !is_report(text) }

#[cfg(test)]
mod tests {
    use super::*;
    fn m(kind: &str, text: &str) -> Msg { Msg { i: 0, kind: kind.into(), text: text.into() } }
    #[test]
    fn the_log_is_read_as_it_grows_and_only_from_where_it_stopped() {
        use std::io::Write;
        let d = std::env::temp_dir().join(format!("facet-logtail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("chat/main")).unwrap();
        let cfg = Cfg(serde_json::json!({"store": d.to_string_lossy()}));
        let f = d.join("chat/main/2026-01-01.jsonl");
        let mut fh = std::fs::File::create(&f).unwrap();
        writeln!(fh, r#"{{"i":0,"kind":"user","text":"a"}}"#).unwrap();
        write!(fh, r#"{{"i":1,"kind":"chat","te"#).unwrap();      // half a line: not yet a message
        assert_eq!(since(&cfg, -1).len(), 1);
        writeln!(fh, r#"xt":"b"}}"#).unwrap();
        writeln!(fh, r#"{{"i":2,"kind":"echo","text":"c"}}"#).unwrap();
        let all = since(&cfg, -1);
        assert_eq!(all.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
        assert_eq!(since_by(&cfg, 0, |m| m.in_chat()).len(), 1);
        assert_eq!(last(&cfg), 2);
        // a file rewritten shorter is read again from the start
        std::fs::write(&f, "{\"i\":0,\"kind\":\"user\",\"text\":\"z\"}\n").unwrap();
        assert_eq!(since(&cfg, -1).iter().map(|m| m.text.as_str()).collect::<Vec<_>>(), ["z"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_chat_venue_shows_user_messages_and_sends_only() {
        assert!(m("chat", "hi").in_chat());
        assert!(m("user", "hello").in_chat());
        assert!(m("user", "[2 command(s) answered on Telegram, shown now as prior context]\nx").in_chat());
        assert!(!m("talk", "thinking aloud").in_chat());
        assert!(!m("answer", "#c1 on [[N]] L3: right").in_chat());
        assert!(!m("user", "[[N]] L3 #c1: \"q\" fix").in_chat());
        assert!(!m("user", "[spawn_5a7e6b2a3878] REPORT").in_chat());
        assert!(!m("tool", "Bash {}").in_chat() && !m("echo", "x").in_chat() && !m("work", "x").in_chat());
    }
}

fn files(cfg: &Cfg) -> Vec<PathBuf> {
    let d = cfg.store().join("chat/main");
    let mut v: Vec<PathBuf> = std::fs::read_dir(&d).into_iter().flatten().flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "jsonl").unwrap_or(false))
        .collect();
    v.sort();
    v
}

/// How much of each log file has been parsed so far, and what it held. The log only ever
/// grows (the engine appends whole lines), so a read picks up at the offset the last one
/// stopped at: a poll costs a `stat` and the new bytes, not the whole multi-MB history.
struct Tail { off: u64, msgs: Vec<Msg> }
fn tails() -> &'static Mutex<HashMap<PathBuf, Tail>> {
    static T: OnceLock<Mutex<HashMap<PathBuf, Tail>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn parse(line: &str) -> Option<Msg> {
    let v = serde_json::from_str::<serde_json::Value>(line).ok()?;
    Some(Msg {
        i: v.get("i").and_then(|x| x.as_i64()).unwrap_or(-1),
        kind: v.get("kind").and_then(|x| x.as_str()).unwrap_or("?").to_string(),
        text: v.get("text").and_then(|x| x.as_str()).unwrap_or("").to_string(),
    })
}

/// Bring every file's parsed tail up to date, then hand the lot to `f`. A shrunken file
/// (rewritten, not appended to) is parsed again from the start; a half-written last line
/// waits for its newline.
fn with_all<T>(cfg: &Cfg, f: impl FnOnce(&mut dyn Iterator<Item = &Msg>) -> T) -> T {
    use std::io::{Read, Seek, SeekFrom};
    let mut g = tails().lock().unwrap_or_else(|e| e.into_inner());
    let fs = files(cfg);
    for p in &fs {
        let len = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        let t = g.entry(p.clone()).or_insert(Tail { off: 0, msgs: Vec::new() });
        if len < t.off { t.off = 0; t.msgs.clear(); }
        if len == t.off { continue }
        let Ok(mut fh) = std::fs::File::open(p) else { continue };
        if fh.seek(SeekFrom::Start(t.off)).is_err() { continue }
        let mut buf = Vec::new();
        if fh.take(len - t.off).read_to_end(&mut buf).is_err() { continue }
        let Some(end) = buf.iter().rposition(|&b| b == b'\n') else { continue };
        for line in String::from_utf8_lossy(&buf[..end]).lines() {
            if line.is_empty() { continue }
            if let Some(m) = parse(line) { t.msgs.push(m) }
        }
        t.off += end as u64 + 1;
    }
    let mut it = fs.iter().filter_map(|p| g.get(p)).flat_map(|t| t.msgs.iter());
    f(&mut it)
}

/// Messages with index > `since` that pass `keep`, in order. Only those are copied out.
pub fn since_by(cfg: &Cfg, since: i64, keep: impl Fn(&Msg) -> bool) -> Vec<Msg> {
    let mut out: Vec<Msg> = with_all(cfg, |it| it.filter(|m| m.i > since && keep(m)).cloned().collect());
    out.sort_by_key(|m| m.i);
    out
}

/// Messages with index > `since`, in order.
pub fn since(cfg: &Cfg, since: i64) -> Vec<Msg> { since_by(cfg, since, |_| true) }

pub fn last(cfg: &Cfg) -> i64 { with_all(cfg, |it| it.map(|m| m.i).max().unwrap_or(-1)) }
