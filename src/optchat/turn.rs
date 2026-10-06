// §7 The turn loop. Each user message starts a fresh `claude -p` call whose input is
// [system][view][new message]; nothing is carried over between calls.
//
// Mid-run messages, the part that is easy to get wrong with `claude -p` (measured, see
// DEVIATIONS.md): Claude Code delivers a message written to its stdin at the next tool
// boundary, as part of the running call, cached normally. But a message written while the
// model is writing its final reply is not delivered: Claude Code runs it as a follow-up turn
// of the SAME conversation, with a stale view, which is the one thing the gist forbids.
// So a message is written to stdin only while a tool is running (its result is the boundary
// the message rides on); otherwise it is held, and written at the next tool call. If the call
// ends with messages written but not consumed (`--replay-user-messages` tells us which were),
// the process is killed at its `result`, before the follow-up turn can run, and those
// messages go back to the queue for a fresh call. Messages held and never written go back too.
use super::claude::{self, Meter, Proc, Tick};
use super::engine::{state_dir, Engine};
use super::{prompts, view, CAP};
use serde_json::{json, Value};
use std::collections::{HashSet, VecDeque};
use std::io::Write;
use std::process::ChildStdin;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct State {
    /// a turn thread exists (settling, priming or calling)
    pub running: bool,
    /// messages for the next fresh call
    pub queue: Vec<String>,
    /// arrived during a live call, waiting for a tool to be running
    pub held: Vec<String>,
    /// written to the live call's stdin, not consumed yet
    pub sent: VecDeque<String>,
    stdin: Option<ChildStdin>,
    /// tool_use ids of the live call without a result yet
    pending: HashSet<String>,
    pub cancel: bool,
    pub phase: String,
}

