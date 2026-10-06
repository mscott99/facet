// The input funnel. One function, called by every route; no route knows how text reaches
// the engine. There is no session object here, because there is no session: the conversation
// is the log, and this appends to it by typing into whatever the engine is reading.
//
// Two facts shape this file:
//   1. One line = one message. A newline in a tmux paste is Enter, and the engine's plain
//      line mode reads one message per line of stdin. So multi-line input cannot be sent as
//      such: it is spilled to a file and the engine is told a one-line pointer to it.
//   2. Telegram's slash-command transcripts queue up as a prelude and ride along with the
//      next real message. That is a mechanism of the Telegram path, hooked in once, here.
use crate::cfg::{self, Cfg};
use std::process::Command;

const INLINE: usize = 900;   // longer than this, or multi-line, goes to a file

pub fn tell(cfg: &Cfg, text: &str, origin: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() { return Err("empty".into()) }
    let prelude = crate::tg::take_prelude();
    let body = match prelude {
        Some(p) => format!("{}\n\n{}", p, text),
        None => text.to_string(),
    };
    let line = if body.contains('\n') || body.len() > INLINE { spill(&body, origin)? } else { body };
    send(cfg, &line)?;
    Ok(line)
}

/// Write the real text to the inbox and return the one line the engine will see.
fn spill(body: &str, origin: &str) -> Result<String, String> {
    let dir = cfg::data_dir().join("inbox");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let p = dir.join(format!("{}-{}.md", crate::stamp(), origin));
    std::fs::write(&p, body).map_err(|e| e.to_string())?;
    let first: String = body.lines().next().unwrap_or("").chars().take(90).collect();
    Ok(format!("[{} · {} lines, read it: {}] {}",
        origin, body.lines().count(), p.display(), first))
}

/// The one adapter. `tmux` types into the pane the engine's TUI is running in; `line-file`
/// appends a line to a file that the engine reads in plain line mode
/// (`tail -n0 -f <file> | optchat`). Everything above is indifferent to which.
fn send(cfg: &Cfg, line: &str) -> Result<(), String> {
    match cfg.str("input.mode", "tmux").as_str() {
        "line-file" => {
            use std::io::Write;
            let p = cfg::tilde(&cfg.str("input.file", "~/.local/share/facet/in.txt"));
            if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&p)
                .map_err(|e| format!("{}: {}", p.display(), e))?;
            writeln!(f, "{}", line).map_err(|e| e.to_string())
        }
        _ => {
            let t = cfg.str("input.tmux_target", "TMUX:1.0");
            let tmux = cfg.str("input.tmux", "/opt/homebrew/bin/tmux");
            let ok = Command::new(&tmux).args(["list-panes", "-t", &t]).output()
                .map(|o| o.status.success()).unwrap_or(false);
            if !ok { return Err(format!("tmux target {} is not there", t)) }
            run(&tmux, &["send-keys", "-t", &t, "-l", line])?;
            run(&tmux, &["send-keys", "-t", &t, "Enter"])
        }
    }
}

fn run(bin: &str, args: &[&str]) -> Result<(), String> {
    let o = Command::new(bin).args(args).output().map_err(|e| e.to_string())?;
    if o.status.success() { Ok(()) }
    else { Err(String::from_utf8_lossy(&o.stderr).trim().to_string()) }
}

/// Is the input path actually there? Not a predicate on a noun — a health check on one adapter.
pub fn healthy(cfg: &Cfg) -> bool {
    match cfg.str("input.mode", "tmux").as_str() {
        "line-file" => true,
        _ => Command::new(cfg.str("input.tmux", "/opt/homebrew/bin/tmux"))
                .args(["list-panes", "-t", &cfg.str("input.tmux_target", "TMUX:1.0")])
                .output().map(|o| o.status.success()).unwrap_or(false),
    }
}
