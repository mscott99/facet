// The memory: an implementation of Taelin's OptChat gist
// (gist.github.com/VictorTaelin/91837951a5ce5b38f341ec1ba1df6449), driving `claude -p`.
// Section numbers in comments (§n) refer to that gist; some older comments still use the
// numbering of its first version (before 2026-10-08). Departures are listed in README.md.
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

// Constants. Sizes are UTF-8 bytes.
pub const NODE: usize = 512;
pub const VIEW: usize = 128_000;
pub const JOBS: usize = 8;
pub const TRIES: usize = 5;
/// A compaction's own view (§4 of the gist): the chat's view merged further, to between half
/// of this and this (16-32 KB), with the same sawtooth.
pub const CVIEW: usize = 32_000;
/// A message's node starts once fewer than this many lines before it are still unbuilt (§4).
pub const AHEAD: usize = 8;
pub const CAP: usize = 30_000;
/// Lines per cache block (§8 of the gist): the view goes out as blocks of BLOCK lines, one
/// cache mark on the last whole block and one on the request's end. The API looks back up to
/// 20 blocks from a mark for an earlier entry, so the next call pays only for the lines after
/// the previous call's mark.
pub const BLOCK: usize = 4;

/// What a batch cuts the view back to once it passes its budget: half of it, as in the gist
/// (128 KB down to 64 KB).
pub const fn inner(budget: usize) -> usize { budget / 2 }

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
