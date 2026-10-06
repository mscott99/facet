// The memory: an implementation of Taelin's OptChat gist
// (gist.github.com/VictorTaelin/91837951a5ce5b38f341ec1ba1df6449), driving `claude -p`.
// Section numbers in comments (§n) refer to that gist. Deviations are listed in DEVIATIONS.md.
//
//   store    the log and the tree, append-only JSONL, fsync per line (§2)
//   view     the fold: append, then merge the most due pair (§5)
//   compact  the pump and the node builder (§4)
//   claude   one `claude -p` process, stream-json in and out
//   turn     the turn loop, priming, mid-run messages (§6, §7, §8)
//   mcp      zoom and date, served over HTTP from this process (§7.1)
//   engine   the one process that owns the chat: lock socket, clients, state
//   usage    one line per API request, and the tables made from them
//   browse   the whole tree as one HTML page (§10)
//   events   introspection log, outside the chat directory (never read back)
pub mod browse;
pub mod claude;
pub mod compact;
pub mod engine;
pub mod events;
pub mod mcp;
pub mod prompts;
pub mod store;
pub mod turn;
pub mod usage;
pub mod view;

use std::time::Duration;

// §1 constants. Sizes are UTF-8 bytes; the cache marks are characters.
pub const NODE: usize = 512;
pub const VIEW: usize = 128_000;
pub const JOBS: usize = 8;
pub const TRIES: usize = 5;
pub const RETRY: Duration = Duration::from_secs(10);
pub const CAP: usize = 30_000;
pub const MARKS: [usize; 3] = [50_000, 80_000, 100_000];

/// A compactor call that has produced no result by then is failed like any other.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(180);
/// How long a cache entry is trusted to be alive (5 min TTL, minus a margin).
pub const WARM: Duration = Duration::from_secs(270);

/// Cut `s` at a byte offset without splitting a UTF-8 character.
pub fn cut_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max { return s }
    let mut k = max;
    while !s.is_char_boundary(k) { k -= 1; }
    &s[..k]
}

/// Newlines shown as single spaces: how a node's text sits on one line.
pub fn flat(s: &str) -> String { s.replace('\n', " ") }
