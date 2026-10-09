// The one process that owns the chat. It holds `<dir>/lock`, a Unix socket, for its whole
// life (§2): a second engine that can connect to it exits; a socket that refuses connections
// is stale and is taken over. The same socket is how everything else talks to the engine:
// the terminal client, `facet send`, the web and Telegram routes. One JSON object per line.
//
//   -> {"op":"send","text":"...","later":bool}  a user message (queued, or delivered mid-run;
//                                     with later, a turn of its own after the running one)
//   -> {"op":"cancel"}                stop the wait or the running call
//   -> {"op":"note","text":..,"date":..}  import a note (queued during a turn; duplicates skipped)
//   -> {"op":"card","do":"new|reply|fix|kind|close|delete|apply|list",..}  a card (cards.rs):
//                                     what the agent does on one is logged as kind `answer`;
//                                     the master's MCP card tools are the same call (`card` below)
//   -> {"op":"answer","id":..,"text":..,"apply":..}  the old name of {"op":"card","do":"reply"}
//   -> {"op":"restart","serve":bool}  queue a restart of this engine (and of serve, if asked) for
//                                     when the running turn has ended and no detached spawn is
//                                     alive; returns at once ("restart queued")
//   -> {"op":"resume"}                lift a compactor pause
//   -> {"op":"view"} / {"op":"status"} / {"op":"zoom","id":..,"n":..}
//   -> {"op":"model","name":..}       the master model for the next turns
//   -> {"op":"watch"}                 then a stream of events, one per line:
//        {"ev":"msg","i":..,"kind":..,"text":..}   a logged message
//        {"ev":"delta","text":..}                  streamed reply text (not logged as such)
//        {"ev":"thought","text":..}                model reasoning: shown, never logged (§2)
//        {"ev":"notice","text":..}                 engine status
//        {"ev":"state","busy":bool,"queued":n}
use super::claude::Req;
use super::store::Store;
use super::view::View;
use super::{compact, mcp, prompts, turn, usage, VIEW, JOBS};
use crate::cfg::{self, Cfg};
use crate::cards;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

pub struct Conf {
    pub name: String,
    pub model: String,
    pub effort: String,
    pub tools: String,
    pub permission: String,
    pub compact_model: String,
    pub compact_effort: String,
    pub agent_model: String,
    pub ttl: String,
    pub cwd: PathBuf,
    pub prime: bool,
    pub safe_mode: bool,
    /// eq per hour above which the compactor parks (0 = no limit)
    pub budget_hour: f64,
    pub jobs: usize,
    pub view: usize,
}

impl Conf {
    pub fn load(_dir: &Path) -> Conf {
        // `chat.<key>` in facet.json; FACET_CHAT_<KEY> in the environment wins (tests)
        let mut c = Cfg::load();
        for (k, v) in std::env::vars() {
            if let Some(key) = k.strip_prefix("FACET_CHAT_") {
                let val = serde_json::from_str::<Value>(&v).unwrap_or(Value::String(v.clone()));
                c.set(&format!("chat.{}", key.to_lowercase()), val);
            }
        }
        let s = |k: &str, d: &str| c.str(&format!("chat.{}", k), d);
        Conf {
            name: s("name", "OptChat"),
            model: s("model", "opus"),
            effort: s("effort", "high"),
            tools: s("tools", "Bash,Read,Edit,Write,Glob,Grep,WebFetch,WebSearch,Task"),
            permission: s("permission", "bypassPermissions"),
            compact_model: s("compact_model", "haiku"),
            compact_effort: s("compact_effort", "xhigh"),
            agent_model: s("agent_model", "sonnet"),
            ttl: s("cache_ttl", "5m"),
            cwd: cfg::tilde(&s("cwd", "~")),
            prime: c.get_bool("chat.prime", true),
            // --safe-mode would be ideal (no CLAUDE.md, skills, hooks) but it also drops --mcp-config
            // servers (measured), so zoom and date would be gone; --setting-sources "" is enough
            safe_mode: c.get_bool("chat.safe_mode", false),
            budget_hour: c.num("chat.budget_hour_eq", 0) as f64,
            jobs: c.num("chat.jobs", JOBS as i64).max(1) as usize,
            view: c.num("chat.view_bytes", VIEW as i64) as usize,
        }
    }
}

