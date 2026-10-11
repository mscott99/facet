// One `claude -p` process, stream-json in and out. Every call this program makes to a model
// goes through here, so this is also where usage is recorded: one line per API request.
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct Proc {
    child: Child,
    stdin: Option<ChildStdin>,
    pub rx: Receiver<Value>,
    pub stderr: Arc<Mutex<String>>,
}

/// The flags every call shares: no session file, no settings, no CLAUDE.md, no MCP but ours.
/// Everything that reaches the request must be the same from call to call, or the cache dies.
pub fn base_args(model: &str, effort: &str, system_file: &str, tools: &str) -> Vec<String> {
    let mut a: Vec<String> = ["-p", "--model", model, "--effort", effort,
        "--system-prompt-file", system_file, "--tools", tools,
        "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
        "--include-partial-messages", "--no-session-persistence",
        "--setting-sources", "", "--strict-mcp-config"]
        .iter().map(|s| s.to_string()).collect();
    if effort.is_empty() { a.drain(3..5); }
    a
}

pub fn bin() -> String {
    std::env::var("FACET_CLAUDE").ok().filter(|s| !s.is_empty())
        .unwrap_or_else(|| crate::cfg::Cfg::load().str("chat.claude", "claude"))
}

impl Proc {
    pub fn spawn(args: &[String], env: &[(&str, &str)], cwd: &std::path::Path) -> std::io::Result<Proc> {
        Self::start(Self::command(args, env, cwd))
    }

    /// Like `spawn`, but in its own process group (a `setsid`-equivalent): it does not share
    /// the master's call's fate. Used for a subagent spawned to outlive the turn that asked
    /// for it (§9, detached) — the engine process that calls this already outlives any one
    /// turn, but a process group of its own keeps a signal meant for a terminal or a job's
    /// whole group from reaching it too.
    pub fn spawn_detached(args: &[String], env: &[(&str, &str)], cwd: &std::path::Path) -> std::io::Result<Proc> {
        use std::os::unix::process::CommandExt;
        let mut cmd = Self::command(args, env, cwd);
        cmd.process_group(0);
        Self::start(cmd)
    }

    /// The flags and environment every call shares, built but not yet spawned.
    fn command(args: &[String], env: &[(&str, &str)], cwd: &std::path::Path) -> Command {
        let mut cmd = Command::new(bin());
        cmd.args(args).current_dir(cwd)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        // A clean environment: the request must depend on nothing but these arguments. An
        // inherited CLAUDE_CODE_* variable can add per-process text in front of the view (a
        // parent Claude Code session adds a "scratchpad" line with a random session id, which
        // made every process a cache miss) or switch billing (ANTHROPIC_API_KEY).
        cmd.env_clear();
        for k in ["HOME", "USER", "LOGNAME", "PATH", "SHELL", "LANG", "LC_ALL", "LC_CTYPE", "TMPDIR", "ANTHROPIC_BASE_URL", "FAKE_LOG"] {
            if let Ok(v) = std::env::var(k) { cmd.env(k, v); }
        }
        // Nothing from the user's Claude Code setup may reach the request: no CLAUDE.md, no
        // auto-memory, no git status (each would put volatile text before the view). And no
        // "nonessential traffic": among it, a session-title request that sends the whole first
        // user message (the ~64k-token view) to Haiku, uncached, for every process (measured).
        for k in ["CLAUDE_CODE_DISABLE_CLAUDE_MDS", "CLAUDE_CODE_DISABLE_AUTO_MEMORY", "CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS",
                  "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"] {
            cmd.env(k, "1");
        }
        for (k, v) in env { cmd.env(k, v); }
        cmd
    }

    fn start(mut cmd: Command) -> std::io::Result<Proc> {
        let mut child = cmd.spawn()?;
        live().lock().unwrap().insert(child.id());
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(v) = serde_json::from_str::<Value>(&line) { if tx.send(v).is_err() { break } }
            }
        });
        let err = Arc::new(Mutex::new(String::new()));
        let e2 = err.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut e = e2.lock().unwrap();
                if e.len() < 20_000 { e.push_str(&line); e.push('\n'); }
            }
        });
        Ok(Proc { stdin: child.stdin.take(), child, rx, stderr: err })
    }

    /// One user message, as content blocks.
    pub fn send(&mut self, content: Value) -> std::io::Result<()> {
        let msg = json!({"type": "user", "message": {"role": "user", "content": content}});
        let w = self.stdin.as_mut().ok_or_else(|| std::io::Error::other("stdin closed"))?;
        w.write_all(format!("{}\n", msg).as_bytes())?;
        w.flush()
    }
    pub fn send_text(&mut self, text: &str) -> std::io::Result<()> {
        self.send(json!([{"type": "text", "text": text}]))
    }
    pub fn take_stdin(&mut self) -> Option<ChildStdin> { self.stdin.take() }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        live().lock().unwrap().remove(&self.child.id());
    }
    /// After stdin is closed the process exits by itself; give it a moment, then make sure.
    pub fn finish(&mut self) {
        self.stdin = None;
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(5) {
            if let Ok(Some(_)) = self.child.try_wait() { live().lock().unwrap().remove(&self.child.id()); return }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.kill();
    }

    /// Next event, or None on timeout / end of stream.
    pub fn next(&self, timeout: Duration) -> Result<Value, RecvTimeoutError> { self.rx.recv_timeout(timeout) }
}

