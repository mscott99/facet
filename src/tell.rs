// The input funnel. One function, called by every route; no route knows how text reaches
// the engine. There is no session object here, because there is no session: the conversation
// is the log, and this hands a message to the engine that writes it.
//
// The engine takes messages whole over its socket (newlines and all), so nothing here
// has to squeeze a message into one typed line any more.
// Telegram's slash-command transcripts queue up as a prelude and ride along with the
//      next real message. That is a mechanism of the Telegram path, hooked in once, here.
use crate::cfg::Cfg;

/// `later`: a turn of its own, after the running one, rather than into it at its next tool call.
pub fn tell(cfg: &Cfg, text: &str, origin: &str, later: bool) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() { return Err("empty".into()) }
    let prelude = crate::tg::take_prelude();
    let body = match prelude {
        Some(p) => format!("{}\n\n{}", p, text),
        None => text.to_string(),
    };
    let _ = origin;
    send(cfg, &body, later)?;
    Ok(body)
}

/// The one adapter: the engine's socket. A message goes whole, newlines and all.
fn send(_cfg: &Cfg, line: &str, later: bool) -> Result<(), String> {
    let v = crate::optchat::engine::request(&crate::optchat::engine::dir(), serde_json::json!({"op": "send", "text": line, "later": later}))?;
    if v["ok"].as_bool() == Some(true) { Ok(()) } else { Err(v["error"].as_str().unwrap_or("refused").to_string()) }
}

/// Is the input path actually there? Not a predicate on a noun — a health check on one adapter.
pub fn healthy(_cfg: &Cfg) -> bool { crate::optchat::engine::running(&crate::optchat::engine::dir()) }
