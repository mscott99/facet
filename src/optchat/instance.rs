// An instance: one `claude -p`, started for one user message or one handed-on task, with the
// view as it stood at its start. It answers the user itself (send_chat, the card tools) and
// its last text is its end record. Co-equal: there is no master; several run at once.
//
// Into the log go, each at its own moment: whatever it sends the user (`chat`, `answer`), as it
// sends it, and its end record (`work`, "[id] (answers K) ..."), when it ends. Nothing at its
// start: instances started while it runs see it in their running list, outside memory. Its own steps (text, tool calls, results) do
// not: they are kept beside the log, `<state dir>/agents/<id>.jsonl`, for zoom("id").
//
// The process is a child of the engine (a process that already outlives any one instance) in
// its own process group, owned by a thread of the engine; `facet restart` waits for every
// instance to end (`Engine::instances`).
use super::claude::{self, Meter, Proc, Tick};
use super::engine::{state_dir, Engine};
use super::turn::{self, Job};
use super::{prompts, usage};
use serde_json::{json, Value};
use std::io::Write;
use super::owners::Inject;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;
use std::time::{Duration, Instant};

/// Where an instance's own steps are kept: never inside the chat directory (they are not
/// memory; what it sent and its end record are).
pub fn dir(e: &Engine) -> std::path::PathBuf {
    let d = state_dir(&e.dir).join("agents");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// What it serves, as its end record's head names it and the running list shows it:
/// "answers 41", "task from [inst_0]: check". No brackets or ';' inside, so the head parses.
/// `extra`: the messages sent to it while it ran (the record names them all: "answers 41, 45").
pub fn serves(job: &Job, msg: Option<usize>, extra: &[usize]) -> String {
    let s = match (msg, &job.from) {
        (Some(i), _) => format!("answers {}", std::iter::once(i).chain(extra.iter().copied()).map(|x| x.to_string()).collect::<Vec<_>>().join(", ")),
        (None, Some(f)) => {
            let by = if f.is_empty() { String::new() } else { format!(" from [{}]", f) };
            let d = if job.desc.is_empty() { String::new() } else { format!(": {}", job.desc) };
            let x = if extra.is_empty() { String::new() } else { format!(", and answers {}", extra.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(", ")) };
            format!("task{}{}{}", by, d, x)
        }
        (None, None) => "task".to_string(),
    };
    s.replace(['(', ')'], "").replace(';', ",").replace('\n', " ")
}

/// The end record as logged: its name and what it served, then where it was posted
/// (`; to chat, card #c1`), then the message itself, as posted, unchanged.
pub fn end_record(id: &str, serves: &str, status: &str, text: &str, to: &[String]) -> String {
    let text = text.trim();
    let to = if to.is_empty() { String::new() } else { format!("; to {}", to.join(", ")) };
    let head = format!("[{}] ({}{})", id, serves, to);
    match (status, text.is_empty()) {
        ("done", false) => format!("{} {}", head, text),
        ("done", true) => format!("{} ended without an end message", head),
        (s, true) => format!("{} {}, no end message", head, s),
        (s, false) => format!("{} {}; its last text: {}", head, s, text),
    }
}

/// Prime, start the process, and hand it to a thread of its own. Nothing is logged at its
/// start: the instances started after it, while it runs, see it in their running list (not
/// memory). Called by the starter (turn.rs), one at a time, with the view as it stands now.
pub fn launch(e: &Arc<Engine>, job: Job, view: String, msg: Option<usize>, epoch: u64, mut stats: Value) {
    // one started again (after `wait_for`) was counted, and listed, all along
    if job.resume.is_none() { e.instances.fetch_add(1, SeqCst); }
    // what was sent to it while it waited to start again: answered with the rest, shown in its
    // start block (one sent from now until its tap goes into its input as it runs)
    let held = if job.resume.is_some() { e.owners.lock().unwrap().take_held(&job.id) } else { vec![] };
    let mut job = job;
    if let Some(r) = job.resume.as_mut() { r.extra.extend(held.iter().map(|(i, _)| *i)); }
    let extra = job.resume.as_ref().map(|r| r.extra.clone()).unwrap_or_default();
    let sv = serves(&job, msg, &extra);
    let running: Vec<String> = {
        let mut t = e.turn.lock().unwrap();
        let r = t.live.iter().filter(|(i, _)| i != &job.id).map(|(_, l)| l.clone()).collect();
        let line = format!("[{}] ({}, {})", job.id, job.model, sv);
        match t.live.iter_mut().find(|(i, _)| i == &job.id) {
            Some(l) => l.1 = line,
            None => t.live.push((job.id.clone(), line)),
        }
        r
    };
    // its own URL names it, so what it says in the chat is known to be its (owners.rs)
    let url = format!("{}?i={}", e.mcp_url.get().cloned().unwrap_or_default(), job.id);
    let args = turn::shared_args(e, &job.model, &job.effort, &url);
    if let Some((r, w)) = turn::prime(e, &args, &view, &job.id) { stats["prime_read"] = json!(r); stats["prime_write"] = json!(w); }
    let mut env = turn::env(e);
    env.push(("FACET_INSTANCE", job.id.clone()));
    let envr: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut head = prompts::start(&job.id, msg, &running, job.from.as_deref(), &job.text);
    if let Some(r) = &job.resume {
        head.push_str(&if r.unpassed { prompts::unpassed(&r.after) } else { prompts::resumed(&r.after, &r.note, &r.extra[..r.extra.len() - held.len()]) });
        for (i, text) in &held { head.push_str(&prompts::held(*i, text)); }
    }
    let started = Proc::spawn_detached(&args, &envr, &e.conf.cwd)
        .and_then(|mut p| p.send(turn::content(&view, &head)).map(|_| p));
    stats["instance"] = json!(job.id); stats["msg"] = json!(msg); stats["model"] = json!(job.model);
    super::events::log(&e.dir, "start", stats);
    let e2 = e.clone();
    match started {
        Ok(p) => {
            let (tx, rx) = std::sync::mpsc::channel();
            e.owners.lock().unwrap().tap(&job.id, tx);
            std::thread::spawn(move || run(e2, p, job, msg, epoch, rx));
        }
        Err(x) => finish(&e2, &job, &sv, msg.is_some(), "failed to start", &x.to_string(), false, None, json!({})),
    }
}

/// Owned by a thread of the engine: runs for as long as the process does.
fn run(e: Arc<Engine>, mut p: Proc, job: Job, msg: Option<usize>, epoch: u64, rx: Receiver<Inject>) {
    let at = Instant::now();
    let logf = dir(&e).join(format!("{}.jsonl", job.id));
    let mut meter = Meter::default();
    let (mut reqs, mut eq, mut tool_uses) = (0usize, 0.0f64, 0u64);
    let mut cc = String::new();
    let mut last = String::new();
    let mut status = "failed";
    // the messages the user sent it while it ran, in the log (they are answered by its end message)
    let mut answered: Vec<usize> = job.resume.as_ref().map(|r| r.extra.clone()).unwrap_or_default();
    // it called `wait_for`: whom for, and its note
    let mut deferred: Option<(String, String)> = None;
    // it called `pass_to`: whom for, and whether that one took its messages
    let mut passed: Option<(String, bool)> = None;
    // its first result is in and its input closed: claude finishes the messages it still has
    // queued (each may end in a result of its own) and exits
    let mut closing = false;
    // it said something to the user: an output tool's call that succeeded
    let mut spoke = false;
    let mut speaking: Vec<String> = Vec::new();
    let mut quiet = Instant::now();
    // where its final message goes: the input of its `route` call, the last one counting
    let mut ending: Option<Value> = None;
    loop {
        if e.turn.lock().unwrap().epoch != epoch { status = "cancelled"; p.kill(); break }
        inject(&mut p, &rx, &mut answered);
        let ev = match p.next(Duration::from_millis(500)) {
            Ok(ev) => ev,
            Err(RecvTimeoutError::Timeout) => {
                if quiet.elapsed() > Duration::from_secs(900) { status = "timed out"; p.kill(); break }
                continue
            }
            // after the first result, the process going away is its normal end
            Err(RecvTimeoutError::Disconnected) => { if !closing { status = "failed"; } break }
        };
        quiet = Instant::now();
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&logf) { let _ = writeln!(f, "{}", ev); }
        e.observe(&ev);
        if let Some(v) = super::events::cc_version(&ev) { cc = v; }
        if let Some(Tick::Done(r)) = meter.feed(&ev) {
            reqs += 1;
            eq += usage::eq(&r.usage);
            super::events::req(&e.dir, "instance", &r, &cc, 0, json!({"instance": job.id, "step": reqs}));
            e.spend("instance", &r);
        }
        match ev["type"].as_str().unwrap_or("") {
            "system" if ev["subtype"] == "init" => {
                let tools = ev["tools"].as_array().cloned().unwrap_or_default();
                if !tools.iter().any(|t| t.as_str().is_some_and(|s| s.ends_with("zoom"))) {
                    e.notice("WARNING: the zoom tool is not available to the model (MCP not connected)");
                }
            }
            "assistant" => for b in ev["message"]["content"].as_array().cloned().unwrap_or_default() {
                match b["type"].as_str().unwrap_or("") {
                    "text" => { let s = b["text"].as_str().unwrap_or(""); if !s.trim().is_empty() { last = s.to_string(); } }
                    "tool_use" => {
                        tool_uses += 1;
                        if b["name"].as_str().is_some_and(|n| n.ends_with("__route")) { ending = Some(b["input"].clone()); }
                        if speaks(b["name"].as_str().unwrap_or("")) { speaking.push(b["id"].as_str().unwrap_or("").to_string()); }
                    }
                    _ => {}
                }
            },
            "user" => for b in ev["message"]["content"].as_array().cloned().unwrap_or_default() {
                let id = b["tool_use_id"].as_str().unwrap_or("");
                if b["type"] == "tool_result" && speaking.iter().any(|x| x == id) && !b["is_error"].as_bool().unwrap_or(false) {
                    let t = match &b["content"] { Value::String(x) => x.clone(), c => c[0]["text"].as_str().unwrap_or("").to_string() };
                    if t == super::mcp::SENT || t.starts_with(super::mcp::ANSWERED) { spoke = true; }
                }
            },
            "result" => {
                let o = claude::outcome(&ev);
                let ok = o.as_ref().is_some_and(|o| !o.error);
                if let Some(o) = &o {
                    if !o.text.trim().is_empty() { last = o.text.clone(); }
                    if limit_hit(&e, &o.text, o.error) { status = "cut off by the usage limit"; break }
                    if o.error { e.notice(&format!("[{}] ended with an error: {}", job.id, o.text)); }
                }
                if ok { e.proof_of_life(); }
                status = if ok { "done" } else { "failed" };
                if !ok { break }
                // it deferred: no more messages into it; those sent meanwhile go to its next start
                let d = e.turn.lock().unwrap().deferring.remove(&job.id);
                if let Some(d) = d {
                    // held: what is sent to it from now on waits for its next start
                    e.owners.lock().unwrap().hold(&job.id);
                    while let Ok(m) = rx.try_recv() { answered.push(m.msg); }
                    deferred = Some(d);
                    break
                }
                // it passed its message on: that, and those sent to it, go into the other's
                // input (under the owners lock, so the other cannot drop its tap meanwhile)
                let pto = e.turn.lock().unwrap().passing.remove(&job.id);
                if let Some(to) = pto {
                    let mut o = e.owners.lock().unwrap();
                    o.untap(&job.id);
                    while let Ok(m) = rx.try_recv() { answered.push(m.msg); }
                    let took = match o.tap_of(&to).cloned() {
                        Some(tap) => {
                            let mut all: Vec<(usize, String)> = msg.map(|i| (i, job.text.clone())).into_iter().collect();
                            {
                                let m = e.mem.lock().unwrap();
                                let pre = format!("(to [{}]) ", job.id);
                                for &i in &answered {
                                    let t = m.store.msgs.get(i).map(|x| x.text.clone()).unwrap_or_default();
                                    all.push((i, t.strip_prefix(&pre).map(str::to_string).unwrap_or(t)));
                                }
                            }
                            for (i, text) in all {
                                o.own(i, &to);
                                let _ = tap.send(Inject { msg: i, text, via: Some(job.id.clone()) });
                            }
                            true
                        }
                        None => false,
                    };
                    // the other had ended: it starts again on its message, so it is held
                    if !took { o.hold(&job.id); }
                    passed = Some((to, took));
                    break
                }
                // no more messages into it from now on (under the lock they are sent under);
                // what was sent before is read, and if it got any, claude is left to finish them
                e.owners.lock().unwrap().untap(&job.id);
                inject(&mut p, &rx, &mut answered);
                if answered.is_empty() { break }
                p.close_input();
                closing = true;
            }
            _ => {}
        }
    }
    if status != "cancelled" && status != "timed out" { p.finish(); }
    if let Some(r) = meter.take() { e.spend("instance", &r); }
    let trace = json!({"reqs": reqs, "eq": eq.round(), "tools": tool_uses, "ms": at.elapsed().as_millis()});
    if let Some((on, note)) = deferred { return park(&e, job, msg, answered, on, note, trace) }
    if let Some((to, took)) = passed { return hand(&e, job, msg, answered, to, took, trace) }
    finish(&e, &job, &serves(&job, msg, &answered), msg.is_some(), status, &turn::cap(last.trim()), spoke, ending, trace);
}