impl Drop for Proc {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() { let _ = self.child.kill(); let _ = self.child.wait(); }
        live().lock().unwrap().remove(&self.child.id());
    }
}

// ---- reaping ------------------------------------------------------------------------------
//
// Every `Proc` waits for its child on every path (kill, finish, drop). What escaped that was
// the restart: `restart_if_idle` re-execs the engine in place, and a child still running then
// (a compaction past the 30 s grace, a priming call) stays this pid's child while the new image
// holds no handle to it. Its pipes close with the exec, it exits, and nothing ever waits for it:
// a `[claude] <defunct>` per restart (47 were found). So: before the exec, the children still
// registered are ended and waited (`end_all`); after it, the children found at startup are
// waited by a backstop thread (`reap_inherited`). No blanket `waitpid(-1)`: it would steal the
// exit status of a `Command::output()` running at the same moment.

unsafe extern "C" {
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
}
const WNOHANG: i32 = 1; // Linux and macOS alike
const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;

/// The pids of the children some `Proc` still owns.
fn live() -> &'static Mutex<HashSet<u32>> {
    static L: std::sync::OnceLock<Mutex<HashSet<u32>>> = std::sync::OnceLock::new();
    L.get_or_init(|| Mutex::new(HashSet::new()))
}

/// True once `pid` is gone (reaped now, or not our child any more).
fn reap(pid: u32) -> bool {
    let mut st = 0i32;
    let r = unsafe { waitpid(pid as i32, &mut st, WNOHANG) };
    r != 0
}

/// Before an exec: end every child a `Proc` owns and wait for it (up to 3 s, then SIGKILL).
pub fn end_all() {
    let pids: Vec<u32> = live().lock().unwrap().drain().collect();
    for &p in &pids { unsafe { kill(p as i32, SIGTERM); } }
    let t = Instant::now();
    let mut left = pids;
    while !left.is_empty() && t.elapsed() < Duration::from_secs(3) {
        left.retain(|&p| !reap(p));
        std::thread::sleep(Duration::from_millis(50));
    }
    for &p in &left { unsafe { kill(p as i32, SIGKILL); } let mut st = 0; unsafe { waitpid(p as i32, &mut st, 0); } }
}

/// The children this process has right now (`pgrep -P`, on Linux and macOS alike).
pub fn children() -> Vec<u32> {
    let me = std::process::id().to_string();
    std::process::Command::new("pgrep").args(["-P", &me]).output().ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().filter_map(|x| x.parse().ok()).collect())
        .unwrap_or_default()
}

/// At startup, before any call: the children found are an earlier image's (an exec keeps the
/// pid and its children). Wait for each, in the background, as it exits. Returns how many.
pub fn reap_inherited() -> usize {
    let mut pids = children();
    let n = pids.len();
    if n == 0 { return 0 }
    std::thread::spawn(move || {
        while !pids.is_empty() {
            pids.retain(|&p| !reap(p));
            std::thread::sleep(Duration::from_secs(10));
        }
    });
    n
}

/// Usage of one API request, put together from its stream events: `message_start` carries
/// the input side (with the cache split), `message_delta` the final output count.
#[derive(Default, Clone, Debug)]
pub struct Req { pub model: String, pub usage: Value }

/// Feed every event here; it returns a finished request when one completes.
#[derive(Default)]
pub struct Meter { cur: Option<Req> }

pub enum Tick { Started(Req), Done(Req) }

impl Meter {
    pub fn feed(&mut self, ev: &Value) -> Option<Tick> {
        if ev["type"] != "stream_event" { return None }
        let e = &ev["event"];
        match e["type"].as_str()? {
            "message_start" => {
                let m = &e["message"];
                let r = Req { model: m["model"].as_str().unwrap_or("").into(), usage: m["usage"].clone() };
                self.cur = Some(r.clone());
                Some(Tick::Started(r))
            }
            "message_delta" => {
                let mut r = self.cur.take()?;
                if let Some(o) = e["usage"]["output_tokens"].as_u64() { r.usage["output_tokens"] = o.into(); }
                Some(Tick::Done(r))
            }
            _ => None,
        }
    }
    /// A request cut off after message_start (a killed priming call): its input was spent.
    pub fn take(&mut self) -> Option<Req> { self.cur.take() }
}

/// The `result` event, simplified.
pub struct Outcome { pub text: String, pub error: bool, pub subtype: String }

pub fn outcome(ev: &Value) -> Option<Outcome> {
    if ev["type"] != "result" { return None }
    let error = ev["is_error"].as_bool().unwrap_or(false) || ev["subtype"].as_str().is_some_and(|s| s != "success");
    let text = ev["result"].as_str().map(String::from)
        .or_else(|| ev["errors"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join("; ")))
        .unwrap_or_default();
    Some(Outcome { text, error, subtype: ev["subtype"].as_str().unwrap_or("").into() })
}

/// Errors that mean "stop calling for a while": the subscription's usage limit, rate limits,
/// overload. Retrying these every 10 s only fills the log (the previous engine made 5,077
/// such calls in one afternoon); the compactor parks until `pause` says.
pub fn limit_hit(text: &str) -> bool {
    let t = text.to_lowercase();
    // Claude Code says "You've hit your session limit · resets 7am (UTC)" (also "weekly
    // limit"): "hit your limit" alone missed it, so such a failure was taken for an ordinary one
    ["usage limit", "rate limit", "rate_limit", "hit your limit", "session limit", "weekly limit", "limit reached",
     "overloaded", "429", "529"]
        .iter().any(|k| t.contains(k))
}
