// §4 Compactions: a pump that starts every node that is ready, in the gist's order, up to JOBS
// at once; one `claude -p` call per node, with the compactions' own view (16-32 KB, cut from the
// chat's) as cached context and the gist's task verbatim, the size shown by a 512-dash ruler and
// enforced by the cut-at-limit retry in the same conversation, at most TRIES, shortest kept.
//
// The cache is marked as in the gist (§3.3): the context goes out in blocks of BLOCK lines, one
// mark on its last whole block and one on the request's end (the task). The gate is the
// gist's too: calls whose marked prefixes are not in the cache yet wait for the one call
// already writing them, instead of all writing the same prefix in parallel.
use super::claude::{self, Meter, Proc, Tick};
use super::engine::Engine;
use super::{cut_bytes, flat, view, AHEAD, NODE, TRIES, CALL_TIMEOUT, WARM};
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

pub struct Job { pub l: usize, pub i: usize, pub chat: String, pub step: String }

/// Start whatever can start (§4 of the gist, "The order"): a message's node once fewer than
/// AHEAD lines before it are still unbuilt, a merge once both its halves are built, up to JOBS
/// at once. Work is found in the queues of ready nodes (`Mem::todo`), never by scanning the
/// tree. Free nodes are built on the spot: they need no model.
pub fn pump(e: &Arc<Engine>) {
    let mut jobs = Vec::new();
    {
        let mut m = e.mem.lock().unwrap();
        let grew = match m.settle_fresh() {
            Ok(g) => g,
            Err(err) => { drop(m); e.notice(&format!("cannot write tree: {}", err)); return }
        };
        if grew { let budget = e.conf.view; m.refit(budget); e.changed.notify_all(); }
        if m.paused().is_some() { return }
        let mm = &mut *m;
        // level 0: the queue holds every unbuilt message (running and failed ones included,
        // until built), so the first AHEAD of it are those with fewer than AHEAD before them
        let mut cand: Vec<(usize, usize)> = mm.todo.first().map(|q| q.iter().take(AHEAD).map(|&i| (0, i)).collect()).unwrap_or_default();
        for (l, q) in mm.todo.iter().enumerate().skip(1) { cand.extend(q.iter().map(|&i| (l, i))); }
        for (l, i) in cand {
            if mm.busy.len() >= e.conf.jobs { break }
            if mm.busy.contains(&(l, i)) || mm.held.contains(&(l, i)) { continue }
            mm.busy.insert((l, i));
            jobs.push(job(mm, l, i));
        }
    }
    for j in jobs {
        let e = e.clone();
        std::thread::spawn(move || run(&e, j));
    }
}

/// The 512-dash ruler that shows the model the length (§4 of the gist): a real sample line got
/// its content copied.
pub fn ruler() -> String { "-".repeat(NODE) }

/// The two blocks of a compaction (§4 of the gist): its view, then its task, verbatim.
/// `</chat>` opens the task block rather than closing the view block, so that one call's view
/// is a prefix of the next one's.
fn job(m: &super::engine::Mem, l: usize, i: usize) -> Job {
    let s = &m.store;
    if l == 0 {
        let chat = m.cview.context(s, i);
        let msg = &s.msgs[i];
        let step = format!("</chat>\n\nCompaction: compress message {} into one line of at most {}\nbytes (about 70 words), the length of this ruler:\n{}\n<input>\n{}: {}\n</input>",
            i, NODE, ruler(), msg.kind, msg.text);
        Job { l, i, chat, step }
    } else {
        let (start, end) = (i << l, (i + 1) << l);
        let chat = m.cview.context(s, end);
        let h = 1usize << (l - 1);
        let (a, b) = (s.node(l - 1, 2 * i).unwrap_or(""), s.node(l - 1, 2 * i + 1).unwrap_or(""));
        let step = format!("</chat>\n\nCompaction: merge lines {}+{} and {}+{}, adjacent, into one line of at most\n{} bytes (about 70 words), the length of this ruler:\n{}\n<chat> may hold their messages, {} to {}, in more detail: take details\nof them from there too.\n<input>\n{}\n{}\n</input>",
            start, h, start + h, h, NODE, ruler(), start, end - 1, flat(a), flat(b));
        Job { l, i, chat, step }
    }
}

/// A reply that begins with an `id+n|` head, copied from the view's format, loses it.
pub fn unhead(line: &str) -> &str {
    let b = line.as_bytes();
    let mut k = 0;
    let digits = |k: &mut usize| { let s = *k; while *k < b.len() && b[*k].is_ascii_digit() { *k += 1; } *k > s };
    if !digits(&mut k) || k >= b.len() || b[k] != b'+' { return line }
    k += 1;
    if !digits(&mut k) || k >= b.len() || b[k] != b'|' { return line }
    line[k + 1..].trim_start()
}

/// What a node's call did, for the event log only (events.rs).
#[derive(Default)]
struct Trace { tries: Vec<usize>, gate_ms: u128, requests: usize }

