// The one process that owns the chat. It holds `<dir>/lock`, a Unix socket, for its whole
// life (§2): a second engine that can connect to it exits; a socket that refuses connections
// is stale and is taken over. The same socket is how everything else talks to the engine:
// the terminal client, `facet send`, the web and Telegram routes. One JSON object per line.
//
//   -> {"op":"send","text":"..."}     a user message: it starts an instance of its own
//                                     (`later`, from older clients, is ignored)
//   -> {"op":"spawn","task":..,"model":..,"effort":..,"desc":..,"from":..,"wait":bool}
//                                     a task handed on: an instance like any other; with wait,
//                                     the reply comes at its end, with its end record
//   -> {"op":"cancel"}                stop the start that waits, and every running instance
//   -> {"op":"note","text":..,"date":..}  import a note (duplicates skipped)
//   -> {"op":"card","do":"new|reply|fix|kind|close|delete|apply|list",..}  a card (cards.rs):
//                                     what the agent does on one is logged as kind `answer`;
//                                     the MCP card tools are the same call (`card` below)
//   -> {"op":"answer","id":..,"text":..,"apply":..}  the old name of {"op":"card","do":"reply"}
//   -> {"op":"restart","serve":bool}  queue a restart of this engine (and of serve, if asked) for
//                                     when no instance is starting or running; returns at once
//   -> {"op":"resume"}                lift a compactor pause (`facet resume`)
//   -> {"op":"view"} / {"op":"status"} / {"op":"zoom","id":..,"n":..}
//   -> {"op":"model","name":..}       the model of the instances user messages start
//   -> {"op":"watch"}                 then a stream of events, one per line:
//        {"ev":"msg","i":..,"kind":..,"text":..}   a logged message
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
    pub agent_model: String,
    pub compact_effort: String,
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
            name: s("name", "Facet"),
            model: s("model", "opus"),
            effort: s("effort", "high"),
            tools: s("tools", "Bash,Read,Edit,Write,Glob,Grep,WebFetch,WebSearch"),
            permission: s("permission", "bypassPermissions"),
            compact_model: s("compact_model", "haiku"),
            agent_model: s("agent_model", "sonnet"),
            compact_effort: s("compact_effort", "xhigh"),
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
    /// A pause has been announced (notice + Telegram) and its end not yet: `watch` announces
    /// the end once, and resumes the compactor itself.
    pub parked: bool,
    /// Usage-limit failures in a row: the pause after each doubles (`compact::run`).
    pub limit_fails: u32,
    /// When held (failed) nodes may be tried again without a new message, if a turn waits on
    /// them (`watch`); backs off with each failure.
    pub retry_at: Option<SystemTime>,
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
        Mem { store, view, cview, busy: HashSet::new(), failed: HashMap::new(), held: HashSet::new(), dir, todo: Vec::new(), fresh, pause: None,
            parked: false, limit_fails: 0, retry_at: None }
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
        if let Err(e) = super::view::save(&self.dir, &self.view) { eprintln!("[{}] cannot save view.json: {}", crate::log_stamp(), e); }
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
    /// Pause until `until`, or keep a pause that already ends later: a short backoff after a
    /// failed call must not cut short a pause until the limit's announced reset.
    pub fn pause(&mut self, until: SystemTime, why: &str) {
        if let Some((u, _)) = &self.pause { if *u >= until { return } }
        self.pause = Some((until, why.into()));
    }
    /// Lift the pause now (`/resume`).
    pub fn lift(&mut self) { self.pause = None; }
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
    /// the last `rate_limit_info` any call reported
    limits: Mutex<Value>,
    /// the model of the instances user messages start, switchable at runtime (/model); starts as chat.model
    model: Mutex<String>,
    /// a restart queued by `facet restart`: Some(also_serve); fires when idle (see `restart_if_idle`)
    pub restart: Mutex<Option<bool>>,
    /// instances alive (instance.rs): a restart waits for them, their readers are threads of this process
    pub instances: std::sync::atomic::AtomicUsize,
    /// who messages belong to; the taps of running instances (owners.rs)
    pub owners: Mutex<super::owners::Owners>,
}