/// The memory, in one lock: the log, the tree, the view, and the compactor's bookkeeping.
pub struct Mem {
    pub store: Store,
    pub view: View,
    /// The compactions' own view (§4 of the gist): the chat's view merged further, 16-32 KB.
    pub cview: View,
    pub busy: HashSet<(usize, usize)>,
    /// failures per node since its last success
    pub failed: HashMap<(usize, usize), u32>,
    /// Nodes whose last call failed: tried again at the next message (§4 of the gist), not before.
    pub held: HashSet<(usize, usize)>,
    /// the chat directory, where `view.json` is saved
    pub dir: PathBuf,
    /// The queues of §4 ("keep the nodes that are ready to build in queues; never scan the
    /// tree for work"): per level, the nodes whose sources exist and that are not built yet.
    /// A node enters when it becomes ready (its message is logged, or its second half is
    /// built) and leaves when it is built; the pump reads only these.
    pub todo: Vec<std::collections::BTreeSet<usize>>,
    /// Nodes that have just become ready, not yet looked at: built on the spot if free (a
    /// source of at most NODE bytes is its own node, §2), else queued in `todo`.
    pub fresh: Vec<(usize, usize)>,
    /// Wall clock, not `Instant`: on macOS `Instant` stops while the machine sleeps, so a
    /// pause until the limit resets would outlast the reset by however long the lid was shut.
    pause: Option<(SystemTime, String)>,
}

impl Mem {
    pub fn new(dir: PathBuf, store: Store, view: View, cview: View) -> Mem {
        // the queues at start: one pass over what is on file (once, not per pump)
        let mut fresh = Vec::new();
        let t = store.t();
        let mut l = 0;
        while (1usize << l) <= t {
            for i in 0..(t >> l) { if !store.built(l, i) && store.ready(l, i) { fresh.push((l, i)); } }
            l += 1;
        }
        Mem { store, view, cview, busy: HashSet::new(), failed: HashMap::new(), held: HashSet::new(), dir, todo: Vec::new(), fresh, pause: None }
    }

    /// Node (l, i) is built: off its queue, and its parent is ready once the sibling is built too.
    fn on_built(&mut self, l: usize, i: usize) {
        if let Some(q) = self.todo.get_mut(l) { q.remove(&i); }
        if self.store.built(l, i ^ 1) && !self.store.built(l + 1, i / 2) { self.fresh.push((l + 1, i / 2)); }
    }

    /// Save a node a call built, and move the queues on.
    pub fn put(&mut self, l: usize, i: usize, text: &str) -> std::io::Result<()> {
        self.store.put(l, i, text)?;
        self.on_built(l, i);
        Ok(())
    }

    /// Look at the nodes that have just become ready: a free one is built now (which may make
    /// its parent ready, and free, in turn); any other goes on its level's queue. Returns
    /// whether a node was built.
    pub fn settle_fresh(&mut self) -> std::io::Result<bool> {
        let mut grew = false;
        while let Some((l, i)) = self.fresh.pop() {
            if self.store.built(l, i) || !self.store.ready(l, i) { continue }
            match self.store.free(l, i) {
                Some(text) => { self.put(l, i, &text)?; grew = true; }
                None => {
                    if self.todo.len() <= l { self.todo.resize(l + 1, Default::default()); }
                    self.todo[l].insert(i);
                }
            }
        }
        Ok(grew)
    }

    /// A new message: its line is appended to both views, then both are fitted (§3.2, §4).
    pub fn append(&mut self, i: usize, budget: usize) {
        self.view.parts.push(super::view::Part { l: 0, i });
        self.cview.parts.push(super::view::Part { l: 0, i });
        self.fresh.push((0, i));
        self.refit(budget);
        self.save();
    }

    /// The view to `view.json` (§3.2 of the gist).
    pub fn save(&self) {
        if let Err(e) = super::view::save(&self.dir, &self.view) { eprintln!("cannot save view.json: {}", e); }
    }

    /// Fit the chat's view (a batch past its budget); when it merges, the compactions' view is
    /// cut from it again, and the view is saved; else the compactions' view only runs its own
    /// batch past CVIEW.
    pub fn refit(&mut self, budget: usize) {
        if self.view.fit(&self.store, budget) {
            self.cview = super::view::compaction_view(&self.view, &self.store);
            self.save();
        } else {
            self.cview.fit(&self.store, super::CVIEW);
        }
    }
    pub fn paused(&mut self) -> Option<SystemTime> {
        if let Some((u, _)) = &self.pause { if *u <= SystemTime::now() { self.pause = None; } }
        self.pause.as_ref().map(|p| p.0)
    }
    pub fn pause(&mut self, until: SystemTime, why: &str) { self.pause = Some((until, why.into())); }
    pub fn pause_reason(&mut self) -> Option<String> { self.paused()?; self.pause.as_ref().map(|p| p.1.clone()) }
}

