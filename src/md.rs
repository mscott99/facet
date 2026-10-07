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

/// The same rendering, but with every block tagged `data-line`/`data-note` from `srcs` (one
/// entry per line of `src`, see `doc::assemble`) — so a click anywhere in the page can name the
/// line, and the note, it fell on, through however many embeds it took to get there.
pub fn render_at(src: &str, note_base: &str, srcs: &[crate::doc::Src]) -> String {
    let mut o = opts();
    o.render.sourcepos = true;        // comrak stamps every block tag with its own source line
    let html = mark_lines(&markdown_to_html(src, &o), srcs);
    if note_base.is_empty() { return html }
    rewrite_links(&html, note_base)
}

/// comrak's `data-sourcepos="startline:col-endline:col"` on each block tag, turned into the
/// `data-line`/`data-note` a click handler reads — a line of the *assembled* text, mapped
/// through `srcs` back to where it actually came from. A line past the end of `srcs` (should
/// not happen; left defensive) just loses its tag rather than panic.
fn mark_lines(html: &str, srcs: &[crate::doc::Src]) -> String {
    const TAG: &str = " data-sourcepos=\"";
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(p) = rest.find(TAG) {
        out.push_str(&rest[..p]);
        let tail = &rest[p + TAG.len()..];
        let Some(q) = tail.find('"') else { out.push_str(&rest[p..]); return out };
        let line: usize = tail[..q].split(':').next().and_then(|n| n.parse().ok()).unwrap_or(0);
        if let Some((note, n)) = line.checked_sub(1).and_then(|i| srcs.get(i)) {
            out.push_str(&format!(" data-line=\"{}\" data-note=\"{}\"", n, esc(note)));
        }
        rest = &tail[q + 1..];
    }
    out.push_str(rest);
    out
}

/// comrak renders `[[A#B]]` as `<a href="A#B">`. Any scheme-less, non-anchor href is a vault note:
/// turn it into `<base>/<note>?h=<section>` and mark it so the page can style it.
///
/// `href` is not necessarily the tag's first attribute: `render_at`'s `data-line`/`data-note`
/// come before it, and comrak's own `data-wikilink="true"` comes after, so this looks for
/// `<a `, then for ` href="` anywhere within that one tag, rather than assuming either order.
fn rewrite_links(html: &str, note_base: &str) -> String {
    let mut out = String::with_capacity(html.len() + 64);
    let mut rest = html;
    while let Some(p) = rest.find("<a ") {
        out.push_str(&rest[..p]);
        rest = &rest[p..];
        let Some(end) = rest.find('>') else { out.push_str(rest); return out };
        let tag = &rest[..end];   // "<a ...attrs..." (no closing '>')
        let Some(hp) = tag.find(" href=\"") else {
            out.push_str(tag); out.push('>'); rest = &rest[end + 1..]; continue;
        };
        let after = &tag[hp + 7..];
        let Some(q) = after.find('"') else {
            out.push_str(tag); out.push('>'); rest = &rest[end + 1..]; continue;
        };
        let href = &after[..q];
        let local = !href.contains("://") && !href.starts_with('/') && !href.starts_with('#')
            && !href.starts_with("mailto:");
        out.push_str(&tag[..hp]);
        out.push_str(" href=\"");
        if local {
            let (note, sec) = match href.split_once('#') { Some((n, s)) => (n, s), None => (href, "") };
            // comrak has already percent-encoded the target; encoding it again gives %2520
            out.push_str(note_base);
            out.push_str(note);
            if !sec.is_empty() { out.push_str("?h="); out.push_str(sec); }
            out.push_str("\" class=\"wl\"");
        } else {
            out.push_str(href);
            out.push('"');
        }
        out.push_str(&after[q + 1..]);
        out.push('>');
        rest = &rest[end + 1..];
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_at_tags_each_block_with_its_source() {
        // Two paragraphs, as if assembled from two different notes: one `Src` per line of
        // the text passed in, the blank line between them included.
        let srcs = vec![("Home".to_string(), 5), ("Home".to_string(), 6), ("Elsewhere".to_string(), 2)];
        let html = render_at("first paragraph\n\nsecond paragraph", "", &srcs);
        assert!(html.contains("data-line=\"5\" data-note=\"Home\""), "{}", html);
        assert!(html.contains("data-line=\"2\" data-note=\"Elsewhere\""), "{}", html);
        // comrak's own attribute never leaks through
        assert!(!html.contains("data-sourcepos"), "{}", html);
    }

    #[test]
    fn a_wikilink_still_resolves_once_sourcepos_comes_first_in_the_tag() {
        // With sourcepos on, comrak puts `data-sourcepos` before `href`, so `render_at`'s
        // `data-line`/`data-note` land there too: the link rewrite must not assume `href`
        // is a tag's first attribute.
        let srcs = vec![("Home".to_string(), 1)];
        let html = render_at("[[Target]]", "/n/", &srcs);
        assert!(html.contains("href=\"/n/Target\""), "{}", html);
        assert!(html.contains("class=\"wl\""), "{}", html);
        assert!(html.contains("data-wikilink=\"true\""), "comrak's own marker survives: {}", html);
        assert!(html.contains("data-line=\"1\" data-note=\"Home\""), "{}", html);
    }

    #[test]
    fn render_at_leaves_an_out_of_range_line_untagged() {
        let html = render_at("a paragraph", "", &[]);
        assert!(!html.contains("data-line"));
        assert!(!html.contains("data-sourcepos"));
    }
}
