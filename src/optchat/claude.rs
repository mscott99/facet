// One `claude -p` process, stream-json in and out. Every call this program makes to a model
// goes through here, so this is also where usage is recorded: one line per API request.
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
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
        let mut child = cmd.spawn()?;
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
    }
    /// After stdin is closed the process exits by itself; give it a moment, then make sure.
    pub fn finish(&mut self) {
        self.stdin = None;
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(5) {
            if let Ok(Some(_)) = self.child.try_wait() { return }
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
    }
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
    ["usage limit", "rate limit", "rate_limit", "hit your limit", "limit reached", "overloaded", "429", "529"]
        .iter().any(|k| t.contains(k))
}