pub struct Engine {
    pub dir: PathBuf,
    pub conf: Conf,
    pub mem: Mutex<Mem>,
    /// signalled whenever the tree or the view changes, and on cancel
    pub changed: Condvar,
    pub gate: compact::Gate,
    pub turn: Mutex<turn::State>,
    pub compact_sys: PathBuf,
    pub mcp_url: OnceLock<String>,
    watchers: Mutex<Vec<UnixStream>>,
    spend: Mutex<VecDeque<(Instant, f64)>>,
    /// eq by kind since the last turn ended
    pub tally: Mutex<std::collections::BTreeMap<String, f64>>,
    /// the last `rate_limit_info` any call reported, and the session use at the last turn's end
    limits: Mutex<Value>,
    pub util_mark: Mutex<Option<f64>>,
    /// the master model, switchable at runtime (/model); starts as chat.model
    model: Mutex<String>,
    /// a restart queued by `facet restart`: Some(also_serve); fires when idle (see `restart_if_idle`)
    pub restart: Mutex<Option<bool>>,
    /// detached subagents alive (agent.rs): a restart waits for them, their readers are threads of this process
    pub spawns: std::sync::atomic::AtomicUsize,
}

pub fn dir() -> PathBuf {
    std::env::var("OPTCHAT_DIR").ok().filter(|s| !s.is_empty()).map(PathBuf::from)
        .unwrap_or_else(|| Cfg::load().store())
}
pub fn sock(dir: &Path) -> PathBuf { dir.join("lock") }
/// Files the engine writes that are not memory: never inside the chat directory.
pub fn state_dir(dir: &Path) -> PathBuf {
    let mut x = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&dir, &mut x);
    cfg::data_dir().join(format!("engine-{:x}", std::hash::Hasher::finish(&x)))
}

impl Engine {
    pub fn notice(&self, text: &str) {
        eprintln!("[{}] {}", chrono::Local::now().format("%H:%M:%S"), text);
        super::events::log(&self.dir, "notice", json!({"text": text}));
        self.emit(json!({"ev": "notice", "text": text}));
    }

    pub fn emit(&self, v: Value) {
        let line = format!("{}\n", v);
        let mut w = self.watchers.lock().unwrap();
        w.retain_mut(|s| s.write_all(line.as_bytes()).is_ok());
    }

    /// Append to the log, extend the view, and let the compactor at it.
    pub fn log(self: &Arc<Self>, kind: &str, text: &str) { self.log_at(kind, text, &super::store::now_iso()); }

    /// §1 of the gist: a text too long for one message is never cut, it is logged as several
    /// messages in a row, each at most CAP characters (tool output is clipped before this).
    /// Returns the first message's id (None if the write failed).
    pub fn log_at(self: &Arc<Self>, kind: &str, text: &str, date: &str) -> Option<usize> {
        let mut first = None;
        for part in split(text, super::CAP) { let i = self.log_one(kind, part, date); if first.is_none() { first = i; } }
        first
    }

    /// A message with images (a tool result that returned some): its text is logged as any
    /// other, and its images are kept beside the log, `chat/images/<id>.json`, so that
    /// zoom(id, 1) gives the message "whole, with its images" (§3 of the gist's prompt).
    pub fn log_images(self: &Arc<Self>, kind: &str, text: &str, images: &[Value]) {
        let Some(i) = self.log_at(kind, text, &super::store::now_iso()) else { return };
        if images.is_empty() { return }
        if let Err(x) = super::store::put_images(&self.dir, i, images) { self.notice(&format!("cannot keep the images of message {}: {}", i, x)); }
    }

    fn log_one(self: &Arc<Self>, kind: &str, text: &str, date: &str) -> Option<usize> {
        let r = {
            let mut m = self.mem.lock().unwrap();
            let mm = &mut *m;
            let r = mm.store.log_at(kind, text, date);
            if let Ok(i) = r { mm.append(i, self.conf.view); }
            // §4 of the gist: a failed call is tried again at the next message
            mm.held.clear();
            self.changed.notify_all();
            r
        };
        let id = match r {
            Ok(i) => { self.emit(json!({"ev": "msg", "i": i, "kind": kind, "text": text})); Some(i) }
            Err(e) => { self.notice(&format!("LOG WRITE FAILED ({}): {}: {}", e, kind, text)); None }
        };
        compact::pump(self);
        id
    }

