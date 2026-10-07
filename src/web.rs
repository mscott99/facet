// The web route: Facet's main face. Server-rendered HTML, HTMX for the three dynamic needs
// (append new messages, re-render a changed note, post a message), ~20 lines of JS for KaTeX
// and the Enter key. All markdown goes through one renderer; math is extracted by the parser,
// never by a regex.
use crate::cfg::Cfg;
use crate::{diag, doc, log, md, tell};
use std::io::Read;
use tiny_http::{Header, Request, Response, Server};

const SHELL: &str = r#"<!DOCTYPE html><html><head><meta charset=utf-8>
<meta name=viewport content="width=device-width,initial-scale=1,viewport-fit=cover">
<title>{{TITLE}}</title>
<link rel=stylesheet href="https://cdn.jsdelivr.net/npm/katex@0.16.11/dist/katex.min.css">
<script defer src="https://cdn.jsdelivr.net/npm/katex@0.16.11/dist/katex.min.js"></script>
<script src="{{TOK}}/static/htmx.js"></script>
<style>
:root{--bg:#14161a;--fg:#dfe3e8;--dim:#8b949e;--line:#262b31;--acc:#7aa2f7;--warn:#e0af68;--err:#f7768e}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--fg);font:16px/1.55 -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}
header{position:sticky;top:0;z-index:5;display:flex;gap:14px;align-items:center;
 padding:10px 14px;background:#11131799;backdrop-filter:blur(8px);border-bottom:1px solid var(--line);font-size:14px}
header a{color:var(--dim);text-decoration:none}header a:hover,header a.on{color:var(--acc)}
header .sp{flex:1}
main{max-width:46rem;margin:0 auto;padding:12px 14px 7rem}
.msg{margin:14px 0}
.who{font-size:12px;color:var(--dim);letter-spacing:.04em}
.user{background:#1b2230;border-left:2px solid var(--acc);padding:8px 12px;border-radius:6px}
.talk{padding:0 2px}
details.step{margin:6px 0;font-size:14px;color:var(--dim)}
details.step summary{cursor:pointer;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
details.step pre{white-space:pre-wrap;background:#0f1115;padding:8px;border-radius:6px;font-size:12.5px}
pre{overflow-x:auto;background:#0f1115;padding:10px;border-radius:6px}
code{background:#0f1115;padding:1px 4px;border-radius:4px;font-size:.92em}
pre code{background:none;padding:0}
a{color:var(--acc)}a.wl{border-bottom:1px dotted var(--acc);text-decoration:none}
.cite{color:var(--dim);cursor:help}
table{border-collapse:collapse;width:100%;font-size:14px}th,td{border:1px solid var(--line);padding:4px 7px}
blockquote{border-left:2px solid var(--line);margin:0;padding-left:12px;color:var(--dim)}
.diag{margin:12px 0;border:1px solid var(--line);border-left:3px solid var(--warn);border-radius:6px;padding:8px 10px;background:#171a1f}
.diag.error{border-left-color:var(--err)}.diag.info,.diag.hint{border-left-color:var(--dim)}
.diag .sev{font-size:11px;text-transform:uppercase;letter-spacing:.08em;color:var(--warn)}
.diag.error .sev{color:var(--err)}.diag .at{color:var(--dim);font-size:12px}
.diag form{display:flex;gap:6px;margin-top:8px;flex-wrap:wrap}
.diag input[type=text]{flex:1;min-width:8rem;background:#0f1115;border:1px solid var(--line);
 color:var(--fg);border-radius:6px;padding:5px 8px;font-size:13px}
button{background:#232a35;color:var(--fg);border:1px solid var(--line);border-radius:6px;
 padding:5px 10px;font-size:13px}button:hover{border-color:var(--acc)}
footer{position:fixed;bottom:0;left:0;right:0;background:#111317f2;border-top:1px solid var(--line);
 padding:8px 10px env(safe-area-inset-bottom)}
footer form{max-width:46rem;margin:0 auto;display:flex;gap:8px;align-items:flex-end}
textarea{flex:1;resize:none;background:#0f1115;color:var(--fg);border:1px solid var(--line);
 border-radius:8px;padding:9px 11px;font:15px/1.4 inherit;max-height:40vh}
#toast{max-width:46rem;margin:4px auto 0;font-size:12.5px;color:var(--dim);min-height:1em}
.katex{font-size:1.02em}.katex-display{overflow-x:auto;overflow-y:hidden}
[data-line]:hover{outline:1px dashed var(--line);outline-offset:2px;cursor:text}
</style></head><body>
<header>{{NAV}}<span class=sp></span><span class=at>{{STATUS}}</span></header>
<main>{{BODY}}</main>
{{FOOT}}
<script>
var TOK="{{TOK}}";
function mathify(r){r.querySelectorAll('span[data-math-style]').forEach(function(s){
  if(s.dataset.k)return; s.dataset.k=1;
  try{katex.render(s.textContent,s,{displayMode:s.dataset.mathStyle=='display',throwOnError:false})}
  catch(e){}});}
function atEnd(){return innerHeight+scrollY>document.body.scrollHeight-120}
var stick=true;
addEventListener('scroll',function(){stick=atEnd()});
document.addEventListener('htmx:afterSwap',function(e){mathify(e.target);if(stick)scrollTo(0,1e7)});
addEventListener('load',function(){mathify(document);if(location.hash=='')scrollTo(0,1e7)});
// Enter sends (into the running turn, at its next tool call); Shift-Enter sends for a turn
// of its own, after the running one; Alt-Enter is a new line
document.addEventListener('keydown',function(e){
  if(e.key!='Enter'||e.target.tagName!='TEXTAREA')return;
  e.preventDefault();
  if(e.altKey){e.target.setRangeText('\n',e.target.selectionStart,e.target.selectionEnd,'end');return}
  var f=e.target.form;f.elements.later.value=e.shiftKey?'1':'0';
  htmx.trigger(f,'submit');});
document.addEventListener('htmx:afterRequest',function(e){var l=e.target.elements&&e.target.elements.later;if(l)l.value='0'});
// A double-click (double-tap) on any rendered line of a note — comment cards, chat and the
// compose box excluded — asks what to say about it, then sends through the same `/x/send`
// a message typed by hand would, shaped the way a reply quoting a line always is.
function comment(e){
  if(e.target.closest('a,form,button,textarea,.diag'))return;
  var b=e.target.closest('[data-line]');if(!b||!b.dataset.note)return;
  var quote=(b.textContent||'').trim().replace(/\s+/g,' ').slice(0,160);
  var where='[['+b.dataset.note+']] L'+b.dataset.line+(quote?': "'+quote+'"':'');
  var said=prompt(where+'\n\nSay what to change:');
  if(!said)return;
  var body='text='+encodeURIComponent(where+'\n'+said)+'&later=1';
  fetch(TOK+'/x/send',{method:'POST',headers:{'Content-Type':'application/x-www-form-urlencoded'},body:body})
    .then(function(r){return r.text()}).then(function(t){var el=document.getElementById('toast');if(el)el.textContent=t});
}
// iOS Safari does not fire `dblclick` reliably on a touch, so a coarse (touch) pointer gets
// its own double-tap detector instead, ported from vault-phone's `pick()`.
if(matchMedia('(pointer: coarse)').matches){
  var lastTap=null;
  document.addEventListener('click',function(e){
    var now={t:e.timeStamp,x:e.clientX,y:e.clientY};
    var isDouble=lastTap&&now.t-lastTap.t<350&&Math.hypot(now.x-lastTap.x,now.y-lastTap.y)<30;
    lastTap=isDouble?null:now;
    if(isDouble)comment(e);
  });
}else{
  document.addEventListener('dblclick',comment);
}
</script></body></html>"#;

fn page(cfg: &Cfg, title: &str, nav_on: &str, body: &str, compose: bool) -> String {
    let t = cfg.token_path();
    let item = |href: &str, label: &str, key: &str| {
        format!("<a href=\"{}{}\"{}>{}</a>", t, href,
            if key == nav_on { " class=on" } else { "" }, label)
    };
    let mut nav = String::new();
    nav.push_str(&item("/", "home", "home"));
    nav.push_str(&item("/chat", "chat", "chat"));
    nav.push_str(&item("/m/", "notes", "notes"));
    nav.push_str(&item("/d/", "comments", "diag"));
    nav.push_str(&item("/tree", "memory", "tree"));
    if !cfg.vault_phone().is_empty() { nav.push_str(&format!("<a href=\"{}\">vault</a>", cfg.vault_phone())); }
    if !cfg.terminal().is_empty() { nav.push_str(&format!("<a href=\"{}\">term</a>", cfg.terminal())); }
    let n = diag::all(cfg).len();
    let status = format!("{}{}",
        if tell::healthy(cfg) { "" } else { "input down · " },
        if n > 0 { format!("{} comments", n) } else { String::new() });
    let foot = if compose {
        format!("<footer><form hx-post=\"{}/x/send\" hx-target=\"#toast\" hx-swap=innerHTML \
            hx-on::after-request=\"if(event.detail.successful)this.querySelector('textarea').value=''\">\
            <textarea name=text rows=1 placeholder=\"message\"></textarea><input type=hidden name=later value=0>\
            <button>send</button></form><div id=toast></div></footer>", t)
    } else { String::new() };
    SHELL.replace("{{TITLE}}", &md::esc(title))
        .replace("{{TOK}}", &t)
        .replace("{{NAV}}", &nav)
        .replace("{{STATUS}}", &status)
        .replace("{{BODY}}", body)
        .replace("{{FOOT}}", &foot)
}

// ---- rendering the three views ----------------------------------------------------------

/// Where a `[[wikilink]]` goes: facet's own `/n/` route, against the vault facet is
/// configured for. It used to go to the vault-phone service, which serves whatever vault its
/// own script was pointed at, so a link out of a doc answered 404 whenever the two differed.
fn note_base(cfg: &Cfg) -> String { format!("{}/n/", cfg.token_path()) }

/// The name a wikilink or an embed uses for this note — its file stem, the same key `doc::find`
/// matches against — so a block rendered from it tags itself the way a click handler expects.
fn home_of(d: &doc::Doc) -> String { d.path.file_stem().unwrap_or_default().to_string_lossy().to_string() }

fn msg_html(cfg: &Cfg, m: &log::Msg) -> String {
    let base = note_base(cfg);
    match m.kind.as_str() {
        "user" => format!("<div class=\"msg user\">{}</div>", md::render(&m.text, &base)),
        "talk" | "note" | "work" => format!("<div class=\"msg talk\">{}</div>", md::render(&m.text, &base)),
        _ => {
            let head: String = m.text.lines().next().unwrap_or("").chars().take(110).collect();
            format!("<details class=step><summary>{} · {}</summary><pre>{}</pre></details>",
                m.kind, md::esc(&head), md::esc(&m.text.chars().take(4000).collect::<String>()))
        }
    }
}

/// The chat fragment: new messages, then a fresh poller carrying the new cursor. The cursor
/// lives in the DOM; there is no client-side state to get out of step.
fn log_fragment(cfg: &Cfg, since: i64) -> String {
    let msgs = log::since(cfg, since);
    let high = msgs.last().map(|m| m.i).unwrap_or(since);
    let mut out: String = msgs.iter().map(|m| msg_html(cfg, m)).collect();
    out.push_str(&format!(
        "<div id=tail hx-get=\"{}/f/log?since={}\" hx-trigger=\"load delay:2s\" hx-swap=outerHTML></div>",
        cfg.token_path(), high));
    out
}

fn chat_page(cfg: &Cfg) -> String {
    let all = log::since(cfg, -1);
    let start = all.len().saturating_sub(40);
    let since = if start == 0 { -1 } else { all[start - 1].i };
    page(cfg, "Facet", "chat", &format!("<div id=log>{}</div>", log_fragment(cfg, since)), true)
}

/// A diagnostic as a card: the message, the explanation with real math, and the three things
/// you can do about it. `where_` is shown when the card is away from its note.
fn diag_card(cfg: &Cfg, d: &diag::Diag, show_where: bool, quote: bool) -> String {
    let t = cfg.token_path();
    let base = note_base(cfg);
    let mut s = format!("<div class=\"diag {}\" id=\"d-{}\"><div><span class=sev>{}</span> \
        <span class=at>{}L{}</span></div><div>{}</div>",
        d.severity, md::esc(&d.code), md::esc(&d.severity),
        if show_where { format!("{} · ", md::esc(d.note.trim_end_matches(".md"))) } else { String::new() },
        d.line, md::render(&d.message, &base));
    if quote {
        s.push_str(&format!("<pre>{}</pre>", md::esc(&diag::context(cfg, d, 1))));
    }
    if let Some(det) = &d.detail {
        s.push_str(&format!("<details class=step><summary>why</summary>{}</details>",
            md::render(det, &base)));
    }
    s.push_str(&format!("<form hx-post=\"{}/x/diag\" hx-target=\"#toast\" hx-swap=innerHTML>\
        <input type=hidden name=code value=\"{}\">\
        <input type=text name=note placeholder=\"reason / question\">{}\
        <button name=do value=dismiss>dismiss</button>\
        <button name=do value=discuss>discuss</button></form></div>",
        t, md::esc(&d.code),
        if d.fixes > 0 { format!("<button name=do value=apply>apply fix ({})</button>", d.fixes) }
        else { String::new() }));
    s
}

/// A note, with its diagnostics anchored in place. The text is cut only at blank lines that
/// are not inside a fence or a display-math block, so a card never lands mid-block.
fn note_html(cfg: &Cfg, d: &doc::Doc) -> String {
    let ds = diag::for_note(cfg, &d.path);
    let base = note_base(cfg);
    let home = home_of(d);
    let lines: Vec<&str> = d.text.split('\n').collect();
    let skip = doc::front_len(&d.text);     // frontmatter is metadata, not prose
    if ds.is_empty() {
        let (text, srcs) = doc::assemble(cfg, &home, &lines[skip.min(lines.len())..], skip);
        return md::render_at(&text, &base, &srcs);
    }
    let mut out = String::new();
    let (mut start, mut fence, mut math) = (skip, false, false);
    let mut emit = |out: &mut String, a: usize, b: usize| {
        if a >= b { return }
        let (text, srcs) = doc::assemble(cfg, &home, &lines[a..b], a);
        out.push_str(&md::render_at(&text, &base, &srcs));
        for g in ds.iter().filter(|g| g.line - 1 >= a as i64 && g.line - 1 < b as i64) {
            out.push_str(&diag_card(cfg, g, false, false));
        }
    };
    for (i, l) in lines.iter().enumerate().skip(skip) {
        let tl = l.trim_start();
        if tl.starts_with("```") { fence = !fence }
        if tl == "$$" { math = !math }
        if fence || math { continue }
        // Cut at blank lines, and at the start of a top-level list item: lists have no blank
        // lines inside them, and without this a comment on one bullet lands under the last.
        if l.trim().is_empty() {
            emit(&mut out, start, i + 1);
            start = i + 1;
        } else if (l.starts_with("- ") || l.starts_with("* ")) && i > start {
            emit(&mut out, start, i);
            start = i;
        }
    }
    emit(&mut out, start, lines.len());
    // anything anchored past the end of the note
    for g in ds.iter().filter(|g| g.line as usize > lines.len()) {
        out.push_str(&diag_card(cfg, g, false, true));
    }
    out
}

/// One `#`-section of a note: what `[[Note#Section]]` asks for, as `?h=`. No comment cards
/// here — their line numbers are the whole file's, and a section does not start where it does.
fn section_html(cfg: &Cfg, d: &doc::Doc, h: &str) -> String {
    let Some((s, start)) = doc::section_at(&d.text, h) else { return note_html(cfg, d) };
    let lines: Vec<&str> = s.split('\n').collect();
    let (text, srcs) = doc::assemble(cfg, &home_of(d), &lines, start - 1);
    format!("<h2>{}</h2>{}", md::esc(h), md::render_at(&text, &note_base(cfg), &srcs))
}

/// Any note of the vault, read-only, on the same page as a published one. Wikilinks in docs,
/// notes and messages all land here, so a name that is not in the vault must say so plainly.
fn note_page(cfg: &Cfg, name: &str, h: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let Some(d) = doc::note(cfg, name) else {
        return html(page(cfg, "no such note", "notes",
            &format!("<h1>no such note</h1><p class=at>{} is not in {}</p>",
                md::esc(name), md::esc(&cfg.vault().to_string_lossy())), true), 404);
    };
    let body = if h.is_empty() { note_html(cfg, &d) } else { section_html(cfg, &d, h) };
    html(page(cfg, &d.title, "notes",
        &format!("<h1>{}</h1>{}", md::esc(&d.title), body), true), 200)
}

fn doc_version(cfg: &Cfg, d: &doc::Doc) -> u64 { d.mtime.max(doc::mtime(&diag::file(cfg))) }

fn doc_fragment(cfg: &Cfg, d: &doc::Doc) -> String {
    format!("<div id=docwrap><h1>{}</h1>{}<div hx-get=\"{}/f/doc/{}?v={}\" \
        hx-trigger=\"every 4s\" hx-target=\"#docwrap\" hx-swap=outerHTML style=display:none></div></div>",
        md::esc(&d.title), note_html(cfg, d), cfg.token_path(), md::urlenc(&d.slug), doc_version(cfg, d))
}

fn diag_page(cfg: &Cfg) -> String {
    let ds = diag::all(cfg);
    let mut body = format!("<h1>Comments</h1><p class=at>{} open</p>", ds.len());
    if ds.is_empty() { body.push_str("<p class=at>Nothing open. Reviews land in <code>.claude/diagnostics.json</code>.</p>"); }
    let mut last = String::new();
    for d in &ds {
        if d.note != last {
            body.push_str(&format!("<h2>{}</h2>", md::esc(d.note.trim_end_matches(".md"))));
            last = d.note.clone();
        }
        body.push_str(&diag_card(cfg, d, false, true));
    }
    page(cfg, "Comments", "diag", &body, true)
}

// ---- the server -------------------------------------------------------------------------

/// One page that links every other one, each with a line of live state.
fn home(cfg: &Cfg) -> String {
    let t = cfg.token_path();
    let st = crate::optchat::engine::request(&crate::optchat::engine::dir(), serde_json::json!({"op": "status"}));
    let (engine, usage) = match &st {
        Ok(v) => (format!("{} · {} messages{}", if v["busy"] == true { "working" } else { "idle" }, v["messages"],
                          v["paused"].as_str().map(|p| format!(" · compactor paused: {}", p)).unwrap_or_default()),
                  v["limits"].as_str().unwrap_or("").to_string()),
        Err(e) => (format!("engine DOWN: {}", e), String::new()),
    };
    let notes = doc::table(cfg).len();
    let comments = diag::all(cfg).len();
    let mut rows: Vec<(String, &str, String)> = vec![
        (format!("{}/chat", t), "Chat", engine),
        (format!("{}/tree", t), "Memory", "the whole tree: summaries down to every message, searchable".into()),
        (format!("{}/m/", t), "Notes", format!("{} published", notes)),
        (format!("{}/d/", t), "Comments", format!("{} open", comments)),
    ];
    if !cfg.terminal().is_empty() { rows.push((cfg.terminal(), "Terminal", "the chat in a terminal (facet chat)".into())); }
    if !cfg.vault_phone().is_empty() { rows.push((cfg.vault_phone(), "Vault", "notes viewer and editor".into())); }
    if let Some(u) = cfg.opt("telegram.username") { rows.push((format!("https://t.me/{}", u), "Telegram", format!("@{} · /ping, /last, /help", u))); }
    let mut b = String::from("<style>.home a.card{display:block;margin:10px 0;padding:12px 14px;border:1px solid var(--line);border-radius:8px;text-decoration:none;color:var(--fg)}\
        .home a.card:hover{border-color:var(--acc)}.home .n{font-weight:600;color:var(--acc)}.home .d{font-size:14px;color:var(--dim)}\
        .home .u{font-size:13px;color:var(--dim);margin:14px 0}</style><div class=home>");
    if !usage.is_empty() { b.push_str(&format!("<div class=u>{}</div>", md::esc(&usage))); }
    for (href, name, desc) in rows {
        b.push_str(&format!("<a class=card href=\"{}\"><div class=n>{}</div><div class=d>{}</div></a>", md::esc(&href), name, md::esc(&desc)));
    }
    b.push_str("</div>");
    b
}

fn html(body: String, code: u16) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body).with_status_code(code)
        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap())
        .with_header(Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap())
}

fn form(body: &str) -> Vec<(String, String)> {
    body.split('&').filter(|p| !p.is_empty()).map(|p| {
        let (k, v) = p.split_once('=').unwrap_or((p, ""));
        (md::urldec(k), md::urldec(v))
    }).collect()
}
fn field(f: &[(String, String)], k: &str) -> String {
    f.iter().find(|(a, _)| a == k).map(|(_, b)| b.clone()).unwrap_or_default()
}

pub fn serve(cfg: Cfg) {
    let addr = format!("{}:{}", cfg.host(), cfg.port());
    let server = Server::http(&addr).unwrap_or_else(|e| { eprintln!("bind {}: {}", addr, e); std::process::exit(1) });
    println!("facet on {} ({})", cfg.url("/"), addr);
    crate::tg::spawn(&cfg);
    for mut rq in server.incoming_requests() {
        let cfg = Cfg::load();
        let res = route(&cfg, &mut rq);
        let _ = rq.respond(res);
    }
}

fn route(cfg: &Cfg, rq: &mut Request) -> Response<std::io::Cursor<Vec<u8>>> {
    let url = rq.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let segs: Vec<String> = path.split('/').filter(|s| !s.is_empty()).map(md::urldec).collect();

    // one token, in the path, for every route — no headers, no cookies, nothing a
    // WebSocket upgrade can decline to carry.
    let tok = cfg.token();
    if tok.is_empty() || segs.first().map(|s| s != &tok).unwrap_or(true) {
        return html("<h1>404</h1>".into(), 404);
    }
    let rest: Vec<&str> = segs[1..].iter().map(|s| s.as_str()).collect();
    let qnum = |k: &str| -> i64 {
        query.split('&').find_map(|p| p.strip_prefix(&format!("{}=", k)))
            .and_then(|v| v.parse().ok()).unwrap_or(-1)
    };
    let qstr = |k: &str| -> String {
        query.split('&').find_map(|p| p.strip_prefix(&format!("{}=", k)))
            .map(md::urldec).unwrap_or_default()
    };
    let post = rq.method() == &tiny_http::Method::Post;
    let mut body = String::new();
    if post { let _ = rq.as_reader().read_to_string(&mut body); }
    let f = form(&body);

    match rest.as_slice() {
        // the root is the home page; /home stays as an alias for old links
        [] | ["home"] => html(page(cfg, "Facet", "home", &home(cfg), false), 200),
        ["chat"] => html(chat_page(cfg), 200),

        ["f", "log"] => html(log_fragment(cfg, qnum("since")), 200),

        // the memory tree (folded from the files: the engine's view is the same fold)
        ["tree"] => {
            let s = crate::optchat::store::Store::open(&cfg.store());
            let v = crate::optchat::view::View::fold(&s, crate::optchat::VIEW);
            html(crate::optchat::browse::html(&s, &v, crate::optchat::VIEW, Some(&format!("{}/", cfg.token_path()))), 200)
        }

        ["m"] => html(page(cfg, "Notes", "notes", &format!("<div id=docwrap>{}</div>",
            md::render(&doc::index(cfg).text, &note_base(cfg))), true), 200),

        ["m", slug] => match doc::get(cfg, slug) {
            Some(d) => { let t = d.title.clone();
                         html(page(cfg, &t, "notes", &doc_fragment(cfg, &d), true), 200) }
            None => html("<h1>no such note</h1>".into(), 404),
        },

        // a vault note by its own name, which is what a wikilink carries; `?h=` is one section
        ["n", name] => note_page(cfg, name, &qstr("h")),

        ["f", "doc", slug] => match doc::get(cfg, slug) {
            // unchanged -> 204, and HTMX leaves the DOM and your scroll position alone
            Some(d) if doc_version(cfg, &d) as i64 == qnum("v") => html(String::new(), 204),
            Some(d) => html(doc_fragment(cfg, &d), 200),
            None => html(String::new(), 204),
        },

        ["d"] => html(diag_page(cfg), 200),

        ["x", "send"] if post => match tell::tell(cfg, &field(&f, "text"), "reader", field(&f, "later") == "1") {
            Ok(_) => html("sent".into(), 200),
            Err(e) => html(format!("not sent: {}", md::esc(&e)), 200),
        },

        ["x", "diag"] if post => {
            let (code, note) = (field(&f, "code"), field(&f, "note"));
            let r = match field(&f, "do").as_str() {
                "apply" => diag::apply(cfg, &code),
                "dismiss" => diag::dismiss(cfg, &code, &note),
                "discuss" => match diag::find(cfg, &code) {
                    Some(d) => {
                        let text = format!("About my comment on [[{}]] L{} ({}): {}\n\nThe text there now:\n\n{}\n\n{}",
                            d.note.trim_end_matches(".md"), d.line, d.code, d.message,
                            diag::context(cfg, &d, 2), note);
                        tell::tell(cfg, &text, "comment", false).map(|_| "sent to the conversation".into())
                    }
                    None => Err("that diagnostic is gone".into()),
                },
                _ => Err("?".into()),
            };
            html(match r { Ok(m) => md::esc(&m), Err(e) => format!("no: {}", md::esc(&e)) }, 200)
        }

        ["static", "htmx.js"] => Response::from_string(include_str!("static/htmx.min.js"))
            .with_header(Header::from_bytes(&b"Content-Type"[..], &b"text/javascript"[..]).unwrap())
            .with_header(Header::from_bytes(&b"Cache-Control"[..], &b"max-age=86400"[..]).unwrap()),

        _ => html("<h1>404</h1>".into(), 404),
    }
}
