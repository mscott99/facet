// §9: a subagent, the one way one is sent out (README §19): its own `claude -p`, as a child of
// the engine — a process that already outlives any one turn — in its own process group
// (`Proc::spawn_detached`), so nothing a turn does can reach it. Not a Claude Code `Task` call,
// which lives and dies with the master's process and whose request has its own system prompt
// and tools. This one's request is the master's [tools][system][view] byte for byte, so it
// reads the cache the master's turn wrote; its instructions (prompts::AGENT) and its task
// follow the view in the same message.
//
// `spawn` returns an id at once; a thread the engine owns (not the turn) reads the child's
// stream-json to a file under `<state dir>/agents/<id>.jsonl`, meters its requests the same way
// a turn meters its own (`claude::Meter`), and once it ends, logs one `agent` event and
// delivers its report as a message starting "[id] ", for a turn of its own (`turn::input`,
// `later = true`) — or, under `facet spawn --wait`, hands it to the waiting caller and logs it
// as `work`, starting no turn.
use super::claude::{self, Meter, Proc, Tick};
use super::engine::{state_dir, Engine};
use super::{prompts, turn, usage};
use serde_json::{json, Value};
use std::io::Write;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Where a detached agent's captured stream and system-prompt file live: never inside the
/// chat directory (it is not memory — the one thing that survives is its report, once logged).
pub fn dir(e: &Engine) -> std::path::PathBuf {
    let d = state_dir(&e.dir).join("agents");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn new_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    format!("spawn_{:x}", n & 0xffff_ffff_ffff)
}

/// Where a waiting `facet spawn --wait` gets the report: the report, and a way to say whether
/// it reached the caller (false: the caller is gone, so it is delivered as a turn of its own).
pub type Waiter = std::sync::mpsc::Sender<(String, std::sync::mpsc::Sender<bool>)>;

/// Launch a detached subagent. Its request shares the master's cached prefix (README §19):
/// the master's system prompt, built-in tools, MCP tool list, model and effort, and the view
/// the live call was sent (or, between turns, the view as it stands), in the same blocks; the
/// subagent's instructions and its task follow the view in the same first message. `model` or
/// `effort` given override the defaults (model: `agent_model`, Sonnet; effort: the master's) (the cache is per model,
/// and a different effort changes the thinking settings). `kind` "explore" is told to change
/// nothing; the tools are the same either way, since a different tool list is a different
/// prefix. Returns its id at once; with a `wait`er, the report goes there too.
pub fn spawn(e: &Arc<Engine>, model: &str, effort: &str, kind: &str, desc: &str, task: &str, wait: Option<Waiter>) -> Result<String, String> {
    let task = task.trim();
    if task.is_empty() { return Err("empty task".into()) }
    let model = if model.trim().is_empty() { e.conf.agent_model.clone() } else { model.trim().to_string() };
    let effort = if effort.trim().is_empty() { e.conf.effort.clone() } else { effort.trim().to_string() };
    let kind = if kind.trim().is_empty() { "general-purpose".to_string() } else { kind.trim().to_string() };
    let id = new_id();

    let view = view_now(e);
    // the master's own MCP tool list, served on a path that refuses the writing tools
    let url = super::mcp::agent_url(&e.mcp_url.get().cloned().unwrap_or_default());
    let args = turn::shared_args(e, &model, &effort, &url);
    let mut env = turn::env(e);
    env.push(("FACET_SPAWN", "1".into()));
    let envr: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let mut p = Proc::spawn_detached(&args, &envr, &e.conf.cwd).map_err(|x| x.to_string())?;
    if let Err(x) = p.send(turn::content(&view, &prompts::agent_task(&e.conf.name, &kind, task))) { return Err(x.to_string()) }

    e.spawns.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    // the task verbatim, as the log's only record of it (the master may have passed it by file);
    // a plain log line, which starts no turn
    e.log("tool", &format!("spawn {} ({}, {}, {}): {}", id, model, kind, desc.trim(), task));
    let rec = Rec { id: id.clone(), kind, desc: desc.trim().to_string(), task: task.to_string() };
    let e2 = e.clone();
    std::thread::spawn(move || run(e2, p, rec, wait));
    Ok(id)
}

/// The view a spawned agent is sent: the live call's, byte for byte, while a turn is calling
/// (that is the prefix in the cache); between turns, the view as it stands, rendered as a
/// turn renders it.
pub fn view_now(e: &Engine) -> String {
    if let Some(v) = e.turn.lock().unwrap().view.clone() { return v }
    let m = e.mem.lock().unwrap();
    m.view.render(&m.store)
}

