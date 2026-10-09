// §4 The compactor: a pump that starts every node whose sources and context are ready, in
// the gist's order, up to JOBS at once; one `claude -p` call per node, with the view as
// cached context, the size enforced by cut-at-limit feedback in the same conversation.
//
// Two things are added for `claude -p`, both about the cache and both invisible to the model
// (see README.md, Departures from the gist):
//   * the chain: the context's tail after the last view mark is kept as one block per call
//     increment, so the next call's end mark finds the previous call's end within the 20-block
//     lookback and reads the whole previous context instead of rewriting its tail;
//   * the gate: calls whose marked prefixes are not in the cache yet wait for the one call
//     already writing them, instead of all writing the same 30-40k tokens in parallel.
use super::claude::{self, Meter, Proc, Tick};
use super::engine::Engine;
use super::{cut_bytes, flat, prompts, view, NODE, RETRY, TRIES, CALL_TIMEOUT, WARM};
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

pub struct Job { pub l: usize, pub i: usize, pub chat: String, pub step: String }

/// Start whatever can start (§4.1). Free nodes are built on the spot: they need no model.
pub fn pump(e: &Arc<Engine>) {
    let mut jobs = Vec::new();
    {
        let mut m = e.mem.lock().unwrap();
        let mut grew = false;
        // free nodes, bottom-up, in one pass: a free child can make its parent free
        let t = m.store.t();
        let mut l = 0;
        while (1usize << l) <= t {
            let mut i = m.lo(l);
            while ((i + 1) << l) <= t {
                if !m.store.built(l, i) && m.store.ready(l, i) {
                    if let Some(text) = m.store.free(l, i) {
                        if let Err(err) = m.store.put(l, i, &text) { e.notice(&format!("cannot write tree: {}", err)); return }
                        grew = true;
                    }
                }
                i += 1;
            }
            l += 1;
        }
        if grew { let budget = e.conf.view; let mm = &mut *m; mm.view.fit(&mm.store, budget); e.changed.notify_all(); }
        if m.paused().is_some() { return }
        let first = m.view.first(&m.store);
        let mut l = 0;
        'levels: while (1usize << l) <= t {
            let mut i = m.lo(l);
            while ((i + 1) << l) <= t {
                if m.busy.len() >= e.conf.jobs { break 'levels }
                let end = if l == 0 { i } else { (i + 1) << l };
                if end > first { break } // rule 3; `end` only grows with i
                if !m.store.built(l, i) && !m.busy.contains(&(l, i)) && m.store.ready(l, i) {
                    m.busy.insert((l, i));
                    jobs.push(job(&m, l, i, &e.conf.name));
                }
                i += 1;
            }
            l += 1;
        }
    }
    for j in jobs {
        let e = e.clone();
        std::thread::spawn(move || run(&e, j));
    }
}

/// The two blocks of a compactor call (§4.2). `</chat>` opens the step block rather than
/// closing the chat block, so that one call's chat is a prefix of the next one's.
fn job(m: &super::engine::Mem, l: usize, i: usize, _name: &str) -> Job {
    let s = &m.store;
    let head = format!("</chat>\n\nFor length only, here is an invented example line about no real chat, exactly {} bytes; never copy or mention its content:\n{}\n\n", NODE, prompts::SCALE);
    if l == 0 {
        let chat = m.view.bare(s, |p| p.end() <= i);
        let msg = &s.msgs[i];
        let step = format!("{}Compress this message into one line, in at most {} bytes:\n{}: {}", head, NODE, msg.kind, msg.text);
        Job { l, i, chat, step }
    } else {
        let end = (i + 1) << l;
        let chat = m.view.bare(s, |p| p.start() < end);
        let (a, b) = (s.node(l - 1, 2 * i).unwrap_or(""), s.node(l - 1, 2 * i + 1).unwrap_or(""));
        let step = format!("{}Merge these two lines into one, in at most {} bytes:\n{}\n{}", head, NODE, flat(a), flat(b));
        Job { l, i, chat, step }
    }
}

