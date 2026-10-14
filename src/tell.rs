// The input funnel. One function, called by every route; no route knows how text reaches
// the engine. There is no session object here, because there is no session: the conversation
// is the log, and this hands a message to the engine that writes it.
//
// The engine takes messages whole over its socket (newlines and all), so nothing here
// has to squeeze a message into one typed line any more.
// Telegram's slash-command transcripts queue up as a prelude and ride along with the
//      next real message. That is a mechanism of the Telegram path, hooked in once, here.
use crate::cfg::Cfg;
use crate::optchat::inbox::Target;

/// Every message starts an instance of its own (optchat/turn.rs), unless it names a running one.
pub fn tell(cfg: &Cfg, text: &str, origin: &str) -> Result<String, String> {
    tell_to(cfg, text, origin, None).map(|r| r.0)
}

/// Like `tell`, for a message that may name an instance; also returns the inbox item it became
/// (Telegram remembers it, to find the instance a reply to this message is for).
pub fn tell_to(cfg: &Cfg, text: &str, origin: &str, to: Option<&Target>) -> Result<(String, String), String> {
    let text = text.trim();
    if text.is_empty() { return Err("empty".into()) }
    let prelude = crate::tg::take_prelude();
    let body = match prelude {
        Some(p) => format!("{}\n\n{}", p, text),
        None => text.to_string(),
    };
    let _ = origin;
    let id = send(cfg, &body, to)?;
    Ok((body, id))
}

/// The one adapter: the inbox (optchat/inbox.rs), a file on disk the engine drains. It is
/// written before anyone is answered, so a message is kept whether or not the engine is up.
fn send(_cfg: &Cfg, line: &str, to: Option<&Target>) -> Result<String, String> {
    crate::optchat::inbox::push(line, to)
}

/// Is the input path actually there? Not a predicate on a noun — a health check on one adapter.
pub fn healthy(_cfg: &Cfg) -> bool { crate::optchat::engine::running(&crate::optchat::engine::dir()) }
