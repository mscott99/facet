// §6/§7 The way in. Every user message starts an instance (instance.rs): a fresh `claude -p`
// whose input is [system][view][message], which answers the user itself and ends by writing
// its end record. Nothing is carried over between instances but the log.
//
// This file is the starter. User messages, and tasks one instance hands to another with
// `facet spawn`, wait in one queue and are started one at a time, in arrival order: settle
// (every line of the view a summary, §6), render the view, log the user's message, then
// `instance::launch` primes the cache and starts the process. A start takes
// seconds; the instance then runs on its own, detached, and the next one starts. So each
// instance's view is the log as it stood at its own start: the end records of instances that
// had ended, nothing later; the ones still running are listed in its start block, not logged.
use super::claude::{self, Meter, Proc, Tick};
use super::engine::{state_dir, Engine};
use super::{prompts, view, CAP};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Where `facet spawn --wait` gets the end record of the instance it started.
pub type Waiter = std::sync::mpsc::Sender<String>;

/// One instance waiting to start: a user message (`from` None) or a task handed on by the
/// instance `from` (or by a shell: `from` empty).
pub struct Job {
    pub id: String,
    pub model: String,
    pub effort: String,
    pub text: String,
    pub from: Option<String>,
    pub desc: String,
    pub wait: Option<Waiter>,
    /// the inbox item a user message came in as (owners.rs: replies to it find its instance)
    pub inbox: Option<String>,
    /// a start again, after `wait_for`: the same instance, the same message
    pub resume: Option<Resume>,
}

/// What an instance that deferred (`wait_for`) keeps for its next start: the message it
/// serves, those sent to it while it ran, whom it waited for and the note it left itself.
#[derive(Clone, Debug, Default)]
pub struct Resume {
    pub msg: Option<usize>,
    pub extra: Vec<usize>,
    pub after: String,
    pub note: String,
    /// started again because its `pass_to` found the other ended, not after `wait_for`
    pub unpassed: bool,
}

#[derive(Default)]
pub struct State {
    /// the starter thread exists (settling, priming or launching)
    pub running: bool,
    /// waiting to start, oldest first
    pub queue: VecDeque<Job>,
    /// the starter is asked to stop waiting
    pub cancel: bool,
    /// bumped by `cancel`: an instance started under an older epoch stops
    pub epoch: u64,
    pub phase: String,
    /// instances running now, oldest first: name, and the line a new instance is shown
    /// (one waiting is still here, its line saying whom it waits for)
    pub live: Vec<(String, String)>,
    /// deferred instances (`wait_for`), each with the one it waits for
    pub waiting: Vec<(String, Job)>,
    /// `wait_for` calls not yet acted on: instance -> (the one it waits for, its note)
    pub deferring: std::collections::HashMap<String, (String, String)>,
    /// `pass_to` calls not yet acted on: instance -> the one it passes its message to
    pub passing: std::collections::HashMap<String, String>,
}

impl State {
    /// Whom `who` waits for, or is about to.
    pub fn waits_for(&self, who: &str) -> Option<&str> {
        self.deferring.get(who).map(|(o, _)| o.as_str())
            .or_else(|| self.waiting.iter().find(|(_, j)| j.id == who).map(|(o, _)| o.as_str()))
    }

    /// `wait_for(on)` by `who`: allowed if `on` runs (or waits) and waiting would not close a loop.
    pub fn defer(&mut self, who: &str, on: &str, note: &str) -> Result<(), String> {
        let on = on.trim().trim_start_matches('[').trim_end_matches(']');
        if on == who { return Err("an instance cannot wait for itself".into()) }
        if !self.live.iter().any(|(i, _)| i == on) { return Err(format!("[{}] is not running: zoom its end record instead", on)) }
        let mut at = on;
        while let Some(n) = self.waits_for(at) {
            if n == who { return Err(format!("[{}] already waits, through others, for you", on)) }
            at = n;
        }
        self.deferring.insert(who.to_string(), (on.to_string(), note.trim().to_string()));
        Ok(())
    }