/// What a node's call did, for the event log only (events.rs).
#[derive(Default)]
struct Trace { tries: Vec<usize>, gate_ms: u128, requests: usize }

fn run(e: &Arc<Engine>, j: Job) {
    let t0 = Instant::now();
    let mut tr = Trace::default();
    let name = format!("{}+{}", j.i << j.l, 1usize << j.l);
    let (blocks, keys) = {
        let mut ch = e.chain.lock().unwrap();
        let blocks = layout(&mut ch, &j.chat, &j.step);
        let keys = keys(&e.conf.compact_model, &blocks);
        (blocks, keys)
    };
    let res = call(e, &blocks, &keys, &name, &mut tr);
    super::events::log(&e.dir, "node", json!({
        "node": name, "l": j.l, "i": j.i, "ok": res.is_ok(),
        "kept": res.as_ref().ok().map(|t| t.len()), "tries": tr.tries,
        "error": res.as_ref().err().map(|f| f.text.clone()), "limit": res.as_ref().err().map(|f| f.limit),
        "requests": tr.requests, "ms": t0.elapsed().as_millis(), "gate_ms": tr.gate_ms,
        "context_bytes": j.chat.len(), "step_bytes": j.step.len(), "blocks": blocks.len(),
        "marks": blocks.iter().filter(|b| b.1).count(),
    }));
    match res {
        Ok(text) => {
            let mut m = e.mem.lock().unwrap();
            let mm = &mut *m;
            if let Err(err) = mm.store.put(j.l, j.i, &text) { drop(m); e.notice(&format!("cannot write tree: {}", err)); return }
            mm.view.fit(&mm.store, e.conf.view);
            mm.busy.remove(&(j.l, j.i));
            mm.failed.remove(&(j.l, j.i));
            e.changed.notify_all();
        }
        Err(f) => {
            let n = { let mut m = e.mem.lock().unwrap(); let c = m.failed.entry((j.l, j.i)).or_insert(0); *c += 1; *c };
            let name = format!("{}+{}", j.i << j.l, 1 << j.l);
            if f.limit {
                e.mem.lock().unwrap().pause(SystemTime::now() + Duration::from_secs(300), &f.text);
                e.notice(&format!("compactor paused for 5 min: {}", f.text));
            } else if n >= PARK {
                // §4.1 says retry forever; a node that fails every time (a refusal, say) would
                // then cost a paid call every 10 s. Park instead, and say so.
                e.mem.lock().unwrap().pause(SystemTime::now() + Duration::from_secs(3600), &format!("node {} failed {} times", name, n));
                e.notice(&format!("compactor parked for 1 h: node {} failed {} times: {} (/resume to retry now)", name, n, f.text));
            } else if n == 1 {
                e.notice(&format!("node {} failed (retrying every {} s): {}", name, RETRY.as_secs(), f.text));
            }
            // Wait RETRY, and then out any pause, in short steps against the wall clock: one
            // long sleep would miss a /resume and, on macOS, stand still while the machine sleeps.
            let t0 = Instant::now();
            let mut m = e.mem.lock().unwrap();
            while t0.elapsed() < RETRY || m.paused().is_some() {
                m = e.changed.wait_timeout(m, RETRY).unwrap().0;
            }
            m.busy.remove(&(j.l, j.i));
            drop(m);
        }
    }
    pump(e);
}

pub struct Fail { pub text: String, pub limit: bool }
fn fail(text: impl Into<String>) -> Fail { let text = text.into(); Fail { limit: claude::limit_hit(&text), text } }

