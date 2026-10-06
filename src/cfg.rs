// Configuration: one JSON file, read fresh when asked. No derive macros, no schema —
// a Value plus typed accessors, so adding a key is a one-line change here and nowhere else.
use serde_json::Value;
use std::path::{Path, PathBuf};

pub fn home() -> PathBuf { PathBuf::from(std::env::var("HOME").expect("HOME")) }
pub fn dir() -> PathBuf { home().join(".config/facet") }
pub fn tilde(s: &str) -> PathBuf {
    if let Some(r) = s.strip_prefix("~/") { home().join(r) } else { PathBuf::from(s) }
}

#[derive(Clone)]
pub struct Cfg(pub Value);

impl Cfg {
    pub fn load() -> Cfg {
        let p = dir().join("facet.json");
        let v = std::fs::read_to_string(&p).ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Object(Default::default()));
        Cfg(v)
    }
    pub fn save(&self) {
        let p = dir().join("facet.json");
        let _ = std::fs::create_dir_all(dir());
        let _ = std::fs::write(&p, serde_json::to_string_pretty(&self.0).unwrap() + "\n");
        let _ = std::process::Command::new("/bin/chmod").arg("600").arg(&p).status();
    }
    fn get(&self, k: &str) -> Option<&Value> {
        let mut cur = &self.0;
        for part in k.split('.') { cur = cur.get(part)?; }
        Some(cur)
    }
    pub fn str(&self, k: &str, dflt: &str) -> String {
        self.get(k).and_then(|v| v.as_str()).unwrap_or(dflt).to_string()
    }
    pub fn opt(&self, k: &str) -> Option<String> {
        self.get(k).and_then(|v| v.as_str()).map(|s| s.to_string()).filter(|s| !s.is_empty())
    }
    pub fn num(&self, k: &str, dflt: i64) -> i64 { self.get(k).and_then(|v| v.as_i64()).unwrap_or(dflt) }
    pub fn set(&mut self, k: &str, v: Value) {
        let parts: Vec<&str> = k.split('.').collect();
        let mut cur = &mut self.0;
        for p in &parts[..parts.len() - 1] {
            if !cur.get(*p).map(|x| x.is_object()).unwrap_or(false) {
                cur[*p] = Value::Object(Default::default());
            }
            cur = cur.get_mut(*p).unwrap();
        }
        cur[parts[parts.len() - 1]] = v;
    }

    // the handful of things the rest of the program asks for
    pub fn store(&self) -> PathBuf { tilde(&self.str("store", "~/.optchat")) }
    pub fn vault(&self) -> PathBuf { tilde(&self.str("vault", "~/Obsidian/myVault")) }
    pub fn token(&self) -> String { self.str("token", "") }
    pub fn host(&self) -> String { self.str("host", "127.0.0.1") }
    pub fn port(&self) -> u16 { self.num("port", 8730) as u16 }
    pub fn base(&self) -> String { self.str("base_url", "").trim_end_matches('/').to_string() }
    pub fn url(&self, tail: &str) -> String {
        let b = self.base();
        let b = if b.is_empty() { format!("http://{}:{}", self.host(), self.port()) } else { b };
        format!("{}/{}{}", b, self.token(), tail)
    }
    pub fn vault_phone(&self) -> String { self.str("vault_phone", "").trim_end_matches('/').to_string() }
    pub fn terminal(&self) -> String { self.str("terminal_url", "").trim_end_matches('/').to_string() }
}

// state: what has already been pushed, where the Telegram cursor is. Written often, never precious.
pub fn state() -> Value {
    std::fs::read_to_string(dir().join("state.json")).ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Object(Default::default()))
}
pub fn put_state(v: &Value) {
    let _ = std::fs::create_dir_all(dir());
    let _ = std::fs::write(dir().join("state.json"), serde_json::to_string_pretty(v).unwrap());
}
pub fn data_dir() -> PathBuf { home().join(".local/share/facet") }
pub fn exists(p: &Path) -> bool { p.exists() }

impl Cfg {
    /// The token as a path prefix: every URL in every page starts with this.
    pub fn token_path(&self) -> String { format!("/{}", self.token()) }
}
