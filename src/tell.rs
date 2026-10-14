// The input funnel. One function, called by every route; no route knows how text reaches
// the engine. There is no session object here, because there is no session: the conversation
// is the log, and this hands a message to the engine that writes it.
//
// The engine takes messages whole over its socket (newlines and all), so nothing here
// has to squeeze a message into one typed line any more.
// Telegram's slash-command transcripts queue up as a prelude and ride along with the
//      next real message. That is a mechanism of the Telegram path, hooked in once, here.
use crate::cfg::Cfg;

/// Every message starts an instance of its own (optchat/turn.rs).
pub fn tell(cfg: &Cfg, text: &str, origin: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() { return Err("empty".into()) }
    let prelude = crate::tg::take_prelude();
    let body = match prelude {
        Some(p) => format!("{}\n\n{}", p, text),
        None => text.to_string(),
    };
    let _ = origin;
    send(cfg, &body)?;
    Ok(body)
}

/// The one adapter: the inbox (optchat/inbox.rs), a file on disk the engine drains. It is
/// written before anyone is answered, so a message is kept whether or not the engine is up.
fn send(_cfg: &Cfg, line: &str) -> Result<(), String> {
    crate::optchat::inbox::push(line).map(|_| ())
}

/// Is the input path actually there? Not a predicate on a noun — a health check on one adapter.
pub fn healthy(_cfg: &Cfg) -> bool { crate::optchat::engine::running(&crate::optchat::engine::dir()) }