    /// Record a request's cost; park the compactor if the hourly budget is spent.
    pub fn spend(&self, kind: &str, r: &Req) {
        usage::record(&self.dir, kind, r);
        let x = usage::eq(&r.usage);
        *self.tally.lock().unwrap().entry(kind.to_string()).or_default() += x;
        let mut s = self.spend.lock().unwrap();
        s.push_back((Instant::now(), x));
        while s.front().is_some_and(|f| f.0.elapsed() > Duration::from_secs(3600)) { s.pop_front(); }
        let hour: f64 = s.iter().map(|f| f.1).sum();
        drop(s);
        if self.conf.budget_hour > 0.0 && hour > self.conf.budget_hour {
            let mut m = self.mem.lock().unwrap();
            if m.paused().is_none() {
                m.pause(SystemTime::now() + Duration::from_secs(600), "hourly budget spent");
                drop(m);
                self.notice(&format!("compactor paused 10 min: {:.0} eq spent in the last hour (budget {:.0}); /resume to go on", hour, self.conf.budget_hour));
            }
        }
    }

    /// Every event of every call passes here: Claude Code reports the subscription's limits
    /// with each call. A rejection parks the compactor until the window resets.
    pub fn observe(&self, ev: &Value) {
        if ev["type"] != "rate_limit_event" { return }
        let info = ev["rate_limit_info"].clone();
        {
            let mut l = self.limits.lock().unwrap();
            if *l == info { return }
            *l = info.clone();
        }
        let _ = std::fs::write(state_dir(&self.dir).join("limits.json"), info.to_string());
        super::events::log(&self.dir, "limits", json!({"info": info}));
        self.emit(json!({"ev": "limits", "line": super::usage::limits_line(&info)}));
        if info["status"] == "rejected" {
            let secs = info["resetsAt"].as_i64().map(|t| t - chrono::Utc::now().timestamp()).unwrap_or(300).clamp(60, 6 * 3600);
            self.mem.lock().unwrap().pause(SystemTime::now() + Duration::from_secs(secs as u64), "usage limit reached");
            self.notice(&format!("usage limit reached; compactor waits: {}", super::usage::limits_line(&info)));
        }
    }
    /// The model the next turn uses.
    pub fn model(&self) -> String { self.model.lock().unwrap().clone() }

    pub fn limits(&self) -> Value { self.limits.lock().unwrap().clone() }
    pub fn session_used(&self) -> Option<f64> { self.limits.lock().unwrap()["unifiedWindows"]["five_hour"]["utilization"].as_f64() }

    pub fn hour_eq(&self) -> f64 {
        self.spend.lock().unwrap().iter().filter(|f| f.0.elapsed() <= Duration::from_secs(3600)).map(|f| f.1).sum()
    }

    pub fn status(&self) -> Value {
        let mut m = self.mem.lock().unwrap();
        let unbuilt = m.view.parts.iter().filter(|p| !m.store.built(p.l, p.i)).count();
        let t = self.turn.lock().unwrap();
        json!({
            "messages": m.store.t(), "view_parts": m.view.parts.len(), "view_bytes": m.view.size(&m.store),
            "unsummarized": unbuilt, "compacting": m.busy.len(), "failing": m.failed.len(), "held": m.held.len(),
            "compact_view_bytes": m.cview.size(&m.store),
            "paused": m.pause_reason(), "busy": t.running, "queued": t.queue.len() + t.held.len() + t.later.len(), "later": t.later.len(),
            "phase": t.phase, "hour_eq": self.hour_eq().round(),
            "model": self.model(), "compact_model": self.conf.compact_model,
            "mcp": self.mcp_url.get().is_some(),
            "limits": super::usage::limits_line(&self.limits()),
            "dir": self.dir.display().to_string(), "state_dir": state_dir(&self.dir).display().to_string(),
        })
    }
}

