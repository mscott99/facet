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

struct B<'a> { s: &'a Store, view: HashSet<(usize, usize)>, t: usize, out: String, lazy: bool }

impl B<'_> {
    fn reach(&self, l: usize, i: usize) -> (usize, usize) { let id = i << l; (id, (1usize << l).min(self.t - id)) }
    fn when(&self, id: usize, n: usize) -> String {
        let (a, b) = (local(&self.s.msgs[id].date), local(&self.s.msgs[id + n - 1].date));
        if a == b { a } else { format!("{} → {}", a, b) }
    }
    fn halves(&self, l: usize, i: usize) -> Vec<(usize, usize)> {
        [(l - 1, 2 * i), (l - 1, 2 * i + 1)].into_iter().filter(|&(l, i)| (i << l) < self.t).collect()
    }
    // `data-l`/`data-i` ride on every node, lazy or not: the lazy page's fetches, and a search
    // hit's jump to where it was found, both locate a node by exactly this pair.
    fn head(&mut self, l: usize, i: usize, cls: &str, text: &str, tail: &str, open: bool) {
        let (id, n) = self.reach(l, i);
        let when = self.when(id, n);
        let _ = write!(self.out, "<details{} class=\"n {}\" data-l=\"{}\" data-i=\"{}\"><summary><code>{}+{}</code> <span class=m>{} · {}</span> <span class=t>{}</span></summary>",
            if open { " open" } else { "" }, cls, l, i, id, n, esc(&when), tail, esc(&flat(text)));
    }
    /// A node that has a summary (or is a view line): its line, then what it was made from.
    /// `top` marks a node with no summarized ancestor above it - the frontier a "collapse all"
    /// should land on: closing exactly these nodes still covers the whole chat, maximally summarized.
    fn thread(&mut self, l: usize, i: usize, open: bool, top: bool) {
        // the frontier is exactly the view's own lines, not whatever built node a walk meets first
        let _ = top;
        let top = self.view.contains(&(l, i));
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
        let above = self.view.iter().any(|&(pl, pi)| pl < l && (pi >> (l - pl)) == i);
        // an older summary the view has since split is only the way down to the view's lines on
        // this page: its text is not shipped
        let text = if self.lazy && above { "(older summary, split in the view)".to_string() }
            else { node.clone().unwrap_or_else(|| PLACEHOLDER.into()) };
        // In lazy mode every node reached here (anything above a leaf) is where this render
        // pass stops: its halves are left unwritten, fetched later by `/f/node` rather than
        // shipped now. It always renders closed - there is nothing under it yet to show open.
        let tail = format!("{} · {} messages{}", size(node.as_ref().map(|x| x.len()).unwrap_or(text.len())), n, if self.lazy { " · ···" } else { "" });
        let cls = format!("{}{}{}", if node.is_some() { "sum" } else { "sum sc" }, top_cls, if self.lazy && !above { " stub" } else { "" });
        // a built node above the view (older summary the view has since split) has to be walked
        // through even in lazy mode, or the view lines under it would never be on the page
        self.head(l, i, &cls, &text, &tail, (open || node.is_none() || above) && (!self.lazy || above));
        if self.lazy && above {
            for (a, b) in self.halves(l, i) { self.walk(a, b, false, false); }
        } else if self.lazy {
            // children left for a fetch to bring in
        } else {
            // Below a node that has its own summary, nothing further is top-level: its ancestor
            // already covers it, so its halves (even if themselves built) are not the frontier.
            for (a, b) in self.halves(l, i) { self.walk(a, b, false, false); }
        }
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
    let mut b = B { s, view: v.parts.iter().map(|p| (p.l, p.i)).collect(), t, out: String::new(), lazy: false };
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
<div class=bar><input id=q placeholder=\"search summaries and messages (Esc clears)\"><button id=c>view</button><button id=n>newest</button><span id=h></span></div>
<div id=tree>{tree}</div>
<script>{JS}</script>
", vl = v.parts.len(), vb = v.size(s), tree = b.out)
}

