// An instance: one `claude -p`, started for one user message or one handed-on task, with the
// view as it stood at its start. It answers the user itself (send_chat, the card tools) and
// its last text is its end record. Co-equal: there is no master; several run at once.
//
// Into the log go, each at its own moment: the kickoff (`start`, "[id] kickoff: ..."), when it
// starts; whatever it sends the user (`chat`, `answer`), as it sends it; and its end record
// (`work`, "[id] (kickoff K) ..."), when it ends. Its own steps (text, tool calls, results) do
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
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;
use std::time::{Duration, Instant};

/// Where an instance's own steps are kept: never inside the chat directory (they are not
/// memory; its kickoff, what it sent and its end record are).
pub fn dir(e: &Engine) -> std::path::PathBuf {
    let d = state_dir(&e.dir).join("agents");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// The kickoff record: who it is, what it serves, on which model.
pub fn kickoff(job: &Job, msg: Option<usize>) -> String {
    let who = format!("[{}] kickoff ({}, effort {})", job.id, job.model, job.effort);
    match (msg, &job.from) {
        (Some(i), _) => format!("{}: answers message {}", who, i),
        (None, Some(f)) => {
            let by = if f.is_empty() { String::new() } else { format!(" from [{}]", f) };
            let d = if job.desc.is_empty() { String::new() } else { format!(" ({})", job.desc) };
            format!("{}: task{}{}:\n\n{}", who, by, d, job.text)
        }
        (None, None) => who,
    }
}

/// The end record as logged: its name and kickoff first, so it points back to it, then where
/// it was posted (`; to chat, card #c1`), then the message itself, as posted, unchanged.
pub fn end_record(id: &str, k: usize, status: &str, text: &str, to: &[String]) -> String {
    let text = text.trim();
    let to = if to.is_empty() { String::new() } else { format!("; to {}", to.join(", ")) };
    let head = format!("[{}] (kickoff {}{})", id, k, to);
    match (status, text.is_empty()) {
        ("done", false) => format!("{} {}", head, text),
        ("done", true) => format!("{} ended without an end message", head),
        (s, true) => format!("{} {}, no end message", head, s),
        (s, false) => format!("{} {}; its last text: {}", head, s, text),
    }
}

/// Log the kickoff, prime, start the process, and hand it to a thread of its own. Called by
/// the starter (turn.rs), one at a time, with the view as it stands now.
pub fn launch(e: &Arc<Engine>, job: Job, view: String, msg: Option<usize>, epoch: u64, mut stats: Value) {
    e.instances.fetch_add(1, SeqCst);
    let k = e.log_at("start", &kickoff(&job, msg), &super::store::now_iso()).unwrap_or(0);
    let url = e.mcp_url.get().cloned().unwrap_or_default();
    let args = turn::shared_args(e, &job.model, &job.effort, &url);
    if let Some((r, w)) = turn::prime(e, &args, &view, &job.id) { stats["prime_read"] = json!(r); stats["prime_write"] = json!(w); }
    let mut env = turn::env(e);
    env.push(("FACET_INSTANCE", job.id.clone()));
    let envr: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let head = prompts::start(&job.id, k, job.from.as_deref(), &job.text);
    let started = Proc::spawn_detached(&args, &envr, &e.conf.cwd)
        .and_then(|mut p| p.send(turn::content(&view, &head)).map(|_| p));
    stats["instance"] = json!(job.id); stats["kickoff"] = json!(k); stats["model"] = json!(job.model);
    super::events::log(&e.dir, "start", stats);
    let e2 = e.clone();
    match started {
        Ok(p) => { std::thread::spawn(move || run(e2, p, job, k, msg.is_some(), epoch)); }
        Err(x) => finish(&e2, &job, k, msg.is_some(), "failed to start", &x.to_string(), false, None, json!({})),
    }
}

/// Owned by a thread of the engine: runs for as long as the process does.
fn run(e: Arc<Engine>, mut p: Proc, job: Job, k: usize, owed: bool, epoch: u64) {
    let at = Instant::now();
    let logf = dir(&e).join(format!("{}.jsonl", job.id));
    let mut meter = Meter::default();
    let (mut reqs, mut eq, mut tool_uses) = (0usize, 0.0f64, 0u64);
    let mut cc = String::new();
    let mut last = String::new();
    let status;
    // it said something to the user: an output tool's call that succeeded
    let mut spoke = false;
    let mut speaking: Vec<String> = Vec::new();
    let mut quiet = Instant::now();
    // where its final message goes: the input of its `route` call, the last one counting
    let mut ending: Option<Value> = None;
    loop {
        if e.turn.lock().unwrap().epoch != epoch { status = "cancelled"; p.kill(); break }
        let ev = match p.next(Duration::from_millis(500)) {
            Ok(ev) => ev,
            Err(RecvTimeoutError::Timeout) => {
                if quiet.elapsed() > Duration::from_secs(900) { status = "timed out"; p.kill(); break }
                continue
            }
            Err(RecvTimeoutError::Disconnected) => { status = "failed"; break }
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
                break;
            }
            _ => {}
        }
    }
    if status != "cancelled" && status != "timed out" { p.finish(); }
    if let Some(r) = meter.take() { e.spend("instance", &r); }
    let trace = json!({"reqs": reqs, "eq": eq.round(), "tools": tool_uses, "ms": at.elapsed().as_millis()});
    finish(&e, &job, k, owed, status, &turn::cap(last.trim()), spoke, ending, trace);
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
/// message to the chat, a card's message to that card, a handed-on task nowhere.
#[allow(clippy::too_many_arguments)]
fn finish(e: &Arc<Engine>, job: &Job, k: usize, owed: bool, status: &str, last: &str, spoke: bool, ending: Option<Value>, mut trace: Value) {
    let served_chat = owed && crate::log::wants_chat(&job.text);
    let origin_card: Vec<String> = if owed { crate::cards::from_card(&job.text).map(|c| vec![c.to_string()]).unwrap_or_default() } else { vec![] };
    let text = last.to_string();
    let (chat, cards) = match &ending {
        Some(v) => (
            v["chat"].as_bool().unwrap_or(served_chat),
            v["cards"].as_array().map(|a| a.iter().filter_map(|c| c.as_str().map(|s| s.trim().trim_start_matches('#').to_string())).filter(|c| !c.is_empty()).collect()).unwrap_or(origin_card),
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
    let record = end_record(&job.id, k, status, &text, &to);
    e.log("work", &record);
    if let Some(w) = &job.wait { let _ = w.send(record.clone()); }
    trace["instance"] = json!(job.id); trace["kickoff"] = json!(k); trace["status"] = json!(status);
    trace["model"] = json!(job.model); trace["spoke"] = json!(spoke); trace["net"] = json!(net); trace["ended"] = json!(ending.is_some()); trace["to"] = json!(to); trace["record_bytes"] = json!(record.len());
    super::events::log(&e.dir, "instance", trace);
    e.instances.fetch_sub(1, SeqCst);
    let line = usage::limits_line(&e.limits());
    e.notice(&format!("[{}] {}{}", job.id, status, if line.is_empty() { String::new() } else { format!(" · {}", line) }));
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
        return format!("No instance {}: zoom takes an instance's name, as its kickoff starts \"[Name]\".", name);
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
            _ => {}
        }
    }
    if out.is_empty() { format!("Instance {} has no steps on file yet.", name) } else { out.join("\n\n") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kickoff_and_end_records_point_at_each_other() {
        let j = Job { id: "inst_1".into(), model: "opus".into(), effort: "high".into(), text: "hi".into(), from: None, desc: String::new(), wait: None };
        assert_eq!(kickoff(&j, Some(41)), "[inst_1] kickoff (opus, effort high): answers message 41");
        let t = Job { from: Some("inst_0".into()), desc: "check".into(), model: "sonnet".into(), text: "do X".into(), ..j };
        assert_eq!(kickoff(&t, None), "[inst_1] kickoff (sonnet, effort high): task from [inst_0] (check):\n\ndo X");
        assert_eq!(end_record("inst_1", 42, "done", " learned Y ", &[]), "[inst_1] (kickoff 42) learned Y");
        assert_eq!(end_record("inst_1", 42, "cancelled", "", &[]), "[inst_1] (kickoff 42) cancelled, no end message");
        let to = ["chat".to_string(), "card #c1".to_string()];
        let r = end_record("inst_1", 42, "done", "Done.", &to);
        assert_eq!(r, "[inst_1] (kickoff 42; to chat, card #c1) Done.");
        assert_eq!(crate::log::end_body(&r), Some("Done."));
        assert!(crate::log::is_report(&end_record("inst_1", 42, "done", "x", &[])));
        assert!(crate::log::is_report(&kickoff(&t, None)));
    }

    /// The architecture end to end, with a stand-in model: each message starts an instance;
    /// kickoff and end record land in the log at their own moments, the end record pointing
    /// at its kickoff; an instance's view is the log as of its start (a running instance's
    /// kickoff, not its end record; an ended one's end record); none of an instance's own
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
        // must stay out of the log); the instance whose kickoff is message 1 takes a while; its
        // last text is its end record
        let fake = d.join("fake.sh");
        std::fs::write(&fake, format!("#!/bin/sh\n\
            [ \"$DISABLE_PROMPT_CACHING\" = 1 ] && {{ echo '{{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"result\":\"no priming here\"}}'; exit 0; }}\n\
            IFS= read -r line\n\
            printf '%s\\n' \"$line\" > '{}'/\"$FACET_INSTANCE\"\n\
            echo '{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Bash\",\"input\":{{\"command\":\"STEP-ONLY\"}}}}]}}}}'\n\
            case \"$line\" in *'your kickoff is message 1.'*) sleep 1.5;; esac\n\
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
        until("first kickoff", &|| msgs(&e).iter().any(|m| m.0 == "start"));
        turn::input(&e, card("b"));
        until("both ended", &|| msgs(&e).iter().filter(|m| m.0 == "work").count() == 2 && idle(&e));
        let m = msgs(&e);
        let kinds: Vec<&str> = m.iter().map(|x| x.0.as_str()).collect();
        // chronological: the fast second instance ends before the slow first one
        assert_eq!(kinds, ["user", "start", "user", "start", "work", "work"], "{:?}", m);
        let name = |m: &[(String, String)], k: usize| m[k].1[1..m[k].1.find(']').unwrap()].to_string();
        let (a, b) = (name(&m, 1), name(&m, 3));
        assert_eq!(m[1].1, format!("[{}] kickoff ({}, effort {}): answers message 0", a, e.model(), e.conf.effort));
        assert_eq!(m[4].1, format!("[{}] (kickoff 3) LEARNED: something", b));
        assert_eq!(m[5].1, format!("[{}] (kickoff 1) LEARNED: something", a));
        // no step of either reached the log
        assert!(!m.iter().any(|x| x.1.contains("STEP-ONLY")));
        // the steps are kept beside it, for zoom("Name")
        assert!(transcript(&e, &a).contains("STEP-ONLY"), "{}", transcript(&e, &a));

        // what each was sent: its view, then its own block naming it and its kickoff
        let sent = |n: &str| std::fs::read_to_string(ins.join(n)).unwrap();
        let (sa, sb) = (sent(&a), sent(&b));
        assert!(sa.contains(&format!("You are instance [{}]; your kickoff is message 1.", a)) && sa.contains("aSLOW"));
        // the second saw the first's kickoff, and not its end record, which came later
        assert!(sb.contains(&format!("[{}] kickoff", a)), "{}", sb);
        assert!(!sb.contains("LEARNED"), "{}", sb);
        assert!(sb.contains(&format!("You are instance [{}]; your kickoff is message 3.", b)));

        // a third, started after both ended, sees both end records; it serves a chat message
        // and says nothing to the user, so its last text is posted to the chat: one message,
        // logged once, as its end record
        turn::input(&e, "plain chat message".into());
        until("third ended", &|| msgs(&e).iter().filter(|m| m.0 == "work").count() == 3 && idle(&e));
        let m = msgs(&e);
        let c = name(&m, 7);
        let sc = sent(&c);
        assert!(sc.contains(&format!("[{}] (kickoff 1) LEARNED", a)) && sc.contains(&format!("[{}] (kickoff 3) LEARNED", b)), "{}", sc);
        let kinds: Vec<&str> = m[6..].iter().map(|x| x.0.as_str()).collect();
        assert_eq!(kinds, ["user", "start", "work"], "{:?}", m);
        assert_eq!(m[8].1, format!("[{}] (kickoff 7; to chat) LEARNED: something", c));
        assert_eq!(crate::log::end_body(&m[8].1), Some("LEARNED: something"));

        // a handed-on task: an instance like any other, on agent_model; --wait gets its record
        let (tx, rx) = std::sync::mpsc::channel();
        let id = turn::spawn(&e, "", "", "look", "find the thing", &c, Some(tx)).unwrap();
        let rec = rx.recv_timeout(Duration::from_secs(20)).unwrap();
        assert_eq!(rec, format!("[{}] (kickoff 9) LEARNED: something", id));
        let m = msgs(&e);
        assert_eq!(m[10].0, "work");
        assert_eq!(m[9].1, format!("[{}] kickoff ({}, effort {}): task from [{}] (look):\n\nfind the thing", id, e.conf.agent_model, e.conf.effort, c));
        assert!(sent(&id).contains(&format!("a task handed to you by [{}]", c)));
        until("idle", &|| idle(&e));

        // one that routes its final message away from the chat: logged once, unchanged, posted
        // nowhere
        turn::input(&e, "ENDME please".into());
        until("fourth ended", &|| msgs(&e).iter().filter(|m| m.0 == "work").count() == 5 && idle(&e));
        let m = msgs(&e);
        let f = name(&m, 12);
        assert_eq!(m[13..].iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["work"], "{:?}", m);
        assert_eq!(m[13].1, format!("[{}] (kickoff 12) LEARNED: something", f));

        let _ = std::fs::remove_dir_all(state_dir(&d));
        let _ = std::fs::remove_dir_all(&d);
    }
}
