// §7 The turn loop. Each user message starts a fresh `claude -p` call whose input is
// [system][view][new message]; nothing is carried over between calls.
//
// Mid-run messages, the part that is easy to get wrong with `claude -p` (measured, see
// README.md, Departures): Claude Code delivers a message written to its stdin at the next tool
// boundary, as part of the running call, cached normally. But a message written while the
// model is writing its final reply is not delivered: Claude Code runs it as a follow-up turn
// of the SAME conversation, with a stale view, which is the one thing the gist forbids.
// So a message is written to stdin only while a tool is running (its result is the boundary
// the message rides on); otherwise it is held, and written at the next tool call. If the call
// ends with messages written but not consumed (`--replay-user-messages` tells us which were),
// the process is killed at its `result`, before the follow-up turn can run, and those
// messages go back to the queue for a fresh call. Messages held and never written go back too.
// A follow-up turn that starts before that `result` reaches us (Claude Code hands a
// backgrounded subagent's report over that way) announces itself with a second `init`: it is
// killed there, and nothing it says is logged, since its view is this call's.
use super::claude::{self, Meter, Proc, Tick};
use super::engine::{state_dir, Engine};
use super::{prompts, view, CAP};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
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
    /// notes imported while a turn runs (text, date): logged when it is done (§10)
    pub notes: Vec<(String, String)>,
    /// sent for a turn of their own: each starts one fresh call, in order, once the running
    /// turn and anything queued for it are done; never delivered mid-run
    pub later: VecDeque<String>,
}