fn run(e: &Arc<Engine>, j: Job) {
    let t0 = Instant::now();
    let mut tr = Trace::default();
    let name = format!("{}+{}", j.i << j.l, 1usize << j.l);
    let blocks = layout(&j.chat, &j.step);
    let keys = keys(&e.conf.compact_model, &blocks);
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
            if let Err(err) = mm.put(j.l, j.i, &text) { drop(m); e.notice(&format!("cannot write tree: {}", err)); return }
            mm.refit(e.conf.view);
            mm.busy.remove(&(j.l, j.i));
            mm.failed.remove(&(j.l, j.i));
            e.changed.notify_all();
        }
        Err(f) => {
            // §4 of the gist: a failed call is tried again at the next message (Engine::log_at,
            // turn::input), not on a timer; a usage limit also pauses the compactor until it resets
            let n = {
                let mut m = e.mem.lock().unwrap();
                m.busy.remove(&(j.l, j.i));
                m.held.insert((j.l, j.i));
                let c = m.failed.entry((j.l, j.i)).or_insert(0); *c += 1; *c
            };
            if f.limit {
                e.mem.lock().unwrap().pause(SystemTime::now() + Duration::from_secs(300), &f.text);
                e.notice(&format!("compactor paused for 5 min: {}", f.text));
                // the limit's reset is the next chance, message or not: wait out the pause (in
                // short steps against the wall clock, so /resume and a sleeping laptop are seen)
                let e = e.clone();
                std::thread::spawn(move || {
                    let mut m = e.mem.lock().unwrap();
                    while m.paused().is_some() { m = e.changed.wait_timeout(m, Duration::from_secs(10)).unwrap().0; }
                    m.held.remove(&(j.l, j.i));
                    drop(m);
                    pump(&e);
                });
            } else if n == 1 {
                e.notice(&format!("node {} failed (tried again at the next message, or /resume): {}", name, f.text));
            }
            e.changed.notify_all();
            return;
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
    // Claude Code's own cache marks off: the two marks are ours
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
            let line = line.strip_suffix("← LIMIT").map(|t| t.trim_end().trim_end_matches('|')).unwrap_or(line).trim();
            let line = unhead(line).to_string();
            if line.is_empty() { return Err(fail("empty reply")) }
            let n = line.len();
            tr.tries.push(n);
            tries.push(line);
            if n <= NODE || tries.len() >= TRIES { break }
            let cut = cut_bytes(tries.last().unwrap(), NODE).trim_end_matches('\u{FFFD}');
            p.send_text(&format!("Too long: your line is {} bytes, over the {}-byte limit. Write\nthe whole line again for the same <input>, cutting just enough of the\nleast valuable items to fit before this cut:\n{}| ← LIMIT", n, NODE, cut))
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

/// The blocks of a compactor call, each with whether it carries a cache mark (§8): the
/// context's whole blocks of BLOCK lines, the last of them marked, then its partial block,
/// then the step, marked as the request's end (it also serves the size retries).
pub fn layout(chat: &str, step: &str) -> Vec<(String, bool)> {
    let cuts = view::cuts(chat);
    let mut out: Vec<(String, bool)> = view::pieces(chat).into_iter().enumerate()
        .map(|(k, p)| (p.to_string(), k + 1 == cuts.len())).collect();
    out.push((step.to_string(), true));
    out
}

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
    fn two_marks_last_whole_block_and_step() {
        let chat = format!("<chat>\n{}", lines(10, "a")); // 11 lines: 2 whole blocks + 3
        let b = layout(&chat, "STEP");
        assert_eq!(b.len(), 4);
        assert_eq!(b.iter().map(|x| x.1).collect::<Vec<_>>(), vec![false, true, false, true]);
        assert_eq!(b.iter().map(|x| x.0.as_str()).collect::<String>(), format!("{}STEP", chat));
        // one more line keeps every earlier block: the next call reads this one's mark
        let longer = format!("{}x\n", chat);
        let b2 = layout(&longer, "STEP2");
        assert_eq!((&b2[0].0, &b2[1].0), (&b[0].0, &b[1].0));
        assert_eq!(b2.iter().filter(|x| x.1).count(), 2);
        // 2 + 1 whole blocks at 12 lines: the mark moves one block on, within the lookback
        assert!(b2[2].1);
    }

    #[test]
    fn a_copied_head_is_dropped() {
        assert_eq!(unhead("40+8|user: x"), "user: x");
        assert_eq!(unhead("40+8| user: x"), "user: x");
        assert_eq!(unhead("user: 40+8|x"), "user: 40+8|x");
        assert_eq!(unhead("40+x|y"), "40+x|y");
    }

    #[test]
    fn short_chat_marks_only_the_step() {
        let b = layout("<chat>\nshort\n", "STEP");
        assert_eq!(b, vec![("<chat>\nshort\n".into(), false), ("STEP".into(), true)]);
        assert_eq!(layout("<chat>\n", "STEP").len(), 2); // node 0: nothing before it
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