struct Rec { id: String, kind: String, desc: String, task: String }

/// Owned by a thread of the engine, not of any turn: runs for as long as the child does, which
/// may be well after the turn that called `spawn` has ended, or across several turns since.
fn run(e: Arc<Engine>, mut p: Proc, rec: Rec, wait: Option<Waiter>) {
    let at = Instant::now();
    let logf = dir(&e).join(format!("{}.jsonl", rec.id));
    let mut meter = Meter::default();
    let (mut reqs, mut eq, mut tokens, mut tool_uses) = (0usize, 0.0f64, 0u64, 0u64);
    let mut report = String::new();
    let mut status = "unfinished".to_string();
    loop {
        let ev = match p.next(Duration::from_secs(900)) {
            Ok(ev) => ev,
            Err(RecvTimeoutError::Timeout) => { status = "timed out".into(); p.kill(); break }
            Err(RecvTimeoutError::Disconnected) => { status = "failed".into(); break }
        };
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&logf) {
            let _ = writeln!(f, "{}", ev);
        }
        if let Some(Tick::Done(r)) = meter.feed(&ev) {
            reqs += 1;
            eq += usage::eq(&r.usage);
            tokens += r.usage["input_tokens"].as_u64().unwrap_or(0) + r.usage["output_tokens"].as_u64().unwrap_or(0)
                + r.usage["cache_read_input_tokens"].as_u64().unwrap_or(0) + r.usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            super::events::req(&e.dir, "agent", &r, "", 0, json!({"tool_use_id": rec.id, "agent_kind": rec.kind, "step": reqs}));
            e.spend("agent", &r);
        }
        match ev["type"].as_str().unwrap_or("") {
            "assistant" => {
                for b in ev["message"]["content"].as_array().cloned().unwrap_or_default() {
                    match b["type"].as_str().unwrap_or("") {
                        "text" => { let s = b["text"].as_str().unwrap_or(""); if !s.trim().is_empty() { report = s.to_string(); } }
                        "tool_use" => tool_uses += 1,
                        _ => {}
                    }
                }
            }
            "result" => {
                let ok = claude::outcome(&ev).is_some_and(|o| !o.error);
                if ok && claude::outcome(&ev).is_some_and(|o| { let t = o.text.trim_start().to_lowercase(); !t.starts_with("you've hit your") && !t.starts_with("you have hit your") }) { e.proof_of_life(); }
                status = if ok { "done".into() } else { "failed".into() };
                if let Some(o) = claude::outcome(&ev) { if !o.text.trim().is_empty() { report = o.text; } }
                break;
            }
            _ => {}
        }
    }
    if !p.stderr.lock().unwrap().trim().is_empty() && status == "unfinished" { status = "failed".into(); }
    p.finish();
    let report = turn::cap(report.trim());
    super::events::log(&e.dir, "agent", json!({
        "tool_use_id": rec.id, "task": rec.id, "agent_kind": rec.kind, "description": rec.desc,
        "status": status, "ask_bytes": rec.task.len(), "report_bytes": report.len(),
        "reqs": reqs, "eq": eq.round(), "tokens": tokens, "tools": tool_uses,
        "task_ms": at.elapsed().as_millis(), "ms": at.elapsed().as_millis(), "detached": true,
    }));
    let said = if report.is_empty() { format!("(no report; the detached agent ended {})", status) } else { report };
    let text = format!("[{}] {}", rec.id, said);
    // `--wait`: the caller prints it, so it is logged as work and starts no turn; if the caller
    // has gone (its shell timed out, say), it arrives as a turn of its own, as without `--wait`
    let (ack, got) = std::sync::mpsc::channel();
    let taken = wait.is_some_and(|w| w.send((text.clone(), ack)).is_ok()) && got.recv().unwrap_or(false);
    if taken { e.log("work", &text) } else { turn::input(&e, text, true) }
    // after the report is queued (it starts a turn, whose end then fires a queued restart)
    e.spawns.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    super::engine::restart_if_idle(&e);
}

/// A name as a file name: letters, digits, `_` and `-` only, so no name can reach outside
/// the agents directory.
pub fn safe(name: &str) -> String {
    name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect()
}

