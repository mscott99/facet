// A Doc is a markdown file in the vault that carries a public name in its own frontmatter:
//
//     ---
//     facet: design
//     ---
//
// There is no registry file. The note knows it is published, Obsidian shows it, and you
// unpublish by deleting the line. Facet therefore owns no durable state about docs at all —
// it derives the slug table by walking the vault (cached for a few seconds), and reads the
// file on each request, so an edit shows up on the next poll without any copying.
use crate::cfg::Cfg;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub struct Doc {
    pub slug: String,
    pub title: String,
    pub path: PathBuf,
    pub text: String,
}

pub fn mtime(p: &Path) -> u64 {
    std::fs::metadata(p).ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs()).unwrap_or(0)
}

/// How many leading lines the frontmatter block occupies (0 if there is none). Rendering skips
/// them; diagnostic line numbers count them, so the count has to be available, not discarded.
pub fn front_len(text: &str) -> usize {
    if !text.starts_with("---\n") { return 0 }
    match text[4..].find("\n---") {
        Some(end) => text[..4 + end + 4].lines().count(),
        None => 0,
    }
}

pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() { out.extend(c.to_lowercase()) }
        else if !out.ends_with('-') { out.push('-') }
    }
    out.trim_matches('-').to_string()
}

/// The frontmatter block, if the file starts with one.
fn front(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

fn front_key(text: &str, key: &str) -> Option<String> {
    for line in front(text)?.lines() {
        if let Some(v) = line.strip_prefix(key).and_then(|r| r.strip_prefix(':')) {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            if !v.is_empty() { return Some(v.to_string()) }
        }
    }
    None
}

fn title_of(path: &Path, text: &str) -> String {
    if let Some(t) = front_key(text, "title") { return t }
    for line in text.lines().take(60) {
        if let Some(h) = line.strip_prefix("# ") { return h.trim().to_string() }
    }
    path.file_stem().unwrap_or_default().to_string_lossy().to_string()
}

/// Every markdown file of the vault. What is skipped is what is not prose: hidden
/// directories, the LaTeX exports, and the dependencies of the vault's own scripts.
fn files(cfg: &Cfg) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![cfg.vault()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name == "Exports" || name == "node_modules" { continue }
            if p.is_dir() { stack.push(p); continue }
            if p.extension().map(|x| x != "md").unwrap_or(true) { continue }
            out.push(p);
        }
    }
    out
}

/// Every published note: (slug, path). Reads only each file's head.
fn walk(cfg: &Cfg) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for p in files(cfg) {
        let Ok(head) = read_head(&p, 600) else { continue };
        if let Some(slug) = front_key(&head, "facet") { out.push((slug, p)); }
    }
    out.sort();
    out
}

fn read_head(p: &Path, n: usize) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(p)?;
    let mut buf = vec![0u8; n];
    let got = f.read(&mut buf)?;
    buf.truncate(got);
    Ok(String::from_utf8_lossy(&buf).to_string())
}

/// Long enough that one page does not walk the vault twice, short enough that an edit in
/// Obsidian shows up while you are still looking at it.
const TTL: Duration = Duration::from_secs(5);

/// A walk of the vault, kept for `TTL`. The vault it was walked for is part of it, so a
/// changed `vault` setting — or a test with a vault of its own — is never served the old one.
type Index = Mutex<Option<(Instant, PathBuf, Vec<(String, PathBuf)>)>>;

fn cache() -> &'static Index {
    static C: OnceLock<Index> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

/// The basename index: every note under the name a `[[wikilink]]` or an embed uses.
fn by_name() -> &'static Index {
    static C: OnceLock<Index> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

fn cached(c: &'static Index, cfg: &Cfg, walk: impl Fn(&Cfg) -> Vec<(String, PathBuf)>)
    -> Vec<(String, PathBuf)> {
    let mut g = c.lock().unwrap();
    if let Some((t, v, rows)) = g.as_ref() {
        if t.elapsed() < TTL && v == &cfg.vault() { return rows.clone() }
    }
    let rows = walk(cfg);
    *g = Some((Instant::now(), cfg.vault(), rows.clone()));
    rows
}

pub fn table(cfg: &Cfg) -> Vec<(String, PathBuf)> { cached(cache(), cfg, walk) }

pub fn forget() {
    *cache().lock().unwrap() = None;
    *by_name().lock().unwrap() = None;
}

/// Where the note of that basename is. Obsidian addresses a note by its name alone, wherever
/// in the vault the file sits, so a longform asks this once per embed: dozens per request.
pub fn find(cfg: &Cfg, name: &str) -> Option<PathBuf> {
    let name = name.trim();
    let rows = cached(by_name(), cfg, |cfg| files(cfg).into_iter()
        .map(|p| (p.file_stem().unwrap_or_default().to_string_lossy().to_string(), p))
        .collect());
    rows.iter().find(|(n, _)| n == name)
        .or_else(|| rows.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)))
        .map(|(_, p)| p.clone())
}

