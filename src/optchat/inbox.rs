// The inbox: the one durable queue between whoever takes a message (serve, the CLI, Telegram)
// and the engine. A message is a file in `<state>/inbox/`, written whole (tmp + fsync + rename)
// before anyone is told "queued", so it exists whether or not the engine is alive, and a reload
// of the page, a restart of serve or of the engine loses none. The engine drains the directory
// in file-name order (= arrival order) into its own accepted-not-logged queue (turn.rs,
// `queue.json`), deleting each file only after the engine has saved it. A crash in that
// instant can replay one message; a lost one is never possible.
//
// What the user sees (web.rs `outbox_html`), from the files alone, no engine needed:
//   queued   a file in the inbox          the server has it, the engine has not taken it
//   working  in the engine's queue.json   the engine has it, its instance has not started
//   (then it is in the log, and shows as an ordinary message)
use super::engine::{self, state_dir};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Debug, PartialEq)]
pub struct Item { pub id: String, pub text: String }

pub fn dir() -> PathBuf { state_dir(&engine::dir()).join("inbox") }

static SEQ: AtomicU64 = AtomicU64::new(0);

/// Store a message; its id is the file name. Atomic: a reader sees all of it or none.
pub fn push_in(d: &Path, text: &str) -> Result<String, String> {
    std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    let us = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|t| t.as_micros()).unwrap_or(0);
    let id = format!("{:020}-{:06}-{}", us, SEQ.fetch_add(1, Ordering::SeqCst), std::process::id());
    let tmp = d.join(format!(".{}.tmp", id));
    let body = serde_json::json!({"text": text}).to_string();
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
        f.write_all(body.as_bytes()).and_then(|_| f.sync_all()).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, d.join(format!("{}.json", id))).map_err(|e| e.to_string())?;
    if let Ok(dd) = std::fs::File::open(d) { let _ = dd.sync_all(); }
    Ok(id)
}
pub fn push(text: &str) -> Result<String, String> { push_in(&dir(), text) }

/// What is waiting, oldest first.
pub fn list_in(d: &Path) -> Vec<Item> {
    let mut names: Vec<String> = std::fs::read_dir(d).into_iter().flatten().flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.') && n.ends_with(".json")).collect();
    names.sort();
    names.into_iter().filter_map(|n| {
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(d.join(&n)).ok()?).ok()?;
        Some(Item { id: n.trim_end_matches(".json").to_string(), text: v["text"].as_str()?.to_string() })
    }).collect()
}
pub fn list() -> Vec<Item> { list_in(&dir()) }

/// Hand each waiting message to `take`, which must have it saved when it returns; the file goes
/// only then. Returns how many.
pub fn drain_in(d: &Path, mut take: impl FnMut(&Item)) -> usize {
    let items = list_in(d);
    for it in &items {
        take(it);
        let _ = std::fs::remove_file(d.join(format!("{}.json", it.id)));
    }
    items.len()
}

/// Accepted by the engine and not yet logged: the texts in its saved queues (turn.rs `save`).
pub fn working_in(sd: &Path) -> Vec<String> {
    std::fs::read_to_string(sd.join("queue.json")).ok().and_then(|b| serde_json::from_str::<Vec<String>>(&b).ok()).unwrap_or_default()
}
pub fn working() -> Vec<String> { working_in(&state_dir(&engine::dir())) }

#[cfg(test)]
mod tests {
    use super::*;
    fn tmp(n: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("facet-inbox-{}-{}", n, std::process::id()));
        let _ = std::fs::remove_dir_all(&d); d
    }
    #[test]
    fn persists_in_order_and_drains_once() {
        let d = tmp("a");
        assert!(list_in(&d).is_empty());
        push_in(&d, "one\nwith newline").unwrap();
        push_in(&d, "two").unwrap();
        // a half-written file (tmp) is never listed
        std::fs::write(d.join(".x.tmp"), "{").unwrap();
        let l = list_in(&d);
        assert_eq!(l.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(), vec!["one\nwith newline", "two"]);
        // "restart": the same directory read afresh still has both
        assert_eq!(list_in(&d).len(), 2);
        let mut got = Vec::new();
        assert_eq!(drain_in(&d, |i| got.push(i.text.clone())), 2);
        assert_eq!(got, vec!["one\nwith newline", "two"]);
        assert!(list_in(&d).is_empty());
        assert_eq!(drain_in(&d, |_| panic!("taken twice")), 0);
    }
    #[test]
    fn working_reads_the_engines_saved_queues() {
        let d = tmp("b"); std::fs::create_dir_all(&d).unwrap();
        assert!(working_in(&d).is_empty());
        std::fs::write(d.join("queue.json"), r#"["a","b"]"#).unwrap();
        assert_eq!(working_in(&d), vec!["a", "b"]);
    }
}
