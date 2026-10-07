// §5 The view: a list of tree nodes that tiles the chat [0, T), oldest first, under a byte
// budget. It only ever appends at the end and coarsens (never splits), so consecutive views
// share a long prefix, which is what makes them cacheable (§8).
use super::store::Store;
use super::{flat, MARKS};

pub const PLACEHOLDER: &str = "(not summarized yet: zoom it)";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Part { pub l: usize, pub i: usize }

impl Part {
    pub fn start(&self) -> usize { self.i << self.l }
    pub fn n(&self) -> usize { 1 << self.l }
    pub fn end(&self) -> usize { (self.i + 1) << self.l }
}

#[derive(Default, Clone)]
pub struct View { pub parts: Vec<Part> }

fn text<'a>(s: &'a Store, p: &Part) -> &'a str { s.node(p.l, p.i).unwrap_or(PLACEHOLDER) }

impl View {
    /// At load the view is not read from disk: it is folded again from message 0 (§5.2).
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

    /// On a new message: append its part, then fit.
    pub fn append(&mut self, s: &Store, i: usize, budget: usize) {
        self.parts.push(Part { l: 0, i });
        self.fit(s, budget);
    }

    /// Past the outer limit, replace the most due mergeable pair by its parent until back at
    /// the budget. A pair whose parent is not built yet is passed over; if none is built, stay
    /// over budget (§5.2).
    pub fn fit(&mut self, s: &Store, budget: usize) -> bool {
        let before = self.parts.len();
        let size = self.size(s);
        self.fit_from(s, budget, size, s.t());
        self.parts.len() != before
    }

    fn fit_from(&mut self, s: &Store, budget: usize, mut size: usize, t: usize) -> usize {
        // Two limits: nothing collapses until the view has drifted past the outer one, and then
        // it collapses all the way back to the budget, so the marked prefix holds still in
        // between and the compactor's cache survives the appends (§8, `over`).
        if size <= super::over(budget) { return size }
        while size > budget {
            let mut best: Option<usize> = None;
            for k in 0..self.parts.len().saturating_sub(1) {
                let (a, b) = (self.parts[k], self.parts[k + 1]);
                if a.l != b.l || a.i % 2 != 0 || b.i != a.i + 1 || !s.built(a.l + 1, a.i / 2) { continue }
                // due = (T - start) / 2^(l+2); compare exactly, keep the first of equals
                best = match best {
                    Some(j) => {
                        let c = self.parts[j];
                        let lhs = ((t - a.start()) as u128) << (c.l + 2);
                        let rhs = ((t - c.start()) as u128) << (a.l + 2);
                        if lhs > rhs { Some(k) } else { Some(j) }
                    }
                    None => Some(k),
                };
            }
            let Some(k) = best else { break };
            let (a, b) = (self.parts[k], self.parts[k + 1]);
            let up = Part { l: a.l + 1, i: a.i / 2 };
            size = size - text(s, &a).len() - text(s, &b).len() + text(s, &up).len();
            self.parts[k] = up;
            self.parts.remove(k + 1);
        }
        size
    }

    /// Every line is a summary: the condition for any call to start (§6).
    pub fn settled(&self, s: &Store) -> bool { self.parts.iter().all(|p| s.built(p.l, p.i)) }

    /// First message whose view line is unbuilt (§4.1 `first`).
    pub fn first(&self, s: &Store) -> usize {
        self.parts.iter().find(|p| !s.built(p.l, p.i)).map(|p| p.start()).unwrap_or(s.t())
    }

    /// What every agent call sees: `id+n|text` per line, inside <chat> tags (§5.1).
    pub fn render(&self, s: &Store) -> String {
        let mut out = String::from("<chat>\n");
        for p in &self.parts {
            out.push_str(&format!("{}+{}|{}\n", p.start(), p.n(), flat(text(s, p))));
        }
        out.push_str("</chat>");
        out
    }

    /// The compactor's context: bare lines, no ids (§4.2), for the parts `keep` selects.
    /// The closing tag is not included: see compact.rs.
    pub fn bare(&self, s: &Store, keep: impl Fn(&Part) -> bool) -> String {
        let mut out = String::from("<chat>\n");
        for p in self.parts.iter().filter(|p| keep(p)) {
            out.push_str(&flat(text(s, p)));
            out.push('\n');
        }
        out
    }
}

/// Byte offsets where `s` is cut for the cache marks: at the last line end before each of
/// MARKS characters, skipping a mark past the end (§8). Each offset is just after a `\n`.
pub fn cuts(s: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut chars = 0;
    let mut last_nl = None;
    let mut m = 0;
    for (off, c) in s.char_indices() {
        if m < MARKS.len() && chars == MARKS[m] {
            if let Some(nl) = last_nl { if out.last() != Some(&nl) { out.push(nl) } }
            m += 1;
        }
        chars += 1;
        if c == '\n' { last_nl = Some(off + 1); }
    }
    out
}

/// `s` split at `cuts`.
pub fn pieces(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut a = 0;
    for c in cuts(s) { out.push(&s[a..c]); a = c; }
    if a < s.len() { out.push(&s[a..]); }
    out
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
        assert!(v.size(&s) <= crate::optchat::over(20_000)); // the outer limit, not the budget
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
    fn placeholder_render_and_first() {
        let mut s = store(3, 1);
        s.put(0, 0, "user: 0").unwrap();
        let v = View::fold(&s, 1000);
        assert!(!v.settled(&s));
        assert_eq!(v.first(&s), 1);
        assert_eq!(v.render(&s), format!("<chat>\n0+1|user: 0\n1+1|{}\n2+1|{}\n</chat>", PLACEHOLDER, PLACEHOLDER));
        s.put(0, 1, "a\nb").unwrap();
        assert_eq!(v.bare(&s, |p| p.end() <= 2), "<chat>\nuser: 0\na b\n");
    }

    #[test]
    fn cut_at_line_ends() {
        let line = format!("{}\n", "a".repeat(99)); // 100 chars per line
        let s = line.repeat(MARKS[2] / 100 + 100); // past the last mark
        let c = cuts(&s);
        // every mark is a whole number of lines in, so each cut lands on the mark itself
        assert_eq!(c, MARKS.iter().map(|m| m / 100 * 100).collect::<Vec<_>>());
        let s2 = format!("é{}", s); // one 2-byte char shifts lines: cut before the line crossing the mark
        let c2 = cuts(&s2);
        assert_eq!(c2, MARKS.iter().map(|m| (m - 1) / 100 * 100 + 2).collect::<Vec<_>>());
        assert!(c2.iter().all(|&k| s2.as_bytes()[k - 1] == b'\n'));
        assert_eq!(pieces(&s).concat(), s);
        assert_eq!(cuts(&"x\n".repeat(100)), Vec::<usize>::new());
        assert_eq!(pieces(&"short\n".to_string()).len(), 1);
    }
}
