// The terminal route: a client of the engine socket. It prints plainly (no screen redraws,
// so the terminal's own scrollback works, §10) with an editable prompt line underneath.
// Closing it does not stop the engine; the chat goes on and other routes still reach it.
//
//   Enter sends (into a running turn, at its next tool call) · Alt-Enter sends for a turn of its
//   own, after the running one · Ctrl-J: new line · Ctrl-C or Ctrl-D: leave (the engine and any
//   running turn carry on) · /cancel stops the turn · /help /usage /tree /view /status /stats /resume
use crate::optchat::engine;
use rustyline::error::ReadlineError;
use rustyline::{Cmd, ConditionalEventHandler, DefaultEditor, Event, EventContext, EventHandler, ExternalPrinter,
    KeyCode, KeyEvent, Modifiers, RepeatCount};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set by Alt-Enter for the line it accepts: that line is sent for a turn of its own.
/// (Terminals send Shift-Enter as a plain Enter; Claude Code's /terminal-setup makes it Alt-Enter.)
static LATER: AtomicBool = AtomicBool::new(false);
struct Later;
impl ConditionalEventHandler for Later {
    fn handle(&self, _: &Event, _: RepeatCount, _: bool, _: &EventContext) -> Option<Cmd> {
        LATER.store(true, Ordering::SeqCst);
        Some(Cmd::AcceptLine)
    }
}

const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const CYAN: &str = "\x1b[36m";
const OFF: &str = "\x1b[0m";

fn one_line(s: &str, n: usize) -> String {
    let f = s.replace('\n', " ⏎ ");
    if f.chars().count() <= n { f } else { format!("{}…", f.chars().take(n).collect::<String>()) }
}

pub fn run() {
    let dir = engine::dir();
    if let Err(e) = engine::ensure(&dir) { eprintln!("{}", e); std::process::exit(1) }
    // §10: on start, print the view, so you see what the agent sees
    if let Ok(v) = engine::request(&dir, json!({"op": "view"})) {
        println!("{}{}{}", DIM, v["view"].as_str().unwrap_or(""), OFF);
    }
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() { return piped(&dir) }
    let mut rl = DefaultEditor::new().expect("terminal");
    rl.bind_sequence(KeyEvent(KeyCode::Char('j'), Modifiers::CTRL), Cmd::Newline);
    rl.bind_sequence(KeyEvent(KeyCode::Enter, Modifiers::ALT), EventHandler::Conditional(Box::new(Later)));
    let mut printer = rl.create_external_printer().expect("printer");
    watch(&dir, move |o| printer.print(o).is_ok());
    editor(&dir, rl);
}