/// `wait_for`: the instance has ended without answering. It stays counted (a restart waits for
/// it) and listed, its line naming whom it waits for, so no other instance takes up its work;
/// nothing is logged. When that one ends (`finish`), it is started again: same name, same
/// message, the view as it then stands. If that one has already ended, at once.
#[allow(clippy::too_many_arguments)]
fn park(e: &Arc<Engine>, mut job: Job, msg: Option<usize>, extra: Vec<usize>, on: String, note: String, mut trace: Value) {
    let sv = serves(&job, msg, &extra);
    let mark = json!({"type": "facet", "text": format!("(deferred: waits for [{}]{})", on, if note.is_empty() { String::new() } else { format!("; note: {}", note) })});
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir(e).join(format!("{}.jsonl", job.id))) { let _ = writeln!(f, "{}", mark); }
    trace["instance"] = json!(job.id); trace["serves"] = json!(sv); trace["status"] = json!("deferred");
    trace["model"] = json!(job.model); trace["on"] = json!(on);
    super::events::log(&e.dir, "instance", trace);
    job.resume = Some(turn::Resume { msg, extra, after: on.clone(), note, unpassed: false });
    let mut t = e.turn.lock().unwrap();
    if let Some(l) = t.live.iter_mut().find(|(i, _)| i == &job.id) {
        l.1 = format!("[{}] ({}, {}, waiting for [{}])", job.id, job.model, sv, on);
    }
    let id = job.id.clone();
    if t.live.iter().any(|(i, _)| i == &on) {
        t.waiting.push((on.clone(), job));
        drop(t);
        e.notice(&format!("[{}] waits for [{}]", id, on));
    } else {
        drop(t);
        turn::requeue(e, job);
    }
    e.emit_state();
}

