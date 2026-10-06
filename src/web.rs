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
</style></head><body>
<header>{{NAV}}<span class=sp></span><span class=at>{{STATUS}}</span></header>
<main>{{BODY}}</main>
{{FOOT}}
<script>
function mathify(r){r.querySelectorAll('span[data-math-style]').forEach(function(s){
  if(s.dataset.k)return; s.dataset.k=1;
  try{katex.render(s.textContent,s,{displayMode:s.dataset.mathStyle=='display',throwOnError:false})}
  catch(e){}});}
function atEnd(){return innerHeight+scrollY>document.body.scrollHeight-120}
var stick=true;
addEventListener('scroll',function(){stick=atEnd()});
document.addEventListener('htmx:afterSwap',function(e){mathify(e.target);if(stick)scrollTo(0,1e7)});
addEventListener('load',function(){mathify(document);if(location.hash=='')scrollTo(0,1e7)});
document.addEventListener('keydown',function(e){
  if(e.key=='Enter'&&!e.shiftKey&&e.target.tagName=='TEXTAREA'){e.preventDefault();
    htmx.trigger(e.target.form,'submit');}});
</script></body></html>"#;

fn page(cfg: &Cfg, title: &str, nav_on: &str, body: &str, compose: bool) -> String {
    let t = cfg.token_path();
    let item = |href: &str, label: &str, key: &str| {
        format!("<a href=\"{}{}\"{}>{}</a>", t, href,
            if key == nav_on { " class=on" } else { "" }, label)
    };
    let mut nav = String::new();
    nav.push_str(&item("/", "chat", "chat"));
    nav.push_str(&item("/m/", "notes", "notes"));
    nav.push_str(&item("/d/", "comments", "diag"));
    if !cfg.vault_phone().is_empty() { nav.push_str(&format!("<a href=\"{}\">vault</a>", cfg.vault_phone())); }
    if !cfg.terminal().is_empty() { nav.push_str(&format!("<a href=\"{}\">term</a>", cfg.terminal())); }
    let n = diag::all(cfg).len();
    let status = format!("{}{}",
        if tell::healthy(cfg) { "" } else { "input down · " },
        if n > 0 { format!("{} comments", n) } else { String::new() });
    let foot = if compose {
        format!("<footer><form hx-post=\"{}/x/send\" hx-target=\"#toast\" hx-swap=innerHTML \
            hx-on::after-request=\"if(event.detail.successful)this.querySelector('textarea').value=''\">\
            <textarea name=text rows=1 placeholder=\"message\"></textarea>\
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

fn note_base(cfg: &Cfg) -> String {
    let vp = cfg.vault_phone();
    if vp.is_empty() { String::new() } else { format!("{}/n/", vp) }
}

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
    let lines: Vec<&str> = d.text.split('\n').collect();
    let skip = doc::front_len(&d.text);     // frontmatter is metadata, not prose
    if ds.is_empty() { return md::render(&lines[skip.min(lines.len())..].join("\n"), &base) }
    let mut out = String::new();
    let (mut start, mut fence, mut math) = (skip, false, false);
    let mut emit = |out: &mut String, a: usize, b: usize| {
        if a >= b { return }
        out.push_str(&md::render(&lines[a..b].join("\n"), &base));
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
    let post = rq.method() == &tiny_http::Method::Post;
    let mut body = String::new();
    if post { let _ = rq.as_reader().read_to_string(&mut body); }
    let f = form(&body);

    match rest.as_slice() {
        [] => html(chat_page(cfg), 200),

        ["f", "log"] => html(log_fragment(cfg, qnum("since")), 200),

        ["m"] => html(page(cfg, "Notes", "notes", &format!("<div id=docwrap>{}</div>",
            md::render(&doc::index(cfg).text, &note_base(cfg))), true), 200),

        ["m", slug] => match doc::get(cfg, slug) {
            Some(d) => { let t = d.title.clone();
                         html(page(cfg, &t, "notes", &doc_fragment(cfg, &d), true), 200) }
            None => html("<h1>no such note</h1>".into(), 404),
        },

        ["f", "doc", slug] => match doc::get(cfg, slug) {
            // unchanged -> 204, and HTMX leaves the DOM and your scroll position alone
            Some(d) if doc_version(cfg, &d) as i64 == qnum("v") => html(String::new(), 204),
            Some(d) => html(doc_fragment(cfg, &d), 200),
            None => html(String::new(), 204),
        },

        ["d"] => html(diag_page(cfg), 200),

        ["x", "send"] if post => match tell::tell(cfg, &field(&f, "text"), "reader") {
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
                        tell::tell(cfg, &text, "comment").map(|_| "sent to the conversation".into())
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