/// zoom("Name") (§6 of the gist): an agent's whole run, from its own log: a `facet spawn`
/// agent's (spawn_...) or a Task subagent's (Claude Code's agentId, the name its `work`
/// message starts with). Its text, its tool calls and their results (each clipped as the log
/// clips tool output).
pub fn transcript(e: &Engine, name: &str) -> String {
    let name = name.trim().trim_start_matches('[').trim_end_matches(']');
    let f = safe(name);
    if f.is_empty() || f != name {
        return format!("No agent {}: zoom takes an agent's name, as its report starts \"[Name]\".", name);
    }
    let Ok(body) = std::fs::read_to_string(dir(e).join(format!("{}.jsonl", f))) else {
        return format!("No agent {}.", name);
    };
    let mut out = Vec::new();
    for line in body.lines() {
        let Ok(ev) = serde_json::from_str::<Value>(line) else { continue };
        let blocks = ev["message"]["content"].as_array().cloned().unwrap_or_default();
        match ev["type"].as_str().unwrap_or("") {
            "assistant" => for b in &blocks {
                match b["type"].as_str().unwrap_or("") {
                    "text" => { let t = b["text"].as_str().unwrap_or("").trim(); if !t.is_empty() { out.push(format!("agent: {}", t)); } }
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
                    out.push(format!("echo: {}", turn::cap(&t)));
                }
            },
            _ => {}
        }
    }
    if out.is_empty() { format!("Agent {} has no steps on file yet.", name) } else { out.join("\n\n") }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A spawned agent's request is the master's up to the end of the view (README §19): the
    /// same arguments but for the MCP path, the same view, cut into the same blocks.
    #[test]
    fn a_spawn_shares_the_master_prefix() {
        let d = std::env::temp_dir().join(format!("facet-spawn-prefix-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let mut s = crate::optchat::store::Store::open(&d);
        for k in 0..40 { s.log(if k % 2 == 0 { "user" } else { "talk" }, &format!("message {} {}", k, "x".repeat(k * 37 % 300))).unwrap(); }
        for k in 0..40 { let t = s.msgs[k].text.clone(); s.put(0, k, &format!("user: {}", t)).unwrap(); }
        drop(s);
        let url = "http://127.0.0.1:9/sec/mcp";
        let e = crate::optchat::engine::bare(&d, url);

        // between turns: the view as a turn renders it
        let master = { let m = e.mem.lock().unwrap(); m.view.render(&m.store) };
        assert!(master.contains("message 39"));
        assert_eq!(view_now(&e), master);
        // during a call: the call's own view, byte for byte, even once the chat has moved on
        e.turn.lock().unwrap().view = Some(master.clone());
        e.log("user", "a message the live call has not seen");
        assert_eq!(view_now(&e), master);
        e.turn.lock().unwrap().view = None;
        assert_ne!(view_now(&e), master);

        // the same blocks for the view, the task only in the last one
        let task = prompts::agent_task("OptChat", "explore", "reply with the word ok");
        let (a, b) = (turn::content(&master, "hello"), turn::content(&master, &task));
        let (a, b) = (a.as_array().unwrap(), b.as_array().unwrap());
        assert_eq!(a.len(), b.len());
        assert!(a.len() > 2, "the view is cut into several blocks");
        assert_eq!(a[..a.len() - 1], b[..b.len() - 1]);
        let joined: String = b[..b.len() - 1].iter().map(|x| x["text"].as_str().unwrap()).collect();
        assert_eq!(joined, master);
        let last = b.last().unwrap()["text"].as_str().unwrap();
        assert!(last.starts_with("Subagent:") && last.ends_with("reply with the word ok") && last.contains("Do not edit"));

        // the same arguments (system file, tools, model, effort, flags) but the MCP path
        let agent_url = crate::optchat::mcp::agent_url(url);
        let m = turn::shared_args(&e, &e.model(), &e.conf.effort, url);
        let s = turn::shared_args(&e, &e.model(), &e.conf.effort, &agent_url);
        assert_eq!(m.len(), s.len());
        let diff: Vec<usize> = (0..m.len()).filter(|&i| m[i] != s[i]).collect();
        assert_eq!(diff.len(), 1);
        assert!(m[diff[0]].contains("/sec/mcp") && s[diff[0]].contains("/sec/agent"));
        let tools = &m[m.iter().position(|x| x == "--tools").unwrap() + 1];
        assert!(!tools.split(',').any(|t| t == "Task"));
        assert!(!m.iter().any(|x| x == "--agents"));
        let sys = &m[m.iter().position(|x| x == "--system-prompt-file").unwrap() + 1];
        assert_eq!(std::fs::read_to_string(sys).unwrap(), prompts::system(&e.conf.name));

        let _ = std::fs::remove_dir_all(state_dir(&d));
        let _ = std::fs::remove_dir_all(&d);
    }
}