fn user_line(text: &str) -> String {
    format!("{}\n", json!({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": text}]}}))
}

/// Messages accepted but not yet in the log (queued, held, or written but not consumed) are
/// kept in a file too, so a crash or restart of the engine loses none of them (§2).
fn save(e: &Engine, t: &State) {
    let all: Vec<&String> = t.sent.iter().chain(t.held.iter()).chain(t.queue.iter()).collect();
    let f = state_dir(&e.dir).join("queue.json");
    if all.is_empty() { let _ = std::fs::remove_file(&f); return }
    let tmp = f.with_extension("tmp");
    if std::fs::write(&tmp, serde_json::to_string(&all).unwrap()).is_ok() { let _ = std::fs::rename(&tmp, &f); }
}

/// At start: whatever a previous engine accepted and never logged goes in again, in order.
pub fn restore(e: &Arc<Engine>) {
    let f = state_dir(&e.dir).join("queue.json");
    let Ok(body) = std::fs::read_to_string(&f) else { return };
    let texts: Vec<String> = serde_json::from_str(&body).unwrap_or_default();
    if texts.is_empty() { return }
    e.notice(&format!("{} message(s) from before the restart, queued again", texts.len()));
    for t in texts { input(e, t); }
}

impl State {
    fn write(&mut self, text: String) {
        let ok = self.stdin.as_mut().is_some_and(|w| w.write_all(user_line(&text).as_bytes()).and_then(|_| w.flush()).is_ok());
        if ok { self.sent.push_back(text) } else { self.held.push(text) }
    }
    fn flush_held(&mut self) {
        for t in std::mem::take(&mut self.held) { self.write(t); }
    }
}

/// What a call did, for the event log only (events.rs).
#[derive(Default)]
struct Trace {
    first: usize, steps: usize, primed: bool, prime_read: u64, prime_write: u64,
    delivered: usize, requeued: usize, outcome: &'static str,
}

/// A user message from any route (§7: "on user input text").
/// Every route gets told at once what happened to its message: `how` is
/// starting | queued (a turn is getting ready) | held (the call is replying; next tool call
/// or next turn) | delivered (a tool is running; the call sees it after the tool).
pub fn input(e: &Arc<Engine>, text: String) {
    let mut t = e.turn.lock().unwrap();
    let how = if t.stdin.is_some() {
        if t.pending.is_empty() { t.held.push(text.clone()); "held" } else { t.write(text.clone()); "delivered" }
    } else {
        t.queue.push(text.clone());
        if t.running { "queued" } else {
            t.running = true;
            t.cancel = false;
            let e = e.clone();
            std::thread::spawn(move || run(&e));
            "starting"
        }
    };
    save(e, &t);
    drop(t);
    super::events::log(&e.dir, "input", json!({"how": how, "bytes": text.len()}));
    e.emit(json!({"ev": "accepted", "how": how, "text": text}));
}

pub fn cancel(e: &Arc<Engine>) {
    let mut t = e.turn.lock().unwrap();
    if !t.running { return }
    t.cancel = true;
    drop(t);
    let _m = e.mem.lock().unwrap();
    e.changed.notify_all();
}

fn phase(e: &Engine, p: &str) {
    e.turn.lock().unwrap().phase = p.into();
    e.emit(json!({"ev": "phase", "phase": p}));
}
fn cancelled(e: &Engine) -> bool { e.turn.lock().unwrap().cancel }

fn run(e: &Arc<Engine>) {
    loop {
        // nothing queued: done now, not after the compactor has caught up
        {
            let mut t = e.turn.lock().unwrap();
            if t.queue.is_empty() { t.running = false; t.phase = "idle".into(); break }
        }
        let ts = Instant::now();
        let settled = settle(e);
        let settle_ms = ts.elapsed().as_millis();
        let mut t = e.turn.lock().unwrap();
        let texts = std::mem::take(&mut t.queue);
        if texts.is_empty() { t.running = false; t.phase = "idle".into(); break }
        if !settled || t.cancel {
            // §6: the user cancelled the wait; their messages stay in the log, unanswered
            t.running = false; t.phase = "idle".into();
            drop(t);
            for x in &texts { e.log("user", x); }
            { let t = e.turn.lock().unwrap(); save(e, &t); }
            e.notice("cancelled");
            break;
        }
        drop(t);
        // the view is rendered BEFORE the new messages are logged (§7)
        let (view, parts, first) = { let m = e.mem.lock().unwrap(); (m.view.render(&m.store), m.view.parts.len(), m.store.t()) };
        let vstats = super::events::view_stats(&view, parts);
        for x in &texts { e.log("user", x); }
        { let t = e.turn.lock().unwrap(); save(e, &t); }
        let tc = Instant::now();
        let mut tr = Trace { first, ..Default::default() };
        call(e, &view, &texts.join("\n\n"), &mut tr);
        let mut rec = json!({
            "first": first, "last": e.mem.lock().unwrap().store.t().saturating_sub(1), "messages_in": texts.len(),
            "settle_ms": settle_ms, "ms": tc.elapsed().as_millis(), "steps": tr.steps, "primed": tr.primed,
            "prime_read": tr.prime_read, "prime_write": tr.prime_write, "midrun_delivered": tr.delivered,
            "queue_after": tr.requeued, "outcome": tr.outcome, "model": e.conf.model, "effort": e.conf.effort,
        });
        if let (Some(r), Some(v)) = (rec.as_object_mut(), vstats.as_object()) { for (k, x) in v { r.insert(k.clone(), x.clone()); } }
        super::events::log(&e.dir, "turn", rec);
        let mut t = e.turn.lock().unwrap();
        if t.cancel {
            // §7: a stopped turn leaves the messages it never took in the log, unanswered
            let left: Vec<String> = t.queue.drain(..).collect();
            t.running = false; t.phase = "idle".into();
            drop(t);
            for x in &left { e.log("user", x); }
            { let t = e.turn.lock().unwrap(); save(e, &t); }
            e.notice("cancelled");
            break;
        }
    }
    // one line: what is left, and what this turn (with the compaction since the last) took
    let tally = std::mem::take(&mut *e.tally.lock().unwrap());
    let eq: f64 = tally.values().sum();
    let now = e.session_used();
    let before = std::mem::replace(&mut *e.util_mark.lock().unwrap(), now);
    let took = match (before, now) {
        (Some(b), Some(n)) if n >= b => format!(" (this turn {}%)", ((n - b) * 100.0).round()),
        _ => String::new(),
    };
    let line = super::usage::limits_line(&e.limits());
    if line.is_empty() { e.notice(&format!("done · {:.0}k eq", eq / 1e3)) }
    else { e.notice(&format!("done · {}", line.replacen(" · ", &format!("{} · ", took), 1))) }
    e.emit(json!({"ev": "state", "busy": false, "queued": 0}));
    commit(e);
}

/// §6: wait until every line of the view is a summary. False if cancelled.
fn settle(e: &Arc<Engine>) -> bool {
    phase(e, "settling");
    let mut m = e.mem.lock().unwrap();
    let mut said = false;
    loop {
        if m.view.settled(&m.store) { return true }
        if cancelled(e) { return false }
        if !said {
            let k = m.view.parts.iter().filter(|p| !m.store.built(p.l, p.i)).count();
            let why = m.pause_reason().map(|r| format!(" (compactor paused: {}; /resume)", r)).unwrap_or_default();
            drop(m);
            e.notice(&format!("waiting for {} summar{}{}", k, if k == 1 { "y" } else { "ies" }, why));
            m = e.mem.lock().unwrap();
            said = true;
        }
        m = e.changed.wait_timeout(m, Duration::from_millis(500)).unwrap().0;
    }
}

/// The arguments of a master call. Priming and the real call must use exactly these.
fn args(e: &Engine) -> Vec<String> {
    let sd = state_dir(&e.dir);
    let instr = std::fs::read_to_string(&e.conf.instructions).unwrap_or_default();
    let sys = prompts::system(&e.conf.name, &instr);
    super::events::system(&e.dir, "master", &sys);
    let f = sd.join("system.txt");
    if std::fs::read_to_string(&f).ok().as_deref() != Some(sys.as_str()) { let _ = std::fs::write(&f, &sys); }
    let mut a = claude::base_args(&e.conf.model, &e.conf.effort, &f.to_string_lossy(), &e.conf.tools);
    let mcp = json!({"mcpServers": {"optchat": {"type": "http", "url": e.mcp_url.get().cloned().unwrap_or_default()}}});
    a.extend(["--mcp-config".into(), mcp.to_string(), "--permission-mode".into(), e.conf.permission.clone(), "--replay-user-messages".into()]);
    if e.conf.safe_mode { a.push("--safe-mode".into()); }
    a
}

/// §8 with `claude -p`: Claude Code's own cache marks leave no room for marks in the view,
/// so a priming request (Claude Code's marks off, ours on each view piece) writes the view
/// into the cache, and is killed as soon as the API has accepted it. The real call's first
/// request then reads the whole view back.
fn prime(e: &Arc<Engine>, args: &[String], pieces: &[&str], tr: &mut Trace) {
    tr.primed = true;
    let mut cc = String::new();
    phase(e, "priming");
    let mut p = match Proc::spawn(args, &[("DISABLE_PROMPT_CACHING", "1")], &e.conf.cwd) {
        Ok(p) => p,
        Err(x) => { e.notice(&format!("priming: spawn failed: {}", x)); return }
    };
    let content: Vec<Value> = pieces.iter().map(|t| json!({"type": "text", "text": t, "cache_control": {"type": "ephemeral"}})).collect();
    if p.send(Value::Array(content)).is_err() { return }
    let mut meter = Meter::default();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(60) {
        if cancelled(e) { break }
        match p.next(Duration::from_millis(200)) {
            Ok(ev) => {
                e.observe(&ev);
                if let Some(v) = super::events::cc_version(&ev) { cc = v; }
                if let Some(Tick::Started(r)) = meter.feed(&ev) {
                    e.spend("prime", &r);
                    tr.prime_read = r.usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
                    tr.prime_write = r.usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                    super::events::req(&e.dir, "prime", &r, &cc, t0.elapsed().as_millis(), json!({"turn": tr.first, "pieces": pieces.len()}));
                    break
                }
                if let Some(o) = claude::outcome(&ev) { e.notice(&format!("priming failed: {}", o.text)); break }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => { e.notice("priming: claude exited"); break }
        }
    }
    p.kill();
}

/// One fresh model call for one batch of user messages.
fn call(e: &Arc<Engine>, view_text: &str, text: &str, tr: &mut Trace) {
    tr.outcome = "error";
    let a = args(e);
    let pieces = view::pieces(view_text);
    if e.conf.prime && pieces.len() > 1 { prime(e, &a, &pieces, tr); }
    if cancelled(e) { tr.outcome = "cancelled"; return }
    phase(e, "calling");
    let ttl = e.conf.ttl.clone();
    let mut p = match Proc::spawn(&a, &[("CLAUDE_CODE_PROMPT_CACHE_TTL", ttl.as_str())], &e.conf.cwd) {
        Ok(p) => p,
        Err(x) => { e.notice(&format!("cannot start claude: {}", x)); return }
    };
    let mut content: Vec<Value> = pieces.iter().map(|t| json!({"type": "text", "text": t})).collect();
    content.push(json!({"type": "text", "text": text}));
    if let Err(x) = p.send(Value::Array(content)) { e.notice(&format!("cannot write to claude: {}", x)); return }
    {
        let mut t = e.turn.lock().unwrap();
        t.stdin = p.take_stdin();
        t.pending.clear();
        t.sent.clear();
    }
    let mut meter = Meter::default();
    let mut replays = 0usize;
    let mut deferred: Vec<String> = Vec::new();
    let mut checked_tools = false;
    let mut killed = false;
    let (mut cc, mut tq) = (String::new(), Instant::now());
    loop {
        if cancelled(e) { p.kill(); killed = true; tr.outcome = "cancelled"; break }
        let ev = match p.next(Duration::from_millis(200)) {
            Ok(ev) => ev,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                let err = p.stderr.lock().unwrap().clone();
                if !err.trim().is_empty() { e.notice(&format!("claude exited: {}", err.trim())); }
                break;
            }
        };
        e.observe(&ev);
        if let Some(v) = super::events::cc_version(&ev) { cc = v; }
        if let Some(Tick::Done(r)) = meter.feed(&ev) {
            e.spend("turn", &r);
            tr.steps += 1;
            super::events::req(&e.dir, "turn", &r, &cc, tq.elapsed().as_millis(), json!({"turn": tr.first, "step": tr.steps}));
            tq = Instant::now();
        }
        match ev["type"].as_str().unwrap_or("") {
            "system" if ev["subtype"] == "init" && !checked_tools => {
                checked_tools = true;
                let tools = ev["tools"].as_array().cloned().unwrap_or_default();
                if !tools.iter().any(|t| t.as_str().is_some_and(|s| s.ends_with("zoom"))) {
                    e.notice("WARNING: the zoom tool is not available to the model (MCP not connected)");
                }
            }
            "stream_event" => {
                if ev["event"]["type"] == "content_block_start" && ev["event"]["content_block"]["type"] == "thinking" {
                    e.emit(json!({"ev": "phase", "phase": "thinking"}));
                }
                let d = &ev["event"]["delta"];
                if ev["event"]["type"] == "content_block_delta" && d["type"] == "text_delta" {
                    e.emit(json!({"ev": "delta", "text": d["text"]}));
                }
            }
            "assistant" => {
                for b in ev["message"]["content"].as_array().cloned().unwrap_or_default() {
                    match b["type"].as_str().unwrap_or("") {
                        "text" => {
                            let s = b["text"].as_str().unwrap_or("");
                            if !s.trim().is_empty() { e.log("talk", s); }
                        }
                        "thinking" => e.emit(json!({"ev": "thought", "text": b["thinking"]})),
                        "tool_use" => {
                            let line = format!("{} {}", b["name"].as_str().unwrap_or("?"), b["input"]);
                            e.log("tool", &line);
                            let mut t = e.turn.lock().unwrap();
                            if let Some(id) = b["id"].as_str() { t.pending.insert(id.into()); }
                            t.flush_held(); // a tool is about to run: its result is the boundary
                        }
                        _ => {}
                    }
                }
            }
            "user" if ev["isReplay"].as_bool().unwrap_or(false) => {
                replays += 1;
                if replays == 1 { continue } // the message that started this call
                let (msg, busy) = {
                    let mut t = e.turn.lock().unwrap();
                    (t.sent.pop_front(), !t.pending.is_empty())
                };
                if let Some(m) = msg {
                    tr.delivered += 1;
                    if busy { deferred.push(m) } else { e.log("user", &m) }
                    // a deferred one is in neither list for a moment: keep it in the file
                    let t = e.turn.lock().unwrap();
                    let mut keep = State::default();
                    keep.sent = t.sent.clone(); keep.held = t.held.clone(); keep.queue = t.queue.clone();
                    keep.queue.extend(deferred.iter().cloned());
                    save(e, &keep);
                }
            }
            "user" => {
                for b in ev["message"]["content"].as_array().cloned().unwrap_or_default() {
                    if b["type"] != "tool_result" { continue }
                    e.log("echo", &cap(&result_text(&b["content"])));
                    if let Some(id) = b["tool_use_id"].as_str() { e.turn.lock().unwrap().pending.remove(id); }
                }
                if e.turn.lock().unwrap().pending.is_empty() {
                    for m in deferred.drain(..) { e.log("user", &m); }
                }
            }
            "result" => {
                if let Some(o) = claude::outcome(&ev) { if o.error { e.notice(&format!("call ended with an error: {}", o.text)); } }
                let unconsumed = !e.turn.lock().unwrap().sent.is_empty();
                if unconsumed { p.kill(); killed = true; }
                tr.outcome = if claude::outcome(&ev).is_some_and(|o| o.error) { "error" } else { "done" };
                break;
            }
            _ => {}
        }
    }
    for m in deferred.drain(..) { e.log("user", &m); }
    let mut t = e.turn.lock().unwrap();
    t.stdin = None;
    t.pending.clear();
    // written but never consumed, then held: both go back for a fresh call, oldest first
    let mut back: Vec<String> = t.sent.drain(..).collect();
    back.extend(t.held.drain(..));
    back.extend(t.queue.drain(..));
    let cancel = t.cancel;
    tr.requeued = back.len();
    t.queue = back;
    save(e, &t);
    drop(t);
    if killed && !cancel { e.notice("messages that arrived after the last tool call go to a fresh call"); }
    if !killed { p.finish(); }
    if let Some(r) = meter.take() { e.spend("turn", &r); }
}

fn result_text(c: &Value) -> String {
    match c {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(|b| match b["type"].as_str() {
            Some("text") => b["text"].as_str().unwrap_or("").to_string(),
            Some(t) => format!("[{}]", t),
            None => String::new(),
        }).collect::<Vec<_>>().join("\n"),
        Value::Null => String::new(),
        x => x.to_string(),
    }
}

/// §7: a tool result is capped at CAP characters, head and tail kept, with a note of the cut.
pub fn cap(s: &str) -> String {
    let n = s.chars().count();
    if n <= CAP { return s.to_string() }
    let half = CAP / 2;
    let head: String = s.chars().take(half).collect();
    let tail: String = s.chars().skip(n - half).collect();
    format!("{}\n[… {} characters cut …]\n{}", head, n - CAP, tail)
}

/// Persist after each turn (§10): the chat directory is a git repository.
fn commit(e: &Engine) {
    let d = e.dir.clone();
    let n = e.mem.lock().unwrap().store.t();
    std::thread::spawn(move || {
        let git = |a: &[&str]| std::process::Command::new("git").arg("-C").arg(&d).args(a)
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
        let _ = git(&["add", "-A"]);
        let _ = git(&["commit", "-q", "-m", &format!("chat: {} messages", n)]);
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn cap_keeps_head_and_tail() {
        let s = format!("{}{}", "a".repeat(20_000), "b".repeat(20_000));
        let c = super::cap(&s);
        assert!(c.starts_with(&"a".repeat(15_000)));
        assert!(c.ends_with(&"b".repeat(15_000)));
        assert!(c.contains("[… 10000 characters cut …]"));
        assert_eq!(super::cap("short"), "short");
    }
}