    /// `pass_to(to)` by `who`: allowed if `who` answers a user message and `to` runs, takes
    /// messages (`taking`: it has its tap), and is not itself passing or waiting.
    pub fn pass(&mut self, who: &str, to: &str, taking: bool) -> Result<String, String> {
        let to = to.trim().trim_start_matches('[').trim_end_matches(']');
        if to == who { return Err("an instance cannot pass to itself".into()) }
        let mine = self.live.iter().find(|(i, _)| i == who).map(|x| x.1.clone()).unwrap_or_default();
        if !mine.contains(", answers ") { return Err("only an instance answering a user message can pass it on".into()) }
        if !self.live.iter().any(|(i, _)| i == to) { return Err(format!("[{}] is not running: answer it yourself, its end record is in the log", to)) }
        if self.waits_for(to).is_some() { return Err(format!("[{}] is waiting for another instance: wait_for it instead, or answer yourself", to)) }
        if self.passing.contains_key(to) { return Err(format!("[{}] is passing its own message on", to)) }
        if !taking { return Err(format!("[{}] is finishing and takes no more messages: wait_for it instead", to)) }
        if self.deferring.contains_key(who) { return Err("you already called wait_for".into()) }
        self.passing.insert(who.to_string(), to.to_string());
        Ok(to.to_string())
    }
}

fn new_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    format!("inst_{:x}", n & 0xffff_ffff_ffff)
}

/// User messages accepted but not yet in the log are kept in a file too, so a crash or restart
/// of the engine loses none of them (§2). Handed-on tasks are not: a restart waits for the
/// starter to be idle, so none is ever queued then.
fn save(e: &Engine, t: &State) {
    let all: Vec<&String> = t.queue.iter().filter(|j| j.from.is_none() && j.resume.is_none()).map(|j| &j.text).collect();
    let f = state_dir(&e.dir).join("queue.json");
    if all.is_empty() { let _ = std::fs::remove_file(&f); return }
    let tmp = f.with_extension("tmp");
    if std::fs::write(&tmp, serde_json::to_string(&all).unwrap()).is_ok() { let _ = std::fs::rename(&tmp, &f); }
}

/// An imported note (§10): straight into the log. The same text twice is added once.
pub fn note(e: &Arc<Engine>, text: String, date: String) -> Value {
    if text.trim().is_empty() { return json!({"ok": false, "error": "empty"}) }
    if e.mem.lock().unwrap().store.msgs.iter().any(|m| m.kind == "note" && m.text == text) {
        return json!({"ok": true, "skipped": "already in the memory"})
    }
    e.log_at("note", &text, &date);
    json!({"ok": true, "i": e.mem.lock().unwrap().store.t() - 1})
}

/// At start: whatever a previous engine accepted and never logged goes in again, in order.
pub fn restore(e: &Arc<Engine>) {
    let f = state_dir(&e.dir).join("queue.json");
    let texts: Vec<String> = std::fs::read_to_string(&f).ok().and_then(|b| serde_json::from_str(&b).ok()).unwrap_or_default();
    if texts.is_empty() { return }
    e.notice(&format!("{} message(s) from before the restart, queued again", texts.len()));
    for t in texts { input(e, t); }
}

/// Take what serve, the CLI and Telegram left in the inbox (inbox.rs) into the engine's own
/// queue, in arrival order. `input` has saved each message (queue.json) before its file goes.
pub fn drain_inbox(e: &Arc<Engine>) -> usize {
    super::inbox::drain_in(&super::inbox::dir(), |it| input_item(e, it))
}

/// A user message from any route (§7: "on user input text"): it starts an instance of its own.
pub fn input(e: &Arc<Engine>, text: String) {
    input_item(e, &super::inbox::Item { id: String::new(), text, to: None });
}

/// A message that names a running instance goes into it (instance.rs), logged at once as
/// `(to [inst_x]) text`, which is no new message to answer but one more for that instance. Any
/// other, or one for an instance that has ended, starts an instance of its own.
pub fn input_item(e: &Arc<Engine>, it: &super::inbox::Item) {
    if let Some(to) = &it.to {
        // the lock is held through the send: an instance takes its tap away under it, then
        // reads what was sent, so no message is sent into a tap that is no longer read
        let mut o = e.owners.lock().unwrap();
        if let Some((inst, tap)) = o.resolve(to) {
            let (inst, tap) = (inst.to_string(), tap.clone());
            let text = it.text.trim().to_string();
            if let Some(i) = e.log_at("user", &format!("(to [{}]) {}", inst, text), &super::store::now_iso()) {
                o.own(i, &inst);
                o.inbox(&it.id, i);
                let _ = tap.send(super::owners::Inject { msg: i, text, via: None });
                return
            }
        }
    }
    let inbox = Some(it.id.clone()).filter(|i| !i.is_empty());
    let job = Job { id: new_id(), model: e.model(), effort: e.conf.effort.clone(), text: it.text.clone(), from: None, desc: String::new(), wait: None, inbox, resume: None };
    enqueue(e, job);
}

