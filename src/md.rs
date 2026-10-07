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
    let html = dim_glyphs(&wrap_embeds(&html));
    if note_base.is_empty() { return html }
    rewrite_links(&html, note_base)
}

/// `doc::inline` surrounds a resolved embed with two sentinel paragraphs, each alone in a
/// `<code>` span (see `doc::open_marker`/`close_marker`): the opening one names the
/// environment and the note it came from, the closing one has no payload at all. Found here
/// and turned into the one piece of HTML on the page that markdown did not generate itself —
/// a `<div>` with a hairline rule, a quiet label, and (nested embeds being just more of the
/// same markers, correctly ordered by construction) a matching `</div>` wherever it closes.
/// The start of the last real `<p>` opening tag in `head` — `<p>` or `<p ` with an attribute,
/// never a same-prefixed tag that merely starts the same two bytes (`<pre`, `<param`,
/// `<picture>`: a plain `rfind("<p")` catches any of those, and backs up past real content
/// sitting between the lookalike and the marker, silently dropping it).
fn rfind_p_open(head: &str) -> Option<usize> {
    let mut end = head.len();
    loop {
        let at = head[..end].rfind("<p")?;
        let after = head.as_bytes().get(at + 2).copied();
        if after.is_none() || after == Some(b'>') || after.is_some_and(|b| b.is_ascii_whitespace()) {
            return Some(at)
        }
        if at == 0 { return None }
        end = at;
    }
}

fn wrap_embeds(html: &str) -> String {
    use crate::doc::{MARK_OPEN, MARK_CLOSE, MARK_SEP};
    let mut out = String::with_capacity(html.len() + 512);
    let mut rest = html;
    loop {
        // the sentinel itself, not the `<code>` around it: comrak's sourcepos option tags
        // that inline span too, so it carries attributes of its own (a marker's `data-line`
        // and `data-note`, harmless but not literal `<code>`) which a fixed string would miss
        let o = rest.find(MARK_OPEN);
        let c = rest.find(MARK_CLOSE);
        let (at, opening) = match (o, c) {
            (None, None) => break,
            (Some(o), None) => (o, true),
            (None, Some(c)) => (c, false),
            (Some(o), Some(c)) => if o < c { (o, true) } else { (c, false) },
        };
        // back up to the start of the `<p>` this marker sits alone in
        let head = &rest[..at];
        let p_at = rfind_p_open(head).unwrap_or(head.len());
        out.push_str(&head[..p_at]);
        let tail = &rest[at..];
        let Some(end) = tail.find("</code></p>") else { out.push_str(tail); break };
        if opening {
            let payload = &tail[MARK_OPEN.len_utf8()..end];
            let (env, note) = payload.split_once(MARK_SEP).unwrap_or(("", payload));
            out.push_str("<div class=embed><p class=envlabel>");
            if !env.is_empty() { out.push_str(env); out.push_str(" &middot; "); }
            out.push_str(note);
            out.push_str("</p>");
        } else {
            out.push_str("</div>");
        }
        rest = &tail[end + "</code></p>".len()..];
    }
    out.push_str(rest);
    out
}