/// An engine over a chat directory, with no socket, compactor or MCP server: for tests.
#[cfg(test)]
pub fn bare(dir: &Path, mcp_url: &str) -> Arc<Engine> {
    let conf = Conf::load(dir);
    let store = Store::open(dir);
    let view = View::fold(&store, conf.view);
    let cview = View::fold(&store, conf.view);
    let e = Arc::new(Engine {
        dir: dir.to_path_buf(), model: Mutex::new(conf.model.clone()), conf,
        mem: Mutex::new(Mem::new(dir.to_path_buf(), store, view, cview)),
        changed: Condvar::new(), gate: Default::default(),
        turn: Mutex::new(turn::State::default()), compact_sys: state_dir(dir).join("system.txt"), mcp_url: OnceLock::new(),
        watchers: Mutex::new(Vec::new()), spend: Mutex::new(VecDeque::new()),
        limits: Mutex::new(Value::Null),
        restart: Mutex::new(None), instances: Default::default(), owners: Default::default(),
    });
    let _ = e.mcp_url.set(mcp_url.to_string());
    let _ = std::fs::create_dir_all(state_dir(dir));
    e
}

/// A wall-clock time as "07:00 UTC" (with the date if not today).
pub fn hhmm(t: SystemTime) -> String {
    let at: chrono::DateTime<chrono::Utc> = t.into();
    if at.date_naive() == chrono::Utc::now().date_naive() { at.format("%H:%M UTC").to_string() }
    else { at.format("%b %-d %H:%M UTC").to_string() }
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
        eprintln!("[{}] {}", crate::log_stamp(), text);
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
        let mut s = self.spend.lock().unwrap();
        s.push_back((Instant::now(), x));
        while s.front().is_some_and(|f| f.0.elapsed() > Duration::from_secs(3600)) { s.pop_front(); }
        let hour: f64 = s.iter().map(|f| f.1).sum();
        drop(s);
        if self.conf.budget_hour > 0.0 && hour > self.conf.budget_hour {
            let paused = self.mem.lock().unwrap().paused().is_some();
            if !paused {
                self.park(SystemTime::now() + Duration::from_secs(600),
                    &format!("hourly budget spent ({:.0} eq in the last hour, budget {:.0})", hour, self.conf.budget_hour));
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
            // until the window resets (and a few seconds more); `watch` resumes by itself then
            let secs = info["resetsAt"].as_i64().map(|t| t - chrono::Utc::now().timestamp() + 10).unwrap_or(300).clamp(60, 6 * 3600);
            self.park(SystemTime::now() + Duration::from_secs(secs as u64), "usage limit reached");
        }
    }

    /// Park the compactor until `until` (or later, if already parked longer). The pause is kept
    /// in `pause.json`, so a restart keeps it, and announced once (notice and a Telegram push)
    /// when it begins; `watch` ends it by itself when the time comes, and announces that once.
    pub fn park(&self, until: SystemTime, why: &str) {
        let (first, until) = {
            let mut m = self.mem.lock().unwrap();
            m.pause(until, why);
            let first = !m.parked;
            m.parked = true;
            (first, m.paused().unwrap_or(until))
        };
        let secs = until.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = std::fs::write(state_dir(&self.dir).join("pause.json"), json!({"until": secs, "why": why}).to_string());
        self.changed.notify_all();
        let line = format!("compactor paused until {}: {}", hhmm(until), why);
        if first { self.alert(&format!("{}; {} message(s) queued; it resumes by itself", line, self.queued())) }
        else { self.notice(&line) }
    }

    /// Messages and tasks waiting to start.
    pub fn queued(&self) -> usize { self.turn.lock().unwrap().queue.len() }

    /// Instances running, for the routes' busy marks.
    pub fn emit_state(&self) {
        let n = self.instances.load(std::sync::atomic::Ordering::SeqCst);
        let q = self.queued();
        self.emit(json!({"ev": "state", "busy": n > 0 || q > 0, "queued": q, "instances": n}));
    }

    /// Lift the pause and resume the compactor now: for `facet resume` and for proof the limit is
    /// gone (a call that succeeded). Announces once (notice + Telegram) only if it was parked.
    pub fn resume(self: &Arc<Self>, why: &str) {
        let was = {
            let mut m = self.mem.lock().unwrap();
            let was = m.parked || m.paused().is_some();
            m.lift();
            m.parked = false;
            m.limit_fails = 0;
            m.failed.clear();
            m.held.clear();
            was
        };
        let _ = std::fs::remove_file(state_dir(&self.dir).join("pause.json"));
        self.changed.notify_all();
        if was { self.alert(&format!("compactor resumed ({}); {} message(s) queued", why, self.queued())); }
        compact::pump(self);
    }
    /// A call that was not refused by the usage limit finished: the limit is gone, so a pause
    /// still on (a stale deadline, a plan upgrade) is lifted at once.
    pub fn proof_of_life(self: &Arc<Self>) {
        let on = { let mut m = self.mem.lock().unwrap(); m.parked || m.paused().is_some() };
        if on { self.resume("a call succeeded, so the limit is gone"); }
    }

    /// A pause kept in `pause.json` from before a restart: dropped silently if it is over;
    /// else it holds, and counts as already announced (`parked`), so no second Telegram push.
    pub fn load_pause(&self) {
        let f = state_dir(&self.dir).join("pause.json");
        let Some(p) = std::fs::read_to_string(&f).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()) else { return };
        let until = SystemTime::UNIX_EPOCH + Duration::from_secs(p["until"].as_u64().unwrap_or(0));
        if until <= SystemTime::now() { let _ = std::fs::remove_file(&f); return }
        let mut m = self.mem.lock().unwrap();
        m.pause(until, p["why"].as_str().unwrap_or("paused"));
        m.parked = true;
    }

    /// A notice that also goes to Telegram (only when parking and resuming: never per call).
    pub fn alert(&self, text: &str) {
        self.notice(text);
        if cfg!(test) { return }
        let text = format!("facet: {}", text);
        std::thread::spawn(move || {
            if let Err(x) = crate::tg::push(&Cfg::load(), &text) { eprintln!("[{}] telegram push failed: {}", crate::log_stamp(), x); }
        });
    }

    /// The compactor's own clock (one thread, started with the engine): a pause ends when the
    /// wall clock passes its end, with no message, /resume or restart needed. Then held nodes
    /// are released and the pump runs. Also, while a turn waits for summaries, failed nodes are
    /// retried after their backoff (`retry_at`) and the pump is run, so no turn waits forever
    /// on work nobody starts. `tick` bounds every wait (a laptop's sleep, a missed notify).
    pub fn watch(self: &Arc<Self>, tick: Duration) {
        let e = self.clone();
        std::thread::spawn(move || loop {
            let mut m = e.mem.lock().unwrap();
            let wait = match m.paused() {
                Some(u) => u.duration_since(SystemTime::now()).unwrap_or_default().min(tick).max(Duration::from_millis(5)),
                None => tick,
            };
            m = e.changed.wait_timeout(m, wait).unwrap().0;
            let now = SystemTime::now();
            if m.parked && m.paused().is_none() {
                m.parked = false;
                m.held.clear();
                drop(m);
                let _ = std::fs::remove_file(state_dir(&e.dir).join("pause.json"));
                e.alert(&format!("compactor resumed; {} message(s) queued", e.queued()));
                e.changed.notify_all();
                compact::pump(&e);
                continue;
            }
            if m.paused().is_some() { continue }
            let retry = !m.held.is_empty() && m.retry_at.is_none_or(|r| r <= now);
            let idle = m.busy.is_empty();
            drop(m);
            let settling = { let t = e.turn.lock().unwrap(); t.running && t.phase == "settling" };
            if settling && (retry || idle) {
                if retry { e.mem.lock().unwrap().held.clear(); }
                compact::pump(&e);
            }
        });
    }
    /// The model the next user message's instance uses.
    pub fn model(&self) -> String { self.model.lock().unwrap().clone() }

    pub fn limits(&self) -> Value { self.limits.lock().unwrap().clone() }

    pub fn hour_eq(&self) -> f64 {
        self.spend.lock().unwrap().iter().filter(|f| f.0.elapsed() <= Duration::from_secs(3600)).map(|f| f.1).sum()
    }

    pub fn status(&self) -> Value {
        let mut m = self.mem.lock().unwrap();
        let unbuilt = m.view.parts.iter().filter(|p| !m.store.built(p.l, p.i)).count();
        let t = self.turn.lock().unwrap();
        let n = self.instances.load(std::sync::atomic::Ordering::SeqCst);
        json!({
            "messages": m.store.t(), "view_parts": m.view.parts.len(), "view_bytes": m.view.size(&m.store),
            "unsummarized": unbuilt, "compacting": m.busy.len(), "failing": m.failed.len(), "held": m.held.len(),
            "compact_view_bytes": m.cview.size(&m.store),
            "paused": m.pause_reason(),
            "paused_until": m.paused().and_then(|u| u.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_secs()),
            "busy": t.running || n > 0, "queued": t.queue.len(), "instances": n,
            "phase": t.phase, "hour_eq": self.hour_eq().round(),
            "model": self.model(), "compact_model": self.conf.compact_model,
            "mcp": self.mcp_url.get().is_some(),
            "limits": super::usage::limits_line(&self.limits()),
            "dir": self.dir.display().to_string(), "state_dir": state_dir(&self.dir).display().to_string(),
            "pause_line": m.paused().map(|u| format!("paused until {} ({}), {} message(s) queued", hhmm(u),
                m.pause_reason().unwrap_or_default(), t.queue.len())),
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
        eprintln!("[{}] another engine is running on {}", crate::log_stamp(), dir.display());
        std::process::exit(1);
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap_or_else(|e| { eprintln!("cannot bind {}: {}", path.display(), e); std::process::exit(1) });
    // children of an earlier image of this pid (a re-exec restart): waited as they exit
    // a test's short usage-limit backoff (the engine end-to-end test parks and resumes)
    if let Some(ms) = std::env::var("FACET_LIMIT_BACKOFF_MS").ok().and_then(|s| s.parse::<u64>().ok()) {
        compact::LIMIT_BACKOFF_MS.store(ms, std::sync::atomic::Ordering::Relaxed);
    }
    let inherited = super::claude::reap_inherited();
    if inherited > 0 { eprintln!("[{}] {} child process(es) from before the restart: reaped as they exit", crate::log_stamp(), inherited); }

    let conf = Conf::load(&dir);
    // a git repository from the start: Claude Code tells the model whether its cwd is one,
    // in a block before the view, so it must not flip after the first commit
    if !dir.join(".git").exists() {
        let _ = std::process::Command::new("git").arg("-C").arg(&dir).args(["init", "-q"]).status();
        let _ = std::fs::write(dir.join(".gitignore"), "lock\n");
    }
    let store = Store::open(&dir);
    for n in &store.notes { eprintln!("[{}] load: {}", crate::log_stamp(), n); }
    let t0 = Instant::now();
    // §3.2: the view is loaded from view.json, never rebuilt from the log (only folded once,
    // for a chat that has none yet)
    let (view, cview, vnote) = super::view::load(&dir, &store, conf.view);
    if let Some(n) = &vnote { eprintln!("[{}] load: {}", crate::log_stamp(), n); }
    eprintln!("[{}] {} messages, view of {} lines / {} bytes, compactions' view {} lines / {} bytes, in {} ms",
        crate::log_stamp(), store.t(), view.parts.len(), view.size(&store), cview.parts.len(), cview.size(&store), t0.elapsed().as_millis());
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
        watchers: Mutex::new(Vec::new()), spend: Mutex::new(VecDeque::new()),
        limits: Mutex::new(std::fs::read_to_string(sd.join("limits.json")).ok()
            .and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null)),
        model: Mutex::new(String::new()),
        restart: Mutex::new(None), instances: Default::default(), owners: Default::default(),
    });
    *e.model.lock().unwrap() = std::fs::read_to_string(sd.join("model")).ok().map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty()).unwrap_or_else(|| e.conf.model.clone());
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
    e.load_pause();
    e.watch(Duration::from_secs(30));
    compact::pump(&e);
    turn::restore(&e);
    // the inbox is the one way in: messages left while this engine was down are taken now, the
    // rest as they arrive (a directory listing twice a second costs nothing)
    {
        let e = e.clone();
        std::thread::spawn(move || loop {
            turn::drain_inbox(&e);
            std::thread::sleep(Duration::from_millis(400));
        });
    }

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
                if text.is_empty() { json!({"ok": false, "error": "empty"}) } else { turn::input(e, text); json!({"ok": true}) }
            }
            "cancel" => { turn::cancel(e); json!({"ok": true}) }
            // a task handed on (`facet spawn`): an instance like any other, which answers the
            // user itself and writes its own end record. With "wait", this connection is held
            // until that end record and gets it in the reply (`facet spawn --wait`).
            "spawn" => {
                let wait = v["wait"].as_bool().unwrap_or(false);
                let (tx, rx) = std::sync::mpsc::channel();
                match turn::spawn(e, v["model"].as_str().unwrap_or(""), v["effort"].as_str().unwrap_or(""),
                    v["desc"].as_str().unwrap_or(""), v["task"].as_str().unwrap_or(""), v["from"].as_str().unwrap_or(""), wait.then_some(tx)) {
                    Ok(id) if wait => {
                        let Ok(record) = rx.recv() else { return };
                        if writeln!(w, "{}", json!({"ok": true, "id": id, "report": record})).and_then(|_| w.flush()).is_err() { return }
                        continue;
                    }
                    Ok(id) => json!({"ok": true, "id": id}),
                    Err(x) => json!({"ok": false, "error": x}),
                }
            }
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
                e.notice("restart queued: once no instance is starting or running");
                // idle already (called from a shell, not from a turn): go now
                let e2 = e.clone();
                std::thread::spawn(move || { std::thread::sleep(Duration::from_millis(300)); restart_if_idle(&e2); });
                json!({"ok": true, "queued": true})
            }
            "resume" => {
                e.resume("manual");
                json!({"ok": true})
            }
            "view" => { let m = e.mem.lock().unwrap(); json!({"view": m.view.render(&m.store)}) }
            // zoom(id, n) as the agent sees it (§7.1), for a person
            "zoom" => {
                // a name (zoom("Name")) gives that agent's whole run
                if let Some(name) = v["id"].as_str() { json!({"text": super::instance::transcript(e, name)}) } else {
                    let m = e.mem.lock().unwrap();
                    json!({"text": super::mcp::zoom(&m.store, v["id"].as_i64().unwrap_or(-1), v["n"].as_i64().unwrap_or(1))})
                }
            }
            // the model of the instances user messages start; kept in the state dir across
            // restarts (it wins over chat.model). A new model starts with a cold cache once.
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

