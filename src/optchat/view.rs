// §5 The view: a list of tree nodes that tiles the chat [0, T), oldest first, under a byte
// budget. It only ever appends at the end and coarsens (never splits), so consecutive views
// share a long prefix, which is what makes them cacheable (§8).
use super::store::Store;
use super::{flat, inner, BLOCK};

pub const PLACEHOLDER: &str = "(not summarized yet: zoom it)";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Part { pub l: usize, pub i: usize }

impl Part {
    pub fn start(&self) -> usize { self.i << self.l }
    pub fn n(&self) -> usize { 1 << self.l }
    pub fn end(&self) -> usize { (self.i + 1) << self.l }
}

#[derive(Default, Clone)]
pub struct View {
    pub parts: Vec<Part>,
    /// A batch is under way: the view passed its budget and has not yet been brought down to
    /// `inner(budget)` (some parents were unbuilt). It goes on merging what it can at each fit
    /// until it gets there (§3.2 of the gist).
    pub cutting: bool,
}

fn text<'a>(s: &'a Store, p: &Part) -> &'a str { s.node(p.l, p.i).unwrap_or(PLACEHOLDER) }

impl View {
    /// Fold the view from message 0, as if every message had arrived in order with the tree as
    /// it is now. Only for a chat that has no saved view yet (or an unreadable one): the gist
    /// says never to rebuild the live view from the log (§3.2), so it is saved and loaded instead
    /// (`save`, `load`).
    pub fn fold(s: &Store, budget: usize) -> View {
        let mut v = View::default();
        let mut size = 0;
        for i in 0..s.t() {
            v.parts.push(Part { l: 0, i });
            size += text(s, &Part { l: 0, i }).len();
            size = v.fit_from(s, budget, size, i + 1);
        }
        v
    }

    pub fn size(&self, s: &Store) -> usize { self.parts.iter().map(|p| text(s, p).len()).sum() }

    /// The gist's batch (§5.2): once the view passes its budget, merge the most due pair whose
    /// parent is built, again and again, until it is at most `inner(budget)` (half). A pair whose
    /// parent is not built yet is passed over; if that stops the batch short, it goes on at each
    /// later fit until it gets there. Returns whether anything merged.
    pub fn fit(&mut self, s: &Store, budget: usize) -> bool {
        let before = self.parts.len();
        let size = self.size(s);
        self.fit_from(s, budget, size, s.t());
        self.parts.len() != before
    }

    fn fit_from(&mut self, s: &Store, budget: usize, mut size: usize, t: usize) -> usize {
        // Nothing merges until the view passes the budget; then it goes down to half the budget
        // in one batch, so the view grows by appends alone, its prefix byte-identical, for the
        // whole climb back up (§8).
        if size > budget { self.cutting = true; }
        if !self.cutting { return size }
        let floor = inner(budget);
        while size > floor {
            let mut best: Option<usize> = None;
            for k in 0..self.parts.len().saturating_sub(1) {
                let (a, b) = (self.parts[k], self.parts[k + 1]);
                if a.l != b.l || a.i % 2 != 0 || b.i != a.i + 1 || !s.built(a.l + 1, a.i / 2) { continue }
                // due = (T - last) / 2^l, last = the pair's last message (b.end() - 1);
                // compare exactly, keep the first (oldest) of equals
                best = match best {
                    Some(j) => {
                        let c = self.parts[j];
                        let lhs = ((t + 1 - b.end()) as u128) << c.l;
                        let rhs = ((t + 1 - self.parts[j + 1].end()) as u128) << a.l;
                        if lhs > rhs { Some(k) } else { Some(j) }
                    }
                    None => Some(k),
                };
            }
            let Some(k) = best else { return size };
            let (a, b) = (self.parts[k], self.parts[k + 1]);
            let up = Part { l: a.l + 1, i: a.i / 2 };
            size = size - text(s, &a).len() - text(s, &b).len() + text(s, &up).len();
            self.parts[k] = up;
            self.parts.remove(k + 1);
        }
        self.cutting = false;
        size
    }

    /// Every line is a summary: the condition for any call to start (§6).
    pub fn settled(&self, s: &Store) -> bool { self.parts.iter().all(|p| s.built(p.l, p.i)) }

    /// What every agent call sees: `id+n|text` per line, inside <chat> tags (§5.1).
    pub fn render(&self, s: &Store) -> String {
        let mut out = String::from("<chat>\n");
        for p in &self.parts {
            out.push_str(&format!("{}+{}|{}\n", p.start(), p.n(), flat(text(s, p))));
        }
        out.push_str("</chat>");
        out
    }

