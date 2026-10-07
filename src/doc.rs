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
    pub mtime: u64,
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
    Some(Doc { slug: slug.into(), title: title_of(&path, &text), mtime: mtime(&path), path, text })
}

/// Any note of the vault, by name rather than by slug: what a wikilink points at, read-only.
pub fn note(cfg: &Cfg, name: &str) -> Option<Doc> {
    let path = find(cfg, name)?;
    let text = std::fs::read_to_string(&path).ok()?;
    Some(Doc { slug: String::new(), title: title_of(&path, &text), mtime: mtime(&path), path, text })
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
    for (slug, path) in table(cfg) {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let nd = crate::diag::for_note(cfg, &path).len();
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
    Doc { slug: String::new(), title: "Notes".into(), path: cfg.vault(), text: md, mtime: 0 }
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
pub fn section(text: &str, want: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let (at, level) = lines.iter().enumerate().find_map(|(i, l)|
        header(l).filter(|(_, h)| h.to_lowercase() == want.trim().to_lowercase()).map(|(lv, _)| (i, lv)))?;
    let end = lines.iter().enumerate().skip(at + 1)
        .find(|(_, l)| header(l).map(|(lv, _)| lv <= level).unwrap_or(false))
        .map(|(i, _)| i).unwrap_or(lines.len());
    Some(lines[at + 1..end].join("\n").trim().to_string())
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
pub fn expand(cfg: &Cfg, line: &str) -> String {
    inline(cfg, line, 0, &mut Vec::new())
}

fn inline(cfg: &Cfg, line: &str, depth: usize, seen: &mut Vec<String>) -> String {
    let Some(e) = embed(line) else { return line.to_string() };
    // A missing embed is a hole in a paper: say so where it is, rather than drop the line.
    let miss = |why: &str| format!("\n`{}` — {}\n", line.trim(), why);
    if depth >= DEPTH { return miss("nested too deep") }
    if seen.iter().any(|n| n == &e.note) { return miss("embeds itself") }
    let Some(path) = find(cfg, &e.note) else { return miss("no such note") };
    let Ok(text) = std::fs::read_to_string(&path) else { return miss("note unreadable") };
    let body = match &e.section {
        Some(h) => match section(&text, h) { Some(b) => b, None => return miss("no such section") },
        None => text.lines().skip(front_len(&text)).collect::<Vec<_>>().join("\n").trim().to_string(),
    };
    if body.is_empty() { return miss("empty") }
    seen.push(e.note.clone());
    let body: Vec<String> = body.lines().map(|l| inline(cfg, l, depth + 1, seen)).collect();
    seen.pop();
    let body = labelled(e.label.as_deref(), body.join("\n").trim());
    // Blank lines around it: in a longform the embeds sit on consecutive lines, and two
    // statements with no blank line between them would render as one paragraph.
    format!("\n{}\n", body)
}

/// The label a reader sees: "Definition." in bold, run into the first line, which is how the
/// paper reads. A body that opens with a block of its own takes the label as its own line.
fn labelled(label: Option<&str>, body: &str) -> String {
    let Some(label) = label else { return body.to_string() };
    let mut c = label.chars();
    let name: String = c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default();
    if body.starts_with(|c: char| "#->|*".contains(c)) || body.starts_with("$$") {
        format!("**{}.**\n\n{}", name, body)
    } else {
        format!("**{}.** {}", name, body)
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
        assert!(out.starts_with("\n**Proposition.** from A"), "run-in label: {}", out);
        assert!(out.contains("**Lemma.** from B") && out.contains("**Lemma.** from C"));
        assert!(out.contains("nested too deep"), "the fourth level is refused: {}", out);
        assert!(!out.contains("from A\n\n**Lemma.** from B\n\n**Lemma.** from C\n\n**Lemma.** from A"));
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