/// Without a terminal: each line of stdin is one message; leave once the engine is idle.
fn piped(dir: &std::path::Path) {
    watch(dir, |o| { print!("{}", o); std::io::stdout().flush().is_ok() });
    for line in std::io::stdin().lines().map_while(Result::ok) {
        if line.trim().is_empty() { continue }
        if let Err(e) = send(dir, &line, false) { eprintln!("{}", e); return }
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    while engine::request(dir, json!({"op": "status"})).map(|v| v["busy"] == true).unwrap_or(false) {
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
}

/// Print the engine's events through `out` until the connection ends.
/// Texts this client sent and has not seen logged yet: their log echo is not printed again,
/// since the prompt line already shows them -- unless they are waiting for a turn of their own,
/// which lands long after it was typed: those are dropped from here when the engine says
/// `later`, so the log echo prints them again where they go into the chat.
static MINE: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
fn mine(text: &str) -> bool {
    let mut m = MINE.lock().unwrap();
    match m.iter().position(|x| x == text) { Some(k) => { m.remove(k); true } None => false }
}
fn send(dir: &std::path::Path, text: &str, later: bool) -> Result<Value, String> {
    MINE.lock().unwrap().push(text.trim().to_string());
    engine::request(dir, json!({"op": "send", "text": text, "later": later}))
}

/// Print the engine's events through `out_fn`; if the engine goes away (a restart), keep
/// trying, and carry on when it is back.
fn watch(dir: &std::path::Path, mut out_fn: impl FnMut(String) -> bool + Send + 'static) {
    let sock = engine::sock(dir);
    let mut hinted = false;
    std::thread::spawn(move || loop {
        let conn = UnixStream::connect(&sock).and_then(|mut c| { writeln!(c, "{}", json!({"op": "watch"}))?; Ok(c) });
        let Ok(watch) = conn else { std::thread::sleep(std::time::Duration::from_secs(1)); continue };
        let mut buf = String::new(); // streamed reply text not yet ended by a newline
        let mut streamed = false;
        for line in BufReader::new(watch).lines() {
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
            let out = match s("ev").as_str() {
                "hello" => {
                    let st = &v["status"];
                    let lim = st["limits"].as_str().filter(|l| !l.is_empty()).map(|l| format!(" · {}", l)).unwrap_or_default();
                    let hint = if hinted { "" } else { "\n· Ctrl-C leaves (the agent keeps working) · /help for commands" };
                    hinted = true;
                    Some(format!("{}· {} messages · {} · compactor {}{}{}{}", DIM,
                        st["messages"], st["model"].as_str().unwrap_or("?"), st["compact_model"].as_str().unwrap_or("?"), lim, hint, OFF))
                }
                "limits" if s("line").starts_with("LIMIT") => Some(format!("{}· {}{}", BOLD, s("line"), OFF)),
                "delta" => {
                    buf.push_str(&s("text"));
                    streamed = true;
                    // print whole lines as they come; a long line without a break is cut at a
                    // space, so a one-paragraph reply still streams
                    match buf.rfind('\n') {
                        Some(k) => { let done = buf[..k].to_string(); buf = buf[k + 1..].to_string(); Some(done) }
                        None if buf.chars().count() > 100 => match buf.rfind(' ') {
                            Some(k) if k > 0 => { let done = buf[..k].to_string(); buf = buf[k + 1..].to_string(); Some(done) }
                            _ => None,
                        },
                        None => None,
                    }
                }
                "accepted" => match s("how").as_str() {
                    "queued" => Some(format!("{}· queued: it joins the turn that is starting{}", DIM, OFF)),
                    "held" => Some(format!("{}· the agent is replying: this goes in at its next tool call, or starts the next turn{}", DIM, OFF)),
                    "delivered" => Some(format!("{}· delivered: the agent sees it when the running tool finishes{}", DIM, OFF)),
                    "later" => {
                        // it goes in once the running turn is done: stop hiding its log echo, so
                        // it is printed again at the point where it joins the chat
                        mine(&s("text"));
                        Some(format!("{}· next turn: starts when the running turn is done ({} waiting){}", DIM, v["waiting"], OFF))
                    }
                    _ => None,
                },
                "phase" => match s("phase").as_str() {
                    "settling" => Some(format!("{}· working…{}", DIM, OFF)),
                    "thinking" => Some(format!("{}✻ thinking…{}", DIM, OFF)),
                    _ => None,
                },
                "msg" => match s("kind").as_str() {
                    "talk" if streamed => {
                        streamed = false;
                        let rest = std::mem::take(&mut buf);
                        if rest.is_empty() { Some(String::new()) } else { Some(format!("{}\n", rest)) }
                    }
                    "talk" => Some(format!("{}\n", s("text"))),
                    // the terminal shows the whole stream; what went to the chat or a card is marked
                    "chat" => Some(format!("{}{}→ chat{}\n{}\n", BOLD, CYAN, OFF, s("text"))),
                    "answer" => Some(format!("{}→ card {}{}", DIM, s("text"), OFF)),
                    "user" if mine(&s("text")) => None,
                    "user" => Some(format!("{}{}› {}{}", BOLD, CYAN, s("text"), OFF)),
                    "tool" => Some(format!("{}⏺ {}{}", DIM, one_line(&s("text"), 160), OFF)),
                    "echo" => {
                        let t = s("text");
                        Some(format!("{}  ⎿ {} ({} lines){}", DIM, one_line(&t, 120), t.lines().count(), OFF))
                    }
                    k => Some(format!("{}: {}", k, s("text"))),
                },
                "thought" => Some(format!("{}✻ {}{}", DIM, one_line(&s("text"), 200), OFF)),
                // the end-of-turn usage line stays in the engine log; here /usage asks for it
                "notice" if s("text").starts_with("done") => None,
                "notice" => Some(format!("{}· {}{}", DIM, s("text"), OFF)),
                _ => None,
            };
            if let Some(o) = out { if !out_fn(format!("{}\n", o)) { return } }
        }
        if !out_fn(format!("{}· engine restarting, reconnecting…{}\n", DIM, OFF)) { return }
        std::thread::sleep(std::time::Duration::from_secs(1));
    });
}

fn editor(dir: &std::path::Path, mut rl: DefaultEditor) {
    let dir = dir.to_path_buf();
    loop {
        LATER.store(false, Ordering::SeqCst);
        match rl.readline("› ") {
            Ok(line) => {
                let later = LATER.load(Ordering::SeqCst);
                let text = line.trim();
                if text.is_empty() { continue }
                let _ = rl.add_history_entry(text);
                let reply = match text {
                    "/view" => engine::request(&dir, json!({"op": "view"})).map(|v| v["view"].as_str().unwrap_or("").to_string()),
                    "/status" => engine::request(&dir, json!({"op": "status"})).map(|v| serde_json::to_string_pretty(&v).unwrap()),
                    "/stats" => Ok(crate::optchat::usage::table(&dir)),
                    "/tree" => tree(&dir),
                    "/help" | "/?" => Ok(format!("{}{}{}", DIM, HELP, OFF)),
                    "/model" => engine::request(&dir, json!({"op": "status"}))
                        .map(|v| format!("{}· model {} · /model opus|sonnet to switch{}", DIM, v["model"].as_str().unwrap_or("?"), OFF)),
                    t if t.starts_with("/model ") => engine::request(&dir, json!({"op": "model", "name": t[7..].trim()}))
                        .map(|v| if v["ok"] == true {
                            format!("{}· next turns use {} (its first turn re-caches the view once){}", DIM, v["model"].as_str().unwrap_or("?"), OFF)
                        } else { format!("{}· {}{}", DIM, v["error"].as_str().unwrap_or("refused"), OFF) }),
                    t if t == "/zoom" || t.starts_with("/zoom ") => {
                        // "/zoom 12", "/zoom 8 4", "/zoom 8+4" (as lines are written in /view)
                        let p: Vec<i64> = t[5..].split(|c: char| c == '+' || c == ',' || c.is_whitespace())
                            .filter_map(|x| x.parse().ok()).collect();
                        match p.first() {
                            Some(&id) => engine::request(&dir, json!({"op": "zoom", "id": id, "n": p.get(1).copied().unwrap_or(1)}))
                                .map(|v| v["text"].as_str().unwrap_or("").to_string()),
                            None => Ok(format!("{}· /zoom <id+n> as written in /view; /zoom <id> is message id whole{}", DIM, OFF)),
                        }
                    }
                    t if t.starts_with("/import ") => Ok(format!("{}{}{}", DIM,
                        crate::optchat::import::files(&crate::optchat::import::split(&t[8..])).iter()
                            .map(|l| format!("· {}", l)).collect::<Vec<_>>().join("\n"), OFF)),
                    "/usage" => engine::request(&dir, json!({"op": "status"})).map(|v| v["limits"].as_str().unwrap_or("not known yet (no call since the engine started)").to_string()),
                    "/resume" => engine::request(&dir, json!({"op": "resume"})).map(|_| String::new()),
                    "/cancel" => engine::request(&dir, json!({"op": "cancel"})).map(|_| String::new()),
                    "/quit" | "/exit" => return,
                    // a mistyped command must not become a paid turn; a path still goes through
                    t if t.starts_with('/') && !t.contains(' ') && !t[1..].contains('/') =>
                        Ok(format!("{}· unknown command {} · /help lists them{}", DIM, t, OFF)),
                    _ => send(&dir, &line, later).map(|_| String::new()),
                };
                match reply {
                    Ok(s) if !s.is_empty() => println!("{}", s),
                    Ok(_) => {}
                    Err(e) => println!("{}· {}{}", DIM, e, OFF),
                }
            }
            // Ctrl-C and Ctrl-D leave this window; the engine (and a running turn) carry on
            Err(ReadlineError::Interrupted) | Err(_) => return,
        }
    }
}

const HELP: &str = "\
/usage    session and week left, reset time
/tree     the memory tree in the browser (also /tree on the web route)
/import <file>...  add files to the memory, one note each (drag files in)
/view     what the agent sees of its memory
/zoom <id+n>  open a line of the view, as the agent does (/zoom 12: message 12 whole)
/model [opus|sonnet]  the model for the next turns (Opus costs more per turn)
/cancel   stop the running turn
/resume   restart the compactor after a pause
/stats    cost per day and kind
/status   engine state (raw)
/quit     leave (also Ctrl-C, Ctrl-D); the engine keeps working
Enter sends: during a turn, the agent sees it at its next tool call
Alt-Enter (Shift-Enter after Claude Code's /terminal-setup) sends for a turn of its own,
  after the running one; several wait in order, one turn each · Ctrl-J: new line";

/// The memory tree as a page (optchat/browse.rs), opened in the browser here; the same page is
/// live at /tree on the web route, which is the way in from the phone.
fn tree(dir: &std::path::Path) -> Result<String, String> {
    use crate::optchat::{browse, store::Store, view::View, VIEW};
    let s = Store::open(dir);
    let v = View::fold(&s, VIEW);
    let out = engine::state_dir(dir).join("memory.html");
    std::fs::write(&out, browse::html(&s, &v, VIEW, None)).map_err(|e| e.to_string())?;
    // /usr/bin/open on macOS, xdg-open elsewhere; if neither exists, nothing happens.
    let opener = if std::path::Path::new("/usr/bin/open").exists() { "/usr/bin/open" } else { "xdg-open" };
    let _ = std::process::Command::new(opener).arg(&out)
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    let web = crate::cfg::Cfg::load().url("/tree");
    Ok(format!("{}· opened {} · on the phone: {}{}", DIM, out.display(), web, OFF))
}