pub fn get(cfg: &Cfg, slug: &str) -> Option<Doc> {
    let (_, path) = table(cfg).into_iter().find(|(s, _)| s == slug)?;
    let text = std::fs::read_to_string(&path).ok()?;
    Some(Doc { slug: slug.into(), title: title_of(&path, &text), path, text })
}

/// Any note of the vault, by name rather than by slug: what a wikilink points at, read-only.
pub fn note(cfg: &Cfg, name: &str) -> Option<Doc> {
    let path = find(cfg, name)?;
    let text = std::fs::read_to_string(&path).ok()?;
    Some(Doc { slug: String::new(), title: title_of(&path, &text), path, text })
}

/// Publish: write `facet: <slug>` into the note's own frontmatter.
pub fn post(cfg: &Cfg, path: &Path, slug: Option<&str>) -> Result<String, String> {
    let path = path.canonicalize().map_err(|e| format!("{}: {}", path.display(), e))?;
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let slug = slug.map(|s| s.to_string()).unwrap_or_else(|| slugify(&stem));
    if let Some((s, p)) = table(cfg).into_iter().find(|(s, p)| s == &slug && p != &path) {
        return Err(format!("slug '{}' is already on {}", s, p.display()));
    }
    let new = match front(&text) {
        Some(f) if front_key(&text, "facet").is_some() => {
            let fixed: Vec<String> = f.lines().map(|l|
                if l.starts_with("facet:") { format!("facet: {}", slug) } else { l.to_string() }
            ).collect();
            text.replacen(f, &fixed.join("\n"), 1)
        }
        Some(f) => text.replacen(f, &format!("{}\nfacet: {}", f, slug), 1),
        None => format!("---\nfacet: {}\n---\n\n{}", slug, text.trim_start()),
    };
    write(&path, &new)?;
    forget();
    Ok(slug)
}

pub fn unpost(cfg: &Cfg, slug: &str) -> Result<(), String> {
    let (_, path) = table(cfg).into_iter().find(|(s, _)| s == slug)
        .ok_or_else(|| format!("no doc '{}'", slug))?;
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let kept: Vec<&str> = text.lines().filter(|l| !l.starts_with("facet:")).collect();
    write(&path, &(kept.join("\n") + "\n"))?;
    forget();
    Ok(())
}