/// `pass_to`: its messages are now the other's, which answers them; it leaves unlogged, and
/// those waiting for it wait for that one instead. If that one had ended before taking them,
/// it is started again on them, that one's end record in its view.
fn hand(e: &Arc<Engine>, mut job: Job, msg: Option<usize>, extra: Vec<usize>, to: String, took: bool, mut trace: Value) {
    let mark = json!({"type": "facet", "text": format!("(passed to [{}]{})", to, if took { "" } else { ", which had ended: starts again" })});
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir(e).join(format!("{}.jsonl", job.id))) { let _ = writeln!(f, "{}", mark); }
    trace["instance"] = json!(job.id); trace["serves"] = json!(serves(&job, msg, &extra));
    trace["status"] = json!(if took { "passed" } else { "pass failed" });
    trace["model"] = json!(job.model); trace["to"] = json!(to);
    super::events::log(&e.dir, "instance", trace);
    if !took {
        job.resume = Some(turn::Resume { msg, extra, after: to, note: String::new(), unpassed: true });
        turn::requeue(e, job);
        e.emit_state();
        return
    }
    {
        let mut t = e.turn.lock().unwrap();
        for (on, j) in t.waiting.iter_mut() {
            if on == &job.id {
                *on = to.clone();
                if let Some(r) = j.resume.as_mut() { r.after = to.clone(); }
            }
        }
        let renamed: Vec<String> = t.waiting.iter().filter(|(on, _)| on == &to).map(|(_, j)| j.id.clone()).collect();
        for (i, l) in t.live.iter_mut() {
            if renamed.contains(i) { *l = l.replace(&format!("waiting for [{}]", job.id), &format!("waiting for [{}]", to)); }
        }
    }
    e.notice(&format!("[{}] passed its message to [{}]", job.id, to));
    leave(e, &job);
}

