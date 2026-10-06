// §2 Storage. Two append-only streams, one line per record, each written with one write
// and an fsync before the function returns. Nothing here ever edits or deletes a line.
//
//   chat/main/YYYY-MM-DD.jsonl   {i, kind, text, size, date}
//   chat/tree/YYYY-MM-DD.jsonl   {l, i, text, size}
use serde_json::json;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Msg {
    pub kind: String,
    pub text: String,
    pub date: String,
}

pub struct Store {
    pub dir: PathBuf,
    pub msgs: Vec<Msg>,
    /// levels[l][i] = text of node (l, i), if built
    pub levels: Vec<Vec<Option<String>>>,
    /// problems found at load (torn lines, gaps): reported once, never fatal
    pub notes: Vec<String>,
}

pub fn now_iso() -> String { chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string() }
fn today() -> String { chrono::Local::now().format("%Y-%m-%d").to_string() }

/// One write, then fsync. A crash loses nothing that this returned from.
fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(d) = path.parent() { fs::create_dir_all(d)?; }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    let mut buf = Vec::with_capacity(line.len() + 1);
    buf.extend_from_slice(line.as_bytes());
    buf.push(b'\n');
    f.write_all(&buf)?;
    f.sync_data()
}

/// Every line of every `*.jsonl` in `dir`, in file order. A file that does not end in `\n`
/// (a crash mid-write) gets one, so the next write starts on its own line.
fn read_lines(dir: &Path, notes: &mut Vec<String>) -> Vec<serde_json::Value> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir).into_iter().flatten().flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect();
    files.sort();
    let mut out = Vec::new();
    for f in files {
        let Ok(body) = fs::read_to_string(&f) else { notes.push(format!("{}: unreadable", f.display())); continue };
        if !body.is_empty() && !body.ends_with('\n') {
            notes.push(format!("{}: torn last line, newline appended", f.display()));
            let _ = OpenOptions::new().append(true).open(&f).and_then(|mut h| { h.write_all(b"\n")?; h.sync_data() });
        }
        for (n, line) in body.lines().enumerate() {
            if line.trim().is_empty() { continue }
            match serde_json::from_str::<serde_json::Value>(line) {
                Ok(v) => out.push(v),
                Err(_) => notes.push(format!("{}:{}: not JSON, skipped", f.display(), n + 1)),
            }
        }
    }
    out
}

impl Store {
    pub fn open(dir: &Path) -> Store {
        let mut notes = Vec::new();
        let mut msgs: Vec<Option<Msg>> = Vec::new();
        for v in read_lines(&dir.join("chat/main"), &mut notes) {
            let (Some(i), Some(kind), Some(text)) = (v["i"].as_u64(), v["kind"].as_str(), v["text"].as_str()) else {
                notes.push(format!("message without i/kind/text: {}", crate::optchat::cut_bytes(&v.to_string(), 80)));
                continue;
            };
            let i = i as usize;
            if msgs.len() <= i { msgs.resize(i + 1, None); }
            if msgs[i].is_some() { notes.push(format!("message {} appears twice; the first is kept", i)); continue }
            msgs[i] = Some(Msg { kind: kind.into(), text: text.into(), date: v["date"].as_str().unwrap_or("").into() });
        }
        // ids are permanent and dense; a hole means lost data, and nothing after it can be placed
        let mut dense = Vec::with_capacity(msgs.len());
        for (i, m) in msgs.into_iter().enumerate() {
            match m {
                Some(m) => dense.push(m),
                None => { notes.push(format!("message {} is missing: the log is read up to it", i)); break }
            }
        }
        let mut s = Store { dir: dir.to_path_buf(), msgs: dense, levels: Vec::new(), notes: Vec::new() };
        for v in read_lines(&dir.join("chat/tree"), &mut notes) {
            let (Some(l), Some(i), Some(text)) = (v["l"].as_u64(), v["i"].as_u64(), v["text"].as_str()) else { continue };
            if ((i + 1) << l) as usize > s.msgs.len() { continue } // covers messages that are not there
            s.set(l as usize, i as usize, text.to_string());
        }
        s.notes = notes;
        s
    }

    pub fn t(&self) -> usize { self.msgs.len() }