    /// A compaction's context (§4 of the gist): its view's lines, `id+n|text` as every call sees
    /// them, for the parts that end by message `end`, stopping at the first unbuilt line, so no
    /// call ever sees a placeholder or half a message. The closing tag is not included: see
    /// compact.rs.
    pub fn context(&self, s: &Store, end: usize) -> String {
        let mut out = String::from("<chat>\n");
        for p in self.parts.iter().take_while(|p| p.end() <= end) {
            let Some(t) = s.node(p.l, p.i) else { break };
            out.push_str(&format!("{}+{}|{}\n", p.start(), p.n(), flat(t)));
        }
        out
    }

    /// The view as saved in `view.json` (§3.2 of the gist): its `[l, i]` pairs, and whether a
    /// batch is still under way.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({"parts": self.parts.iter().map(|p| [p.l, p.i]).collect::<Vec<_>>(), "cutting": self.cutting})
    }

    /// A saved view, checked against the log: it must tile the chat from message 0 on, every
    /// part above level 0 built. Messages logged after it was saved (a crash between the two
    /// writes) are appended as their own lines. None if it does not fit this log.
    pub fn from_json(v: &serde_json::Value, s: &Store) -> Option<View> {
        let mut parts = Vec::new();
        let mut at = 0;
        for x in v["parts"].as_array()? {
            let p = Part { l: x[0].as_u64()? as usize, i: x[1].as_u64()? as usize };
            if p.start() != at || p.end() > s.t() || (p.l > 0 && !s.built(p.l, p.i)) { return None }
            at = p.end();
            parts.push(p);
        }
        for i in at..s.t() { parts.push(Part { l: 0, i }); }
        Some(View { parts, cutting: v["cutting"].as_bool().unwrap_or(false) })
    }
}

/// Where the saved views live: `chat/view.json`, beside `chat/main/` and `chat/tree/`.
pub fn path(dir: &std::path::Path) -> std::path::PathBuf { dir.join("chat").join("view.json") }

/// Save the chat's view and the compactions' view, whole, by write-then-rename.
pub fn save(dir: &std::path::Path, view: &View, cview: &View) -> std::io::Result<()> {
    let p = path(dir);
    if let Some(d) = p.parent() { std::fs::create_dir_all(d)?; }
    let tmp = p.with_extension("json.tmp");
    let body = serde_json::json!({"view": view.to_json(), "compact": cview.to_json()}).to_string();
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &p)
}

/// The views at start: loaded from `view.json`; folded from the log only when there is none
/// that fits it (the first start of a chat, or a file that does not match this log), with a
/// note saying so. The compactions' view, if missing, is cut from the chat's view (§4).
pub fn load(dir: &std::path::Path, s: &Store, budget: usize) -> (View, View, Option<String>) {
    let saved = std::fs::read_to_string(path(dir)).ok().and_then(|b| serde_json::from_str::<serde_json::Value>(&b).ok());
    let (view, note) = match saved.as_ref().and_then(|v| View::from_json(&v["view"], s)) {
        Some(mut v) => { v.fit(s, budget); (v, None) }
        None => (View::fold(s, budget), Some(if saved.is_some() { "view.json does not fit the log: view folded again" } else { "no view.json: view folded from the log" }.to_string())),
    };
    let cview = match saved.as_ref().and_then(|v| View::from_json(&v["compact"], s)) {
        Some(mut c) if note.is_none() => { c.fit(s, super::CVIEW); c }
        _ => compaction_view(&view, s),
    };
    (view, cview, note)
}

/// The compactions' view cut from the chat's (§4 of the gist): the same lines, merged further
/// by the same rule down to half of CVIEW.
pub fn compaction_view(view: &View, s: &Store) -> View {
    let mut c = view.clone();
    c.cutting = true;
    c.fit(s, super::CVIEW);
    c
}

/// Byte offsets where `s` is cut into cache blocks (§8): just after every BLOCK-th `\n`.
/// Only whole blocks are cut; what follows the last offset is the partial block.
pub fn cuts(s: &str) -> Vec<usize> {
    s.match_indices('\n').enumerate().filter(|(k, _)| (k + 1) % BLOCK == 0).map(|(_, (o, _))| o + 1).collect()
}