/// One node: the call, then cut-at-limit feedback until it fits or TRIES run out (§4.3).
fn call(e: &Arc<Engine>, blocks: &[(String, bool)], keys: &[u64], name: &str, tr: &mut Trace) -> Result<String, Fail> {
    let tg = Instant::now();
    let claimed = e.gate.acquire(keys);
    tr.gate_ms = tg.elapsed().as_millis();
    let mut warmed = false;
    let args = {
        let mut a = claude::base_args(&e.conf.compact_model, &e.conf.compact_effort, &e.compact_sys.to_string_lossy(), "");
        a.push("--safe-mode".into());
        a
    };
    // Claude Code's own cache marks off: the four marks are ours
    let mut p = Proc::spawn(&args, &[("DISABLE_PROMPT_CACHING", "1")], &e.dir).map_err(|x| { e.gate.release(&claimed); fail(format!("spawn: {}", x)) })?;
    let content: Vec<Value> = blocks.iter().map(|(t, mark)| {
        if *mark { json!({"type": "text", "text": t, "cache_control": {"type": "ephemeral"}}) } else { json!({"type": "text", "text": t}) }
    }).collect();
    let r = (|| {
        p.send(Value::Array(content)).map_err(|x| fail(format!("write: {}", x)))?;
        let mut tries: Vec<String> = Vec::new();
        let mut meter = Meter::default();
        let t0 = Instant::now();
        let (mut cc, mut tr0) = (String::new(), Instant::now());
        loop {
            let out = loop {
                let left = CALL_TIMEOUT.saturating_sub(t0.elapsed());
                let ev = p.next(left).map_err(|_| fail(format!("no result after {} s{}", CALL_TIMEOUT.as_secs(), stderr_tail(&p))))?;
                e.observe(&ev);
                if let Some(v) = super::events::cc_version(&ev) { cc = v; }
                match meter.feed(&ev) {
                    Some(Tick::Started(_)) if !warmed => { e.gate.warmed(&claimed, keys); warmed = true; }
                    Some(Tick::Done(r)) => {
                        e.spend("compact", &r);
                        tr.requests += 1;
                        super::events::req(&e.dir, "compact", &r, &cc, tr0.elapsed().as_millis(), json!({"node": name, "try": tries.len() + 1}));
                        tr0 = Instant::now();
                    }
                    _ => {}
                }
                if let Some(o) = claude::outcome(&ev) { break o }
            };
            if out.error { return Err(fail(if out.text.is_empty() { out.subtype } else { out.text })) }
            // the retry note shows the cut ending in "| ← LIMIT"; a model may copy it back
            let line = out.text.trim();
            let line = line.strip_suffix("← LIMIT").map(|t| t.trim_end().trim_end_matches('|')).unwrap_or(line).trim().to_string();
            if line.is_empty() { return Err(fail("empty reply")) }
            let n = line.len();
            tr.tries.push(n);
            tries.push(line);
            if n <= NODE || tries.len() >= TRIES { break }
            let cut = cut_bytes(tries.last().unwrap(), NODE).trim_end_matches('\u{FFFD}');
            p.send_text(&format!("That line is {} bytes; the limit is {}. It must end where it is cut here:\n{}| ← LIMIT", n, NODE, cut))
                .map_err(|x| fail(format!("write: {}", x)))?;
        }
        Ok(tries.into_iter().min_by_key(|t| t.len()).unwrap())
    })();
    if !warmed { e.gate.release(&claimed); }
    p.finish();
    r
}

fn stderr_tail(p: &Proc) -> String {
    let e = p.stderr.lock().unwrap();
    let t = e.trim();
    if t.is_empty() { String::new() } else { format!(": {}", &t[t.len().saturating_sub(300)..]) }
}

// ---- the chain -------------------------------------------------------------------------

/// The tail blocks of the last context (after its last view mark), and what came before it.
#[derive(Default)]
pub struct Chain { base: u64, blocks: Vec<String> }

const CHAIN_MAX: usize = 16;
/// failures of one node after which the compactor parks
const PARK: u32 = 5; // the API looks back 20 blocks from a mark

fn h(s: &str) -> u64 { let mut x = DefaultHasher::new(); s.hash(&mut x); x.finish() }

