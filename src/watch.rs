//! Wake a waiting note page the moment a file in the vault changes, whoever changed it.
//!
//! Linux only: one inotify thread per vault watches every directory under it (new ones too)
//! and bumps a counter, waking anyone in `wait`. Elsewhere, or if inotify can't be had,
//! `wait` is a plain sleep, and pages fall back to checking every so often.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

struct Bell { n: Mutex<u64>, cv: Condvar, vaults: Mutex<HashSet<PathBuf>> }

fn bell() -> &'static Bell {
    static B: OnceLock<Bell> = OnceLock::new();
    B.get_or_init(|| Bell { n: Mutex::new(0), cv: Condvar::new(), vaults: Mutex::new(HashSet::new()) })
}

/// Make sure `vault` is watched (once per vault per process).
pub fn ensure(vault: &Path) {
    let b = bell();
    if !b.vaults.lock().unwrap().insert(vault.to_path_buf()) { return }
    #[cfg(target_os = "linux")]
    {
        let v = vault.to_path_buf();
        std::thread::spawn(move || {
            if let Err(e) = linux::run(&v) {
                eprintln!("watch {}: {} (pages fall back to polling)", v.display(), e);
                bell().vaults.lock().unwrap().remove(&v);
            }
        });
    }
}

/// Block until something changes after `seen`, or `max` passes. Returns the new count.
pub fn wait(seen: u64, max: Duration) -> u64 {
    let b = bell();
    let g = b.n.lock().unwrap();
    let (g, _) = b.cv.wait_timeout_while(g, max, |n| *n == seen).unwrap();
    *g
}

/// The current count, to pass to `wait`.
pub fn now() -> u64 { *bell().n.lock().unwrap() }

fn ring() { let b = bell(); *b.n.lock().unwrap() += 1; b.cv.notify_all(); }

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashMap;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    unsafe extern "C" {
        fn inotify_init1(flags: i32) -> i32;
        fn inotify_add_watch(fd: i32, path: *const i8, mask: u32) -> i32;
    }
    const IN_MODIFY: u32 = 0x2; const IN_CLOSE_WRITE: u32 = 0x8; const IN_MOVED_FROM: u32 = 0x40;
    const IN_MOVED_TO: u32 = 0x80; const IN_CREATE: u32 = 0x100; const IN_DELETE: u32 = 0x200;
    const IN_ISDIR: u32 = 0x4000_0000; const IN_CLOEXEC: i32 = 0o2000000;
    const MASK: u32 = IN_MODIFY | IN_CLOSE_WRITE | IN_MOVED_FROM | IN_MOVED_TO | IN_CREATE | IN_DELETE;

    fn skip(p: &Path) -> bool {
        p.file_name().map_or(false, |n| { let n = n.to_string_lossy(); n == ".git" || n == ".trash" || n == "node_modules" })
    }

    fn add(fd: i32, dir: &Path, wds: &mut HashMap<i32, PathBuf>) {
        if skip(dir) { return }
        let Ok(c) = CString::new(dir.as_os_str().as_bytes()) else { return };
        let wd = unsafe { inotify_add_watch(fd, c.as_ptr(), MASK) };
        if wd >= 0 { wds.insert(wd, dir.to_path_buf()); }
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                if e.file_type().map_or(false, |t| t.is_dir()) { add(fd, &e.path(), wds) }
            }
        }
    }

    pub fn run(vault: &Path) -> Result<(), String> {
        let fd = unsafe { inotify_init1(IN_CLOEXEC) };
        if fd < 0 { return Err(std::io::Error::last_os_error().to_string()) }
        let mut wds = HashMap::new();
        add(fd, vault, &mut wds);
        if wds.is_empty() { return Err("no directory could be watched".into()) }
        use std::io::Read;
        use std::os::fd::FromRawFd;
        let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = match f.read(&mut buf) {
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.to_string()),
            };
            let (mut i, mut names) = (0usize, false);
            // struct inotify_event { int wd; u32 mask; u32 cookie; u32 len; char name[len]; }
            while i + 16 <= n {
                let wd = i32::from_ne_bytes(buf[i..i + 4].try_into().unwrap());
                let mask = u32::from_ne_bytes(buf[i + 4..i + 8].try_into().unwrap());
                let len = u32::from_ne_bytes(buf[i + 12..i + 16].try_into().unwrap()) as usize;
                let name = &buf[i + 16..(i + 16 + len).min(n)];
                let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
                // a file appearing, going or renamed: the name index is stale at once
                if mask & (IN_CREATE | IN_DELETE | IN_MOVED_FROM | IN_MOVED_TO) != 0 { names = true }
                if mask & IN_ISDIR != 0 && mask & (IN_CREATE | IN_MOVED_TO) != 0 {
                    if let Some(d) = wds.get(&wd).cloned() {
                        add(fd, &d.join(std::ffi::OsStr::from_bytes(name)), &mut wds);
                    }
                }
                i += 16 + len;
            }
            if names { crate::doc::forget() }
            super::ring();
        }
    }
}