/// The web route's own tree: the scaffold down to the view-line frontier (the `top` nodes)
/// and no further. Everything below a frontier node is a stub - `class=stub`, carrying its
/// own `data-l`/`data-i` - left for `/f/node` to fetch the first time it is opened. Keeps
/// `html` above untouched: `facet browse`'s standalone file still renders every message.
pub fn web(s: &Store, v: &View, budget: usize, prefix: &str) -> String {
    let t = s.t();
    let mut b = B { s, view: v.parts.iter().map(|p| (p.l, p.i)).collect(), t, out: String::new(), lazy: true };
    if t == 0 { b.out.push_str("<p class=s>empty</p>") } else {
        let mut top = 0;
        while (1usize << top) < t { top += 1; }
        b.walk(top, 0, true, true);
    }
    let nodes: usize = s.levels.iter().map(|l| l.iter().filter(|x| x.is_some()).count()).sum();
    let js = JS_WEB.replace("__BASE__", prefix).replace("__T__", &t.to_string());
    format!("<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>Memory</title><style>{CSS}{WEB_CSS}</style>
<h1><a class=back href=\"{prefix}/\">← home</a> Memory</h1>
<p class=s>{t} messages · {nodes} tree nodes · view {vl} lines, {vb} of {budget} bytes · loaded as you open it</p>
<p class=s>one root; each entry opens into the two halves it is (or would be) summarized from, down to the messages; a line ending in <code>···</code> fetches its halves the first time it opens</p>
<div class=bar><input id=q placeholder=\"search everything, loaded or not (Esc clears)\"><button id=c>view</button><button id=n>newest</button></div>
<div id=h></div>
<div id=tree>{tree}</div>
<script>{js}</script>
", vl = v.parts.len(), vb = v.size(s), tree = b.out)
}

/// What a stub's first open fetches: `(l, i)`'s two halves, each stubbed one level further if
/// it has halves of its own. No page chrome - this is spliced straight inside the stub's own
/// `<details>`, reusing exactly the rendering `html`/`web` already do.
pub fn node(s: &Store, v: &View, l: usize, i: usize) -> String {
    let t = s.t();
    let mut b = B { s, view: v.parts.iter().map(|p| (p.l, p.i)).collect(), t, out: String::new(), lazy: true };
    let kids = b.halves(l, i);
    for (a, bi) in kids { b.walk(a, bi, false, false); }
    b.out
}

/// Server-side search: the client only ever has the frontier loaded, so a search has to reach
/// past that into every message and every built summary, case-insensitively, char by char
/// (byte offsets drift under `to_lowercase`, so matching stays off `char` vectors throughout).
/// Capped at 200 hits; each carries the `(l, i)` a click on it hands to `reveal` in `JS_WEB`.
fn ci_find(hay: &str, needle_lower: &[char]) -> Option<usize> {
    if needle_lower.is_empty() { return None }
    let hay_c: Vec<char> = hay.chars().collect();
    if hay_c.len() < needle_lower.len() { return None }
    'outer: for start in 0..=(hay_c.len() - needle_lower.len()) {
        for (k, want) in needle_lower.iter().enumerate() {
            let have = hay_c[start + k].to_lowercase().next().unwrap_or(hay_c[start + k]);
            if have != *want { continue 'outer }
        }
        return Some(start);
    }
    None
}
fn snippet(text: &str, at: usize, qlen: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let start = at.saturating_sub(40);
    let end = (at + qlen + 80).min(chars.len());
    flat(&chars[start..end].iter().collect::<String>())
}
pub fn find(s: &Store, q: &str) -> String {
    let q_lower: Vec<char> = q.trim().to_lowercase().chars().collect();
    if q_lower.is_empty() { return String::new() }
    let mut hits: Vec<(usize, usize, String, String, String)> = Vec::new();
    for (i, m) in s.msgs.iter().enumerate() {
        if hits.len() >= 200 { break }
        if let Some(at) = ci_find(&m.text, &q_lower) {
            hits.push((0, i, format!("{}+1", i), format!("{} · {}", local(&m.date), m.kind), snippet(&m.text, at, q_lower.len())));
        }
    }
    'levels: for (l, lv) in s.levels.iter().enumerate() {
        if l == 0 { continue }
        for (i, slot) in lv.iter().enumerate() {
            if hits.len() >= 200 { break 'levels }
            let Some(text) = slot else { continue };
            if let Some(at) = ci_find(text, &q_lower) {
                let id = i << l;
                let n = (1usize << l).min(s.t().saturating_sub(id));
                hits.push((l, i, format!("{}+{}", id, n), "summary".into(), snippet(text, at, q_lower.len())));
            }
        }
    }
    if hits.is_empty() { return "<p class=s>no matches</p>".into() }
    let mut out = String::from("<ul class=hits>");
    for (l, i, addr, meta, snip) in hits {
        let _ = write!(out, "<li data-l=\"{}\" data-i=\"{}\"><code>{}</code> <span class=m>{}</span> — {}</li>", l, i, addr, esc(&meta), esc(&snip));
    }
    out.push_str("</ul>");
    out
}

// Dark and quiet, like the rest of the pages. This one is a dense data view, so it keeps a
// sans face and its monospace addresses: the `id+n` of a line is what a zoom is written from,
// and is the only thing here carrying the accent.
const CSS: &str = ":root{color-scheme:dark;--bg:#15161a;--fg:#bdbcb8;--dim:#75767a;--line:#272930;--acc:#8fa8c8;
 --mono:ui-monospace,SFMono-Regular,Menlo,monospace}