/// `text` in pieces of at most `max` characters, each ending at a line end where one falls in
/// its last quarter; one piece if it fits.
pub fn split(text: &str, max: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while rest.chars().count() > max {
        let at = rest.char_indices().nth(max).map(|(k, _)| k).unwrap_or(rest.len());
        let min = rest.char_indices().nth(max * 3 / 4).map(|(k, _)| k).unwrap_or(0);
        let cut = rest[..at].rfind('\n').filter(|&k| k + 1 > min).map(|k| k + 1).unwrap_or(at);
        out.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    out.push(rest);
    out
}

/// Run the engine in this process until stopped. Exits if another engine holds the chat.
pub fn serve() -> ! {
    let dir = dir();
    std::fs::create_dir_all(&dir).expect("chat dir");
    let path = sock(&dir);
    if UnixStream::connect(&path).is_ok() {
        eprintln!("another engine is running on {}", dir.display());
        std::process::exit(1);
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap_or_else(|e| { eprintln!("cannot bind {}: {}", path.display(), e); std::process::exit(1) });

    let conf = Conf::load(&dir);
    // a git repository from the start: Claude Code tells the model whether its cwd is one,
    // in a block before the view, so it must not flip after the first commit
    if !dir.join(".git").exists() {
        let _ = std::process::Command::new("git").arg("-C").arg(&dir).args(["init", "-q"]).status();
        let _ = std::fs::write(dir.join(".gitignore"), "lock\n");
    }
    let store = Store::open(&dir);
    for n in &store.notes { eprintln!("load: {}", n); }
    let t0 = Instant::now();
    // §3.2: the view is loaded from view.json, never rebuilt from the log (only folded once,
    // for a chat that has none yet)
    let (view, cview, vnote) = super::view::load(&dir, &store, conf.view);
    if let Some(n) = &vnote { eprintln!("load: {}", n); }
    eprintln!("{} messages, view of {} lines / {} bytes, compactions' view {} lines / {} bytes, in {} ms",
        store.t(), view.parts.len(), view.size(&store), cview.parts.len(), cview.size(&store), t0.elapsed().as_millis());
    // saved at once: a first start, or a file in an earlier version's format, is written as
    // the gist's list of pairs
    let _ = super::view::save(&dir, &view);
    // §10: on start, print the view
    println!("{}", view.render(&store));

    let sd = state_dir(&dir);
    std::fs::create_dir_all(&sd).expect("state dir");
    // one system prompt for turns and compactions (§5 of the gist); the compactor reads it
    // from the same file the turns do
    let compact_sys = sd.join("system.txt");
    std::fs::write(&compact_sys, prompts::system(&conf.name)).expect("system prompt");

    let e = Arc::new(Engine {
        dir: dir.clone(), conf,
        mem: Mutex::new(Mem::new(dir.clone(), store, view, cview)),
        changed: Condvar::new(), gate: Default::default(),
        turn: Mutex::new(turn::State::default()), compact_sys, mcp_url: OnceLock::new(),
        watchers: Mutex::new(Vec::new()), spend: Mutex::new(VecDeque::new()), tally: Default::default(),
        limits: Mutex::new(std::fs::read_to_string(sd.join("limits.json")).ok()
            .and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null)),
        util_mark: Mutex::new(None),
        model: Mutex::new(String::new()),
        restart: Mutex::new(None), spawns: Default::default(),
    });
    *e.model.lock().unwrap() = std::fs::read_to_string(sd.join("model")).ok().map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty()).unwrap_or_else(|| e.conf.model.clone());
    let u = e.session_used();
    *e.util_mark.lock().unwrap() = u;
    {
        let m = e.mem.lock().unwrap();
        let c = &e.conf;
        super::events::log(&dir, "engine", json!({
            "pid": std::process::id(), "exe": std::env::current_exe().ok().map(|p| p.display().to_string()),
            "dir": dir.display().to_string(), "messages": m.store.t(),
            "nodes": m.store.levels.iter().map(|l| l.iter().filter(|x| x.is_some()).count()).sum::<usize>(),
            "view_lines": m.view.parts.len(), "view_bytes": m.view.size(&m.store), "startup_ms": t0.elapsed().as_millis(),
            "load_notes": m.store.notes,
            "conf": {"name": c.name, "model": c.model, "effort": c.effort, "tools": c.tools, "compact_model": c.compact_model,
                     "compact_effort": c.compact_effort, "ttl": c.ttl, "prime": c.prime, "jobs": c.jobs, "view": c.view,
                     "budget_hour_eq": c.budget_hour, "cwd": c.cwd.display().to_string()},
        }));
    }
    super::events::system(&dir, "system", &prompts::system(&e.conf.name));
    let url = mcp::start(e.clone());
    let _ = e.mcp_url.set(url);
    compact::pump(&e);
    turn::restore(&e);

    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let e = e.clone();
        std::thread::spawn(move || client(&e, conn));
    }
    std::process::exit(0)
}