    pub fn node(&self, l: usize, i: usize) -> Option<&str> {
        self.levels.get(l)?.get(i)?.as_deref()
    }
    pub fn built(&self, l: usize, i: usize) -> bool { self.node(l, i).is_some() }

    pub(crate) fn set(&mut self, l: usize, i: usize, text: String) {
        if self.levels.len() <= l { self.levels.resize(l + 1, Vec::new()); }
        let lv = &mut self.levels[l];
        if lv.len() <= i { lv.resize(i + 1, None); }
        if lv[i].is_none() { lv[i] = Some(text); }
    }

    /// Append message `i = T`. Returns its id.
    pub fn log(&mut self, kind: &str, text: &str) -> std::io::Result<usize> { self.log_at(kind, text, &now_iso()) }

    /// Append with a given date (an imported note keeps the date it was written).
    pub fn log_at(&mut self, kind: &str, text: &str, date: &str) -> std::io::Result<usize> {
        let i = self.msgs.len();
        let size = kind.len() + 2 + text.len();
        let date = date.to_string();
        let line = json!({"i": i, "kind": kind, "text": text, "size": size, "date": date}).to_string();
        append_line(&self.dir.join("chat/main").join(format!("{}.jsonl", today())), &line)?;
        self.msgs.push(Msg { kind: kind.into(), text: text.into(), date });
        Ok(i)
    }

    /// Save node (l, i). A node is written once; a second build of it is dropped.
    pub fn put(&mut self, l: usize, i: usize, text: &str) -> std::io::Result<()> {
        if self.built(l, i) { return Ok(()) }
        let line = json!({"l": l, "i": i, "text": text, "size": text.len()}).to_string();
        append_line(&self.dir.join("chat/tree").join(format!("{}.jsonl", today())), &line)?;
        self.set(l, i, text.to_string());
        Ok(())
    }

    /// The source of node (l, i) if it already fits in NODE bytes: then it IS the node (§3).
    pub fn free(&self, l: usize, i: usize) -> Option<String> {
        let s = if l == 0 {
            let m = &self.msgs[i];
            format!("{}: {}", m.kind, m.text)
        } else {
            format!("{}\n{}", self.node(l - 1, 2 * i)?, self.node(l - 1, 2 * i + 1)?)
        };
        (s.len() <= super::NODE).then_some(s)
    }

    /// Has its sources: the message, or both children.
    pub fn ready(&self, l: usize, i: usize) -> bool {
        if l == 0 { i < self.t() } else { self.built(l - 1, 2 * i) && self.built(l - 1, 2 * i + 1) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("facet-test-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn log_put_reload_and_torn_lines() {
        let d = tmp("store");
        let mut s = Store::open(&d);
        assert_eq!(s.log("user", "hello").unwrap(), 0);
        assert_eq!(s.log("talk", "hé").unwrap(), 1);
        s.put(0, 0, "user: hello").unwrap();
        // a torn line at the end of today's file
        let f = d.join("chat/main").join(format!("{}.jsonl", today()));
        OpenOptions::new().append(true).open(&f).unwrap().write_all(b"{\"i\":2,\"ki").unwrap();
        let mut s = Store::open(&d);
        assert_eq!(s.t(), 2);
        assert_eq!(s.notes.len(), 2, "{:?}", s.notes); // torn + not JSON
        assert_eq!(s.node(0, 0), Some("user: hello"));
        // the next write starts on its own line
        assert_eq!(s.log("user", "again").unwrap(), 2);
        let s = Store::open(&d);
        assert_eq!(s.t(), 3);
        assert_eq!(s.msgs[2].text, "again");
        let line = fs::read_to_string(&f).unwrap().lines().find(|l| l.contains("\"i\":1")).unwrap().to_string();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["size"], 9); // "talk: hé": é is 2 bytes
    }

    #[test]
    fn free_nodes() {
        let d = tmp("free");
        let mut s = Store::open(&d);
        s.log("user", "a").unwrap();
        s.log("user", &"x".repeat(600)).unwrap();
        assert_eq!(s.free(0, 0).as_deref(), Some("user: a"));
        assert_eq!(s.free(0, 1), None);
        s.put(0, 0, "user: a").unwrap();
        s.put(0, 1, "user: long x").unwrap();
        assert_eq!(s.free(1, 0).as_deref(), Some("user: a\nuser: long x"));
    }
}