/// `s` split at `cuts`: its whole blocks, then the partial block if any.
pub fn pieces(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut a = 0;
    for c in cuts(s) { out.push(&s[a..c]); a = c; }
    if a < s.len() { out.push(&s[a..]); }
    out
}

/// Which of `pieces(s)` carry a cache mark: the last whole block and the last piece (§8).
pub fn marked(s: &str) -> Vec<bool> {
    let n = pieces(s).len();
    let whole = cuts(s).len();
    (0..n).map(|k| k + 1 == whole || k + 1 == n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::optchat::store::Store;

    fn store(n: usize, len: usize) -> Store {
        let d = std::env::temp_dir().join(format!("facet-view-{}-{}-{}", n, len, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let mut s = Store::open(&d);
        for i in 0..n { s.msgs.push(crate::optchat::store::Msg { kind: "user".into(), text: format!("{:0>w$}", i, w = len), date: String::new() }); }
        s
    }
    /// build every node, bottom-up, with a fixed-size text
    fn build_all(s: &mut Store, size: usize) {
        let t = s.t();
        let mut l = 0;
        while (1 << l) <= t {
            for i in 0..(t >> l) { s.set(l, i, "y".repeat(size)); }
            l += 1;
        }
    }
    fn tiles(v: &View, t: usize) -> bool {
        let mut at = 0;
        for p in &v.parts { if p.start() != at { return false } at = p.end(); }
        at == t
    }

    #[test]
    fn fold_tiles_fits_and_coarsens_with_age() {
        let mut s = store(3000, 10);
        build_all(&mut s, 100);
        let v = View::fold(&s, 20_000);
        assert!(tiles(&v, 3000));
        assert!(v.size(&s) <= 20_000);
        // older parts are at least as coarse as newer ones, roughly: the first is the coarsest
        assert!(v.parts[0].l >= v.parts.last().unwrap().l);
        assert_eq!(v.parts.last().unwrap().l, 0);
    }

    #[test]
    fn live_equals_fold_and_never_splits() {
        let mut s = store(2000, 10);
        build_all(&mut s, 100);
        // grow live: fold of the first 1000, then append the rest one by one
        let mut v = View::default();
        let mut prev: Vec<Part> = Vec::new();
        for i in 0..2000 {
            v.parts.push(Part { l: 0, i });
            let size = v.size(&s);
            v.fit_from(&s, 15_000, size, i + 1);
            // never split: every previous part is inside some current part
            for p in &prev {
                assert!(v.parts.iter().any(|q| q.start() <= p.start() && p.end() <= q.end()));
            }
            prev = v.parts.clone();
        }
        let f = View::fold(&s, 15_000);
        assert_eq!(f.parts, v.parts);
    }

    #[test]
    fn unbuilt_parents_are_passed_over() {
        let mut s = store(8, 10);
        for i in 0..8 { s.put(0, i, &"z".repeat(100)).unwrap(); }
        // budget forces merges, but no parent is built: the view stays over budget
        let mut v = View::fold(&s, 300);
        assert_eq!(v.parts.len(), 8);
        s.put(1, 2, &"p".repeat(50)).unwrap();
        assert!(v.fit(&s, 300));
        assert_eq!(v.parts.len(), 7);
        assert_eq!(v.parts[4], Part { l: 1, i: 2 });
    }

    #[test]
    fn batch_cuts_to_half_and_resumes_when_short() {
        let mut s = store(8, 10);
        for i in 0..8 { s.put(0, i, &"z".repeat(100)).unwrap(); }
        let mut v = View::fold(&s, 750); // 800 > 750: a batch starts, nothing to merge yet
        assert!(v.cutting);
        assert_eq!(v.parts.len(), 8);
        s.put(1, 0, &"p".repeat(50)).unwrap();
        assert!(v.fit(&s, 750)); // one merge, 650 bytes, still over 375: the batch goes on
        assert!(v.cutting && v.size(&s) == 650);
        for i in 1..4 { s.put(1, i, &"p".repeat(50)).unwrap(); }
        s.put(2, 0, &"q".repeat(50)).unwrap();
        assert!(v.fit(&s, 750)); // under its budget, but the batch runs on down to half
        assert!(v.size(&s) <= 375 && !v.cutting);
        // most due first, (T - last) / 2^l: 2+2 (due 5), then 4+2 (3) over 0+4 (2.5)
        let p = |l, i| Part { l, i };
        assert_eq!(v.parts, vec![p(1, 0), p(1, 1), p(1, 2), p(0, 6), p(0, 7)]);
        assert!(!v.fit(&s, 750)); // and then nothing until the budget is passed again
    }

    #[test]
    fn placeholder_render_and_first() {
        let mut s = store(3, 1);
        s.put(0, 0, "user: 0").unwrap();
        let v = View::fold(&s, 1000);
        assert!(!v.settled(&s));
        assert_eq!(v.render(&s), format!("<chat>\n0+1|user: 0\n1+1|{}\n2+1|{}\n</chat>", PLACEHOLDER, PLACEHOLDER));
        s.put(0, 1, "a\nb").unwrap();
        assert_eq!(v.context(&s, 2), "<chat>\n0+1|user: 0\n1+1|a b\n");
        // it stops at the first unbuilt line, and at the node's end
        assert_eq!(v.context(&s, 3), "<chat>\n0+1|user: 0\n1+1|a b\n");
        assert_eq!(v.context(&s, 1), "<chat>\n0+1|user: 0\n");
    }

    #[test]
    fn saved_view_loads_as_it_was_never_refolded() {
        let mut s = store(40, 10);
        build_all(&mut s, 100);
        let d = s.dir.clone();
        std::fs::create_dir_all(&d).unwrap();
        // a live view that differs from a fold: grown with a smaller budget, then saved
        let live = View::fold(&s, 1_500);
        let c = compaction_view(&live, &s);
        save(&d, &live, &c).unwrap();
        let (v, cv, note) = load(&d, &s, 3_000);
        assert!(note.is_none());
        assert_eq!(v.parts, live.parts);
        assert_eq!(cv.parts, c.parts);
        assert_ne!(View::fold(&s, 3_000).parts, live.parts);
        // messages logged after the save come in as their own lines
        s.msgs.push(crate::optchat::store::Msg { kind: "user".into(), text: "new".into(), date: String::new() });
        let (v2, _, _) = load(&d, &s, 3_000);
        assert_eq!(v2.parts.last(), Some(&Part { l: 0, i: 40 }));
        assert_eq!(&v2.parts[..v2.parts.len() - 1], &live.parts[..]);
        // a file that does not fit the log is folded again, and says so
        std::fs::write(path(&d), r#"{"view":{"parts":[[0,5]]}}"#).unwrap();
        assert!(load(&d, &s, 3_000).2.is_some());
        // no file: folded
        std::fs::remove_file(path(&d)).unwrap();
        let (f, fc, n) = load(&d, &s, 3_000);
        assert!(n.is_some() && tiles(&f, 41) && tiles(&fc, 41));
    }

    #[test]
    fn compaction_view_is_the_chat_view_merged_to_half_of_cview() {
        let mut s = store(3000, 10);
        build_all(&mut s, 100);
        let v = View::fold(&s, 128_000);
        let c = compaction_view(&v, &s);
        assert!(tiles(&c, 3000));
        assert!(c.size(&s) <= crate::optchat::CVIEW / 2 && !c.cutting);
        // every line of it is a line of the view or a merge of some
        for p in &v.parts { assert!(c.parts.iter().any(|q| q.start() <= p.start() && p.end() <= q.end())); }
    }

    #[test]
    fn blocks_of_four_lines_marked_at_last_whole_and_end() {
        let s = "a\nb\nc\nd\ne\nf\ng\nh\ni\n</chat>";
        assert_eq!(cuts(s), vec![8, 16]);
        assert_eq!(pieces(s), vec!["a\nb\nc\nd\n", "e\nf\ng\nh\n", "i\n</chat>"]);
        assert_eq!(marked(s), vec![false, true, true]);
        assert_eq!(pieces(s).concat(), s);
        // ends on a whole block: that block is both the last whole block and the end
        let w = "é\n2\n3\n4\n5\n6\n7\n8\n";
        assert_eq!(cuts(w), vec![9, 17]);
        assert!(cuts(w).iter().all(|&k| w.as_bytes()[k - 1] == b'\n'));
        assert_eq!(marked(w), vec![false, true]);
        // shorter than a block: one piece, marked as the end
        assert_eq!(pieces("x\ny\n"), vec!["x\ny\n"]);
        assert_eq!(marked("x\ny\n"), vec![true]);
        // appending lines never moves an earlier cut: consecutive calls share their blocks
        let longer = format!("{}j\nk\n", &s[..s.len() - "</chat>".len()]);
        assert_eq!(cuts(&longer)[..2], cuts(s)[..]);
    }
}
