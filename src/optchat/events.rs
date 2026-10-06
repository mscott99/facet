// Introspection: what the engine did and why, one JSON object per line in
// <state dir>/events.jsonl (~/.local/share/facet/engine-*/), never in the chat directory.
// Only things that cannot be rebuilt later from the log and the tree are recorded: what each
// API request was for and how long it took, the subscription's limits over time, per-turn and
// per-node traces, every distinct system prompt. Logging never changes what the engine does:
// every function here swallows its own errors, and takes no lock but its own.
//
//   engine   start: binary, settings, what was loaded
//   req      one API request: what for (turn+step, node+try, prime), usage, ms, Claude Code version
//   turn     one fresh call: message ids, settle wait, view size and how much of it the previous
//            turn's view shared (the cache's chance), steps, duration, how it ended
//   node     one compactor node: tries' sizes, the kept size, gate wait, duration, failure
//   limits   the subscription's rate_limit_info, each time it changes
//   input    a message arrived: how it was taken (starting / queued / held / delivered)
//   system   a new system prompt version (its text is stored once, by hash, beside the log)
//   notice   every notice shown to the user
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

static FILE: Mutex<()> = Mutex::new(());
/// The previous turn's rendered view, to measure how much of it the next one shares.
static PREV_VIEW: Mutex<String> = Mutex::new(String::new());

pub fn log(dir: &Path, ev: &str, mut fields: Value) {
    if !fields.is_object() { fields = json!({"value": fields}); }
    fields["t"] = json!(super::store::now_iso());
    fields["ev"] = json!(ev);
    let line = format!("{}\n", fields);
    let path = super::engine::state_dir(dir).join("events.jsonl");
    let _g = FILE.lock().unwrap_or_else(|p| p.into_inner());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// One API request, with what it was for.
pub fn req(dir: &Path, kind: &str, r: &super::claude::Req, cc: &str, ms: u128, mut tag: Value) {
    if !tag.is_object() { tag = json!({}); }
    tag["kind"] = json!(kind);
    tag["model"] = json!(r.model);
    tag["usage"] = r.usage.clone();
    tag["eq"] = json!(super::usage::eq(&r.usage).round());
    tag["ms"] = json!(ms);
    tag["cc"] = json!(cc);
    log(dir, "req", tag);
}

/// View measurements for a turn: size, lines, and the prefix shared with the previous turn's
/// view (in bytes, and as the last cache mark inside it: what priming could read back).
pub fn view_stats(view: &str, parts: usize) -> Value {
    let mut prev = PREV_VIEW.lock().unwrap_or_else(|p| p.into_inner());
    let shared = prev.bytes().zip(view.bytes()).take_while(|(a, b)| a == b).count();
    let cuts = super::view::cuts(view);
    let mark = cuts.iter().rev().find(|&&c| c <= shared).copied().unwrap_or(0);
    let had = !prev.is_empty();
    *prev = view.to_string();
    json!({"view_bytes": view.len(), "view_lines": parts, "view_marks": cuts.len(),
           "shared_bytes": if had { json!(shared) } else { Value::Null }, "shared_to_mark": mark})
}

/// A system prompt version: logged once per distinct text, the text kept by hash.
pub fn system(dir: &Path, name: &str, text: &str) {
    let mut x = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(text, &mut x);
    let h = format!("{:016x}", std::hash::Hasher::finish(&x));
    let d = super::engine::state_dir(dir).join("prompts");
    let f = d.join(format!("{}-{}.txt", name, h));
    if f.exists() { return }
    let _ = std::fs::create_dir_all(&d);
    let _ = std::fs::write(&f, text);
    log(dir, "system", json!({"name": name, "hash": h, "bytes": text.len()}));
}

/// The Claude Code version, from a call's `system/init` event.
pub fn cc_version(ev: &Value) -> Option<String> {
    (ev["type"] == "system" && ev["subtype"] == "init").then(|| ev["claude_code_version"].as_str().unwrap_or("").to_string())
}
