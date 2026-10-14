// Telegram is a *route*, not an output avenue of its own: it carries the same conversation as
// the web chat page — together they are the chat venue (README, "Stream and venues"). What it
// alone has is push. So it pushes what is worth waking a phone for — a chat send (kind `chat`;
// the agent's plain `talk` and its card `answer`s stay off it), a new
// note's link, new comments on a paper — and renders none of it in depth; fidelity lives in
// the reader.
//
// Slash commands are answered here, locally: free, instant, no engine turn, never in the log.
// Their transcripts queue in the prelude and ride along with the next real message, so the
// conversation still learns what was asked and when. That queue is this file's business only;
// `tell` drains it through one call.
//
// A plain message starts an instance of its own, like a message from any route.
use crate::cfg::{self, Cfg};
use crate::{doc, log};
use serde_json::{json, Value};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

// `inbound` and `outbound` each read the whole of state.json, change the few fields they own,
// and write the whole thing back; with no lock, a write from one can land between the other's
// read and write and so overwrite with a stale copy of fields it never touched — Telegram
// re-delivering `tg_offset`-acked updates, or a reply dropped off `tg_sent`. One mutex around
// every read-modify-write below makes each of those atomic; the two threads touch disjoint
// fields, so what it loses in concurrency is nothing either loop depends on.
static STATE: Mutex<()> = Mutex::new(());

/// Set `tg_offset` alone, against a state read fresh under the lock — never against a copy
/// of the whole object taken before the lock, which could already be behind an `outbound`
/// write of its own fields.
fn bump_offset(id: i64) {
    let _g = STATE.lock().unwrap();
    let mut st = cfg::state();
    st["tg_offset"] = Value::from(id);
    cfg::put_state(&st);
}

const LIMIT: usize = 3500;
const OUTCAP: usize = 700;      // per command, in the prelude
const PRECAP: usize = 2500;     // whole prelude

fn buffer() -> std::path::PathBuf { cfg::dir().join("buffer.jsonl") }

// A long poll held open across a machine sleep leaves a socket that is dead but never closed:
// without a cap the read blocks forever, the inbound thread is gone, and nothing says so —
// outbound keeps pushing, so the bridge looks alive while receiving nothing. The cap turns
// that into an error the loop retries. It must clear POLL, or every poll is a timeout.
const WAIT: Duration = Duration::from_secs(45);
const POLL: i64 = 20;