body{font:14px/1.6 -apple-system,system-ui,ui-sans-serif,sans-serif;margin:0 auto;max-width:62rem;padding:1.1rem;background:var(--bg);color:var(--fg)}
h1{font-size:1.05rem;font-weight:600;margin:0}
a.back{font-weight:normal;font-size:.9rem;color:var(--acc);text-decoration:none}
p.s{margin:.15rem 0;color:var(--dim);font:12px/1.5 var(--mono)}
.bar{display:flex;flex-wrap:wrap;gap:.9rem;align-items:center;margin:.9rem 0 .5rem;position:sticky;top:0;background:var(--bg);padding:.5rem 0;z-index:1}
input{flex:1;min-width:10rem;padding:.25rem 0;font:12.5px var(--mono);color:var(--fg);background:none;border:0;border-bottom:1px solid var(--line)}
input:focus{outline:0;border-bottom-color:var(--acc)}
button{font:12.5px var(--mono);padding:0;color:var(--dim);background:none;border:0;cursor:pointer}
button:hover{color:var(--acc)}
#h{font:12px var(--mono);color:var(--dim);white-space:nowrap}
details.n{margin:.1rem 0}
details.n>details.n{margin-left:.5rem;border-left:1px solid var(--line);padding-left:.6rem}
summary{cursor:pointer;padding:.1rem .2rem;border-radius:3px;overflow-wrap:anywhere}
summary:hover{background:#ffffff08}
summary>code{font:11px var(--mono);color:var(--acc)}
.m{font:11px var(--mono);color:var(--dim);white-space:nowrap}
.sc>summary>.t{color:var(--dim);font-style:italic}
.b{white-space:pre-wrap;word-break:break-word;font:12px/1.6 var(--mono);margin:.25rem 0 .6rem;padding:.1rem 0 .1rem .7rem;border-left:1px solid var(--line)}
.hit>summary{background:#b5956626}
details.user>summary b{color:var(--acc)}details.talk>summary b{color:#8fab8f}
details.tool>summary b{color:#b59566}details.echo>summary b{color:var(--dim)}details.note>summary b{color:#a692b5}";

const JS: &str = "const $=s=>document.querySelectorAll(s),T='#tree details';
const all=o=>$(T).forEach(d=>d.open=o);
const up=d=>{for(let p=d;p;p=p.parentElement)if(p.tagName==='DETAILS')p.open=true;};
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

// The one addition over `CSS`: a results list for `/f/find`, and `#h` loosened from the
// one-line match count it was for `html`'s client-side search into something a list fits in.
const WEB_CSS: &str = "
#h{white-space:normal;margin:.2rem 0 .6rem}
ul.hits{list-style:none;margin:0;padding:0;font:12px var(--mono);max-height:18rem;overflow:auto;border:1px solid var(--line)}
ul.hits li{padding:.3rem .5rem;cursor:pointer;border-bottom:1px solid var(--line)}
ul.hits li:last-child{border-bottom:0}
ul.hits li:hover{background:#ffffff08}
ul.hits li>code{color:var(--acc)}
ul.hits li>.m{color:var(--dim)}";

// The web route's script: a stub fetches its own halves the first time it opens (`load`),
// `reveal(l,i)` walks down to an arbitrary node fetching along the way (used by both `newest`
// and a search hit), and search goes to `/f/find` since the page no longer has everything to
// search client-side. `view` closes everything but the path to the view's lines, fetching nothing.
const JS_WEB: &str = "const $=s=>document.querySelectorAll(s),T='#tree details',BASE='__BASE__';
const up=d=>{for(let p=d;p;p=p.parentElement)if(p.tagName==='DETAILS')p.open=true;};
function wire(root){
  root.querySelectorAll('.stub').forEach(function(d){
    if(d.dataset.wired)return;d.dataset.wired='1';
    d.addEventListener('toggle',function(){if(d.open)load(d)});
  });
}
function load(d){
  if(d.dataset.loading||d.dataset.loaded)return Promise.resolve();
  d.dataset.loading='1';
  return fetch(BASE+'/f/node?l='+d.dataset.l+'&i='+d.dataset.i).then(function(r){return r.text()}).then(function(h){
    d.insertAdjacentHTML('beforeend',h);
    d.dataset.loaded='1';delete d.dataset.loading;
    wire(d);
  });
}
wire(document);
document.getElementById('c').onclick=function(){
  $(T).forEach(function(d){d.open=false});
  $('.top').forEach(function(d){up(d.parentElement)});
};
function reveal(l,i){
  return new Promise(function(res){
    (function step(){
      var el=document.querySelector('[data-l=\"'+l+'\"][data-i=\"'+i+'\"]');
      if(el){up(el);el.open=true;el.scrollIntoView({block:'center'});el.classList.add('hit');
        setTimeout(function(){el.classList.remove('hit')},2000);res(el);return;}
      var ts=i<<l,te=ts+(1<<l),target=null;
      $('.stub').forEach(function(d){
        if(target)return;
        var L=+d.dataset.l,I=+d.dataset.i,s=I<<L,e=s+(1<<L);
        if(s<=ts&&te<=e)target=d;
      });
      if(!target){res(null);return;}
      load(target).then(step);
    })();
  });
}
document.getElementById('n').onclick=function(){reveal(0,__T__-1)};
var q=document.getElementById('q'),h=document.getElementById('h'),st=null;
function bindHits(){
  h.querySelectorAll('li').forEach(function(li){
    li.addEventListener('click',function(){reveal(+li.dataset.l,+li.dataset.i)});
  });
}
q.oninput=function(){
  clearTimeout(st);var v=q.value.trim();
  if(!v){h.innerHTML='';return;}
  st=setTimeout(function(){
    fetch(BASE+'/f/find?q='+encodeURIComponent(v)).then(function(r){return r.text()}).then(function(html){
      h.innerHTML=html;bindHits();
    });
  },250);
};
q.onkeydown=function(e){if(e.key==='Escape'){q.value='';h.innerHTML='';}};";

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

    /// `n` messages, all but the newest 64 summarized at every level (realistically stale,
    /// built history), the recent tail left entirely unbuilt (fresh, not-yet-summarized) - so
    /// the lazy page has both a real stub to test and a real unstubbed leaf to test against.
    fn store(n: usize) -> crate::optchat::store::Store {
        let d = std::env::temp_dir().join(format!("facet-browse-web-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&d);
        let mut s = crate::optchat::store::Store::open(&d);
        for i in 0..n {
            s.msgs.push(crate::optchat::store::Msg { kind: "user".into(), text: format!("{:0>6}", i), date: String::new() });
        }
        let built = n - 64;
        let t = s.t();
        let mut l = 0;
        while (1 << l) <= t {
            for i in 0..(t >> l) { if (i << l) < built { s.set(l, i, "y".repeat(100)); } }
            l += 1;
        }
        s
    }

    /// The lazy page stops at the frontier: it must be far smaller than the standalone one,
    /// still balanced, carry stub markers, and still show the most recent (unstubbed) message.
    #[test]
    fn web_is_far_smaller_than_html_and_stays_balanced() {
        let s = store(3000);
        let v = crate::optchat::view::View::fold(&s, 20_000);
        let full = super::html(&s, &v, 20_000, None);
        let lazy = super::web(&s, &v, 20_000, "/tok");
        assert!(lazy.len() * 8 < full.len(), "lazy {} should be far below full {}", lazy.len(), full.len());
        assert_eq!(lazy.matches("<details").count(), lazy.matches("</details>").count());
        assert!(lazy.contains(" stub"));
        assert!(lazy.contains("data-l=\"0\" data-i=\"2999\""), "the last message still rendered in full");
        // untouched: the standalone page for `facet browse` keeps rendering everything
        assert!(!full.contains(" stub"));
    }

    /// The `view` option lands on the view's own lines: each is on the page marked `top`,
    /// nothing else is, and there is no expand button.
    #[test]
    fn every_view_line_is_top_and_only_they_are() {
        let s = store(3000);
        let v = crate::optchat::view::View::fold(&s, 20_000);
        let lazy = super::web(&s, &v, 20_000, "/tok");
        for p in &v.parts {
            let a = format!("data-l=\"{}\" data-i=\"{}\"", p.l, p.i);
            let at = lazy.find(&a).unwrap_or_else(|| panic!("view line {}.{} missing", p.l, p.i));
            let head = &lazy[lazy[..at].rfind("<details").unwrap()..at];
            assert!(head.contains(" top"), "{} not top: {}", a, head);
        }
        assert_eq!(lazy.matches("class=\"n ").filter(|_| true).count() > 0, true);
        let tops = lazy.split("class=\"n ").skip(1).filter(|c| c.split('"').next().unwrap().split(' ').any(|w| w == "top")).count();
        assert_eq!(tops, v.parts.len());
        assert!(!lazy.contains("expand"));
    }

    /// A stub's fetch (`node`) hands back a balanced fragment, itself stubbed one level
    /// further wherever it still has halves of its own.
    #[test]
    fn node_fetch_returns_one_balanced_level() {
        let s = store(3000);
        let v = crate::optchat::view::View::fold(&s, 20_000);
        let frag = super::node(&s, &v, 11, 0);
        assert!(frag.contains("<details"));
        assert_eq!(frag.matches("<details").count(), frag.matches("</details>").count());
    }

    /// Search reaches text the page never shipped: a message planted past the synthetic
    /// history, found by a case-insensitive, substring match, addressed by `(l, i)`.
    #[test]
    fn find_reaches_past_what_is_loaded() {
        let mut s = store(3000);
        s.log("user", "a ZEBRA-shaped needle").unwrap();
        let hits = super::find(&s, "zebra");
        assert!(hits.contains("data-l=\"0\" data-i=\"3000\""), "{}", hits);
        assert_eq!(super::find(&s, "no-such-marker-anywhere"), "<p class=s>no matches</p>");
        assert_eq!(super::find(&s, "   "), "");
    }
}