fn client(e: &Arc<Engine>, conn: UnixStream) {
    let mut w = match conn.try_clone() { Ok(w) => w, Err(_) => return };
    let reader = BufReader::new(conn);
    for line in reader.lines() {
        let Ok(line) = line else { return };
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let reply = match v["op"].as_str().unwrap_or("") {
            "send" => {
                let text = v["text"].as_str().unwrap_or("").trim().to_string();
                let later = v["later"].as_bool().unwrap_or(false);
                if text.is_empty() { json!({"ok": false, "error": "empty"}) } else { turn::input(e, text, later); json!({"ok": true}) }
            }
            "cancel" => { turn::cancel(e); json!({"ok": true}) }
            // a detached subagent (§9): its own `claude -p`, owned by this engine, not by the
            // turn that asked for it, so it outlives the turn's reply; its report arrives
            // later as a message of its own
            "spawn" => match super::agent::spawn(e, v["model"].as_str().unwrap_or(""), v["effort"].as_str().unwrap_or(""), v["kind"].as_str().unwrap_or(""),
                v["desc"].as_str().unwrap_or(""), v["task"].as_str().unwrap_or("")) {
                Ok(id) => json!({"ok": true, "id": id}),
                Err(x) => json!({"ok": false, "error": x}),
            },
            // §10 importing: a note (kind `note`) with its own date, appended between turns
            // (during a turn it is queued until the turn is done); the same text twice is added once
            "note" => turn::note(e, v["text"].as_str().unwrap_or("").to_string(),
                v["date"].as_str().map(String::from).unwrap_or_else(super::store::now_iso)),
            // a deliberate reply to a line-comment card: the id is how a card is told apart
            // from an ordinary talk reply, which is why one is never mistaken for the other
            // any more (a card's poll only ever sees what landed here, by its own id)
            // a card: opened, answered, fixed, closed by the agent (or by `facet card`); the id
            // is how an answer reaches its own card and never the chat
            "card" | "answer" => {
                let mut v = v.clone();
                if v["op"] == "answer" { v["do"] = "reply".into(); }
                match card(e, &v) {
                    Ok(mut r) => { r["ok"] = true.into(); r }
                    Err(x) => json!({"ok": false, "error": x}),
                }
            }
            "restart" => {
                *e.restart.lock().unwrap() = Some(v["serve"].as_bool().unwrap_or(false));
                e.notice("restart queued: after the running reply, once no detached agent is alive");
                // idle already (called from a shell, not from a turn): go now
                let e2 = e.clone();
                std::thread::spawn(move || { std::thread::sleep(Duration::from_millis(300)); restart_if_idle(&e2); });
                json!({"ok": true, "queued": true})
            }
            "resume" => {
                let mut m = e.mem.lock().unwrap();
                m.pause = None;
                m.failed.clear();
                m.held.clear();
                drop(m);
                e.changed.notify_all(); // a failed job waiting out the pause
                e.notice("compactor resumed");
                compact::pump(e);
                json!({"ok": true})
            }
            "view" => { let m = e.mem.lock().unwrap(); json!({"view": m.view.render(&m.store)}) }
            // zoom(id, n) as the agent sees it (§7.1), for a person
            "zoom" => {
                // a name (zoom("Name")) gives that agent's whole run
                if let Some(name) = v["id"].as_str() { json!({"text": super::agent::transcript(e, name)}) } else {
                    let m = e.mem.lock().unwrap();
                    json!({"text": super::mcp::zoom(&m.store, v["id"].as_i64().unwrap_or(-1), v["n"].as_i64().unwrap_or(1))})
                }
            }
            // the master model for the next turns; kept in the state dir across restarts (it wins
            // over chat.model). A new model starts with a cold cache once: its first turn writes
            // the view again.
            "model" => {
                let name = v["name"].as_str().unwrap_or("").trim().to_string();
                let ok = matches!(name.as_str(), "opus" | "sonnet" | "haiku" | "fable") || name.starts_with("claude-");
                if !ok { json!({"ok": false, "error": "opus, sonnet, haiku, fable, or a full model id (claude-…)"}) }
                else {
                    *e.model.lock().unwrap() = name.clone();
                    let _ = std::fs::write(state_dir(&e.dir).join("model"), &name);
                    super::events::log(&e.dir, "model", json!({"model": name}));
                    json!({"ok": true, "model": name})
                }
            }
            "status" => e.status(),
            "watch" => {
                let _ = w.set_write_timeout(Some(Duration::from_secs(2)));
                let hello = json!({"ev": "hello", "status": e.status()});
                if writeln!(w, "{}", hello).is_err() { return }
                e.watchers.lock().unwrap().push(w);
                return;
            }
            "stop" => {
                let _ = writeln!(w, "{}", json!({"ok": true}));
                turn::cancel(e);
                std::process::exit(0);
            }
            op => json!({"ok": false, "error": format!("unknown op {:?}", op)}),
        };
        if writeln!(w, "{}", reply).is_err() { return }
    }
}