/// The blocks of a compactor call, each with whether it carries a cache mark.
/// View pieces at the MARKS (60k/92k/120k characters) marked, the tail as chain blocks with the last one marked;
/// a mark left over goes on the step, which then serves the size retries.
pub fn layout(ch: &mut Chain, chat: &str, step: &str) -> Vec<(String, bool)> {
    let cuts = view::cuts(chat);
    let split = cuts.last().copied().unwrap_or(0);
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut a = 0;
    for c in &cuts { out.push((chat[a..*c].to_string(), true)); a = *c; }
    let tail = &chat[split..];
    let base = h(&chat[..split]);
    if ch.base != base { ch.base = base; ch.blocks.clear(); }
    let mut off = 0;
    let mut used = 0;
    for b in &ch.blocks {
        if tail[off..].starts_with(b.as_str()) { off += b.len(); used += 1; } else { break }
    }
    let mut tb: Vec<String> = ch.blocks[..used].to_vec();
    if off < tail.len() { tb.push(tail[off..].to_string()); }
    // a tail that extends the chain, or differs from it, becomes the chain; a shorter
    // context (an older merge) leaves it alone
    let shorter = used < ch.blocks.len() && ch.concat().starts_with(tail);
    if !shorter && off < tail.len() {
        if tb.len() > CHAIN_MAX { tb = vec![tail.to_string()]; }
        ch.blocks = tb.clone();
    }
    let n = tb.len();
    for (k, b) in tb.into_iter().enumerate() { out.push((b, k + 1 == n)); }
    let marks = out.iter().filter(|b| b.1).count();
    out.push((step.to_string(), marks < 4));
    out
}

impl Chain { fn concat(&self) -> String { self.blocks.concat() } }

/// One key per marked prefix: what the cache holds an entry for once the call has started.
pub fn keys(model: &str, blocks: &[(String, bool)]) -> Vec<u64> {
    let mut x = DefaultHasher::new();
    model.hash(&mut x);
    let mut out = Vec::new();
    for (t, mark) in blocks {
        t.hash(&mut x);
        if *mark { out.push(x.finish()); }
    }
    out
}

// ---- the gate --------------------------------------------------------------------------

enum Slot { Writing, Warm(Instant) }

#[derive(Default)]
pub struct Gate { slots: Mutex<HashMap<u64, Slot>>, cv: Condvar }