/// A deferred instance stopped by a cancel before it started again: it ends as cancelled.
pub fn cancel_waiting(e: &Arc<Engine>, job: Job) {
    let r = job.resume.clone().unwrap_or_default();
    finish(e, &job, &serves(&job, r.msg, &r.extra), r.msg.is_some(), "cancelled", "", false, None, json!({}));
}

/// Write the messages the user has sent to this instance into its input, as they came.
fn inject(p: &mut Proc, rx: &Receiver<Inject>, answered: &mut Vec<usize>) {
    while let Ok(m) = rx.try_recv() {
        let t = match &m.via { Some(by) => prompts::passed(m.msg, by, &m.text), None => prompts::injected(m.msg, &m.text) };
        let _ = p.send_text(&t);
        answered.push(m.msg);
    }
}

/// The output tools: what an instance says to the user.
fn speaks(tool: &str) -> bool {
    ["__send_chat", "__send_file", "__answer_card", "__new_card"].iter().any(|t| tool.ends_with(t))
}

/// The usage limit cut the instance off ("You've hit your session limit · resets 7am (UTC)",
/// as an error or as its own text): park, so the pause is shown and pushed to Telegram.
fn limit_hit(e: &Arc<Engine>, text: &str, error: bool) -> bool {
    let t = text.trim_start().to_lowercase();
    if !((error && claude::limit_hit(text)) || t.starts_with("you've hit your") || t.starts_with("you have hit your")) { return false }
    let base = super::compact::LIMIT_BACKOFF_MS.load(std::sync::atomic::Ordering::Relaxed);
    let back = std::time::SystemTime::now() + Duration::from_millis(base);
    let until = claude::reset_at(text, chrono::Utc::now()).map(|r| r + Duration::from_secs(10)).map_or(back, |r| r.max(back));
    e.park(until, &format!("a reply was cut off by the usage limit ({}); send that message again after the pause", text.trim()));
    true
}

/// The end: one message, its final text (claude -p's last word), posted where it chose with
/// `route` (chat, cards; the last call counts) and logged once, unchanged, as its end record
/// (`work`), at this moment. The chat venue reads it from the log (`log::end_body`); a card
/// gets it as a reply. With no `route` call it goes back where the message came from: a chat
/// message to the chat, a card's message to that card, a handed-on task nowhere (to the chat
/// if spawned with `--chat`). With one,
/// what it leaves out is off: `route {chat: false}` posts nowhere (memory only), as when a
/// card is closed with nothing to say on it.
#[allow(clippy::too_many_arguments)]
fn finish(e: &Arc<Engine>, job: &Job, sv: &str, owed: bool, status: &str, last: &str, spoke: bool, ending: Option<Value>, mut trace: Value) {
    let served_chat = job.chat || (owed && crate::log::wants_chat(&job.text));
    let origin_card: Vec<String> = if owed { crate::cards::from_card(&job.text).map(|c| vec![c.to_string()]).unwrap_or_default() } else { vec![] };
    let text = last.to_string();
    let (chat, cards) = match &ending {
        Some(v) => (
            v["chat"].as_bool().unwrap_or(served_chat),
            v["cards"].as_array().map(|a| a.iter().filter_map(|c| c.as_str().map(|s| s.trim().trim_start_matches('#').to_string())).filter(|c| !c.is_empty()).collect()).unwrap_or_default(),
        ),
        None => (served_chat, origin_card),
    };
    let net = ending.is_none() && (chat || !cards.is_empty());
    let mut to = Vec::new();
    if chat { to.push("chat".to_string()); }
    let posted = if text.trim().is_empty() && status != "done" { format!("(no reply: the instance {})", status) } else { text.clone() };
    for c in &cards {
        match crate::cards::op(&crate::cfg::Cfg::load(), &json!({"do": "reply", "id": c, "text": posted})) {
            Ok(_) => to.push(format!("card #{}", c)),
            Err(x) => e.notice(&format!("[{}] end message to card #{} failed: {}", job.id, c, x)),
        }
    }
    let record = end_record(&job.id, sv, status, &text, &to);
    e.log("work", &record);
    if let Some(w) = &job.wait { let _ = w.send(record.clone()); }
    trace["instance"] = json!(job.id); trace["serves"] = json!(sv); trace["status"] = json!(status);
    trace["model"] = json!(job.model); trace["spoke"] = json!(spoke); trace["net"] = json!(net); trace["ended"] = json!(ending.is_some()); trace["to"] = json!(to); trace["record_bytes"] = json!(record.len());
    super::events::log(&e.dir, "instance", trace);
    let line = usage::limits_line(&e.limits());
    e.notice(&format!("[{}] {}{}", job.id, status, if line.is_empty() { String::new() } else { format!(" · {}", line) }));
    leave(e, job);
}

/// It is gone: off the live list, no more messages, no longer counted; those waiting for it
/// start again.
fn leave(e: &Arc<Engine>, job: &Job) {
    // those waiting for it start again (under the same lock its leaving the live list is
    // decided under, so a `wait_for` racing its end cannot be lost)
    let woken: Vec<Job> = {
        let mut t = e.turn.lock().unwrap();
        t.live.retain(|(i, _)| i != &job.id);
        t.deferring.remove(&job.id);
        t.passing.remove(&job.id);
        let (w, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut t.waiting).into_iter().partition(|(on, _)| on == &job.id);
        t.waiting = keep;
        w.into_iter().map(|(_, j)| j).collect()
    };
    for j in woken { turn::requeue(e, j); }
    e.owners.lock().unwrap().untap(&job.id);
    e.instances.fetch_sub(1, SeqCst);
    e.emit_state();
    turn::commit(e);
    super::engine::restart_if_idle(e);
}

/// A name as a file name: letters, digits, `_` and `-` only, so no name can reach outside
/// the agents directory.
pub fn safe(name: &str) -> String {
    name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect()
}