/// The two venues output reaches (README, "Stream and venues"). The log is the stream, and
/// everything lands in it; a venue is where some of it is also delivered. `chat` is the
/// conversation as the user reads it, on Telegram and the web chat page alike: only what
/// lands here as kind `chat` (and the user's own messages) is shown there. The master's
/// plain text (kind `talk`) reaches the stream only.
pub fn chat(e: &Arc<Engine>, text: &str) -> Result<(), String> {
    let text = text.trim();
    if text.is_empty() { return Err("empty".into()) }
    e.log("chat", text);
    Ok(())
}

/// Anything done on a card (cards.rs `op`): the card file changes — which is what an open page
/// is waiting on — and what the agent did goes into the stream as kind `answer`, so memory has
/// it too, but never the chat venue: Telegram and the web chat page skip the kind.
pub fn card(e: &Arc<Engine>, v: &Value) -> Result<Value, String> {
    let (r, line) = cards::op(&Cfg::load(), v)?;
    if let Some(l) = line { e.log("answer", &l); }
    Ok(r)
}

/// Fire a queued restart if there is one and nothing would be lost: no turn running, no
/// detached subagent alive (each has a reader thread in this process that exec would end).
/// Called when a turn ends and when a detached agent ends. Re-execs this binary with the same
/// arguments and environment: same pid (so systemd/launchd see no exit), picks up a rebuilt
/// binary. The socket and the MCP listener are close-on-exec, so the new process finds the old
/// lock refusing connections, calls it stale, removes it and binds afresh.
pub fn restart_if_idle(e: &Arc<Engine>) {
    use std::sync::atomic::Ordering::SeqCst;
    let Some(serve) = *e.restart.lock().unwrap() else { return };
    if e.turn.lock().unwrap().running { return }
    let n = e.spawns.load(SeqCst);
    if n > 0 {
        if !e.restart.lock().unwrap().is_some() { return }
        e.notice(&format!("restart waits for {} detached agent(s)", n));
        return;
    }
    // let in-flight compaction finish rather than kill it mid-spend (at most 30 s)
    for _ in 0..60 {
        if e.mem.lock().unwrap().busy.is_empty() { break }
        std::thread::sleep(Duration::from_millis(500));
    }
    {
        // claim the restart exactly once; a turn that started meanwhile postpones it
        let mut r = e.restart.lock().unwrap();
        if r.is_none() || e.turn.lock().unwrap().running || e.spawns.load(SeqCst) > 0 { return }
        *r = None;
    }
    e.log("echo", "(engine restarting on request: re-exec of the facet binary)");
    super::events::log(&e.dir, "restart", json!({"pid": std::process::id(), "serve": serve}));
    if serve {
        let ok = if cfg!(target_os = "macos") {
            let uid = std::process::Command::new("id").arg("-u").output().ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
            std::process::Command::new("launchctl").args(["kickstart", "-k", &format!("gui/{}/com.facet", uid)]).status()
        } else {
            std::process::Command::new("systemctl").args(["--user", "restart", "facet.service"]).status()
        };
        if !ok.is_ok_and(|s| s.success()) { e.notice("serve restart failed (no service manager?); restart it by hand") }
    }
    use std::os::unix::process::CommandExt;
    let mut exe = std::env::current_exe().unwrap_or_else(|_| "facet".into());
    // a rebuilt binary replaces the file: /proc/self/exe then reads "<path> (deleted)"
    if let Some(p) = exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) { exe = p.into(); }
    let err = std::process::Command::new(&exe).args(std::env::args().skip(1)).exec();
    // exec failed: the old engine goes on; say so
    *e.restart.lock().unwrap() = None;
    e.notice(&format!("restart failed ({}): {}; engine keeps running", exe.display(), err));
}

