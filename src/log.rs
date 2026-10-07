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
    pub fn is_prose(&self) -> bool { matches!(self.kind.as_str(), "user" | "talk" | "note" | "work") }
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
