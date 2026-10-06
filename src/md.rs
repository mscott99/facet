// One markdown renderer for everything: chat messages, posted notes, the index.
// comrak does the two hard parts natively, which is why it was chosen:
//   math      -> <span data-math-style="inline|display">TeX</span>   (KaTeX renders those spans)
//   wikilinks -> <a href="Target">                                   (rewritten below to a note route)
// So no regex ever hunts for $ delimiters again: that was the source of every past math bug.
use comrak::{markdown_to_html, Options};

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn opts() -> Options<'static> {
    let mut o = Options::default();
    let e = &mut o.extension;
    e.strikethrough = true;
    e.table = true;
    e.autolink = true;
    e.tasklist = true;
    e.footnotes = true;
    e.math_dollars = true;              // $x$ and $$x$$
    e.math_code = true;                 // ```math blocks
    e.wikilinks_title_after_pipe = true; // [[Target|title]]
    let r = &mut o.render;
    r.r#unsafe = false;  // note text is data, never markup: raw HTML stays escaped
    r.hardbreaks = false;
    o
}

/// `note_base` is the URL prefix a `[[wikilink]]` resolves to, e.g. ".../n/" ; empty disables links.
pub fn render(src: &str, note_base: &str) -> String {
    let html = markdown_to_html(src, &opts());
    if note_base.is_empty() { return html }
    rewrite_links(&html, note_base)
}

/// comrak renders `[[A#B]]` as `<a href="A#B">`. Any scheme-less, non-anchor href is a vault note:
/// turn it into `<base>/<note>?h=<section>` and mark it so the page can style it.
fn rewrite_links(html: &str, note_base: &str) -> String {
    let mut out = String::with_capacity(html.len() + 64);
    let mut rest = html;
    while let Some(p) = rest.find("<a href=\"") {
        let (head, tail) = rest.split_at(p + 9);
        out.push_str(head);
        let Some(q) = tail.find('"') else { out.push_str(tail); return out };
        let href = &tail[..q];
        let local = !href.contains("://") && !href.starts_with('/') && !href.starts_with('#')
            && !href.starts_with("mailto:");
        if local {
            let (note, sec) = match href.split_once('#') { Some((n, s)) => (n, s), None => (href, "") };
            // comrak has already percent-encoded the target; encoding it again gives %2520
            let mut u = format!("{}{}", note_base, note);
            if !sec.is_empty() { u.push_str(&format!("?h={}", sec)); }
            out.push_str(&u);
            out.push('"');
            out.push_str(" class=\"wl\"");
        } else {
            out.push_str(href);
            out.push('"');
        }
        rest = &tail[q + 1..];
    }
    out.push_str(rest);
    out
}

pub fn urlenc(s: &str) -> String {
    let mut o = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => o.push(*b as char),
            b' ' => o.push_str("%20"),
            _ => o.push_str(&format!("%{:02X}", b)),
        }
    }
    o
}

pub fn urldec(s: &str) -> String {
    let b = s.as_bytes();
    let mut o: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(x) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                o.push(x); i += 3; continue;
            }
        }
        if b[i] == b'+' { o.push(b' '); } else { o.push(b[i]); }
        i += 1;
    }
    String::from_utf8_lossy(&o).to_string()
}
