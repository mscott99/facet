// A line-comment's reply thread. A double-click on a rendered line opens a card (web.rs
// `say()`), given a short id of its own right there in the page; that id is what lets
// `facet answer` find its way back to the right card, instead of a card's poll catching
// whatever the master happened to say next — the bug this file exists to fix.
//
// One small file under the engine's state directory, not the memory: who a card was about
// (its note and line, so `diag::propose` knows where a fix would land) and what has been
// answered. Loaded fresh and rewritten whole on every change, the same way `diag.rs` keeps
// `diagnostics.json` — cards are few, and bytes are cheap next to a model call.
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub fn file(dir: &Path) -> PathBuf { dir.join("cards.json") }

fn load(dir: &Path) -> Value {
    std::fs::read_to_string(file(dir)).ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Object(Default::default()))
}

fn save(dir: &Path, v: &Value) {
    let tmp = file(dir).with_extension("tmp");
    if std::fs::write(&tmp, v.to_string()).is_ok() { let _ = std::fs::rename(&tmp, file(dir)); }
}

/// The first send of a card: remembered once, by its id, so the id itself can stay short —
/// it does not have to carry the note and line, only point at where they are kept. A second
/// registration (a reply to an answer, through the same card) changes nothing.
pub fn register(dir: &Path, id: &str, note: &str, line: i64) {
    let mut data = load(dir);
    if data.get(id).is_some() { return }
    if let Some(m) = data.as_object_mut() {
        m.insert(id.into(), json!({"note": note, "line": line, "answers": []}));
    }
    save(dir, &data);
}

pub fn get(dir: &Path, id: &str) -> Option<Value> { load(dir).get(id).cloned() }

/// The answer, with the code of the fix it offered, if it offered one. An id this file has
/// never seen is refused, not silently dropped — the client invented it, or the state
/// directory was wiped since the card was sent.
pub fn answer(dir: &Path, id: &str, text: &str, code: Option<&str>) -> Result<(), String> {
    let mut data = load(dir);
    let Some(card) = data.get_mut(id) else { return Err(format!("no card {}", id)) };
    let answers = card["answers"].as_array_mut().expect("a registered card always has answers: []");
    answers.push(json!({"text": text, "code": code, "at": crate::optchat::store::now_iso()}));
    save(dir, &data);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("facet-cards-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn an_answer_lands_on_the_card_it_was_for() {
        let d = dir("answer");
        register(&d, "c1", "Some Note", 12);
        register(&d, "c1", "ignored", 999); // a second registration changes nothing
        assert!(answer(&d, "unknown", "hi", None).is_err());
        answer(&d, "c1", "looks right", Some("ab12")).unwrap();
        answer(&d, "c1", "a follow-up", None).unwrap();
        let card = get(&d, "c1").unwrap();
        assert_eq!(card["note"], "Some Note");
        assert_eq!(card["line"], 12);
        let answers = card["answers"].as_array().unwrap();
        assert_eq!(answers.len(), 2);
        assert_eq!(answers[0]["text"], "looks right");
        assert_eq!(answers[0]["code"], "ab12");
        assert!(answers[1]["code"].is_null());
        assert!(get(&d, "nope").is_none());
    }
}
