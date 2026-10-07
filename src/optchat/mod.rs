// The memory: an implementation of Taelin's OptChat gist
// (gist.github.com/VictorTaelin/91837951a5ce5b38f341ec1ba1df6449), driving `claude -p`.
// Section numbers in comments (§n) refer to that gist. Departures are listed in README.md.
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
//   agent    a subagent spawned to outlive its turn: its own `claude -p`, owned by the
//            engine, not the turn (§9, detached)
pub mod agent;
pub mod browse;
pub mod claude;
pub mod compact;
pub mod engine;
pub mod events;
pub mod import;
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
/// Where the view is cut for the cache marks. Tied to the budget, the last one just under it,
/// so that a view sitting at its budget is almost entirely inside the cacheable prefix (§8).
pub const MARKS: [usize; 3] = [VIEW * 3 / 8, VIEW * 5 / 8, VIEW * 15 / 16];

/// The outer limit on the view: appends may carry it this far past its budget before anything
/// collapses, and a collapse then takes it back to the budget in one batch. One limit instead
/// of two means a collapse on nearly every append, each one reshaping the front of the view and
/// so throwing away the compactor's cached prefix. The view holds its full budget of history
/// either way; it is simply allowed to drift a little above it between collapses.
pub const fn over(budget: usize) -> usize { budget * 27 / 25 }

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
