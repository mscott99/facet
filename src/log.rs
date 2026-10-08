// The log is the one source of truth about the conversation and it is read-only to us:
// ~/.optchat/chat/main/*.jsonl, one JSON object per line, appended by the engine.
// Nothing here writes. Every view in this program is a fold over this sequence.
use crate::cfg::Cfg;
use std::path::PathBuf;

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

/// Messages with index > `since`, in order. The whole log is a few hundred KB; parsing it
/// on each poll costs under a millisecond, so there is no index and no cache to invalidate.
pub fn since(cfg: &Cfg, since: i64) -> Vec<Msg> {
    let mut out = Vec::new();
    for f in files(cfg) {
        let Ok(body) = std::fs::read_to_string(&f) else { continue };
        for line in body.lines() {
            if line.is_empty() { continue }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            let i = v.get("i").and_then(|x| x.as_i64()).unwrap_or(-1);
            if i <= since { continue }
            out.push(Msg {
                i,
                kind: v.get("kind").and_then(|x| x.as_str()).unwrap_or("?").to_string(),
                text: v.get("text").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            });
        }
    }
    out.sort_by_key(|m| m.i);
    out
}

pub fn last(cfg: &Cfg) -> i64 { since(cfg, -1).last().map(|m| m.i).unwrap_or(-1) }