fn api(cfg: &Cfg, method: &str, body: Value) -> Result<Value, String> {
    let tok = cfg.opt("telegram.bot_token").ok_or("no telegram.bot_token")?;
    // `telegram.api`: another Bot API server (a local one, in tests)
    let base = cfg.opt("telegram.api").unwrap_or_else(|| "https://api.telegram.org".into());
    let url = format!("{}/bot{}/{}", base.trim_end_matches('/'), tok, method);
    let mut r = ureq::post(&url).config().timeout_global(Some(WAIT)).build()
        .send_json(&body).map_err(|e| e.to_string())?;
    let text = r.body_mut().read_to_string().map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

pub fn push(cfg: &Cfg, text: &str) -> Result<(), String> {
    let chat = cfg.num("telegram.chat_id", 0);
    if chat == 0 { return Err("not paired".into()) }
    for part in chunks(text) {
        api(cfg, "sendMessage", json!({"chat_id": chat, "text": part,
            "disable_web_page_preview": true}))?;
    }
    Ok(())
}

/// Send a file to the paired chat as a document (full quality). Bot API limit: 50 MB.
pub fn send_file(cfg: &Cfg, path: &str, caption: Option<&str>) -> Result<(), String> {
    let tok = cfg.opt("telegram.bot_token").ok_or("no telegram.bot_token")?;
    let chat = cfg.num("telegram.chat_id", 0);
    if chat == 0 { return Err("not paired".into()) }
    let p = std::path::Path::new(path);
    let data = std::fs::read(p).map_err(|e| format!("{}: {}", path, e))?;
    if data.len() > 50 * 1024 * 1024 {
        return Err(format!("{}: {} bytes exceeds the 50 MB Telegram bot limit", path, data.len()))
    }
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("file").replace('"', "_");
    let b = format!("----facet{:x}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    let mut body: Vec<u8> = Vec::new();
    let mut field = |k: &str, v: &str| body.extend(format!(
        "--{}\r\nContent-Disposition: form-data; name=\"{}\"\r\n\r\n{}\r\n", b, k, v).into_bytes());
    field("chat_id", &chat.to_string());
    if let Some(c) = caption.filter(|c| !c.is_empty()) { field("caption", c); }
    body.extend(format!("--{}\r\nContent-Disposition: form-data; name=\"document\"; filename=\"{}\"\r\nContent-Type: application/octet-stream\r\n\r\n", b, name).into_bytes());
    body.extend(data);
    body.extend(format!("\r\n--{}--\r\n", b).into_bytes());
    let base = cfg.opt("telegram.api").unwrap_or_else(|| "https://api.telegram.org".into());
    let url = format!("{}/bot{}/sendDocument", base.trim_end_matches('/'), tok);
    let mut r = ureq::post(&url).config().timeout_global(Some(Duration::from_secs(300)))
        .http_status_as_error(false).build()
        .header("Content-Type", format!("multipart/form-data; boundary={}", b))
        .send(&body[..]).map_err(|e| e.to_string())?;
    let text = r.body_mut().read_to_string().map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    if v["ok"] == true { Ok(()) } else { Err(v["description"].as_str().unwrap_or("telegram error").to_string()) }
}

/// Download a file the user sent (Bot API getFile) into ~/telegram_inbox/<date>_<name>.
fn download(cfg: &Cfg, file_id: &str, name: &str) -> Result<std::path::PathBuf, String> {
    let tok = cfg.opt("telegram.bot_token").ok_or("no telegram.bot_token")?;
    let v = api(cfg, "getFile", json!({"file_id": file_id}))?;
    let fp = v["result"]["file_path"].as_str().ok_or("getFile failed (files over 20 MB cannot be fetched)")?;
    let base = cfg.opt("telegram.api").unwrap_or_else(|| "https://api.telegram.org".into());
    let url = format!("{}/file/bot{}/{}", base.trim_end_matches('/'), tok, fp);
    let mut r = ureq::get(&url).config().timeout_global(Some(Duration::from_secs(300))).build()
        .call().map_err(|e| e.to_string())?;
    let bytes = r.body_mut().with_config().limit(60 * 1024 * 1024).read_to_vec().map_err(|e| e.to_string())?;
    let dir = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join("telegram_inbox");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let safe: String = name.chars().map(|c| if c.is_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
    let path = dir.join(format!("{}_{}", crate::log_stamp().replace([' ', ':'], "-"), safe));
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    Ok(path)
}

fn chunks(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in s.split('\n') {
        let mut rest: &str = line;
        loop {
            let sep = if cur.is_empty() { 0 } else { 1 };
            if cur.chars().count() + sep + rest.chars().count() <= LIMIT {
                if sep == 1 { cur.push('\n'); }
                cur.push_str(rest);
                break;
            }
            if !cur.is_empty() { out.push(std::mem::take(&mut cur)); continue; }
            // this line alone outgrows a whole chunk: split it rather than lose its tail
            let at = rest.char_indices().nth(LIMIT).map(|(i, _)| i).unwrap_or(rest.len());
            out.push(rest[..at].to_string());
            rest = &rest[at..];
        }
    }
    if !cur.is_empty() { out.push(cur) }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_is_recognised_only_when_it_is_the_whole_message() {
        for t in ["--help", " -H ", "/help", "Help"] { assert!(is_help(t), "{}", t); }
        for t in ["help me with the paper", "/helpx", ""] { assert!(!is_help(t), "{}", t); }
    }

    #[test]
    fn a_short_message_is_one_chunk() {
        assert_eq!(chunks("hello\nworld"), vec!["hello\nworld".to_string()]);
    }

    #[test]
    fn a_long_message_splits_on_lines_without_losing_any_of_it() {
        let line = "x".repeat(2000);
        let s = format!("{}\n{}\n{}", line, line, line);   // 3 lines, 2000 chars each
        let out = chunks(&s);
        assert!(out.iter().all(|p| p.chars().count() <= LIMIT));
        // every character sent somewhere, in order, nothing dropped
        let joined: String = out.join("\n");
        assert_eq!(joined.chars().filter(|&c| c != '\n').count(), s.chars().filter(|&c| c != '\n').count());
    }

    #[test]
    fn a_single_line_over_the_limit_is_split_not_truncated() {
        let line = "y".repeat(LIMIT + 500);
        let out = chunks(&line);
        assert!(out.len() >= 2);
        assert!(out.iter().all(|p| p.chars().count() <= LIMIT));
        let total: usize = out.iter().map(|p| p.chars().count()).sum();
        assert_eq!(total, line.chars().count());   // the old bug dropped the tail past LIMIT
    }
}

// ---- the prelude queue ------------------------------------------------------------------

fn remember(cmd: &str, out: &str) {
    let rec = json!({"at": crate::stamp(), "cmd": cmd,
        "out": out.chars().take(OUTCAP).collect::<String>()});
    let _ = std::fs::create_dir_all(cfg::dir());
    let old = std::fs::read_to_string(buffer()).unwrap_or_default();
    let _ = std::fs::write(buffer(), old + &rec.to_string() + "\n");
}

fn records() -> Vec<Value> {
    std::fs::read_to_string(buffer()).unwrap_or_default().lines()
        .filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// Render the queue as the block that precedes the next real message, and clear it.
pub fn take_prelude() -> Option<String> {
    let recs = records();
    if recs.is_empty() { return None }
    let _ = std::fs::remove_file(buffer());
    let mut out = format!("[{} command(s) answered on Telegram, shown now as prior context]\n", recs.len());
    for r in &recs {
        out.push_str(&format!("\n{} · {}\n{}\n", r["at"].as_str().unwrap_or(""),
            r["cmd"].as_str().unwrap_or(""), r["out"].as_str().unwrap_or("")));
        if out.len() > PRECAP { out.push_str("\n[truncated]\n"); break }
    }
    out.push_str("[end prior context]");
    Some(out)
}

pub fn queued() -> usize { records().len() }

pub fn peek_prelude() -> String {
    let recs = records();
    if recs.is_empty() { return "nothing queued".into() }
    recs.iter().map(|r| format!("{} {}", r["at"].as_str().unwrap_or(""), r["cmd"].as_str().unwrap_or("")))
        .collect::<Vec<_>>().join("\n")
}

// ---- commands ---------------------------------------------------------------------------

fn life(cfg: &Cfg, args: &[&str]) -> String {
    let bin = cfg::tilde(&cfg.str("life", "~/.local/bin/life"));
    match Command::new(&bin).args(args).output() {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() { String::from_utf8_lossy(&o.stderr).trim().to_string() } else { s }
        }
        Err(e) => format!("{}: {}", bin.display(), e),
    }
}

/// `/model` shows the master model, `/model opus|sonnet|…` switches it for the next turns.
/// The web chat box and Telegram share this; it is the engine's `model` op, not a message.
pub fn model_cmd(arg: &str) -> String {
    let dir = crate::optchat::engine::dir();
    let arg = arg.trim();
    if arg.is_empty() {
        return match crate::optchat::engine::request(&dir, json!({"op": "status"})) {
            Ok(v) => format!("model: {} (/model opus|sonnet to switch)", v["model"].as_str().unwrap_or("?")),
            Err(e) => format!("engine DOWN: {}", e),
        };
    }
    match crate::optchat::engine::request(&dir, json!({"op": "model", "name": arg})) {
        Ok(v) if v["ok"] == true => format!("model: {} (from the next turn)", v["model"].as_str().unwrap_or(arg)),
        Ok(v) => v["error"].as_str().unwrap_or("refused").to_string(),
        Err(e) => format!("engine DOWN: {}", e),
    }
}

/// The one help text (Telegram, web chat). Edit here.
pub const HELP: &str = "\
Facet: plain text goes to the agent. Say it in words:
- export and compile: LaTeX/PDF of a paper note; master logs are read
- push to Overleaf: pulls first, never force-pushes
- draft / send: an email draft is shown first; sent only when you say send; attachments ok
- check mail, update calendar
- paper feedback: comment on a note line (a card); replies give choices or a fix
- coding is handed on to Sonnet instances

Commands:
/model [opus|sonnet]  show or switch the main model
/diag open cards   /notes  published notes   /cal [N]  calendar
/mail [query]   /last N  recent messages   /link  reader links
/ping  engine and usage left   /buffer  /flush  queued prelude
/help  this text

Tools on the server: life mail (send --attach), export-cycle, facet spawn, facet restart";

/// Is this whole message a request for help? (`--help`, `-h`, `/help`, `help`)
pub fn is_help(text: &str) -> bool {
    matches!(text.trim().to_lowercase().as_str(), "--help" | "-h" | "/help" | "help")
}

/// Returns (reply, remember_it). Meta commands are not worth replaying to the conversation.
pub fn command(cfg: &Cfg, text: &str) -> (String, bool) {
    let mut it = text.trim_start_matches('/').split_whitespace();
    let cmd = it.next().unwrap_or("").to_lowercase();
    let rest: Vec<&str> = it.collect();
    let n = rest.first().and_then(|x| x.parse::<i64>().ok());
    let tail = text.trim_start_matches('/').splitn(2, char::is_whitespace).nth(1).unwrap_or("").trim();
    match cmd.as_str() {
        // from before instances, when a message could cut into a running turn: now every
        // message starts its own, so both just send
        "now" | "later" => (if tail.is_empty() { "nothing to send".into() } else {
            match crate::tell::tell(cfg, tail, "telegram") { Ok(_) => "sent".into(), Err(e) => e }
        }, false),
        "model" => (model_cmd(tail), false),
        "start" | "help" => (format!("{}\n\n{}", HELP, cfg.url("/")), false),
        "link" => (format!("home {}\nchat {}\nnotes {}\ncomments {}\nmemory {}",
            cfg.url("/"), cfg.url("/chat"), cfg.url("/m/"), cfg.url("/d/"), cfg.url("/tree")), false),
        "ping" | "usage" => (engine_line(cfg), false),
        "buffer" => (peek_prelude(), false),
        "flush" => (match take_prelude() {
            Some(p) => match crate::tell::tell(cfg, &p, "telegram") { Ok(_) => "sent".into(), Err(e) => e },
            None => "nothing queued".into(),
        }, false),
        "diag" | "cards" => (crate::cards::brief(cfg, None), true),
        "notes" => {
            let t = doc::table(cfg);
            if t.is_empty() { ("nothing published".to_string(), true) }
            else { (t.iter().map(|(s, _)| format!("{}  {}", s, cfg.url(&format!("/m/{}", s))))
                .collect::<Vec<_>>().join("\n"), true) }
        }
        "cal" => (life(cfg, &["cal", "next", "--days", &n.unwrap_or(7).to_string()]), true),
        "mail" => (if rest.is_empty() { life(cfg, &["mail", "inbox", "-n", "8"]) }
                   else { life(cfg, &["mail", "search", &rest.join(" "), "-n", "8"]) }, true),
        "last" => {
            let k = n.unwrap_or(3).clamp(1, 20) as usize;
            let prose: Vec<log::Msg> = log::since_by(cfg, -1, |m| m.is_prose());
            let msgs: Vec<String> = prose[prose.len().saturating_sub(k)..].iter()
                .map(|m| format!("[{} {}] {}", m.i, m.kind, m.text.chars().take(600).collect::<String>()))
                .collect();
            (msgs.join("\n\n"), false)
        }
        _ => (format!("no such command: /{}  (try /help)", cmd), false),
    }
}

/// "engine idle · 12 messages · session 88% left · resets 15:00 · week 72% left"
fn engine_line(cfg: &Cfg) -> String {
    match crate::optchat::engine::request(&crate::optchat::engine::dir(), json!({"op": "status"})) {
        Ok(v) => {
            let mut s = format!("engine {} · {} messages", if v["busy"] == true { "working" } else { "idle" }, v["messages"]);
            if let Some(l) = v["limits"].as_str().filter(|l| !l.is_empty()) { s.push_str(" · "); s.push_str(l); }
            if let Some(p) = v["paused"].as_str() { s.push_str(&format!("\ncompactor paused: {}", p)); }
            s
        }
        Err(e) => format!("engine DOWN: {} · {} messages in the log", e, log::last(cfg) + 1),
    }
}

// ---- the two loops ----------------------------------------------------------------------

fn inbound(cfg: Cfg) {
    let _ = api(&cfg, "setMyCommands", json!({"commands": [
        {"command": "help", "description": "what I can do"},
        {"command": "model", "description": "show or switch the model"},
        {"command": "ping", "description": "engine and usage left"},
        {"command": "diag", "description": "open cards"},
    ]}));
    // First run: start from the newest update rather than replaying a day of backlog into
    // the conversation.
    if cfg::state()["tg_offset"].as_i64().is_none() {
        let newest = api(&cfg, "getUpdates", json!({"offset": -1, "timeout": 0})).ok()
            .and_then(|v| v["result"].as_array().and_then(|a| a.last())
                .and_then(|u| u["update_id"].as_i64()));
        bump_offset(newest.map(|i| i + 1).unwrap_or(0));
    }
    // Silence was the bug: a stalled bridge must say so once, and say when it is back.
    let mut down = false;
    loop {
        let offset = cfg::state()["tg_offset"].as_i64().unwrap_or(0);
        let r = api(&cfg, "getUpdates", json!({"offset": offset, "timeout": POLL}));
        let v = match r {
            Ok(v) => {
                if down { eprintln!("[{}] telegram: receiving again", crate::log_stamp()); down = false }
                v
            }
            Err(e) => {
                if !down { eprintln!("[{}] telegram: not receiving: {}", crate::log_stamp(), e); down = true }
                std::thread::sleep(Duration::from_secs(5));
                continue
            }
        };
        for up in v["result"].as_array().into_iter().flatten() {
            let id = up["update_id"].as_i64().unwrap_or(0);
            bump_offset(id + 1);
            let Some(msg) = up.get("message") else { continue };
            let chat = msg["chat"]["id"].as_i64().unwrap_or(0);
            let mut text = msg["text"].as_str().unwrap_or("").to_string();
            let fileinfo = if let Some(d) = msg.get("document") {
                Some((d["file_id"].as_str().unwrap_or("").to_string(), d["file_name"].as_str().unwrap_or("document").to_string()))
            } else {
                msg["photo"].as_array().and_then(|a| a.last()).map(|p|
                    (p["file_id"].as_str().unwrap_or("").to_string(), "photo.jpg".to_string()))
            };
            if text.is_empty() && fileinfo.is_none() { continue }

            // pairing: the first chat to speak owns the bridge
            let mut c = Cfg::load();
            if c.num("telegram.chat_id", 0) == 0 {
                c.set("telegram.chat_id", Value::from(chat));
                c.save();
                let _ = push(&c, "Paired. Facet is listening here.");
            }
            let c = Cfg::load();
            if c.num("telegram.chat_id", 0) != chat { continue }
            if let Some((fid, name)) = fileinfo {
                let cap = msg["caption"].as_str().unwrap_or("");
                match download(&c, &fid, &name) {
                    Ok(p) => text = if cap.is_empty() { format!("[file] {}", p.display()) }
                                    else { format!("[file] {} ({})", p.display(), cap) },
                    Err(e) => { let _ = push(&c, &format!("could not fetch file: {}", e)); continue }
                }
            }

            if is_help(&text) {
                let _ = push(&c, HELP);
            } else if text.starts_with('/') {
                let (reply, keep) = command(&c, &text);
                if keep { remember(&text, &reply); }
                let _ = push(&c, if reply.is_empty() { "(nothing)" } else { &reply });
            } else {
                match crate::tell::tell(&c, &text, "telegram") {
                    Ok(_) => {}
                    Err(e) => { let _ = push(&c, &format!("could not deliver: {}", e)); }
                }
            }
        }
    }
}

fn outbound(_cfg: Cfg) {
    loop {
        std::thread::sleep(Duration::from_secs(3));
        let c = Cfg::load();
        if c.num("telegram.chat_id", 0) == 0 { continue }
        let st = cfg::state();   // a read-only look at the fields only this loop ever writes

        // replies
        // never ahead of the log: a fresh log (ids from 0 again) would otherwise stay silent
        let last = st["tg_sent"].as_i64().unwrap_or_else(|| log::last(&c)).min(log::last(&c));
        let mut high = last;
        for m in log::since(&c, last) {
            high = high.max(m.i);
            // only what the agent sent to the chat venue (kind `chat`): never its plain text
            // (`talk`, stream-only) and never a card's answer (`answer`, card venue)
            if m.kind == "chat" && !m.text.trim().is_empty() { let _ = push(&c, &m.text); }
        }

        // new published notes: title and link, never the body
        let known: Vec<String> = st["tg_slugs"].as_array().map(|a|
            a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()).unwrap_or_default();
        let now: Vec<String> = doc::table(&c).into_iter().map(|(s, _)| s).collect();
        if !known.is_empty() {
            for s in now.iter().filter(|s| !known.contains(s)) {
                let title = doc::get(&c, s).map(|d| d.title).unwrap_or_else(|| s.clone());
                let _ = push(&c, &format!("note: {}\n{}", title, c.url(&format!("/m/{}", s))));
            }
        }

        // new comments on notes
        let before = st["tg_diag"].as_i64().unwrap_or(-1);
        // the agent's own cards only: one the user opened by double-click is no news to them
        let count = crate::cards::open(&c).iter().filter(|x| x["by"] == "server").count() as i64;
        if before >= 0 && count > before {
            let _ = push(&c, &format!("{} new card(s), {} open\n{}",
                count - before, count, c.url("/d/")));
        }

        // write back what this loop owns (tg_sent/tg_slugs/tg_diag) always: otherwise a
        // cursor resets to "now" each pass and anything in between is lost. Re-read fresh
        // under the lock first, so a concurrent `bump_offset` from `inbound` is never the
        // one that gets overwritten.
        let _g = STATE.lock().unwrap();
        let mut fresh = cfg::state();
        fresh["tg_sent"] = Value::from(high);
        fresh["tg_slugs"] = json!(now);
        fresh["tg_diag"] = Value::from(count);
        cfg::put_state(&fresh);
    }
}

pub fn spawn(cfg: &Cfg) {
    if cfg.opt("telegram.bot_token").is_none() { return }
    let a = cfg.clone(); std::thread::spawn(move || inbound(a));
    let b = cfg.clone(); std::thread::spawn(move || outbound(b));
}