fn user_line(text: &str) -> String {
    format!("{}\n", json!({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": text}]}}))
}

/// Messages accepted but not yet in the log (queued, held, or written but not consumed) are
/// kept in a file too, so a crash or restart of the engine loses none of them (§2).
/// Messages for their own turns go in `later.json`, so a restart keeps them apart.
fn save(e: &Engine, t: &State) {
    let all: Vec<&String> = t.sent.iter().chain(t.held.iter()).chain(t.queue.iter()).collect();
    let later: Vec<&String> = t.later.iter().collect();
    for (name, all) in [("queue.json", all), ("later.json", later)] {
        let f = state_dir(&e.dir).join(name);
        if all.is_empty() { let _ = std::fs::remove_file(&f); continue }
        let tmp = f.with_extension("tmp");
        if std::fs::write(&tmp, serde_json::to_string(&all).unwrap()).is_ok() { let _ = std::fs::rename(&tmp, &f); }
    }
}

fn save_notes(e: &Engine, t: &State) {
    let f = state_dir(&e.dir).join("notes.json");
    if t.notes.is_empty() { let _ = std::fs::remove_file(&f); return }
    let tmp = f.with_extension("tmp");
    if std::fs::write(&tmp, serde_json::to_string(&t.notes).unwrap()).is_ok() { let _ = std::fs::rename(&tmp, &f); }
}

/// An imported note (§10). Between turns it goes straight into the log; during a turn (the
/// agent importing a file itself, say) it waits for the turn to end, so the turn's own
/// messages stay together. The same text twice is added once.
pub fn note(e: &Arc<Engine>, text: String, date: String) -> Value {
    if text.trim().is_empty() { return json!({"ok": false, "error": "empty"}) }
    if e.mem.lock().unwrap().store.msgs.iter().any(|m| m.kind == "note" && m.text == text) {
        return json!({"ok": true, "skipped": "already in the memory"})
    }
    let mut t = e.turn.lock().unwrap();
    if t.notes.iter().any(|n| n.0 == text) { return json!({"ok": true, "skipped": "already queued"}) }
    if t.running {
        t.notes.push((text, date));
        save_notes(e, &t);
        return json!({"ok": true, "queued": true})
    }
    drop(t);
    e.log_at("note", &text, &date);
    json!({"ok": true, "i": e.mem.lock().unwrap().store.t() - 1})
}

/// Log the notes that waited for a turn to end, and forget them.
fn log_notes(e: &Arc<Engine>, notes: Vec<(String, String)>) {
    if notes.is_empty() { return }
    for (text, date) in &notes { e.log_at("note", text, date); }
    save_notes(e, &e.turn.lock().unwrap());
    e.notice(&format!("{} imported note{} added", notes.len(), if notes.len() == 1 { "" } else { "s" }));
}

/// At start: whatever a previous engine accepted and never logged goes in again, in order.
pub fn restore(e: &Arc<Engine>) {
    let f = state_dir(&e.dir).join("notes.json");
    if let Ok(body) = std::fs::read_to_string(&f) {
        let _ = std::fs::remove_file(&f);
        for (text, date) in serde_json::from_str::<Vec<(String, String)>>(&body).unwrap_or_default() { note(e, text, date); }
    }
    let read = |name: &str| -> Vec<String> {
        std::fs::read_to_string(state_dir(&e.dir).join(name)).ok()
            .and_then(|b| serde_json::from_str(&b).ok()).unwrap_or_default()
    };
    let (texts, later) = (read("queue.json"), read("later.json"));
    if texts.is_empty() && later.is_empty() { return }
    e.notice(&format!("{} message(s) from before the restart, queued again", texts.len() + later.len()));
    for t in texts { input(e, t, false); }
    for t in later { input(e, t, true); }
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
    /// subagents sent out this turn, their requests, and what they cost (§9)
    agents: usize, agent_reqs: usize, agent_eq: f64, agent_bytes: usize,
    /// the turn sent nothing to the chat, so its final text was sent there for it (`fallback`)
    fallback: bool,
}

/// One subagent, from the tool call that sent it to the report it hands back. Its own
/// requests never reach the chat, so nothing downstream can see them: they are counted
/// here, logged as `agent` in usage.jsonl and as one `agent` record in events.jsonl, so
/// delegating can later be weighed against doing the same work in the turn (§19).
struct Agent {
    kind: String, desc: String, task: String, ask: usize,
    at: Instant, reqs: usize, eq: f64, tokens: u64, tools: u64, ms: u64, status: String,
    /// Claude Code backgrounded it: no report comes back with the tool result, the
    /// notification of its end carries it instead (§9)
    bg: bool,
    /// its last words, the report should that notification come without a summary
    last: String,
}

impl Agent {
    fn new(input: &Value) -> Self {
        Agent {
            kind: input["subagent_type"].as_str().unwrap_or("").into(),
            desc: input["description"].as_str().unwrap_or("").into(),
            task: String::new(), ask: input["prompt"].as_str().unwrap_or("").len(),
            at: Instant::now(), reqs: 0, eq: 0.0, tokens: 0, tools: 0, ms: 0,
            status: "unfinished".into(), bg: false, last: String::new(),
        }
    }
}

/// A user message from any route (§7: "on user input text").
/// Every route gets told at once what happened to its message: `how` is
/// starting | queued (a turn is getting ready) | held (the call is replying; next tool call
/// or next turn) | delivered (a tool is running; the call sees it after the tool) |
/// later (`later` was asked and a turn is running: it gets a turn of its own after it).
/// An incoming message in the log: the user's own words, or a detached agent's report
/// ("[id] ..."), which is `work` like an in-turn subagent's and not the user's.
fn log_in(e: &Arc<Engine>, text: &str) {
    e.log(if crate::log::is_report(text) { "work" } else { "user" }, text);
}

pub fn input(e: &Arc<Engine>, text: String, later: bool) {
    let mut t = e.turn.lock().unwrap();
    let how = if later && t.running {
        t.later.push_back(text.clone());
        "later"
    } else if t.stdin.is_some() {
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
    e.emit(json!({"ev": "accepted", "how": how, "text": text, "waiting": t_later(e)}));
}
fn t_later(e: &Engine) -> usize { e.turn.lock().unwrap().later.len() }

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
        // notes imported during the last turn go in first; then what is queued, or else the
        // next message sent for a turn of its own; nothing: done now, not after the compactor
        // has caught up
        {
            let mut t = e.turn.lock().unwrap();
            let notes = std::mem::take(&mut t.notes);
            if t.queue.is_empty() {
                if let Some(x) = t.later.pop_front() { t.queue.push(x); save(e, &t); }
            }
            if notes.is_empty() && t.queue.is_empty() { t.running = false; t.phase = "idle".into(); break }
            drop(t);
            log_notes(e, notes);
        }
        let ts = Instant::now();
        let settled = settle(e);
        let settle_ms = ts.elapsed().as_millis();
        let mut t = e.turn.lock().unwrap();
        let texts = std::mem::take(&mut t.queue);
        if texts.is_empty() { t.running = false; t.phase = "idle".into(); break }
        if !settled || t.cancel {
            // §6: the user cancelled the wait; their messages stay in the log, unanswered,
            // and so do those waiting for turns of their own
            t.running = false; t.phase = "idle".into();
            let notes = std::mem::take(&mut t.notes);
            let later: Vec<String> = t.later.drain(..).collect();
            drop(t);
            for x in texts.iter().chain(&later) { log_in(e, x); }
            log_notes(e, notes);
            { let t = e.turn.lock().unwrap(); save(e, &t); }
            e.notice("cancelled");
            break;
        }
        drop(t);
        // the view is rendered BEFORE the new messages are logged (§7)
        let (view, parts, first) = { let m = e.mem.lock().unwrap(); (m.view.render(&m.store), m.view.parts.len(), m.store.t()) };
        let vstats = super::events::view_stats(&view, parts);
        for x in &texts { log_in(e, x); }
        { let t = e.turn.lock().unwrap(); save(e, &t); }
        let tc = Instant::now();
        let mut tr = Trace { first, ..Default::default() };
        call(e, &view, &texts.join("\n\n"), &mut tr);
        let mut rec = json!({
            "first": first, "last": e.mem.lock().unwrap().store.t().saturating_sub(1), "messages_in": texts.len(),
            "settle_ms": settle_ms, "ms": tc.elapsed().as_millis(), "steps": tr.steps, "primed": tr.primed,
            "prime_read": tr.prime_read, "prime_write": tr.prime_write, "midrun_delivered": tr.delivered,
            "queue_after": tr.requeued, "outcome": tr.outcome, "model": e.model(), "effort": e.conf.effort,
            "agents": tr.agents, "agent_reqs": tr.agent_reqs, "agent_eq": tr.agent_eq.round(),
            "agent_bytes": tr.agent_bytes, "fallback": tr.fallback,
        });
        if let (Some(r), Some(v)) = (rec.as_object_mut(), vstats.as_object()) { for (k, x) in v { r.insert(k.clone(), x.clone()); } }
        super::events::log(&e.dir, "turn", rec);
        let mut t = e.turn.lock().unwrap();
        if t.cancel {
            // §7: a stopped turn leaves the messages it never took in the log, unanswered,
            // and those waiting for turns of their own
            let mut left: Vec<String> = t.queue.drain(..).collect();
            left.extend(t.later.drain(..));
            t.running = false; t.phase = "idle".into();
            let notes = std::mem::take(&mut t.notes);
            drop(t);
            for x in &left { log_in(e, x); }
            log_notes(e, notes);
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
    super::engine::restart_if_idle(e);
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
fn args(e: &Engine, view: &str) -> Vec<String> {
    let sd = state_dir(&e.dir);
    let sys = prompts::system(&e.conf.name);
    super::events::system(&e.dir, "master", &sys);
    let f = sd.join("system.txt");
    if std::fs::read_to_string(&f).ok().as_deref() != Some(sys.as_str()) { let _ = std::fs::write(&f, &sys); }
    let mut a = claude::base_args(&e.model(), &e.conf.effort, &f.to_string_lossy(), &e.conf.tools);
    let mcp = json!({"mcpServers": {"optchat": {"type": "http", "url": e.mcp_url.get().cloned().unwrap_or_default()}}});
    a.extend(["--mcp-config".into(), mcp.to_string(), "--permission-mode".into(), e.conf.permission.clone(), "--replay-user-messages".into(),
        "--add-dir".into(), "/tmp".into()]);
    // Subagents run on the cheap model and are sent the view as it stands (§9): they read a
    // lot and write one short report. The view in there costs this call nothing: an agent
    // definition's prompt never enters the master's own request, so it cannot move the marks.
    if e.conf.tools.contains("Task") && !e.conf.agent_model.is_empty() {
        // By file, not inline: the view alone can pass Linux's 128 KiB cap on one argv string.
        let f = sd.join("agents.json");
        let defs = prompts::agents(&e.conf.name, &e.conf.agent_model, view);
        if std::fs::read_to_string(&f).ok().as_deref() != Some(defs.as_str()) { let _ = std::fs::write(&f, &defs); }
        a.extend(["--agents".into(), f.to_string_lossy().into_owned()]);
    }
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
    let a = args(e, view_text);
    let pieces = view::pieces(view_text);
    if e.conf.prime && pieces.len() > 1 { prime(e, &a, &pieces, tr); }
    if cancelled(e) { tr.outcome = "cancelled"; return }
    phase(e, "calling");
    let ttl = e.conf.ttl.clone();
    // OPTCHAT_DIR: so `facet import` run by the agent reaches this engine, not the default one
    let dir = e.dir.to_string_lossy().to_string();
    let mut p = match Proc::spawn(&a, &[("CLAUDE_CODE_PROMPT_CACHE_TTL", ttl.as_str()), ("OPTCHAT_DIR", dir.as_str())], &e.conf.cwd) {
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
    // Text is held until the next step (then logged as talk) or the end of the turn. After a
    // `send_chat` the chat message already said it and is in the stream word for word, so a
    // recap of it at the end is dropped (one reply, not two); with no send, the end text is
    // sent to the chat by `fallback` and logged only there.
    let mut after_send = false;
    let mut held: Option<String> = None;
    let chats = |e: &Arc<Engine>| e.mem.lock().unwrap().store.msgs.iter().filter(|x| x.kind == "chat").count();
    let chats0 = chats(e);
    let mut agents: HashMap<String, Agent> = HashMap::new(); // live subagents, by tool id
    let mut checked_tools = false;
    let mut mute: HashSet<String> = HashSet::new(); // zoom / date calls, by tool id: no result logged
    let mut quiet: HashSet<String> = HashSet::new(); // send_chat / answer_card calls, by tool id
    let mut killed = false;
    let mut stale = false; // a follow-up turn of this conversation has started (see `init`)
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
        // A subagent's own steps are not the chat: its calls, results and prose carry
        // `parent_tool_use_id` and are dropped here, so the log keeps the one report it
        // hands back (a top-level tool result) and nothing else. They are not streamed
        // either (no `stream_event` carries a parent), so the meter above never sees them:
        // each of the subagent's messages is one request, and is counted here instead.
        if let Some(id) = ev["parent_tool_use_id"].as_str() {
            if ev["type"] == "assistant" {
                let m = &ev["message"];
                let r = claude::Req { model: m["model"].as_str().unwrap_or("").into(), usage: m["usage"].clone() };
                let eq = super::usage::eq(&r.usage);
                e.spend("agent", &r);
                tr.agent_reqs += 1;
                tr.agent_eq += eq;
                let (kind, step) = match agents.get_mut(id) {
                    Some(a) => {
                        a.reqs += 1;
                        a.eq += eq;
                        for b in m["content"].as_array().into_iter().flatten() {
                            if let Some(s) = b["text"].as_str() { a.last = s.into(); }
                        }
                        (a.kind.clone(), a.reqs)
                    }
                    None => (String::new(), 0), // a subagent's own subagent: counted, not traced
                };
                super::events::req(&e.dir, "agent", &r, &cc, 0, json!({"turn": tr.first, "tool_use_id": id, "agent_kind": kind, "step": step}));
            }
            continue;
        }
        // nothing a follow-up turn says is the chat's: only the task notifications that
        // arrive beside it, and the result that lets this call go, are read from here on
        if stale && !matches!(ev["type"].as_str().unwrap_or(""), "system" | "result") { continue }
        match ev["type"].as_str().unwrap_or("") {
            "system" if ev["subtype"] == "init" && !checked_tools => {
                checked_tools = true;
                let tools = ev["tools"].as_array().cloned().unwrap_or_default();
                if !tools.iter().any(|t| t.as_str().is_some_and(|s| s.ends_with("zoom"))) {
                    e.notice("WARNING: the zoom tool is not available to the model (MCP not connected)");
                }
            }
            // A second init: Claude Code is starting a follow-up turn of the same
            // conversation, to hand the master a background report (or a message that came
            // in during the reply). Its view is the one this call started with, stale, which
            // §7 forbids — so nothing it says is logged, and the call ends as soon as no
            // subagent is still out. What caused it comes back as a message of its own.
            "system" if ev["subtype"] == "init" => {
                stale = true;
                if agents.is_empty() { p.kill(); killed = true; tr.outcome = "followup"; break }
            }
            // Claude Code's own account of a subagent: its id and type when it starts, and
            // its tokens, tool calls and duration when it ends. Kept beside our own count.
            "system" if ev["subtype"] == "task_started" || ev["subtype"] == "task_notification" => {
                let id = ev["tool_use_id"].as_str().unwrap_or("").to_string();
                if let Some(a) = agents.get_mut(&id) {
                    if let Some(t) = ev["task_id"].as_str() { a.task = t.into(); }
                    if let Some(k) = ev["subagent_type"].as_str() { a.kind = k.into(); }
                    if let Some(d) = ev["description"].as_str() { if a.desc.is_empty() { a.desc = d.into(); } }
                    if let Some(s) = ev["status"].as_str() { a.status = s.into(); }
                    if ev["is_backgrounded"].as_bool().unwrap_or(false) { a.bg = true; }
                    let u = &ev["usage"];
                    a.tokens = u["total_tokens"].as_u64().unwrap_or(a.tokens);
                    a.tools = u["tool_uses"].as_u64().unwrap_or(a.tools);
                    a.ms = u["duration_ms"].as_u64().unwrap_or(a.ms);
                }
                // A backgrounded subagent's report has no tool result to ride on: this
                // notification carries it, in `summary` (its own last words, word for word,
                // measured). It comes after the reply that sent it, so it cannot belong to
                // that turn: it goes in as the gist has it (§9), a message of its own
                // starting "[id] ", which starts a turn with a view that has it.
                if ev["subtype"] == "task_notification" && agents.get(&id).is_some_and(|a| a.bg) {
                    let a = agents.remove(&id).unwrap();
                    let said = ev["summary"].as_str().unwrap_or("");
                    let said = if said.trim().is_empty() { a.last.as_str() } else { said };
                    let report = if said.trim().is_empty() {
                        format!("(no report; the subagent ended {})", a.status)
                    } else { cap(said) };
                    let name = if a.task.is_empty() { id.clone() } else { a.task.clone() };
                    done(e, &a, &id, &report, tr);
                    e.notice(&format!("subagent {} reported after the reply: it starts a turn of its own", name));
                    input(e, format!("[{}] {}", name, report), true);
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
                            if s.trim().is_empty() {
                            } else {
                                held = Some(match held.take() { Some(h) => h + "\n\n" + s, None => s.to_string() });
                            }
                        }
                        "thinking" => e.emit(json!({"ev": "thought", "text": b["thinking"]})),
                        "tool_use" => {
                            let name = b["name"].as_str().unwrap_or("");
                            // an output tool's call is not logged as a step: the `chat` or
                            // `answer` message it logs itself is the record (mcp.rs)
                            // zoom and date leave no trace: their content is already memory, and
                            // they are no step either, so a recap held after a send stays held
                            let silent = name.ends_with("__zoom") || name.ends_with("__date");
                            if !silent {
                                if let Some(h) = held.take() { e.log("talk", &h); }
                                after_send = name.ends_with("__send_chat");
                            }
                            if silent {
                                if let Some(id) = b["id"].as_str() { mute.insert(id.to_string()); }
                            } else if is_output(name) {
                                if let Some(id) = b["id"].as_str() { quiet.insert(id.to_string()); }
                            } else {
                                e.log("tool", &logged_call(name, &b["input"]));
                            }
                            if name == "Task" || name == "Agent" {
                                if let Some(id) = b["id"].as_str() { agents.insert(id.to_string(), Agent::new(&b["input"])); }
                            }
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
                    if busy { deferred.push(m) } else { log_in(e, &m) }
                    // a deferred one is in neither list for a moment: keep it in the file
                    let t = e.turn.lock().unwrap();
                    let mut keep = State::default();
                    keep.sent = t.sent.clone(); keep.held = t.held.clone(); keep.queue = t.queue.clone(); keep.later = t.later.clone();
                    keep.queue.extend(deferred.iter().cloned());
                    save(e, &keep);
                }
            }
            "user" => {
                for b in ev["message"]["content"].as_array().cloned().unwrap_or_default() {
                    if b["type"] != "tool_result" { continue }
                    let id = b["tool_use_id"].as_str().unwrap_or("");
                    let text = cap(&result_text(&b["content"]));
                    // A backgrounded spawn's result is only Claude Code's receipt for it: an
                    // internal id and a warning never to quote it. It is not a report and
                    // does not end the agent, which stays out until its notification.
                    let bg = match agents.get_mut(id) {
                        Some(a) => { if text.starts_with("Async agent launched") { a.bg = true; } a.bg }
                        None => false,
                    };
                    let text = strip_cwd_note(&text);
                    let silent = mute.remove(id);
                    let ok = silent || (quiet.remove(id) && (text == super::mcp::SENT || text == super::mcp::ANSWERED));
                    if !bg && !ok {
                        // a subagent's report is the one thing it leaves behind: its own kind (§9)
                        let sent = agents.remove(id);
                        let kind = if sent.is_some() { "work" } else { "echo" };
                        if let Some(a) = sent { done(e, &a, id, &text, tr); }
                        e.log(kind, &text);
                    }
                    if !id.is_empty() { e.turn.lock().unwrap().pending.remove(id); }
                }
                if e.turn.lock().unwrap().pending.is_empty() {
                    for m in deferred.drain(..) { log_in(e, &m); }
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
    for m in deferred.drain(..) { log_in(e, &m); }
    // The text after the last step is held to the end, so that it is logged once: dropped if
    // it only recaps a send_chat that landed; else it is the reply the fallback sends (as
    // `chat`, not also as `talk`); else (a chat landed earlier in the turn) it is plain talk.
    let mut last = held.take();
    if let Some(h) = &last {
        if after_send && chats(e) > chats0 {
            super::events::log(&e.dir, "recap_dropped", json!({"turn": tr.first, "bytes": h.len()}));
            last = None;
        }
    }
    if tr.outcome != "cancelled" { fallback(e, tr, last); }
    else if let Some(h) = last { e.log("talk", &h); }
    // a subagent whose report never came back (the call ended or was killed first) still spent
    let left: Vec<(String, Agent)> = agents.drain().collect();
    for (id, a) in left { done(e, &a, &id, "", tr); }
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
    if killed && !cancel && tr.requeued > 0 { e.notice("messages that arrived after the last tool call go to a fresh call"); }
    if !killed { p.finish(); }
    if let Some(r) = meter.take() { e.spend("turn", &r); }
}

/// What a tool call is logged as. A `facet spawn` Bash call carries its whole task, which
/// `agent::spawn` logs verbatim as its own entry: the call is cut to its first line (the
/// command and flags, up to the heredoc) so the task is not in the stream three times. Only
/// the log is shortened, not the command that runs.
fn logged_call(name: &str, input: &Value) -> String {
    if name == "Bash" {
        if let Some(cmd) = input["command"].as_str() {
            if cmd.contains("facet spawn") && (cmd.contains('\n') || cmd.len() > 160) {
                let first = cmd.lines().next().unwrap_or("");
                let head: String = first.chars().take(160).collect();
                return format!("Bash {}", json!({"command": format!("{}… (task: see the spawn entry)", head)}));
            }
        }
    }
    format!("{} {}", name, input)
}

fn is_output(name: &str) -> bool { name.ends_with("__send_chat") || name.ends_with("__answer_card") }

/// The safety net for the chat venue (README, "Stream and venues"): the agent's plain text
/// reaches the stream only, so a turn that ends without a `send_chat` after a chat message
/// would leave the user with silence. Then the turn's final text (the `talk` lines after its
/// last step) goes to the chat as it stands, as kind `chat`, and an event records the
/// fallback; with no text at all, one line saying the turn ended without a reply. Card
/// comments and subagent reports are not owed a chat reply, so a turn of only those gets none.
fn fallback(e: &Arc<Engine>, tr: &mut Trace, last: Option<String>) {
    let keep = |last: Option<String>| if let Some(h) = last { e.log("talk", &h) };
    let pick = {
        let m = e.mem.lock().unwrap();
        let msgs = &m.store.msgs[tr.first.min(m.store.msgs.len())..];
        // the last chat message the user is owed an answer to, and whether one came after it
        let Some(k) = msgs.iter().rposition(|x| x.kind == "user" && crate::log::wants_chat(&x.text)) else { drop(m); keep(last); return };
        if msgs[k..].iter().any(|x| x.kind == "chat") { drop(m); keep(last); return }
        let after = &msgs[k + 1..];
        let tail = after.iter().rev().take_while(|x| x.kind == "talk").collect::<Vec<_>>();
        let text = if let Some(h) = &last {
            let mut v: Vec<&str> = tail.iter().rev().map(|x| x.text.trim()).collect();
            v.push(h.trim());
            v.join("\n\n")
        } else if !tail.is_empty() {
            tail.iter().rev().map(|x| x.text.trim()).collect::<Vec<_>>().join("\n\n")
        } else {
            after.iter().rev().find(|x| x.kind == "talk").map(|x| x.text.trim().to_string())
                .unwrap_or_else(|| format!("(the turn ended without a reply: {})", tr.outcome))
        };
        text
    };
    tr.fallback = true;
    super::events::log(&e.dir, "fallback", json!({"turn": tr.first, "bytes": pick.len()}));
    e.log("chat", &pick);
}

/// One subagent, finished (or cut off with the call): what it was asked, what it cost, and
/// the one thing it leaves in the chat. `eq` and `reqs` are ours, counted from its messages;
/// `tokens`, `tools` and `ms` are Claude Code's own account of the task. `report` is what the
/// chat has to carry afterwards, the number to weigh against the tool and result messages the
/// same work would have logged had the turn done it itself (§19).
fn done(e: &Arc<Engine>, a: &Agent, id: &str, report: &str, tr: &mut Trace) {
    tr.agents += 1;
    tr.agent_bytes += report.len();
    super::events::log(&e.dir, "agent", json!({
        "turn": tr.first, "tool_use_id": id, "task": a.task, "agent_kind": a.kind,
        "description": a.desc, "status": a.status, "ask_bytes": a.ask, "report_bytes": report.len(),
        "reqs": a.reqs, "eq": a.eq.round(), "tokens": a.tokens, "tools": a.tools,
        "task_ms": a.ms, "ms": a.at.elapsed().as_millis(),
    }));
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

/// A Bash result that ends "Shell cwd was reset to <dir>" carries that note only because the
/// command left the working directory: noise in the log, not something the command printed.
pub fn strip_cwd_note(t: &str) -> String {
    let t = t.trim_end();
    match t.rfind('\n') {
        Some(i) if t[i + 1..].starts_with("Shell cwd was reset to ") => t[..i].trim_end().to_string(),
        None if t.starts_with("Shell cwd was reset to ") => String::new(),
        _ => t.to_string(),
    }
}

#[cfg(test)]
mod logged_call_tests {
    use super::logged_call;
    use serde_json::json;
    #[test]
    fn spawn_task_is_not_repeated_in_the_tool_entry() {
        let c = logged_call("Bash", &json!({"command": "facet spawn --model sonnet --desc D - <<'EOF'\nSECRET TASK BODY\nEOF"}));
        assert!(c.contains("--desc D") && !c.contains("SECRET"), "{}", c);
        let c = logged_call("Bash", &json!({"command": "ls -la"}));
        assert_eq!(c, "Bash {\"command\":\"ls -la\"}");
    }
}

#[cfg(test)]
mod cwd_tests {
    use super::strip_cwd_note;
    #[test]
    fn strips_trailing_cwd_note() {
        assert_eq!(strip_cwd_note("ok\nShell cwd was reset to /home/facet"), "ok");
        assert_eq!(strip_cwd_note("a\nb\n\nShell cwd was reset to /x\n"), "a\nb");
        assert_eq!(strip_cwd_note("plain"), "plain");
        assert_eq!(strip_cwd_note("Shell cwd was reset to /x"), "");
        assert_eq!(strip_cwd_note("Shell cwd was reset to /x\nmore"), "Shell cwd was reset to /x\nmore");
    }
}