/// A task handed on (`facet spawn`): an instance like any other, by default on `agent_model`
/// (Sonnet). Returns its name at once; with a waiter, its end record goes there too.
pub fn spawn(e: &Arc<Engine>, model: &str, effort: &str, desc: &str, task: &str, from: &str, wait: Option<Waiter>) -> Result<String, String> {
    let task = task.trim();
    if task.is_empty() { return Err("empty task".into()) }
    let pick = |x: &str, d: &str| if x.trim().is_empty() { d.to_string() } else { x.trim().to_string() };
    let job = Job {
        id: new_id(), model: pick(model, &e.conf.agent_model), effort: pick(effort, &e.conf.effort),
        text: task.to_string(), from: Some(from.trim().to_string()), desc: desc.trim().to_string(), wait, inbox: None, resume: None,
    };
    let id = job.id.clone();
    enqueue(e, job);
    Ok(id)
}

/// A deferred instance, started again (instance.rs): ahead of the queue.
pub fn requeue(e: &Arc<Engine>, job: Job) { enqueue(e, job) }

fn enqueue(e: &Arc<Engine>, job: Job) {
    let bytes = job.text.len();
    let text = if job.from.is_none() && job.resume.is_none() { job.text.clone() } else { String::new() };
    let mut t = e.turn.lock().unwrap();
    // one started again goes first: its message came before any still queued
    if job.resume.is_some() { t.queue.push_front(job) } else { t.queue.push_back(job) }
    let how = if t.running { "queued" } else {
        t.running = true;
        t.cancel = false;
        let e = e.clone();
        std::thread::spawn(move || run(&e));
        "starting"
    };
    save(e, &t);
    drop(t);
    // a message has arrived: nodes whose call failed are tried again now (§4 of the gist), so
    // a start waiting on one is not stuck
    let retry = { let mut m = e.mem.lock().unwrap(); let r = !m.held.is_empty(); m.held.clear(); r };
    if retry { super::compact::pump(e); }
    super::events::log(&e.dir, "input", json!({"how": how, "bytes": bytes}));
    if !text.is_empty() { e.emit(json!({"ev": "accepted", "how": how, "text": text})); }
}

/// Stop: the starter stops waiting (its queued messages are logged, unanswered), and every
/// running instance is stopped.
pub fn cancel(e: &Arc<Engine>) {
    let mut t = e.turn.lock().unwrap();
    t.epoch += 1;
    if t.running { t.cancel = true; }
    let waiting: Vec<Job> = t.waiting.drain(..).map(|(_, j)| j).collect();
    drop(t);
    for j in waiting { super::instance::cancel_waiting(e, j); }
    let _m = e.mem.lock().unwrap();
    e.changed.notify_all();
}

fn phase(e: &Engine, p: &str) {
    e.turn.lock().unwrap().phase = p.into();
    e.emit(json!({"ev": "phase", "phase": p}));
}
pub fn cancelled(e: &Engine) -> bool { e.turn.lock().unwrap().cancel }

fn run(e: &Arc<Engine>) {
    loop {
        if e.turn.lock().unwrap().queue.is_empty() { break }
        let ts = Instant::now();
        let settled = settle(e);
        let settle_ms = ts.elapsed().as_millis();
        let mut t = e.turn.lock().unwrap();
        if !settled || t.cancel {
            // §6: the user cancelled the wait; their messages stay in the log, unanswered
            let left: Vec<Job> = t.queue.drain(..).collect();
            t.cancel = false;
            save(e, &t);
            drop(t);
            for j in left {
                if j.resume.is_some() { super::instance::cancel_waiting(e, j); }
                else if j.from.is_none() { e.log("user", &j.text); }
            }
            e.notice("cancelled");
            break;
        }
        let Some(job) = t.queue.pop_front() else { break };
        let epoch = t.epoch;
        drop(t);
        // the view is rendered BEFORE the message is logged (§7)
        let (view, parts) = { let m = e.mem.lock().unwrap(); (m.view.render(&m.store), m.view.parts.len()) };
        let mut vstats = super::events::view_stats(&view, parts);
        vstats["settle_ms"] = json!(settle_ms);
        if let Some(r) = &job.resume {
            let msg = r.msg;
            { let t = e.turn.lock().unwrap(); save(e, &t); }
            super::instance::launch(e, job, view, msg, epoch, vstats);
            continue;
        }
        let msg = if job.from.is_none() { e.log("user", &job.text); Some(e.mem.lock().unwrap().store.t() - 1) } else { None };
        if let Some(i) = msg {
            let mut o = e.owners.lock().unwrap();
            o.own(i, &job.id);
            if let Some(x) = &job.inbox { o.inbox(x, i); }
        }
        { let t = e.turn.lock().unwrap(); save(e, &t); }
        super::instance::launch(e, job, view, msg, epoch, vstats);
    }
    let mut t = e.turn.lock().unwrap();
    // a message that came in after the last check finds `running` still set: start it
    if !t.queue.is_empty() { drop(t); let e2 = e.clone(); std::thread::spawn(move || run(&e2)); return }
    t.running = false;
    t.phase = "idle".into();
    drop(t);
    e.emit_state();
    super::engine::restart_if_idle(e);
}