/// One request to a running engine, one reply.
pub fn request(dir: &Path, v: Value) -> Result<Value, String> {
    let mut s = UnixStream::connect(sock(dir)).map_err(|_| format!("no engine running on {}", dir.display()))?;
    writeln!(s, "{}", v).map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).map_err(|e| e.to_string())?;
    serde_json::from_str(&line).map_err(|e| format!("bad reply: {}", e))
}

pub fn running(dir: &Path) -> bool { UnixStream::connect(sock(dir)).is_ok() }

/// Start an engine in the background (detached from the terminal) and wait for its socket.
pub fn ensure(dir: &Path) -> Result<(), String> {
    if running(dir) { return Ok(()) }
    use std::os::unix::process::CommandExt;
    let sd = state_dir(dir);
    std::fs::create_dir_all(&sd).map_err(|e| e.to_string())?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(sd.join("engine.log")).map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    std::process::Command::new(exe).arg("engine").env("OPTCHAT_DIR", dir)
        .stdin(std::process::Stdio::null()).stdout(log.try_clone().unwrap()).stderr(log)
        .process_group(0).spawn().map_err(|e| e.to_string())?;
    for _ in 0..100 {
        if running(dir) { return Ok(()) }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("engine did not start; see {}", sd.join("engine.log").display()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn ready_nodes_are_queued_not_scanned_for() {
        let d = std::env::temp_dir().join(format!("facet-queues-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let mut s = super::Store::open(&d);
        // 0, 1 short (free); 2, 3 long (need a model)
        for t in ["a", "b", &"x".repeat(600), &"y".repeat(600)] { s.log("user", t).unwrap(); }
        let v = super::View::fold(&s, 100_000);
        let mut m = super::Mem::new(d.clone(), s, v.clone(), v);
        assert!(m.settle_fresh().unwrap());
        // the free messages and their free parent are built at once; the long ones are queued
        assert!(m.store.built(0, 0) && m.store.built(0, 1) && m.store.built(1, 0));
        assert_eq!(m.todo[0].iter().copied().collect::<Vec<_>>(), vec![2, 3]);
        assert!(m.todo.get(1).is_none_or(|q| q.is_empty()));
        // one half built: its parent is not ready yet; both: the parent is queued (not free)
        m.put(0, 2, &"p".repeat(400)).unwrap();
        m.settle_fresh().unwrap();
        assert_eq!(m.todo[0].iter().copied().collect::<Vec<_>>(), vec![3]);
        assert!(m.todo.get(1).is_none_or(|q| q.is_empty()));
        m.put(0, 3, &"q".repeat(400)).unwrap();
        m.settle_fresh().unwrap();
        assert!(m.todo[0].is_empty());
        assert_eq!(m.todo[1].iter().copied().collect::<Vec<_>>(), vec![1]);
        // a new message enters the queue when it is appended
        let i = m.store.log("user", &"z".repeat(600)).unwrap();
        m.append(i, 100_000);
        m.settle_fresh().unwrap();
        assert_eq!(m.todo[0].iter().copied().collect::<Vec<_>>(), vec![4]);
        // a restart finds the same queues from what is on file
        let s2 = super::Store::open(&d);
        let v2 = super::View::fold(&s2, 100_000);
        let mut m2 = super::Mem::new(d.clone(), s2, v2.clone(), v2);
        m2.settle_fresh().unwrap();
        assert_eq!(m2.todo[0].iter().copied().collect::<Vec<_>>(), vec![4]);
        assert_eq!(m2.todo[1].iter().copied().collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn long_text_is_split_not_cut() {
        assert_eq!(super::split("short", 10), vec!["short"]);
        let t = format!("{}\n{}", "a".repeat(8), "b".repeat(8)); // 17 chars
        assert_eq!(super::split(&t, 10), vec!["aaaaaaaa\n", "bbbbbbbb"]);
        let u = "é".repeat(25);
        let p = super::split(&u, 10);
        assert_eq!(p.iter().map(|x| x.chars().count()).collect::<Vec<_>>(), vec![10, 10, 5]);
        assert_eq!(p.concat(), u);
    }
}
