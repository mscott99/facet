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

/// Every published note: (slug, path). Walks the vault reading only each file's head.
fn walk(cfg: &Cfg) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let mut stack = vec![cfg.vault()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name == "Exports" || name == "node_modules" { continue }
            if p.is_dir() { stack.push(p); continue }
            if p.extension().map(|x| x != "md").unwrap_or(true) { continue }
            let Ok(head) = read_head(&p, 600) else { continue };
            if let Some(slug) = front_key(&head, "facet") { out.push((slug, p)); }
        }
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

fn cache() -> &'static Mutex<Option<(Instant, Vec<(String, PathBuf)>)>> {
    static C: OnceLock<Mutex<Option<(Instant, Vec<(String, PathBuf)>)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

pub fn table(cfg: &Cfg) -> Vec<(String, PathBuf)> {
    let mut g = cache().lock().unwrap();
    if let Some((t, v)) = g.as_ref() {
        if t.elapsed() < Duration::from_secs(5) { return v.clone() }
    }
    let v = walk(cfg);
    *g = Some((Instant::now(), v.clone()));
    v
}

pub fn forget() { *cache().lock().unwrap() = None; }

pub fn get(cfg: &Cfg, slug: &str) -> Option<Doc> {
    let (_, path) = table(cfg).into_iter().find(|(s, _)| s == slug)?;
    let text = std::fs::read_to_string(&path).ok()?;
    Some(Doc { slug: slug.into(), title: title_of(&path, &text), mtime: mtime(&path), path, text })
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
        md.push_str(&format!("- [{}]({}/m/{}) · {} kB · {}{}\n",
            title, cfg.token(), slug, (size + 512) / 1024, crate::when(*mt), marks));
    }
    Doc { slug: String::new(), title: "Notes".into(), path: cfg.vault(), text: md, mtime: 0 }
}
