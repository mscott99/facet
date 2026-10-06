// Telegram is a *route*, not an output avenue of its own: it carries the same conversation.
// What it alone has is push. So it pushes what is worth waking a phone for — a reply, a new
// note's link, new comments on a paper — and renders none of it in depth; fidelity lives in
// the reader.
//
// Slash commands are answered here, locally: free, instant, no engine turn, never in the log.
// Their transcripts queue in the prelude and ride along with the next real message, so the
// conversation still learns what was asked and when. That queue is this file's business only;
// `tell` drains it through one call.
use crate::cfg::{self, Cfg};
use crate::{diag, doc, log};
use serde_json::{json, Value};
use std::process::Command;
use std::time::Duration;

const LIMIT: usize = 3500;
const OUTCAP: usize = 700;      // per command, in the prelude
const PRECAP: usize = 2500;     // whole prelude

fn buffer() -> std::path::PathBuf { cfg::dir().join("buffer.jsonl") }

fn api(cfg: &Cfg, method: &str, body: Value) -> Result<Value, String> {
    let tok = cfg.opt("telegram.bot_token").ok_or("no telegram.bot_token")?;
    let url = format!("https://api.telegram.org/bot{}/{}", tok, method);
    let mut r = ureq::post(&url).send_json(&body).map_err(|e| e.to_string())?;
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

fn chunks(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in s.split('\n') {
        if cur.len() + line.len() + 1 > LIMIT && !cur.is_empty() { out.push(std::mem::take(&mut cur)); }
        if !cur.is_empty() { cur.push('\n'); }
        cur.push_str(&line.chars().take(LIMIT).collect::<String>());
    }
    if !cur.is_empty() { out.push(cur) }
    out
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

/// Returns (reply, remember_it). Meta commands are not worth replaying to the conversation.
pub fn command(cfg: &Cfg, text: &str) -> (String, bool) {
    let mut it = text.trim_start_matches('/').split_whitespace();
    let cmd = it.next().unwrap_or("").to_lowercase();
    let rest: Vec<&str> = it.collect();
    let n = rest.first().and_then(|x| x.parse::<i64>().ok());
    match cmd.as_str() {
        "start" | "help" => (format!(
            "Facet — your agent on this machine.\n\n\
             Plain text goes into the running conversation.\n\
             These are answered here instead, and ride along with your next message:\n\
             /diag  open comments      /notes  published notes\n\
             /cal N days ahead         /mail [query]\n\
             /last N  recent messages  /buffer  what is queued   /flush  send it now\n\
             /link  the reader         /ping  engine and usage left\n\n{}",
            cfg.url("/")), false),
        "link" => (format!("home {}\nchat {}\nnotes {}\ncomments {}\nmemory {}",
            cfg.url("/"), cfg.url("/chat"), cfg.url("/m/"), cfg.url("/d/"), cfg.url("/tree")), false),
        "ping" | "usage" => (engine_line(cfg), false),
        "buffer" => (peek_prelude(), false),
        "flush" => (match take_prelude() {
            Some(p) => match crate::tell::tell(cfg, &p, "telegram") { Ok(_) => "sent".into(), Err(e) => e },
            None => "nothing queued".into(),
        }, false),
        "diag" => (diag::brief(cfg), true),
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
            let prose: Vec<log::Msg> = log::since(cfg, -1).into_iter().filter(|m| m.is_prose()).collect();
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
    // First run: start from the newest update rather than replaying a day of backlog into
    // the conversation.
    if cfg::state()["tg_offset"].as_i64().is_none() {
        let mut st = cfg::state();
        let newest = api(&cfg, "getUpdates", json!({"offset": -1, "timeout": 0})).ok()
            .and_then(|v| v["result"].as_array().and_then(|a| a.last())
                .and_then(|u| u["update_id"].as_i64()));
        st["tg_offset"] = Value::from(newest.map(|i| i + 1).unwrap_or(0));
        cfg::put_state(&st);
    }
    loop {
        let mut st = cfg::state();
        let offset = st["tg_offset"].as_i64().unwrap_or(0);
        let r = api(&cfg, "getUpdates", json!({"offset": offset, "timeout": 20}));
        let Ok(v) = r else { std::thread::sleep(Duration::from_secs(5)); continue };
        for up in v["result"].as_array().into_iter().flatten() {
            let id = up["update_id"].as_i64().unwrap_or(0);
            st["tg_offset"] = Value::from(id + 1);
            cfg::put_state(&st);
            let Some(msg) = up.get("message") else { continue };
            let chat = msg["chat"]["id"].as_i64().unwrap_or(0);
            let text = msg["text"].as_str().unwrap_or("").to_string();
            if text.is_empty() { continue }

            // pairing: the first chat to speak owns the bridge
            let mut c = Cfg::load();
            if c.num("telegram.chat_id", 0) == 0 {
                c.set("telegram.chat_id", Value::from(chat));
                c.save();
                let _ = push(&c, "Paired. Facet is listening here.");
            }
            let c = Cfg::load();
            if c.num("telegram.chat_id", 0) != chat { continue }

            if text.starts_with('/') {
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
        let mut st = cfg::state();

        // replies
        // never ahead of the log: a fresh log (ids from 0 again) would otherwise stay silent
        let last = st["tg_sent"].as_i64().unwrap_or_else(|| log::last(&c)).min(log::last(&c));
        let mut high = last;
        for m in log::since(&c, last) {
            high = high.max(m.i);
            if m.kind == "talk" && !m.text.trim().is_empty() { let _ = push(&c, &m.text); }
        }
        st["tg_sent"] = Value::from(high);   // always: otherwise the cursor resets to "now"
                                            // each pass and anything in between is lost

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
        st["tg_slugs"] = json!(now);

        // new comments on notes
        let before = st["tg_diag"].as_i64().unwrap_or(-1);
        let count = diag::all(&c).len() as i64;
        if before >= 0 && count > before {
            let _ = push(&c, &format!("{} new comment(s), {} open\n{}",
                count - before, count, c.url("/d/")));
        }
        st["tg_diag"] = Value::from(count);
        cfg::put_state(&st);
    }
}

pub fn spawn(cfg: &Cfg) {
    if cfg.opt("telegram.bot_token").is_none() { return }
    let a = cfg.clone(); std::thread::spawn(move || inbound(a));
    let b = cfg.clone(); std::thread::spawn(move || outbound(b));
}