/// zoom("Name") (§6 of the gist): an instance's whole run, from its own file (an older
/// subagent's too, `spawn_...` or a Task agent's id). Its text, tool calls and their results
/// (each clipped as the log clips tool output).
pub fn transcript(e: &Engine, name: &str) -> String {
    let name = name.trim().trim_start_matches('[').trim_end_matches(']');
    let f = safe(name);
    if f.is_empty() || f != name {
        return format!("No instance {}: zoom takes an instance's name, as its end record starts \"[Name]\".", name);
    }
    let Ok(body) = std::fs::read_to_string(dir(e).join(format!("{}.jsonl", f))) else {
        return format!("No instance {}.", name);
    };
    let mut out = Vec::new();
    for line in body.lines() {
        let Ok(ev) = serde_json::from_str::<Value>(line) else { continue };
        let blocks = ev["message"]["content"].as_array().cloned().unwrap_or_default();
        match ev["type"].as_str().unwrap_or("") {
            "assistant" => for b in &blocks {
                match b["type"].as_str().unwrap_or("") {
                    "text" => { let t = b["text"].as_str().unwrap_or("").trim(); if !t.is_empty() { out.push(format!("talk: {}", t)); } }
                    "tool_use" => out.push(format!("tool: {} {}", b["name"].as_str().unwrap_or(""), b["input"])),
                    _ => {}
                }
            },
            "user" => for b in &blocks {
                if b["type"] == "tool_result" {
                    let t = match &b["content"] {
                        Value::String(x) => x.clone(),
                        Value::Array(a) => a.iter().filter_map(|x| x["text"].as_str()).collect::<Vec<_>>().join("\n"),
                        _ => String::new(),
                    };
                    out.push(format!("echo: {}", turn::cap(&turn::strip_cwd_note(&t))));
                }
            },
            "facet" => out.push(format!("echo: {}", ev["text"].as_str().unwrap_or(""))),
            _ => {}
        }
    }
    if out.is_empty() { format!("Instance {} has no steps on file yet.", name) } else { out.join("\n\n") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_records_name_what_they_serve() {
        let j = Job { id: "inst_1".into(), model: "opus".into(), effort: "high".into(), text: "hi".into(), from: None, desc: String::new(), wait: None, inbox: None, resume: None, chat: false };
        assert_eq!(serves(&j, Some(41), &[]), "answers 41");
        assert_eq!(serves(&j, Some(41), &[45, 47]), "answers 41, 45, 47");
        let t = Job { from: Some("inst_0".into()), desc: "check (a); b".into(), model: "sonnet".into(), text: "do X".into(), ..j };
        assert_eq!(serves(&t, None, &[]), "task from [inst_0]: check a, b");
        assert_eq!(end_record("inst_1", "answers 42", "done", " learned Y ", &[]), "[inst_1] (answers 42) learned Y");
        assert_eq!(end_record("inst_1", "answers 42", "cancelled", "", &[]), "[inst_1] (answers 42) cancelled, no end message");
        let to = ["chat".to_string(), "card #c1".to_string()];
        let r = end_record("inst_1", "answers 42", "done", "Done.", &to);
        assert_eq!(r, "[inst_1] (answers 42; to chat, card #c1) Done.");
        assert_eq!(crate::log::end_body(&r), Some("Done."));
        assert!(crate::log::is_report(&end_record("inst_1", "answers 42", "done", "x", &[])));
        assert!(crate::log::is_report(&end_record("inst_1", &serves(&t, None, &[]), "done", "x", &[])));
    }

    /// The architecture end to end, with a stand-in model: each message starts an instance;
    /// nothing is logged at a start, the end record at the end, naming the message it answers;
    /// an instance's view is the log as of its start (an ended one's end record; a running one
    /// only in its running list, outside the view); none of an instance's own
    /// steps reach the log; a chat message answered with nothing gets the safety net.
    #[test]
    fn instances_see_the_log_as_of_their_start() {
        let _env = crate::optchat::CLAUDE_ENV.lock().unwrap_or_else(|p| p.into_inner());
        let d = std::env::temp_dir().join(format!("facet-instances-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let ins = d.join("in");
        std::fs::create_dir_all(&ins).unwrap();
        // the stand-in: keeps what it was sent, under its instance's name; one tool step (which
        // must stay out of the log); the instance answering message 0 takes a while; its
        // last text is its end record
        let fake = d.join("fake.sh");
        std::fs::write(&fake, format!("#!/bin/sh\n\
            [ \"$DISABLE_PROMPT_CACHING\" = 1 ] && {{ echo '{{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"result\":\"no priming here\"}}'; exit 0; }}\n\
            IFS= read -r line\n\
            printf '%s\\n' \"$line\" > '{}'/\"$FACET_INSTANCE\"\n\
            echo '{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Bash\",\"input\":{{\"command\":\"STEP-ONLY\"}}}}]}}}}'\n\
            case \"$line\" in *'(message 0).'*) sleep 1.5;; esac\n\
            case \"$line\" in *ENDME*) echo '{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"id\":\"t2\",\"name\":\"mcp__optchat__route\",\"input\":{{\"chat\":false}}}}]}}}}';; esac\n\
            echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"LEARNED: something\"}}'\n",
            ins.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        unsafe { std::env::set_var("FACET_CLAUDE", &fake); }

        let e = crate::optchat::engine::bare(&d, "");
        let msgs = |e: &Engine| e.mem.lock().unwrap().store.msgs.iter().map(|m| (m.kind.clone(), m.text.clone())).collect::<Vec<_>>();
        let until = |what: &str, f: &dyn Fn() -> bool| {
            let t = Instant::now();
            while !f() { assert!(t.elapsed() < Duration::from_secs(20), "timed out: {}", what); std::thread::sleep(Duration::from_millis(20)); }
        };
        let idle = |e: &Engine| e.instances.load(SeqCst) == 0 && !e.turn.lock().unwrap().running;
        let card = |q: &str| format!("[[Some Note]] L3 #c{}: \"quote\" {}", q, q);

        turn::input(&e, card("aSLOW"));
        until("first started", &|| e.instances.load(SeqCst) == 1);
        turn::input(&e, card("b"));
        until("both ended", &|| msgs(&e).iter().filter(|m| m.0 == "work").count() == 2 && idle(&e));
        let m = msgs(&e);
        let kinds: Vec<&str> = m.iter().map(|x| x.0.as_str()).collect();
        // chronological: the fast second instance ends before the slow first one
        assert_eq!(kinds, ["user", "user", "work", "work"], "{:?}", m);
        let name = |m: &[(String, String)], k: usize| m[k].1[1..m[k].1.find(']').unwrap()].to_string();
        let (b, a) = (name(&m, 2), name(&m, 3));
        assert_eq!(m[2].1, format!("[{}] (answers 1) LEARNED: something", b));
        assert_eq!(m[3].1, format!("[{}] (answers 0) LEARNED: something", a));
        // no step of either reached the log
        assert!(!m.iter().any(|x| x.1.contains("STEP-ONLY")));
        // the steps are kept beside it, for zoom("Name")
        assert!(transcript(&e, &a).contains("STEP-ONLY"), "{}", transcript(&e, &a));

        // what each was sent: its view, then its own block naming it, the running ones and its message
        let sent = |n: &str| std::fs::read_to_string(ins.join(n)).unwrap();
        let (sa, sb) = (sent(&a), sent(&b));
        assert!(sa.contains(&format!("You are instance [{}]. No other instance is running.", a)) && sa.contains("aSLOW"));
        // the second saw the first in its running list, not in the log, and not its end record, which came later
        assert!(sb.contains(&format!("Running now, started before you: [{}] ({}, answers 0).", a, e.model())), "{}", sb);
        assert!(!sb.contains("LEARNED"), "{}", sb);
        assert!(sb.contains("(message 1)"));

        // a third, started after both ended, sees both end records; it serves a chat message
        // and says nothing to the user, so its last text is posted to the chat: one message,
        // logged once, as its end record
        turn::input(&e, "plain chat message".into());
        until("third ended", &|| msgs(&e).iter().filter(|m| m.0 == "work").count() == 3 && idle(&e));
        let m = msgs(&e);
        let c = name(&m, 5);
        let sc = sent(&c);
        assert!(sc.contains(&format!("[{}] (answers 0) LEARNED", a)) && sc.contains(&format!("[{}] (answers 1) LEARNED", b)), "{}", sc);
        let kinds: Vec<&str> = m[4..].iter().map(|x| x.0.as_str()).collect();
        assert_eq!(kinds, ["user", "work"], "{:?}", m);
        assert_eq!(m[5].1, format!("[{}] (answers 4; to chat) LEARNED: something", c));
        assert_eq!(crate::log::end_body(&m[5].1), Some("LEARNED: something"));

        // a handed-on task: an instance like any other, on agent_model; --wait gets its record
        let (tx, rx) = std::sync::mpsc::channel();
        let id = turn::spawn(&e, "", "", "look", "find the thing", &c, false, Some(tx)).unwrap();
        let rec = rx.recv_timeout(Duration::from_secs(20)).unwrap();
        assert_eq!(rec, format!("[{}] (task from [{}]: look) LEARNED: something", id, c));
        let m = msgs(&e);
        assert_eq!(m[6].0, "work");
        assert!(sent(&id).contains(&format!("a task handed to you by [{}]", c)));
        until("idle", &|| idle(&e));

        // one that routes its final message away from the chat: logged once, unchanged, posted
        // nowhere
        turn::input(&e, "ENDME please".into());
        until("fourth ended", &|| msgs(&e).iter().filter(|m| m.0 == "work").count() == 5 && idle(&e));
        let m = msgs(&e);
        let f = name(&m, 8);
        assert_eq!(m[8..].iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["work"], "{:?}", m);
        assert_eq!(m[8].1, format!("[{}] (answers 7) LEARNED: something", f));

        let _ = std::fs::remove_dir_all(state_dir(&d));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `wait_for`: the deferring instance ends unanswered and unlogged, stays listed as waiting,
    /// and starts again, same name and message, once the other has ended, its end record in view.
    #[test]
    fn a_deferred_instance_starts_again_after_the_one_it_waits_for() {
        let _env = crate::optchat::CLAUDE_ENV.lock().unwrap_or_else(|p| p.into_inner());
        let d = std::env::temp_dir().join(format!("facet-defer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let ins = d.join("in");
        std::fs::create_dir_all(&ins).unwrap();
        let fake = d.join("fake.sh");
        std::fs::write(&fake, format!("#!/bin/sh\n\
            [ \"$DISABLE_PROMPT_CACHING\" = 1 ] && {{ echo '{{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"result\":\"no priming here\"}}'; exit 0; }}\n\
            IFS= read -r line\n\
            printf '%s\\n' \"$line\" >> '{}'/\"$FACET_INSTANCE\"\n\
            case \"$line\" in *'(message 0).'*) sleep 2;; *) sleep 0.7;; esac\n\
            echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"DONE-HERE\"}}'\n",
            ins.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        unsafe { std::env::set_var("FACET_CLAUDE", &fake); }
        let e = crate::optchat::engine::bare(&d, "");
        let msgs = |e: &Engine| e.mem.lock().unwrap().store.msgs.iter().map(|m| (m.kind.clone(), m.text.clone())).collect::<Vec<_>>();
        let until = |what: &str, f: &dyn Fn() -> bool| {
            let t = Instant::now();
            while !f() { assert!(t.elapsed() < Duration::from_secs(20), "timed out: {}", what); std::thread::sleep(Duration::from_millis(20)); }
        };
        let live = |e: &Engine| e.turn.lock().unwrap().live.iter().map(|x| x.0.clone()).collect::<Vec<_>>();
        turn::input(&e, "SLOW one".into());
        until("first running", &|| live(&e).len() == 1);
        turn::input(&e, "waiter two".into());
        until("second running", &|| live(&e).len() == 2);
        let (a, b) = { let l = live(&e); (l[0].clone(), l[1].clone()) };
        {
            let mut t = e.turn.lock().unwrap();
            assert!(t.defer(&b, &b, "").is_err());
            assert!(t.defer(&b, "inst_gone", "").is_err());
            t.defer(&b, &format!("[{}]", a), "half done").unwrap();
            // a loop is refused
            assert!(t.defer(&a, &b, "").is_err());
        }
        until("second waits", &|| e.turn.lock().unwrap().waiting.len() == 1);
        assert!(e.turn.lock().unwrap().live.iter().any(|(i, l)| i == &b && l.contains(&format!("waiting for [{}]", a))));
        assert!(!msgs(&e).iter().any(|m| m.0 == "work"), "nothing logged at a defer");
        assert_eq!(e.instances.load(SeqCst), 2);
        // a message sent to it while it waits starts no instance: it is kept for its next start
        turn::input_item(&e, &crate::optchat::inbox::Item { id: String::new(), text: "HELD extra".into(), to: Some(crate::optchat::inbox::Target::Log { i: 1 }) });
        assert_eq!(msgs(&e)[2].1, format!("(to [{}]) HELD extra", b));
        assert_eq!(e.instances.load(SeqCst), 2);
        assert!(e.turn.lock().unwrap().queue.is_empty());
        until("both ended", &|| msgs(&e).iter().filter(|m| m.0 == "work").count() == 2 && e.instances.load(SeqCst) == 0 && !e.turn.lock().unwrap().running);
        let m = msgs(&e);
        assert_eq!(m[3].1, format!("[{}] (answers 0; to chat) DONE-HERE", a));
        assert_eq!(m[4].1, format!("[{}] (answers 1, 2; to chat) DONE-HERE", b));
        assert_eq!(m.len(), 5, "{:?}", m);
        let sent = std::fs::read_to_string(ins.join(&b)).unwrap();
        let starts: Vec<&str> = sent.lines().collect();
        assert_eq!(starts.len(), 2, "{}", sent);
        assert!(!starts[0].contains("DONE-HERE"));
        assert!(starts[1].contains(&format!("[{}] (answers 0; to chat) DONE-HERE", a)) && starts[1].contains("half done") && starts[1].contains("(message 1)") && starts[1].contains("Message 2 from the user, sent to you while you waited") && starts[1].contains("HELD extra"), "{}", starts[1]);
        assert!(transcript(&e, &b).contains("deferred: waits for"));
        let _ = std::fs::remove_dir_all(state_dir(&d));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `pass_to`: the passing instance leaves unlogged; its message goes into the other's input,
    /// told apart as passed on, and the other's one end record answers both.
    #[test]
    fn a_passed_message_is_answered_by_the_instance_it_went_to() {
        let _env = crate::optchat::CLAUDE_ENV.lock().unwrap_or_else(|p| p.into_inner());
        let d = std::env::temp_dir().join(format!("facet-pass-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let ins = d.join("in");
        std::fs::create_dir_all(&ins).unwrap();
        let fake = d.join("fake.sh");
        std::fs::write(&fake, format!("#!/bin/sh\n\
            [ \"$DISABLE_PROMPT_CACHING\" = 1 ] && {{ echo '{{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"result\":\"no priming here\"}}'; exit 0; }}\n\
            IFS= read -r line\n\
            printf '%s\\n' \"$line\" > '{0}'/\"$FACET_INSTANCE\"\n\
            case \"$line\" in *'(message 0).'*) sleep 3;; *) sleep 0.5;; esac\n\
            echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"FIRST\"}}'\n\
            if IFS= read -r second; then\n\
              printf '%s\\n' \"$second\" > '{0}'/\"$FACET_INSTANCE\".2\n\
              echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"SECOND\"}}'\n\
            fi\n", ins.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        unsafe { std::env::set_var("FACET_CLAUDE", &fake); }
        let e = crate::optchat::engine::bare(&d, "");
        let msgs = |e: &Engine| e.mem.lock().unwrap().store.msgs.iter().map(|m| (m.kind.clone(), m.text.clone())).collect::<Vec<_>>();
        let until = |what: &str, f: &dyn Fn() -> bool| {
            let t = Instant::now();
            while !f() { assert!(t.elapsed() < Duration::from_secs(20), "timed out: {}", what); std::thread::sleep(Duration::from_millis(20)); }
        };
        let live = |e: &Engine| e.turn.lock().unwrap().live.iter().map(|x| x.0.clone()).collect::<Vec<_>>();
        turn::input(&e, "SLOW one".into());
        until("first running", &|| live(&e).len() == 1);
        turn::input(&e, "also do this".into());
        until("second running", &|| live(&e).len() == 2);
        let (a, b) = { let l = live(&e); (l[0].clone(), l[1].clone()) };
        until("both take messages", &|| { let o = e.owners.lock().unwrap(); o.tap_of(&a).is_some() && o.tap_of(&b).is_some() });
        {
            let mut t = e.turn.lock().unwrap();
            assert!(t.pass(&b, &b, true).is_err());
            assert!(t.pass(&b, "inst_gone", true).is_err());
            assert!(t.pass(&b, &a, false).is_err(), "one finishing takes no more");
            t.pass(&b, &format!("[{}]", a), true).unwrap();
            assert!(t.pass(&a, &b, true).is_err(), "no passing back");
        }
        until("all ended", &|| e.instances.load(SeqCst) == 0 && !e.turn.lock().unwrap().running);
        let m = msgs(&e);
        let kinds: Vec<&str> = m.iter().map(|x| x.0.as_str()).collect();
        assert_eq!(kinds, ["user", "user", "work"], "nothing logged for the pass: {:?}", m);
        assert_eq!(m[2].1, format!("[{}] (answers 0, 1; to chat) SECOND", a));
        let sent = std::fs::read_to_string(ins.join(format!("{}.2", a))).unwrap();
        assert!(sent.contains(&format!("passed on to you by [{}]", b)) && sent.contains("also do this"), "{}", sent);
        assert!(transcript(&e, &b).contains(&format!("passed to [{}]", a)));
        let _ = std::fs::remove_dir_all(state_dir(&d));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A message sent to a running instance: logged `(to [id]) ...`, written to the model's
    /// input, answered in the same end message, whose head names both messages. One sent to an
    /// instance that has ended (by `Latest` or by a log message of its own) is an ordinary
    /// message and starts an instance.
    #[test]
    fn a_message_can_be_sent_to_a_running_instance() {
        use crate::optchat::inbox::{Item, Target};
        let _env = crate::optchat::CLAUDE_ENV.lock().unwrap_or_else(|p| p.into_inner());
        let d = std::env::temp_dir().join(format!("facet-sendto-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let ins = d.join("in");
        std::fs::create_dir_all(&ins).unwrap();
        // the stand-in, like claude -p: takes a first message, answers it, then answers a
        // second one if one was written to its input before it was closed
        let fake = d.join("fake.sh");
        std::fs::write(&fake, format!("#!/bin/sh\n\
            [ \"$DISABLE_PROMPT_CACHING\" = 1 ] && {{ echo '{{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"result\":\"no priming here\"}}'; exit 0; }}\n\
            IFS= read -r line\n\
            printf '%s\\n' \"$line\" > '{0}'/\"$FACET_INSTANCE\"\n\
            case \"$line\" in *SLOW*) sleep 1.5;; esac\n\
            echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"FIRST\"}}'\n\
            if IFS= read -r second; then\n\
              printf '%s\\n' \"$second\" > '{0}'/\"$FACET_INSTANCE\".2\n\
              echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"SECOND\"}}'\n\
            fi\n", ins.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        unsafe { std::env::set_var("FACET_CLAUDE", &fake); }

        let e = crate::optchat::engine::bare(&d, "");
        let msgs = |e: &Engine| e.mem.lock().unwrap().store.msgs.iter().map(|m| (m.kind.clone(), m.text.clone())).collect::<Vec<_>>();
        let until = |what: &str, f: &dyn Fn() -> bool| {
            let t = Instant::now();
            while !f() { assert!(t.elapsed() < Duration::from_secs(20), "timed out: {}", what); std::thread::sleep(Duration::from_millis(20)); }
        };
        let idle = |e: &Engine| e.instances.load(SeqCst) == 0 && !e.turn.lock().unwrap().running;
        let item = |text: &str, to: Option<Target>| Item { id: format!("item-{}", text), text: text.into(), to };

        turn::input_item(&e, &item("first SLOW", None));
        until("the first takes messages", &|| e.owners.lock().unwrap().resolve(&Target::Latest).is_some());
        turn::input_item(&e, &item("second", Some(Target::Latest)));
        until("it ended", &|| msgs(&e).iter().filter(|m| m.0 == "work").count() == 1 && idle(&e));
        let m = msgs(&e);
        let a = m[2].1[1..m[2].1.find(']').unwrap()].to_string();
        let kinds: Vec<&str> = m.iter().map(|x| x.0.as_str()).collect();
        assert_eq!(kinds, ["user", "user", "work"], "{:?}", m);
        assert_eq!(m[1].1, format!("(to [{}]) second", a));
        // one end message, naming both; the last result is the message
        assert_eq!(m[2].1, format!("[{}] (answers 0, 1; to chat) SECOND", a), "{:?}", m);
        // what the model was sent: the message, told apart as one sent while it ran
        let sent = std::fs::read_to_string(ins.join(format!("{}.2", a))).unwrap();
        assert!(sent.contains("Message 1 from the user") && sent.contains("second"), "{}", sent);

        // after it ended: to the latest, or to a message of its own, it is an ordinary message
        turn::input_item(&e, &item("third", Some(Target::Latest)));
        until("third ended", &|| msgs(&e).iter().filter(|m| m.0 == "work").count() == 2 && idle(&e));
        turn::input_item(&e, &item("fourth", Some(Target::Log { i: 0 })));
        until("fourth ended", &|| msgs(&e).iter().filter(|m| m.0 == "work").count() == 3 && idle(&e));
        turn::input_item(&e, &item("fifth", Some(Target::Inbox { id: "item-first SLOW".into() })));
        until("fifth ended", &|| msgs(&e).iter().filter(|m| m.0 == "work").count() == 4 && idle(&e));
        let m = msgs(&e);
        let users: Vec<&str> = m.iter().filter(|x| x.0 == "user").map(|x| x.1.as_str()).collect();
        assert_eq!(users, ["first SLOW", format!("(to [{}]) second", a).as_str(), "third", "fourth", "fifth"], "{:?}", m);
        assert!(m.iter().filter(|x| x.0 == "work").skip(1).all(|x| x.1.ends_with("FIRST") && !x.1.contains("answers 0, 1")), "{:?}", m);

        let _ = std::fs::remove_dir_all(state_dir(&d));
        let _ = std::fs::remove_dir_all(&d);
    }
}
