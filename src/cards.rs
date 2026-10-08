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

/// Every send through a card: the first remembers it, by its id, so the id itself can stay
/// short — it does not have to carry the note and line, only point at where they are kept.
/// Each send (the first comment, every reply after) is counted, so an answer can record how
/// many of the card's messages it came after and a reloaded card shows the thread in order.
pub fn register(dir: &Path, id: &str, note: &str, line: i64) {
    let mut data = load(dir);
    match data.get_mut(id) {
        Some(c) => { let n = c["sent"].as_u64().unwrap_or(1); c["sent"] = json!(n + 1); }
        None => if let Some(m) = data.as_object_mut() {
            m.insert(id.into(), json!({"note": note, "line": line, "answers": [], "sent": 1}));
        },
    }
    save(dir, &data);
}

/// The card a message came from, if it came from one: the id in `[[Note]] L<n> #<id>...`,
/// the shape web.rs's `comment()` gives every line-comment (after a Telegram prelude, if
/// one rode along). Such a message belongs to the card venue: it is answered on the card, it
/// is not shown in the chat, and a turn it starts gets no fallback reply in the chat.
pub fn from_card(text: &str) -> Option<&str> {
    let t = match text.find("[end prior context]\n\n") { Some(k) => &text[k + 21..], None => text };
    let t = t.trim_start().strip_prefix("[[")?;
    let t = &t[t.find("]] L")? + 4..];
    let t = t.trim_start_matches(|c: char| c.is_ascii_digit());
    let t = t.strip_prefix(" #")?;
    let n = t.find(|c: char| !c.is_ascii_alphanumeric()).unwrap_or(t.len());
    if n == 0 { None } else { Some(&t[..n]) }
}

pub fn all(dir: &Path) -> Value { load(dir) }

/// Take a card off its page for good; its answers stay (a later `facet answer` still lands).
pub fn hide(dir: &Path, id: &str) {
    let mut data = load(dir);
    if let Some(c) = data.get_mut(id) { c["hidden"] = json!(true); save(dir, &data); }
}

pub fn get(dir: &Path, id: &str) -> Option<Value> { load(dir).get(id).cloned() }

/// The answer, with the code of the fix it offered, if it offered one. An id this file has
/// never seen is refused, not silently dropped — the client invented it, or the state
/// directory was wiped since the card was sent.
pub fn answer(dir: &Path, id: &str, text: &str, code: Option<&str>) -> Result<(), String> {
    let mut data = load(dir);
    let Some(card) = data.get_mut(id) else { return Err(format!("no card {}", id)) };
    let after = card["sent"].clone();
    let answers = card["answers"].as_array_mut().expect("a registered card always has answers: []");
    answers.push(json!({"text": text, "code": code, "after": after, "at": crate::optchat::store::now_iso()}));
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
    fn a_card_comment_is_told_from_a_chat_message() {
        assert_eq!(from_card("[[Some Note]] L16 #cmuyx4le1x81: \"$x$\" fix this"), Some("cmuyx4le1x81"));
        assert_eq!(from_card("[[Some Note]] L3 #c1 hi"), Some("c1"));
        assert_eq!(from_card("[2 command(s) answered on Telegram]\nx\n[end prior context]\n\n[[N]] L1 #ab: q"), Some("ab"));
        assert_eq!(from_card("look at [[Some Note]] L3 #c1"), None);
        assert_eq!(from_card("[[Some Note]] is wrong"), None);
        assert_eq!(from_card("hello"), None);
    }

    #[test]
    fn an_answer_lands_on_the_card_it_was_for() {
        let d = dir("answer");
        register(&d, "c1", "Some Note", 12);
        register(&d, "c1", "ignored", 999); // a second send keeps who the card is about
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
        assert_eq!(answers[0]["after"], 2); // both sends came before it
    }

    #[test]
    fn an_answer_remembers_which_message_it_followed() {
        let d = dir("after");
        register(&d, "c2", "N", 1);
        answer(&d, "c2", "first", None).unwrap();
        register(&d, "c2", "N", 1);
        answer(&d, "c2", "second", None).unwrap();
        let a = get(&d, "c2").unwrap()["answers"].clone();
        assert_eq!((a[0]["after"].as_u64(), a[1]["after"].as_u64()), (Some(1), Some(2)));
    }
}