/// The two venues output reaches (README, "Stream and venues"). The log is the stream; a venue
/// is where some of it is also delivered. `chat` is the conversation as the user reads it, on
/// Telegram and the web chat page alike: only what lands here as kind `chat` (and the user's
/// own messages) is shown there.
pub fn chat(e: &Arc<Engine>, text: &str, inst: Option<&str>) -> Result<(), String> {
    let text = text.trim();
    if text.is_empty() { return Err("empty".into()) }
    let i = e.log_at("chat", text, &super::store::now_iso());
    // so a reply to this message (Telegram) can reach the instance, while it runs
    if let (Some(i), Some(inst)) = (i, inst) { e.owners.lock().unwrap().own(i, inst); }
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

/// Fire a queued restart if there is one and nothing would be lost: nothing starting, no
/// instance alive (each has a reader thread in this process that exec would end). Called when
/// the starter goes idle and when an instance ends. Re-execs this binary with the same
/// arguments and environment: same pid (so systemd/launchd see no exit), picks up a rebuilt
/// binary. The socket and the MCP listener are close-on-exec, so the new process finds the old
/// lock refusing connections, calls it stale, removes it and binds afresh.
pub fn restart_if_idle(e: &Arc<Engine>) {
    use std::sync::atomic::Ordering::SeqCst;
    let Some(serve) = *e.restart.lock().unwrap() else { return };
    if e.turn.lock().unwrap().running { return }
    let n = e.instances.load(SeqCst);
    if n > 0 {
        e.notice(&format!("restart waits for {} instance(s)", n));
        return;
    }
    // let in-flight compaction finish rather than kill it mid-spend (at most 30 s)
    for _ in 0..60 {
        if e.mem.lock().unwrap().busy.is_empty() { break }
        std::thread::sleep(Duration::from_millis(500));
    }
    {
        // claim the restart exactly once; an instance that started meanwhile postpones it
        let mut r = e.restart.lock().unwrap();
        if r.is_none() || e.turn.lock().unwrap().running || e.instances.load(SeqCst) > 0 { return }
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
    // children still running would outlive this image as zombies nobody waits for
    super::claude::end_all();
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

/// `request`, giving up after `within` (a page must not hang on a busy engine).
pub fn request_within(dir: &Path, v: Value, within: Duration) -> Result<Value, String> {
    let mut s = UnixStream::connect(sock(dir)).map_err(|_| format!("no engine running on {}", dir.display()))?;
    s.set_read_timeout(Some(within)).map_err(|e| e.to_string())?;
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

    /// A compaction hits the usage limit: the compactor parks, and once the reset time has
    /// passed it resumes by itself, with no message, /resume or restart, and builds the node.
    #[test]
    fn a_limit_pause_ends_by_itself() {
        use std::time::{Duration, Instant};
        let _env = crate::optchat::CLAUDE_ENV.lock().unwrap_or_else(|p| p.into_inner());
        let d = std::env::temp_dir().join(format!("facet-limit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        // a stand-in model: refuses its first call with Claude Code's limit message, then answers
        let calls = d.join("calls");
        let fake = d.join("fake.sh");
        std::fs::write(&fake, format!("#!/bin/sh\necho call >> '{}'\n\
            if [ $(wc -l < '{}') -eq 1 ]; then echo '{{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"result\":\"You have hit your session limit\"}}'\n\
            else echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"user: a summary\"}}'; fi\n",
            calls.display(), calls.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        unsafe { std::env::set_var("FACET_CLAUDE", &fake); }
        super::compact::LIMIT_BACKOFF_MS.store(400, std::sync::atomic::Ordering::Relaxed);

        let e = super::bare(&d, "");
        // the clock's tick is long: the wake-up must come from the pause's end, not polling
        e.watch(Duration::from_secs(60));
        let i = e.log_at("user", &"x".repeat(600), &crate::optchat::store::now_iso()).unwrap();
        let until = |what: &str, secs: u64, f: &dyn Fn() -> bool| {
            let t = Instant::now();
            while !f() { assert!(t.elapsed() < Duration::from_secs(secs), "timed out: {}", what); std::thread::sleep(Duration::from_millis(20)); }
        };
        until("parked", 10, &|| e.mem.lock().unwrap().parked);
        let parked_at = Instant::now();
        assert!(e.status()["pause_line"].as_str().unwrap().starts_with("paused until "));
        assert!(super::state_dir(&d).join("pause.json").exists(), "the pause is kept on file");
        assert!(!e.mem.lock().unwrap().store.built(0, i));
        // nothing more is done from outside: no message, no /resume
        until("built after the reset", 10, &|| e.mem.lock().unwrap().store.built(0, i));
        assert!(parked_at.elapsed() >= Duration::from_millis(250), "not before the reset");
        let m = e.mem.lock().unwrap();
        assert!(!m.parked && m.limit_fails == 0);
        drop(m);
        assert!(e.status()["pause_line"].is_null());
        assert!(!super::state_dir(&d).join("pause.json").exists());
        assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 2);

        // the stall that was seen: a turn's call reports the limit (observe parks the compactor),
        // no compaction has failed, and messages logged meanwhile wait in the queue; past the
        // reset they are built with nothing else happening
        e.park(std::time::SystemTime::now() + Duration::from_millis(400), "usage limit reached");
        let j = e.log_at("user", &"y".repeat(600), &crate::optchat::store::now_iso()).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!e.mem.lock().unwrap().store.built(0, j), "nothing is built while parked");
        until("built after the second reset", 10, &|| e.mem.lock().unwrap().store.built(0, j));
        assert!(!e.mem.lock().unwrap().parked);
        let _ = std::fs::remove_dir_all(super::state_dir(&d));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Fake limits: a stale pause is dropped on load, a future one holds without a second
    /// announcement; a successful call and `resume` each lift a pause.
    #[test]
    fn pause_stale_success_and_resume() {
        use std::time::{Duration, SystemTime};
        let d = std::env::temp_dir().join(format!("facet-pause-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let e = super::bare(&d, "");
        let pj = super::state_dir(&d).join("pause.json");
        let secs = |t: SystemTime| t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        let write = |t: SystemTime| std::fs::write(&pj, serde_json::json!({"until": secs(t), "why": "x"}).to_string()).unwrap();
        // stale: dropped, not parked
        write(SystemTime::now() - Duration::from_secs(3600));
        e.load_pause();
        assert!(!pj.exists());
        { let mut m = e.mem.lock().unwrap(); assert!(!m.parked && m.paused().is_none()); }
        // future: held, counted as announced
        write(SystemTime::now() + Duration::from_secs(3600));
        e.load_pause();
        { let mut m = e.mem.lock().unwrap(); assert!(m.parked && m.paused().is_some()); }
        // a successful call clears it
        e.proof_of_life();
        { let mut m = e.mem.lock().unwrap(); assert!(!m.parked && m.paused().is_none()); }
        assert!(!pj.exists());
        // resume clears it
        e.park(SystemTime::now() + Duration::from_secs(3600), "usage limit reached");
        assert!(pj.exists());
        e.resume("manual");
        { let mut m = e.mem.lock().unwrap(); assert!(!m.parked && m.paused().is_none()); }
        assert!(!pj.exists());
        let _ = std::fs::remove_dir_all(super::state_dir(&d));
        let _ = std::fs::remove_dir_all(&d);
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