/// §6: wait until every line of the view is a summary. False if cancelled.
fn settle(e: &Arc<Engine>) -> bool {
    phase(e, "settling");
    // whatever is ready is started now: a pause that has ended since the last message would
    // otherwise leave its nodes waiting for one (Engine::watch would also start them)
    super::compact::pump(e);
    let mut m = e.mem.lock().unwrap();
    let mut said = false;
    loop {
        if m.view.settled(&m.store) { return true }
        if cancelled(e) { return false }
        if !said {
            let k = m.view.parts.iter().filter(|p| !m.store.built(p.l, p.i)).count();
            let why = m.paused().map(|u| format!(" (compactor paused until {}: {})", super::engine::hhmm(u), m.pause_reason().unwrap_or_default())).unwrap_or_default();
            drop(m);
            e.notice(&format!("waiting for {} summar{}{}", k, if k == 1 { "y" } else { "ies" }, why));
            m = e.mem.lock().unwrap();
            said = true;
        }
        m = e.changed.wait_timeout(m, Duration::from_millis(500)).unwrap().0;
    }
}

/// The built-in tools of every instance: the configured list, never `Task`. Work is handed on
/// through `facet spawn` only: a Task call's request has its own system prompt and tools, and
/// lives and dies with its caller.
pub fn tools(e: &Engine) -> String {
    e.conf.tools.split(',').map(str::trim).filter(|t| !t.is_empty() && *t != "Task" && *t != "Agent")
        .collect::<Vec<_>>().join(",")
}

/// The flags of an instance's call, whose request is [tools][system][view][message] (§3.3).
/// Its priming request must use exactly these.
pub fn shared_args(e: &Engine, model: &str, effort: &str, mcp_url: &str) -> Vec<String> {
    let sd = state_dir(&e.dir);
    let sys = prompts::system(&e.conf.name);
    super::events::system(&e.dir, "system", &sys);
    let f = sd.join("system.txt");
    if std::fs::read_to_string(&f).ok().as_deref() != Some(sys.as_str()) { let _ = std::fs::write(&f, &sys); }
    let mut a = claude::base_args(model, effort, &f.to_string_lossy(), &tools(e));
    let mcp = json!({"mcpServers": {"optchat": {"type": "http", "url": mcp_url}}});
    a.extend(["--mcp-config".into(), mcp.to_string(), "--permission-mode".into(), e.conf.permission.clone(),
        "--add-dir".into(), "/tmp".into()]);
    if e.conf.safe_mode { a.push("--safe-mode".into()); }
    a
}

/// The first user message of an instance: the view, cut into the blocks the cache marks sit
/// on (§8), then one more block with the rest. The same view gives the same blocks, so
/// instances started from one view share its cached prefix.
pub fn content(view: &str, rest: &str) -> Value {
    let mut c: Vec<Value> = view::pieces(view).iter().map(|t| json!({"type": "text", "text": t})).collect();
    c.push(json!({"type": "text", "text": rest}));
    Value::Array(c)
}

/// The environment of an instance: the cache TTL (it sets the marks' ttl, part of what the
/// cache matches on) and the chat directory (so `facet ...` run inside reaches this engine).
pub fn env(e: &Engine) -> Vec<(&'static str, String)> {
    vec![("CLAUDE_CODE_PROMPT_CACHE_TTL", e.conf.ttl.clone()), ("OPTCHAT_DIR", e.dir.to_string_lossy().to_string())]
}

