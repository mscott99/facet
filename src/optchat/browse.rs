// §10 Browsing: the whole memory as one self-contained HTML page, nested like a comment tree.
// The view tiles the log, so its lines are the tops of the summarized threads: each opens into
// the two nodes it was made from, down to the messages. Above the view nothing is summarized
// (a node is built only when the view needs it), so the levels above it are scaffolding: an
// entry with no text, only the halves it would be made from, which gives the page one root.
// A scaffold level with a single half is skipped. Entries with no text of their own (scaffold,
// or a summary still pending) start open: there is nothing to read in their line.
use super::store::Store;
use super::view::{View, PLACEHOLDER};
use super::flat;
use std::collections::HashSet;
use std::fmt::Write;

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c { '&' => o.push_str("&amp;"), '<' => o.push_str("&lt;"), '>' => o.push_str("&gt;"),
                  '"' => o.push_str("&quot;"), '\'' => o.push_str("&#39;"), c => o.push(c) }
    }
    o
}
fn size(b: usize) -> String { if b < 1000 { format!("{} B", b) } else { format!("{:.1} kB", b as f64 / 1000.0) } }
fn local(iso: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(iso)
        .map(|d| d.with_timezone(&chrono::Local).format("%b %-d %H:%M").to_string())
        .unwrap_or_default()
}

struct B<'a> { s: &'a Store, view: HashSet<(usize, usize)>, t: usize, out: String }

impl B<'_> {
    fn reach(&self, l: usize, i: usize) -> (usize, usize) { let id = i << l; (id, (1usize << l).min(self.t - id)) }
    fn when(&self, id: usize, n: usize) -> String {
        let (a, b) = (local(&self.s.msgs[id].date), local(&self.s.msgs[id + n - 1].date));
        if a == b { a } else { format!("{} → {}", a, b) }
    }
    fn halves(&self, l: usize, i: usize) -> Vec<(usize, usize)> {
        [(l - 1, 2 * i), (l - 1, 2 * i + 1)].into_iter().filter(|&(l, i)| (i << l) < self.t).collect()
    }
    fn head(&mut self, l: usize, i: usize, cls: &str, text: &str, tail: &str, open: bool) {
        let (id, n) = self.reach(l, i);
        let when = self.when(id, n);
        let _ = write!(self.out, "<details{} class=\"n {}\"><summary><code>{}+{}</code> <span class=m>{} · {}</span> <span class=t>{}</span></summary>",
            if open { " open" } else { "" }, cls, id, n, esc(&when), tail, esc(&flat(text)));
    }
    /// A node that has a summary (or is a view line): its line, then what it was made from.
    /// `top` marks a node with no summarized ancestor above it - the frontier a "collapse all"
    /// should land on: closing exactly these nodes still covers the whole chat, maximally summarized.
    fn thread(&mut self, l: usize, i: usize, open: bool, top: bool) {
        let top_cls = if top { " top" } else { "" };
        if l == 0 {
            let m = &self.s.msgs[i];
            let (kind, text) = (m.kind.clone(), m.text.clone());
            let last = if i + 1 == self.t { " last" } else { "" };
            let tail = format!("{} · <b>{}</b>", size(kind.len() + 2 + text.len()), kind);
            self.head(0, i, &format!("{}{}{}", kind, last, top_cls), &text, &tail, open);
            let _ = write!(self.out, "<div class=b>{}</div></details>", esc(&text));
            return;
        }
        let node = self.s.node(l, i).map(String::from);
        let (_, n) = self.reach(l, i);
        let text = node.clone().unwrap_or_else(|| PLACEHOLDER.into());
        let tail = format!("{} · {} messages", size(text.len()), n);
        self.head(l, i, &format!("{}{}", if node.is_some() { "sum" } else { "sum sc" }, top_cls), &text, &tail, open || node.is_none());
        // Below a node that has its own summary, nothing further is top-level: its ancestor
        // already covers it, so its halves (even if themselves built) are not the frontier.
        for (a, b) in self.halves(l, i) { self.walk(a, b, false, false); }
        self.out.push_str("</details>");
    }
    fn walk(&mut self, l: usize, i: usize, open: bool, top: bool) {
        if l == 0 || self.view.contains(&(l, i)) || self.s.built(l, i) { return self.thread(l, i, open, top) }
        let kids = self.halves(l, i);
        if kids.len() == 1 { return self.walk(kids[0].0, kids[0].1, open, top) }
        let (_, n) = self.reach(l, i);
        self.head(l, i, "sc", "(not summarized: opens into its halves)", &format!("{} messages", n), true);
        // Pure scaffolding (no summary of its own): top-ness passes through unchanged.
        for (a, b) in kids { self.walk(a, b, false, top); }
        self.out.push_str("</details>");
    }
}