/// Atomic enough for a vault that Obsidian also writes to: temp file, then rename.
pub fn write(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_file_name(format!(".{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()));
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// The index is a Doc too — generated, but rendered by the same function and shown on the
/// same page, so there is no second code path for it.
pub fn index(cfg: &Cfg) -> Doc {
    let mut rows: Vec<(String, String, u64, u64, usize)> = Vec::new();
    let open = crate::cards::open(cfg);
    for (slug, path) in table(cfg) {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let rel = path.strip_prefix(cfg.vault()).unwrap_or(&path).to_string_lossy().to_string();
        let nd = open.iter().filter(|c| c["note"] == rel.as_str()).count();
        rows.push((slug, title_of(&path, &text), mtime(&path), text.len() as u64, nd));
    }
    rows.sort_by(|a, b| b.2.cmp(&a.2));
    let mut md = String::from("# Notes\n\n");
    if rows.is_empty() { md.push_str("Nothing published yet: `facet post <file>`\n"); }
    for (slug, title, mt, size, nd) in &rows {
        let marks = if *nd > 0 { format!(" · {} comments", nd) } else { String::new() };
        // an absolute href: a relative one looks like a note name to the wikilink rewriter
        md.push_str(&format!("- [{}]({}/m/{}) · {} kB · {}{}\n",
            title, cfg.token_path(), slug, (size + 512) / 1024, crate::when(*mt), marks));
    }
    Doc { slug: String::new(), title: "Notes".into(), path: cfg.vault(), text: md }
}

// ---- sections and embeds: reading a longform ---------------------------------------------
//
// A longform keeps its prose in one note and every statement and proof in a note of its own,
// embedded on a line like
//
//     proposition::![[Range cover of a piecewise-linear network#Statement]]
//
// which markdown renders as literal text, the `!` stopping even the wikilink extension. So
// the published paper has to be assembled on the way out, by reading the embedded notes.

/// A header line: one to six `#`, a space, then text. Both this and the rule in `section`
/// are the vault's own, taken from its `Scripts/read_section_rust`.
fn header(line: &str) -> Option<(usize, &str)> {
    let t = line.trim_start();
    let level = t.chars().take_while(|&c| c == '#').count();
    if level == 0 || level > 6 { return None }
    let text = t[level..].strip_prefix(' ')?.trim();
    if text.is_empty() { None } else { Some((level, text)) }
}

/// The body under the header of that name, the header line itself excluded, ending at the
/// next header of the same or a higher level. The name is matched without case, as there.
/// (test-only: nothing in production calls this directly, only `section_at`.)
#[cfg(test)]
pub fn section(text: &str, want: &str) -> Option<String> { section_at(text, want).map(|(b, _)| b) }

/// The same, with the line the body starts on (1-based, blank lines at its head skipped as the
/// trim skips them): an assembled page says which line of which note each block came from.
pub fn section_at(text: &str, want: &str) -> Option<(String, usize)> {
    let lines: Vec<&str> = text.lines().collect();
    let (at, level) = lines.iter().enumerate().find_map(|(i, l)|
        header(l).filter(|(_, h)| h.to_lowercase() == want.trim().to_lowercase()).map(|(lv, _)| (i, lv)))?;
    let end = lines.iter().enumerate().skip(at + 1)
        .find(|(_, l)| header(l).map(|(lv, _)| lv <= level).unwrap_or(false))
        .map(|(i, _)| i).unwrap_or(lines.len());
    let body = &lines[at + 1..end];
    let skip = body.iter().position(|l| !l.trim().is_empty()).unwrap_or(0);
    Some((body.join("\n").trim().to_string(), at + 2 + skip))
}

/// One embed line. The `label::` before it and the `#Section` within it are both optional;
/// without a section the whole note is meant.
pub struct Embed {
    pub label: Option<String>,
    pub note: String,
    pub section: Option<String>,
}

/// An embed line, or nothing: the line must be the embed and no more, since a line with prose
/// around the embed is prose, and rewriting it would move what the reader is looking at.
pub fn embed(line: &str) -> Option<Embed> {
    let t = line.trim();
    let (label, rest) = match t.split_once("::") {
        Some((a, b)) if !a.is_empty() && !a.contains('[') => (Some(a.trim().to_string()), b),
        _ => (None, t),
    };
    let inner = rest.trim().strip_prefix("![[")?.strip_suffix("]]")?;
    let inner = inner.split('|').next().unwrap_or(inner);   // an alias names a link, not a block
    let (note, sec) = match inner.split_once('#') {
        Some((n, s)) => (n.trim(), Some(s.trim().to_string())),
        None => (inner.trim(), None),
    };
    if note.is_empty() { return None }
    Some(Embed { label, note: note.to_string(), section: sec.filter(|s| !s.is_empty()) })
}

/// How far embeds within embeds are followed. The vault nests one level in practice; this
/// leaves room for that and stops a chain of notes from costing a page its request.
const DEPTH: usize = 3;

/// An embed line expanded into the text it stands for. Any other line comes back unchanged,
/// so this can be mapped over a whole note without reading the note's structure.
/// (test-only: nothing in production calls this directly, only `assemble`.)
#[cfg(test)]
pub fn expand(cfg: &Cfg, line: &str) -> String {
    assemble(cfg, "", &[line], 0).0
}

/// Where one line of an assembled page came from: the note holding it, and its line number
/// there. An embed's lines answer with the embedded note and its own numbering, so a reader
/// who clicks a statement reaches the note that states it, not the longform that quotes it.
pub type Src = (String, usize);

/// A stretch of a note's lines with their embeds expanded, and one `Src` per line of the
/// result. `from` is the index of `lines[0]` in `home`, so the numbers are the file's own.
pub fn assemble(cfg: &Cfg, home: &str, lines: &[&str], from: usize) -> (String, Vec<Src>) {
    let mut out: Vec<(String, Src)> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        inline(cfg, l, &(home.to_string(), from + i + 1), 0, &mut Vec::new(), &mut out);
    }
    (out.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>().join("\n"),
     out.into_iter().map(|(_, s)| s).collect())
}

fn inline(cfg: &Cfg, line: &str, at: &Src, depth: usize, seen: &mut Vec<String>,
          out: &mut Vec<(String, Src)>) {
    let Some(e) = embed(line) else { return out.push((line.to_string(), at.clone())) };
    // A missing embed is a hole in a paper: say so where it is, rather than drop the line.
    let miss = |out: &mut Vec<(String, Src)>, why: &str| {
        out.push((String::new(), at.clone()));
        out.push((format!("`{}` — {}", line.trim(), why), at.clone()));
        out.push((String::new(), at.clone()));
    };
    if depth >= DEPTH { return miss(out, "nested too deep") }
    if seen.iter().any(|n| n == &e.note) { return miss(out, "embeds itself") }
    let Some(path) = find(cfg, &e.note) else { return miss(out, "no such note") };
    let Ok(text) = std::fs::read_to_string(&path) else { return miss(out, "note unreadable") };
    let (body, start) = match &e.section {
        Some(h) => match section_at(&text, h) { Some(b) => b, None => return miss(out, "no such section") },
        None => {
            let rest: Vec<&str> = text.lines().skip(front_len(&text)).collect();
            let skip = rest.iter().position(|l| !l.trim().is_empty()).unwrap_or(0);
            (rest.join("\n").trim().to_string(), front_len(&text) + skip + 1)
        }
    };
    if body.is_empty() { return miss(out, "empty") }
    seen.push(e.note.clone());
    let mut acc: Vec<(String, Src)> = Vec::new();
    for (j, l) in body.lines().enumerate() {
        inline(cfg, l, &(e.note.clone(), start + j), depth + 1, seen, &mut acc);
    }
    seen.pop();
    // what the trim did when a body was one string: a nested expansion pads its own ends
    while acc.first().is_some_and(|(t, _)| t.trim().is_empty()) { acc.remove(0); }
    while acc.last().is_some_and(|(t, _)| t.trim().is_empty()) { acc.pop(); }
    labelled(e.label.as_deref(), &mut acc);
    let proof = e.label.as_deref().is_some_and(|l| l.eq_ignore_ascii_case("proof"))
        || e.section.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("proof"));
    close(proof, &mut acc);
    let env = e.label.as_deref().map(cap).or_else(|| e.section.clone());
    // Blank lines around it: in a longform the embeds sit on consecutive lines, and two
    // statements with no blank line between them would render as one paragraph. The open
    // and close markers are each a paragraph of their own for the same reason.
    let blank = (String::new(), at.clone());
    out.push(blank.clone());
    out.push((open_marker(env.as_deref(), &e.note), at.clone()));
    out.push(blank.clone());
    out.append(&mut acc);
    out.push(blank.clone());
    out.push((close_marker(), at.clone()));
    out.push(blank);
}

/// The capitalized word a reader sees for a label or a section name: `"proof"` or `"Proof"`
/// alike become `"Proof"`.
fn cap(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// The label a reader sees: "Definition." in bold, run into the first line, which is how the
/// paper reads. A body that opens with a block of its own takes the label as its own line —
/// which belongs to the embedded note too, so the label is clickable like the rest of it.
fn labelled(label: Option<&str>, body: &mut Vec<(String, Src)>) {
    let (Some(label), Some(first)) = (label, body.first().cloned()) else { return };
    let name = cap(label);
    // A first line that is itself a nested embed's opening marker is run into nothing:
    // merging text into it would corrupt the marker `labelled` has no business touching.
    if first.0.starts_with(|c: char| "#->|*".contains(c)) || first.0.starts_with("$$")
        || first.0.contains(MARK_OPEN) {
        body.insert(0, (String::new(), first.1.clone()));
        body.insert(0, (format!("**{}.**", name), first.1));
    } else {
        body[0].0 = format!("**{}.** {}", name, first.0);
    }
}

/// Sentinels threading an embed's edges through plain markdown text: invisible to a reader,
/// each alone in a code span (so no markdown feature — emphasis, a link, a heading — can
/// touch the text riding with it), and gone by the time the page is served. `md::render_at`
/// finds them again in the rendered HTML and turns each into the block's rule, its quiet
/// opening label, and the `</div>` that closes it; `MARK_GLYPH` just dims the tombstone that
/// was already there. None of the four is a character a note would plausibly contain itself.
pub const MARK_OPEN: char = '\u{E001}';
pub const MARK_CLOSE: char = '\u{E002}';
pub const MARK_SEP: char = '\u{E003}';
pub const MARK_GLYPH: char = '\u{E004}';

/// The paragraph that opens a delimited embed: its environment name (a label, or failing
/// that a section name — "Lemma", "Proof", "Statement" — empty when the embed names neither)
/// and the note it came from.
fn open_marker(env: Option<&str>, note: &str) -> String {
    format!("`{}{}{}{}`", MARK_OPEN, env.unwrap_or(""), MARK_SEP, note)
}

fn close_marker() -> String { format!("`{}`", MARK_CLOSE) }

/// Where an embedded environment ends, for a reader who only sees it inlined and has no page
/// boundary to tell it apart from the prose around it. A proof earns the usual tombstone; any
/// other block (a theorem, a definition, even one named by neither a label nor a section) a
/// plainer mark — either way, appended to the last line it can safely join, or its own line
/// when that would break a fence or a display, and marked for `md::render_at` to dim.
fn close(proof: bool, body: &mut Vec<(String, Src)>) {
    let glyph = if proof { '∎' } else { '□' };
    let Some(last) = body.last_mut() else { return };
    // A last line that is itself a nested embed's closing marker is left untouched, same
    // reasoning as `labelled`'s: the mark gets its own line rather than joining that text.
    let risky = { let t = last.0.trim();
        t == "$$" || t.starts_with("```") || t.ends_with("```") || t.contains(MARK_CLOSE) };
    if risky {
        let src = last.1.clone();
        body.push((String::new(), src.clone()));
        body.push((format!("{}{}", MARK_GLYPH, glyph), src));
    } else {
        last.0.push_str(&format!(" {}{}", MARK_GLYPH, glyph));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTE: &str = "---\nctime: 2026-09-10\n---\n#theorem\n\nSome prose.\n\n\
# Statement\nLet $G$ be a network.\n\n## Aside\nStill the statement.\n\n\
# Proof\nBy induction.\n\n# Remark\nNot the proof.\n";

    #[test]
    fn section_is_the_body_under_its_header() {
        assert_eq!(section(NOTE, "Proof").unwrap(), "By induction.");
        // a deeper header belongs to the section; one of the same level ends it
        assert_eq!(section(NOTE, "Statement").unwrap(),
            "Let $G$ be a network.\n\n## Aside\nStill the statement.");
        assert_eq!(section(NOTE, "statement"), section(NOTE, "Statement"));
        assert!(section(NOTE, "Lemma").is_none());
        assert!(section("#notaheader\ntext", "notaheader").is_none());
    }

    #[test]
    fn an_embed_line_is_the_line_and_no_more() {
        let e = embed("proposition::![[Range cover#Statement]]").unwrap();
        assert_eq!((e.label.unwrap(), e.note, e.section.unwrap()),
            ("proposition".into(), "Range cover".into(), "Statement".into()));
        let e = embed("![[Gaussian measurements]]").unwrap();
        assert!(e.label.is_none() && e.section.is_none() && e.note == "Gaussian measurements");
        assert_eq!(embed("proof::![[A#Proof|short]]").unwrap().note, "A");
        assert!(embed("as in ![[A#Proof]], above").is_none());
        assert!(embed("[[A#Proof]]").is_none());
        assert!(embed("see http://example.com").is_none());
    }

    /// A vault of three notes that embed each other, to see the depth limit and the cycle
    /// stop. Its own directory, since the name index is cached per vault.
    fn vault(name: &str, notes: &[(&str, &str)]) -> Cfg {
        let d = std::env::temp_dir().join(format!("facet-doc-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for (n, text) in notes { std::fs::write(d.join(format!("{}.md", n)), text).unwrap(); }
        Cfg(serde_json::json!({"vault": d.to_string_lossy()}))
    }

    #[test]
    fn expansion_labels_the_block_and_stops_at_the_depth_limit() {
        let cfg = vault("depth", &[
            ("A", "# Statement\nfrom A\n\nlemma::![[B#Statement]]\n"),
            ("B", "# Statement\nfrom B\n\nlemma::![[C#Statement]]\n"),
            ("C", "# Statement\nfrom C\n\nlemma::![[A#Statement]]\n"),
        ]);
        let out = expand(&cfg, "proposition::![[A#Statement]]");
        assert!(out.contains(&format!("{}Proposition{}A", MARK_OPEN, MARK_SEP)),
            "the opening cue names the environment and the note: {}", out);
        assert!(out.contains("**Proposition.** from A"), "run-in label: {}", out);
        assert!(out.contains("**Lemma.** from B") && out.contains("**Lemma.** from C"));
        assert!(out.contains("nested too deep"), "the fourth level is refused: {}", out);
        assert!(!out.contains("from A\n\n**Lemma.** from B\n\n**Lemma.** from C\n\n**Lemma.** from A"));
    }

    #[test]
    fn assemble_tags_each_line_with_where_it_came_from() {
        // A slice starting at file line 5 (`from` 0-based = 4), of a note "A" whose second
        // line embeds a two-line section of a note "B" starting at B's own line 2.
        let cfg = vault("srcs", &[("B", "# Statement\nfirst\nsecond\n")]);
        let lines = ["intro line", "theorem::![[B#Statement]]"];
        let (text, srcs) = assemble(&cfg, "A", &lines, 4);
        let home = |n| ("A".to_string(), n);
        let there = |n| ("B".to_string(), n);
        // every blank and marker around the embed is tagged with the embed line itself (6)
        assert_eq!(srcs, vec![home(5), home(6), home(6), home(6), there(2), there(3), home(6), home(6), home(6)]);
        let got: Vec<&str> = text.lines().collect();
        assert_eq!(got[0], "intro line");
        assert!(got[2].contains(&format!("{}Theorem{}B", MARK_OPEN, MARK_SEP)), "open marker: {}", got[2]);
        assert_eq!(got[4], "**Theorem.** first");
        assert_eq!(got[5], format!("second {}□", MARK_GLYPH));   // the close mark lands on B's own last line
        assert!(got[7].contains(MARK_CLOSE), "close marker: {}", got[7]);
    }

    #[test]
    fn every_embed_is_delimited_and_closed_even_without_a_label() {
        let cfg = vault("closed", &[
            ("A", "# Statement\nA network $G$.\n"),
            ("B", "# Proof\nBy induction on depth.\n"),
        ]);
        let thm = expand(&cfg, "theorem::![[A#Statement]]");
        assert!(thm.contains(&format!("A network $G$. {}□", MARK_GLYPH)), "theorem gets a plain mark: {}", thm);
        assert!(thm.contains(&format!("{}Theorem{}A", MARK_OPEN, MARK_SEP)), "opening cue: {}", thm);
        assert!(thm.contains(MARK_CLOSE), "closing cue: {}", thm);
        let proof = expand(&cfg, "proof::![[B#Proof]]");
        assert!(proof.contains(&format!("By induction on depth. {}∎", MARK_GLYPH)), "proof gets a tombstone: {}", proof);
        // an unlabelled embed has no label to run in, but still gets delimited and closed —
        // named by its section instead
        let cfg2 = vault("closed-plain", &[("A", "# Statement\nA network $G$.\n")]);
        let plain = expand(&cfg2, "![[A#Statement]]");
        assert!(plain.contains(&format!("{}□", MARK_GLYPH)), "an unlabelled embed still closes: {}", plain);
        assert!(plain.contains(&format!("{}Statement{}A", MARK_OPEN, MARK_SEP)), "named by its section: {}", plain);
    }

    #[test]
    fn nested_embeds_each_get_their_own_markers_correctly_ordered() {
        let cfg = vault("nest-marks", &[
            ("A", "# Statement\nfrom A\n\nlemma::![[B#Statement]]\n"),
            ("B", "# Statement\nfrom B\n"),
        ]);
        let out = expand(&cfg, "theorem::![[A#Statement]]");
        let open_a = out.find(&format!("{}Theorem{}A", MARK_OPEN, MARK_SEP)).expect("A's open marker");
        let open_b = out.find(&format!("{}Lemma{}B", MARK_OPEN, MARK_SEP)).expect("B's open marker");
        let close_b = out[open_b..].find(MARK_CLOSE).map(|i| i + open_b).expect("B's close marker");
        let after_b = close_b + MARK_CLOSE.len_utf8();
        let close_a = out[after_b..].find(MARK_CLOSE).map(|i| i + after_b).expect("A's close marker");
        assert!(open_a < open_b && open_b < close_b && close_b < close_a,
            "B nests inside A, opened after and closed before it: {}", out);
    }

    #[test]
    fn a_missing_or_circular_embed_is_shown_not_dropped() {
        let cfg = vault("missing", &[("A", "# Statement\nfrom A\n\n![[A]]\n")]);
        assert!(expand(&cfg, "![[Nowhere#Statement]]").contains("no such note"));
        assert!(expand(&cfg, "![[A#Lemma]]").contains("no such section"));
        assert!(expand(&cfg, "![[A]]").contains("embeds itself"));
        assert_eq!(expand(&cfg, "ordinary prose"), "ordinary prose");
    }
}
