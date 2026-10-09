// §9, detached: a subagent spawned to outlive the turn that asked for it, not a Claude Code
// `Task` call inside the master's own `claude -p` process. A Task subagent lives and dies with
// that call: when the master's reply ends, Claude Code kills whatever it was still running in
// the background (measured: three backgrounded Task agents, 831k eq between them, logged
// "unfinished" with no report, the instant their parent's process exited). This spawns its own
// `claude -p`, as a child of the engine — a process that already outlives any one turn — and in
// its own process group besides (`Proc::spawn_detached`), so nothing a turn does can reach it.
//
// `spawn` returns an id at once; a thread the engine owns (not the turn) reads the child's
// stream-json to a file under `<state dir>/agents/<id>.jsonl`, meters its requests the same way
// a turn meters its own (`claude::Meter`), and once it ends, logs one `agent` event (as
// `turn::done` does for a Task subagent) and delivers its report the same way a backgrounded
// Task subagent's report is delivered: as a message starting "[id] ", for a turn of its own
// (`turn::input`, `later = true`) — so it reaches the chat whether or not a turn is running,
// and whether or not the turn that spawned it is still alive.
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

/// Launch a detached subagent. `model` empty means the configured cheap default
/// (`chat.agent_model`); `kind` "explore" gets the read-only toolset, anything else (including
/// empty, taken as "general-purpose") gets Edit/Write too. Returns its id at once: the caller
/// (the CLI, `facet spawn`) does not wait on it.
pub fn spawn(e: &Arc<Engine>, model: &str, effort: &str, kind: &str, desc: &str, task: &str) -> Result<String, String> {
    let task = task.trim();
    if task.is_empty() { return Err("empty task".into()) }
    let model = if model.trim().is_empty() { e.conf.agent_model.clone() } else { model.trim().to_string() };
    let kind = if kind.trim().is_empty() { "general-purpose".to_string() } else { kind.trim().to_string() };
    let id = new_id();

    let view = { let m = e.mem.lock().unwrap(); m.view.render(&m.store) };
    let sys = prompts::named(prompts::AGENT, &e.conf.name);
    let sd = dir(e);
    let sysf = sd.join(format!("{}.system.txt", id));
    std::fs::write(&sysf, &sys).map_err(|x| x.to_string())?;

    let read = "Bash,Read,Glob,Grep,WebFetch,WebSearch,mcp__optchat__zoom,mcp__optchat__date";
    let tools = if kind == "explore" { read.to_string() } else { format!("{},Edit,Write", read) };
    let mut args = claude::base_args(&model, effort.trim(), &sysf.to_string_lossy(), &tools);
    // the read-only path: zoom and date, never the master's send_chat / answer_card
    let url = super::mcp::agent_url(&e.mcp_url.get().cloned().unwrap_or_default());
    let mcp = json!({"mcpServers": {"optchat": {"type": "http", "url": url}}});
    args.extend(["--mcp-config".into(), mcp.to_string(), "--permission-mode".into(), e.conf.permission.clone(),
        "--add-dir".into(), "/tmp".into()]);

    let mut p = Proc::spawn_detached(&args, &[("FACET_SPAWN", "1")], &e.conf.cwd).map_err(|x| x.to_string())?;
    let content = Value::Array(vec![json!({"type": "text", "text": view}), json!({"type": "text", "text": task})]);
    if let Err(x) = p.send(content) { return Err(x.to_string()) }

    e.spawns.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    // the task verbatim, as the log's only record of it (the master may have passed it by file);
    // a plain log line, which starts no turn
    e.log("tool", &format!("spawn {} ({}, {}, {}): {}", id, model, kind, desc.trim(), task));
    let rec = Rec { id: id.clone(), kind, desc: desc.trim().to_string(), task: task.to_string() };
    let e2 = e.clone();
    std::thread::spawn(move || run(e2, p, rec));
    Ok(id)
}

struct Rec { id: String, kind: String, desc: String, task: String }

/// Owned by a thread of the engine, not of any turn: runs for as long as the child does, which
/// may be well after the turn that called `spawn` has ended, or across several turns since.
fn run(e: Arc<Engine>, mut p: Proc, rec: Rec) {
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
    turn::input(&e, format!("[{}] {}", rec.id, said), true);
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