/// §8 with `claude -p`: Claude Code's own cache marks leave no room for marks in the view,
/// so a priming request (Claude Code's marks off, ours as in the gist: on the view's last
/// whole block of BLOCK lines and on the request's end) writes the view into the cache, and
/// is killed as soon as the API has accepted it. The instance then sends the same blocks, so
/// its first request reads the whole view back from the end mark; the next instance's priming
/// finds this one's last-whole-block mark within the API's 20-block lookback.
/// Returns the priming request's cache read and write, if it got that far.
pub fn prime(e: &Arc<Engine>, args: &[String], view_text: &str, tag: &str) -> Option<(u64, u64)> {
    let pieces = view::pieces(view_text);
    // with no whole block there is nothing worth priming: the call's own end mark covers it
    if !e.conf.prime || pieces.len() < 2 { return None }
    let marks = view::marked(view_text);
    let mut cc = String::new();
    phase(e, "priming");
    let mut p = match Proc::spawn(args, &[("DISABLE_PROMPT_CACHING", "1")], &e.conf.cwd) {
        Ok(p) => p,
        Err(x) => { e.notice(&format!("priming: spawn failed: {}", x)); return None }
    };
    let content: Vec<Value> = pieces.iter().zip(&marks).map(|(t, m)| if *m { json!({"type": "text", "text": t, "cache_control": {"type": "ephemeral"}}) } else { json!({"type": "text", "text": t}) }).collect();
    if p.send(Value::Array(content)).is_err() { return None }
    let mut meter = Meter::default();
    let t0 = Instant::now();
    let mut got = None;
    while t0.elapsed() < Duration::from_secs(60) {
        if cancelled(e) { break }
        match p.next(Duration::from_millis(200)) {
            Ok(ev) => {
                e.observe(&ev);
                if let Some(v) = super::events::cc_version(&ev) { cc = v; }
                if let Some(Tick::Started(r)) = meter.feed(&ev) {
                    e.spend("prime", &r);
                    let rw = (r.usage["cache_read_input_tokens"].as_u64().unwrap_or(0), r.usage["cache_creation_input_tokens"].as_u64().unwrap_or(0));
                    super::events::req(&e.dir, "prime", &r, &cc, t0.elapsed().as_millis(), json!({"instance": tag, "pieces": pieces.len()}));
                    got = Some(rw);
                    break
                }
                if let Some(o) = claude::outcome(&ev) { e.notice(&format!("priming failed: {}", o.text)); break }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => { e.notice("priming: claude exited"); break }
        }
    }
    p.kill();
    got
}

/// §1 of the gist: a tool's output is clipped to its head and tail, CAP characters in all,
/// the note of the cut included.
pub fn cap(s: &str) -> String {
    let n = s.chars().count();
    if n <= CAP { return s.to_string() }
    let keep = CAP - 64; // room for the note
    let head: String = s.chars().take(keep / 2).collect();
    let tail: String = s.chars().skip(n - (keep - keep / 2)).collect();
    format!("{}\n[… {} characters cut …]\n{}", head, n - keep, tail)
}

/// A Bash result that ends "Shell cwd was reset to <dir>" carries that note only because the
/// command left the working directory: noise, not something the command printed.
pub fn strip_cwd_note(t: &str) -> String {
    let t = t.trim_end();
    match t.rfind('\n') {
        Some(i) if t[i + 1..].starts_with("Shell cwd was reset to ") => t[..i].trim_end().to_string(),
        None if t.starts_with("Shell cwd was reset to ") => String::new(),
        _ => t.to_string(),
    }
}

/// Persist after each instance (§10): the chat directory is a git repository.
pub fn commit(e: &Engine) {
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
    use super::strip_cwd_note;
    #[test]
    fn cap_keeps_head_and_tail() {
        let s = format!("{}{}", "a".repeat(20_000), "b".repeat(20_000));
        let c = super::cap(&s);
        assert!(c.starts_with(&"a".repeat(14_968)));
        assert!(c.ends_with(&"b".repeat(14_968)));
        assert!(c.contains("[… 10064 characters cut …]"));
        assert!(c.chars().count() <= crate::optchat::CAP);
        assert_eq!(super::cap("short"), "short");
    }
    #[test]
    fn strips_trailing_cwd_note() {
        assert_eq!(strip_cwd_note("ok\nShell cwd was reset to /home/facet"), "ok");
        assert_eq!(strip_cwd_note("a\nb\n\nShell cwd was reset to /x\n"), "a\nb");
        assert_eq!(strip_cwd_note("plain"), "plain");
        assert_eq!(strip_cwd_note("Shell cwd was reset to /x"), "");
        assert_eq!(strip_cwd_note("Shell cwd was reset to /x\nmore"), "Shell cwd was reset to /x\nmore");
    }
}