/// The page. `back` is a link shown at the top when served from the web route.
pub fn html(s: &Store, v: &View, budget: usize, back: Option<&str>) -> String {
    let t = s.t();
    let mut b = B { s, view: v.parts.iter().map(|p| (p.l, p.i)).collect(), t, out: String::new() };
    if t == 0 { b.out.push_str("<p class=s>empty</p>") } else {
        let mut top = 0;
        while (1usize << top) < t { top += 1; }
        b.walk(top, 0, true, true);
    }
    let nodes: usize = s.levels.iter().map(|l| l.iter().filter(|x| x.is_some()).count()).sum();
    let back = back.map(|h| format!("<a class=back href=\"{}\">← home</a> ", esc(h))).unwrap_or_default();
    format!("<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>Memory</title><style>{CSS}</style>
<h1>{back}Memory</h1>
<p class=s>{t} messages · {nodes} tree nodes · view {vl} lines, {vb} of {budget} bytes</p>
<p class=s>one root; each entry opens into the two halves it is (or would be) summarized from, down to the messages; entries with no summary of their own start open</p>
<div class=bar><input id=q placeholder=\"search summaries and messages (Esc clears)\"><button id=x>expand all</button><button id=c>collapse</button><button id=n>newest</button><span id=h></span></div>
<div id=tree>{tree}</div>
<script>{JS}</script>
", vl = v.parts.len(), vb = v.size(s), tree = b.out)
}

const CSS: &str = ":root{color-scheme:light dark}
body{font:14px/1.5 -apple-system,system-ui,ui-sans-serif,sans-serif;margin:0 auto;max-width:1000px;padding:1rem;background:Canvas;color:CanvasText}
h1{font-size:1.05rem;margin:0}a.back{font-weight:normal;font-size:.9rem;color:#79a7d3;text-decoration:none}
p.s{margin:.15rem 0;color:#888;font:12px/1.45 ui-monospace,monospace}
.bar{display:flex;flex-wrap:wrap;gap:.4rem;align-items:center;margin:.8rem 0 .4rem;position:sticky;top:0;background:Canvas;padding:.45rem 0;border-bottom:1px solid #8884;z-index:1}
input{flex:1;min-width:10rem;padding:.3rem .5rem;font:inherit;color:inherit;background:Canvas;border:1px solid #8886;border-radius:5px}
button{font:inherit;padding:.3rem .55rem;color:inherit;background:transparent;border:1px solid #8886;border-radius:5px;cursor:pointer}
button:hover{background:#8881}
#h{font:12px ui-monospace,monospace;color:#888;white-space:nowrap}
details.n{margin:.1rem 0}
details.n>details.n{margin-left:.5rem;border-left:1px solid #8883;padding-left:.5rem}
details.n>details.n:hover{border-left-color:#8886}
summary{cursor:pointer;padding:.1rem .2rem;border-radius:4px;overflow-wrap:anywhere}
summary:hover{background:#8881}
summary>code{font-size:11px;color:#79a7d3}
.m{font-size:11px;color:#888;white-space:nowrap}
.sc>summary>.t{color:#888;font-style:italic}
.b{white-space:pre-wrap;word-break:break-word;font:12px/1.5 ui-monospace,monospace;margin:.25rem 0 .5rem;padding:.4rem .6rem;background:#8881;border-left:2px solid #8886;border-radius:0 4px 4px 0}
.hit>summary{background:#f9c74f40;outline:1px solid #f9c74f80}
details.user>summary b{color:#4ea1ff}details.talk>summary b{color:#5cb87a}
details.tool>summary b{color:#c9a227}details.echo>summary b{color:#999}details.note>summary b{color:#c678dd}";

const JS: &str = "const $=s=>document.querySelectorAll(s),T='#tree details';
const all=o=>$(T).forEach(d=>d.open=o);
const up=d=>{for(let p=d;p;p=p.parentElement)if(p.tagName==='DETAILS')p.open=true;};
document.getElementById('x').onclick=()=>all(true);
document.getElementById('c').onclick=()=>{all(false);$('.top').forEach(d=>up(d.parentElement));};
document.getElementById('n').onclick=()=>{const l=document.querySelector('.last');if(l){up(l);l.scrollIntoView({block:'center'});}};
const q=document.getElementById('q'),h=document.getElementById('h');
const search=()=>{
  const s=q.value.trim().toLowerCase();
  $('.hit').forEach(d=>d.classList.remove('hit'));
  if(!s){h.textContent='';return;}
  let n=0,first=null;
  $(T).forEach(d=>{
    const own=d.querySelector(':scope>summary').textContent,body=d.querySelector(':scope>.b');
    if(!(own+(body?body.textContent:'')).toLowerCase().includes(s))return;
    n++;d.classList.add('hit');if(!first)first=d;up(d.parentElement);
  });
  h.textContent=n+(n===1?' match':' matches');
  if(first)first.scrollIntoView({block:'center'});
};
q.oninput=search;
q.onkeydown=e=>{if(e.key==='Escape'){q.value='';search();}};";

#[cfg(test)]
mod tests {
    #[test]
    fn one_root_every_message_escaped() {
        let d = std::env::temp_dir().join(format!("facet-browse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let mut s = crate::optchat::store::Store::open(&d);
        for k in 0..5 { s.log("user", &format!("msg {} <b>", k)).unwrap(); }
        for k in 0..5 { s.put(0, k, &format!("user: msg {}", k)).unwrap(); }
        s.put(1, 0, "pair one").unwrap();
        let v = crate::optchat::view::View::fold(&s, 100_000);
        let h = super::html(&s, &v, 100_000, Some("/tok/"));
        for k in 0..5 { assert!(h.contains(&format!("msg {} &lt;b&gt;", k))); }
        assert!(!h.contains("msg 0 <b>"));
        assert_eq!(h.matches("<details").count(), h.matches("</details>").count());
        assert!(h.contains("<code>0+8</code>") || h.contains("<code>0+5</code>"), "one root");
        assert!(h.contains("class=\"n user last\"") || h.contains("class=\"n user last top\""));
        assert!(h.contains("pair one"));
    }
}