/// `doc::close` dims the ∎ / □ it appends by prefixing it with `MARK_GLYPH`, wherever that
/// line ends up — inline on prose, or alone when a fence or a display forced its own line.
fn dim_glyphs(html: &str) -> String {
    let g = crate::doc::MARK_GLYPH.to_string();
    html.replace(&format!("{}∎", g), "<span class=envclose>∎</span>")
        .replace(&format!("{}□", g), "<span class=envclose>□</span>")
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

/// `[[@bibkey]]` is a citation, not a note: comrak still renders it as `<a href="@bibkey">`
/// (wikilinks do not know the vault), so without this it would both 404 against `/n/` and show
/// the raw key as its text. Ported from vault-phone's `render_wikilink`, which formats the same
/// key as `[Who Year]`; here it lands as an unlinked span, since neither viewer has anywhere to
/// send such a click (no Zotero/BibTeX route exists), so a dead link is worse than no link.
fn cite_label(key: &str) -> String {
    let bytes = key.as_bytes();
    // the year: a run of 4 ascii digits (a Zotero key ends `...Word2020` or `...Word2020a`);
    // the last such run in the key is the one that matters.
    let mut year_at = None;
    for i in 0..bytes.len().saturating_sub(3) {
        if bytes[i..i + 4].iter().all(|b| b.is_ascii_digit()) { year_at = Some(i); }
    }
    let Some(y) = year_at else { return format!("@{}", key) };
    // the author: the key's leading run of lowercase letters only (a Zotero key runs the
    // surname straight into the capitalized title words that follow it)
    let who_end = key.find(|c: char| !c.is_ascii_lowercase()).unwrap_or(y.min(key.len()));
    let who = &key[..who_end];
    let year = &key[y..y + 4];
    let mut c = who.chars();
    let cap: String = c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default();
    format!("[{} {}]", if cap.is_empty() { key.to_string() } else { cap }, year)
}

/// comrak renders `[[A#B]]` as `<a href="A#B">`. Any scheme-less, non-anchor href is a vault note
/// (or, starting with `@`, a citation — see `cite_label`): turn the former into
/// `<base>/<note>?h=<section>` and mark it so the page can style it.
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
        let href = after[..q].to_string();
        let local = !href.contains("://") && !href.starts_with('/') && !href.starts_with('#')
            && !href.starts_with("mailto:");
        if local && href.starts_with('@') {
            // the anchor's own text, up to its close tag: comrak's wikilink text is the raw
            // target unless a `|label` pipe overrode it, in which case the override stands.
            let after_tag = &rest[end + 1..];
            let Some(close) = after_tag.find("</a>") else {
                out.push_str(tag); out.push('>'); rest = after_tag; continue;
            };
            let text = &after_tag[..close];
            let shown = if text == href { cite_label(&href[1..]) } else { text.to_string() };
            out.push_str(&format!("<span class=\"cite\" title=\"{}\">{}</span>", esc(&href), shown));
            rest = &after_tag[close + 4..];
            continue;
        }
        out.push_str(&tag[..hp]);
        out.push_str(" href=\"");
        if local {
            let (note, sec) = match href.split_once('#') { Some((n, s)) => (n, s), None => (href.as_str(), "") };
            // comrak has already percent-encoded the target; encoding it again gives %2520
            out.push_str(note_base);
            out.push_str(note);
            if !sec.is_empty() { out.push_str("?h="); out.push_str(sec); }
            out.push_str("\" class=\"wl\"");
        } else {
            out.push_str(&href);
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

    /// A vault of one or more notes, to assemble a real embed through `doc::assemble` and
    /// render it, rather than hand-building the sentinel text `wrap_embeds` expects.
    fn vault(name: &str, notes: &[(&str, &str)]) -> crate::cfg::Cfg {
        let d = std::env::temp_dir().join(format!("facet-md-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for (n, text) in notes { std::fs::write(d.join(format!("{}.md", n)), text).unwrap(); }
        crate::cfg::Cfg(serde_json::json!({"vault": d.to_string_lossy()}))
    }

    #[test]
    fn an_embed_renders_as_a_delimited_block_with_its_label_and_dimmed_close() {
        let cfg = vault("embed-render", &[("A", "# Statement\nA network $G$.\n")]);
        let (text, srcs) = crate::doc::assemble(&cfg, "Home", &["theorem::![[A#Statement]]"], 0);
        let html = render_at(&text, "/n/", &srcs);
        assert!(html.contains("<div class=embed>"), "the block gets a wrapper: {}", html);
        assert!(html.contains("<p class=envlabel>Theorem &middot; A</p>"),
            "the opening cue names the environment and the note: {}", html);
        assert!(html.contains("<span class=envclose>□</span>"), "the closing glyph is dimmed: {}", html);
        // no sentinel survives into the served page
        for c in ['\u{E001}', '\u{E002}', '\u{E003}', '\u{E004}'] {
            assert!(!html.contains(c), "sentinel {:?} leaked into: {}", c, html);
        }
        // the content inside still carries its click-to-comment provenance, from the note
        // the statement actually came from — the wrapper must not have swallowed it
        assert!(html.contains("data-note=\"A\""), "provenance survives the wrap: {}", html);
    }

    #[test]
    fn an_unlabelled_embed_is_still_delimited_by_its_section_name() {
        let cfg = vault("embed-plain", &[("A", "# Statement\nA network $G$.\n")]);
        let (text, srcs) = crate::doc::assemble(&cfg, "Home", &["![[A#Statement]]"], 0);
        let html = render_at(&text, "", &srcs);
        assert!(html.contains("<p class=envlabel>Statement &middot; A</p>"), "{}", html);
        assert!(html.contains("<span class=envclose>□</span>"), "{}", html);
    }

    #[test]
    fn nested_embeds_render_as_nested_wrappers_that_close_innermost_first() {
        let cfg = vault("embed-nest", &[
            ("A", "# Statement\nfrom A\n\nlemma::![[B#Statement]]\n"),
            ("B", "# Statement\nfrom B\n"),
        ]);
        let (text, srcs) = crate::doc::assemble(&cfg, "Home", &["theorem::![[A#Statement]]"], 0);
        let html = render_at(&text, "", &srcs);
        assert_eq!(html.matches("<div class=embed>").count(), 2, "one wrapper each for A and B: {}", html);
        let closes: Vec<usize> = html.match_indices("</div>").map(|(i, _)| i).collect();
        assert_eq!(closes.len(), 2, "one close each: {}", html);
        let open_a = html.find("Theorem &middot; A").unwrap();
        let open_b = html.find("Lemma &middot; B").unwrap();
        assert!(open_a < open_b, "A opens before the embed nested inside it: {}", html);
        assert!(open_b < closes[0] && closes[0] < closes[1],
            "B's wrapper closes before A's own, which closes last: {}", html);
    }

    #[test]
    fn backing_up_to_the_marker_s_p_tag_does_not_stop_at_a_lookalike() {
        // A hand-built fragment, not comrak's: the marker's own code span sits in a `<div>`,
        // not a `<p>` (comrak would always give it one, but the point is the backup must not
        // mistake `<pre` for `<p` and cut into the real content before it regardless). The
        // only "<p"-looking thing before the marker is inside `<pre>`; the fix must not stop
        // there, so none of "keep me" is lost.
        use crate::doc::{MARK_OPEN, MARK_SEP};
        let html = format!("<div><pre>unrelated code</pre>keep me<code>{}Theorem{}A</code></p></div>",
            MARK_OPEN, MARK_SEP);
        let out = wrap_embeds(&html);
        assert!(out.contains("keep me"), "real content before the marker survives: {}", out);
        assert!(out.contains("<pre>unrelated code</pre>"), "and so does the code block: {}", out);
    }

    #[test]
    fn a_citation_wikilink_gets_an_author_year_label_and_no_dead_link() {
        // ported from vault-phone's `render_wikilink`: `[[@key]]` names a citation, not a
        // note, so it must not 404 against `/n/`, and should read better than the raw key.
        let html = render("[[@gajjarSubspaceEmbeddingsNonlinear2020]]", "/n/");
        assert!(html.contains("class=\"cite\""), "{}", html);
        assert!(html.contains(">[Gajjar 2020]<"), "{}", html);
        assert!(!html.contains("href"), "a citation has nowhere to link to: {}", html);
        // a pipe-given label is kept, not overridden
        let html = render("[[@gajjarSubspaceEmbeddingsNonlinear2020|their Theorem 2]]", "/n/");
        assert!(html.contains(">their Theorem 2<"), "{}", html);
    }
}