impl Gate {
    /// Wait while another call is writing the longest prefix of ours that is not warm, then
    /// claim the prefixes we will write. Never waits more than a minute in total.
    pub fn acquire(&self, keys: &[u64]) -> Vec<u64> {
        let t0 = Instant::now();
        let mut g = self.slots.lock().unwrap();
        loop {
            g.retain(|_, s| match s { Slot::Warm(t) => t.elapsed() < WARM, Slot::Writing => true });
            let mut wait = false;
            for k in keys.iter().rev() {
                match g.get(k) {
                    Some(Slot::Warm(_)) => break,
                    Some(Slot::Writing) => { wait = true; break }
                    None => continue,
                }
            }
            if !wait || t0.elapsed() > Duration::from_secs(60) {
                let mine: Vec<u64> = keys.iter().copied().filter(|k| !g.contains_key(k)).collect();
                for k in &mine { g.insert(*k, Slot::Writing); }
                return mine;
            }
            g = self.cv.wait_timeout(g, Duration::from_secs(5)).unwrap().0;
        }
    }
    /// The call's first request has started: everything it marked is in the cache (and
    /// everything it read was renewed).
    pub fn warmed(&self, _claimed: &[u64], keys: &[u64]) {
        let mut g = self.slots.lock().unwrap();
        for k in keys { g.insert(*k, Slot::Warm(Instant::now())); }
        self.cv.notify_all();
    }
    pub fn release(&self, claimed: &[u64]) {
        let mut g = self.slots.lock().unwrap();
        for k in claimed { if let Some(Slot::Writing) = g.get(k) { g.remove(k); } }
        self.cv.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(n: usize, tag: &str) -> String { (0..n).map(|k| format!("{} line {:04} {}\n", tag, k, "x".repeat(80))).collect() }

    #[test]
    fn chain_grows_one_block_per_call_and_marks_stay_four() {
        let mut ch = Chain::default();
        let mut chat = format!("<chat>\n{}", lines(crate::optchat::MARKS[2] / 90 + 100, "a")); // past the last cut
        let b1 = layout(&mut ch, &chat, "STEP1");
        assert_eq!(b1.iter().filter(|b| b.1).count(), 4);
        assert_eq!(b1.len(), 5); // 3 pieces, tail, step
        assert!(!b1[4].1);
        chat.push_str("new line one\n");
        let b2 = layout(&mut ch, &chat, "STEP2");
        assert_eq!(b2.len(), 6); // 3 pieces, old tail, increment, step
        assert_eq!(b2[3], (b1[3].0.clone(), false));
        assert_eq!(b2[4], ("new line one\n".to_string(), true));
        assert_eq!(b2.iter().filter(|b| b.1).count(), 4);
        // a merge's context equal to the current chat: same blocks
        let b3 = layout(&mut ch, &chat, "MERGE");
        assert_eq!(b3[..5], b2[..5]);
        // the concatenation is always the chat, then the step
        let all: String = b3.iter().map(|b| b.0.as_str()).collect();
        assert_eq!(all, format!("{}MERGE", chat));
        // a shorter context (an older merge) does not reset the chain
        let short = chat[..chat.len() - "new line one\n".len()].to_string();
        let b4 = layout(&mut ch, &short, "OLD");
        assert_eq!(b4[3].0, b1[3].0);
        chat.push_str("two\n");
        let b5 = layout(&mut ch, &chat, "S");
        assert_eq!(b5.len(), 7);
        // a change inside the tail restarts the chain from the changed text
        let changed = chat.replace("a line 1149", "b line 1149");
        let b6 = layout(&mut ch, &changed, "S");
        assert_eq!(b6.len(), 5);
        assert_eq!(b6.iter().filter(|b| b.1).count(), 4);
    }

    #[test]
    fn short_chat_marks_the_step() {
        let mut ch = Chain::default();
        let b = layout(&mut ch, "<chat>\nshort\n", "STEP");
        assert_eq!(b, vec![("<chat>\nshort\n".into(), true), ("STEP".into(), true)]);
        let b = layout(&mut ch, "<chat>\n", "STEP"); // node 0: nothing before it
        assert_eq!(b.len(), 2);
    }

    #[test]
    fn chain_is_capped() {
        let mut ch = Chain::default();
        let mut chat = String::from("<chat>\n");
        let mut last = 0;
        for k in 0..40 {
            chat.push_str(&format!("l{}\n", k));
            last = layout(&mut ch, &chat, "S").len();
            assert!(last <= CHAIN_MAX + 2);
        }
        assert!(last >= 2);
    }

    #[test]
    fn gate_makes_followers_wait_for_the_writer() {
        let g = Arc::new(Gate::default());
        let k = vec![1, 2, 3];
        let mine = g.acquire(&k);
        assert_eq!(mine, k);
        let g2 = g.clone();
        let t = std::thread::spawn(move || { let t0 = Instant::now(); let c = g2.acquire(&[1, 2, 3, 4]); (t0.elapsed(), c) });
        std::thread::sleep(Duration::from_millis(300));
        g.warmed(&mine, &k);
        let (waited, claimed) = t.join().unwrap();
        assert!(waited >= Duration::from_millis(250));
        assert_eq!(claimed, vec![4]); // only the part not written yet
        // a failed writer releases: the next one claims
        let c = g.acquire(&[9]);
        g.release(&c);
        assert_eq!(g.acquire(&[9]), vec![9]);
    }
}
