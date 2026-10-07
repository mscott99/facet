# facet

Facet is the agent: a memory, and the input and output through which it meets the world.
Both halves live in this crate. The memory is the OptChat engine in `src/optchat/`, a Rust
implementation of Taelin's OptChat gist
(gist.github.com/VictorTaelin/91837951a5ce5b38f341ec1ba1df6449) driving `claude -p`; every
place it departs from the gist is listed below, under **Departures from the gist**. The engine
owns `~/.optchat`. Everything else only reads the log, and reaches the engine through its
Unix socket.

    optchat/  the engine: store, view fold, compactor, turn loop, zoom/date over MCP,
              the lock socket every route talks to, browse, import, introspection log
    tui.rs    the terminal route: `facet chat`

    log.rs    the memory, read-only: a fold over ~/.optchat/chat/main/*.jsonl
    doc.rs    output addressed by a slug — a vault note, published via its own frontmatter
    diag.rs   output addressed by a code, anchored to a live line — a comment, triaged
    tell.rs   input: one funnel, from any route, into the engine socket
    md.rs     one markdown renderer (comrak: math and wikilinks are parsed, never regexed)
    web.rs    the route with a screen (server-rendered HTML + HTMX)
    tg.rs     the route with push (Telegram)
    cfg.rs    one JSON file at ~/.config/facet/facet.json
    main.rs   the CLI

## run

Everything runs from this repository, as macOS LaunchAgents: the plists in `launchd/` (edit
the paths in them for your machine, copy them to `~/Library/LaunchAgents/`, `launchctl load`
them). They expect `facet` and the scripts in `bin/` on `~/.local/bin` (symlinks into
`target/release/` and `bin/`).

    com.facet.engine   facet engine            the memory; owns ~/.optchat and its socket
    com.facet          facet serve             web pages + Telegram         127.0.0.1:8730
    com.facet.term     bin/facet-term          ttyd running `facet chat`    127.0.0.1:8731
    com.facet.vault    bin/facet-vault-bridge  vault-phone (a separate note viewer; its
                                               path is set in the plist) and its
                                               feedback into the chat       127.0.0.1:8765

    bin/life           mail / calendar / web (used by Telegram /cal and /mail)
    bin/facet-scrub    length-preserving secret redaction over the chat log

    cargo build --release
    facet init          # writes ~/.config/facet/facet.json (token, Telegram bot token, paths)

The engine needs Claude Code (`claude`) logged in with a subscription; set `chat.claude` to
its full path if launchd's PATH does not find it.

## routes

Every service binds 127.0.0.1 only. To reach them from other devices, publish them on a
tailnet with `tailscale serve` (real TLS certificates, tailnet devices only), for example:

    tailscale serve --bg --https=10443 127.0.0.1:8730    https://<host>.<tailnet>.ts.net:10443/<token>/   web
    tailscale serve --bg --https=8443  127.0.0.1:8731    https://<host>.<tailnet>.ts.net:8443/<token>/    terminal
    tailscale serve --bg --https=9443  127.0.0.1:8765    https://<host>.<tailnet>.ts.net:9443/            vault-phone

One secret token in the path gates the web pages and the terminal (`token` in facet.json; the
terminal script reads the same value from `~/.config/optchat-web/token`). Set `base_url`,
`terminal_url` and `vault_phone` in facet.json to the published addresses, so pages and
messages link to them.

Web pages (`facet serve`, all under `/<token>`):

    /                  home: every page below, with live state and usage left
    /home              the same (old links)
    /chat              the conversation: live log (HTMX), and a box to send a message
    /tree              the memory tree: one root down to every message, searchable
    /m/                published notes; /m/<slug> one note
    /d/                open comments on notes
    POST /x/send       a message into the conversation (form field `text`; `later=1`: a turn of
                       its own; in the box, Enter sends, Shift-Enter sends later, Alt-Enter is
                       a new line)
    POST /x/diag       apply | dismiss | discuss a comment (`code`, `note`, `do`)
    /f/log, /f/doc/<slug>, /static/htmx.js     fragments and assets the pages poll

Telegram (bot `telegram.username`, polled by `facet serve`; only the paired chat is heard):
plain text is a message into the conversation; replies come back as they are written, and
new notes and comments are announced. Commands are answered on the spot and ride along with
the next message: `/help`, `/ping` (= `/usage`: engine state and usage left), `/last N`,
`/link`, `/notes`, `/diag`, `/cal N`, `/mail [query]`, `/buffer`, `/flush`.

The engine socket, `~/.optchat/lock` (one JSON object per line; `facet send`, the web,
Telegram and every `facet chat` use it):

    send {text, later}  a user message: queued, or delivered between tool calls; with `later`,
                        it waits for a turn of its own (several: one turn each, in order), and
                        a cancel logs the waiting ones unanswered
    note {text, date}   an imported note (during a turn, queued until it is done; the same text twice
                        is added once)
    cancel / resume     stop the turn or the wait / lift a compactor pause
    view / status       the rendered view / engine state, usage left
    zoom {id, n}        a line of the view opened, as the agent's zoom
    model {name}        the master model for the next turns
    watch               then a stream of events: msg, delta, thought, accepted, phase, notice, limits

The MCP server for `zoom` and `date` listens on a random loopback port with a random
secret path, for the engine's own `claude` calls only.

## chat

    facet chat          the conversation in this terminal; starts the engine if needed
    facet engine        the engine in the foreground
    facet view          the view the model sees
    facet stats         usage per day and kind, from ~/.optchat/usage.jsonl
    facet browse [f]    the memory tree as one page (also /tree on the web)
    facet import <f>... add files to the memory, one note each (during a turn, when it is done;
                        so the agent can import files itself)

In `facet chat`: Enter sends (during a turn, the agent sees it at its next tool call);
Alt-Enter sends for a turn of its own, after the running one (terminals send Shift-Enter as
a plain Enter, unless set up to send Alt-Enter, as Claude Code's `/terminal-setup` does);
Ctrl-J is a new line; Ctrl-C or Ctrl-D leaves (the engine and a running turn carry on). `/help` lists the commands: `/usage`, `/tree`,
`/view`, `/zoom <id+n>`, `/model [opus|sonnet]`, `/import <file>...`, `/cancel`,
`/resume`, `/stats`, `/status`, `/quit`. A mistyped `/command` is refused, never sent.

Settings live under `chat` in facet.json (all optional): `model` (opus), `effort` (high),
`compact_model` (sonnet), `compact_effort` (medium), `tools`, `permission`
(bypassPermissions), `cwd` (~), `instructions` (~/.optchat/instructions.md, appended to the
system prompt), `cache_ttl` (5m), `prime` (true), `budget_hour_eq` (0 = none), `claude`
(path to the binary). `/model` overrides `model`, and is kept across restarts.

## Departures from the gist

The engine follows the gist exactly where it can. These are the places it does not, each
with its reason. Measurements: Claude Code 2.1.268, subscription (OAuth), Sonnet 5.5, through
a structure-only logging proxy (`tests/live_test.py` with `WIRE_LOG`).

**Forced by `claude -p`**

1. *Priming instead of cache marks inside the view (§8).* From its second step on, a call
   uses all 4 of the API's cache breakpoints itself (2 on the system prompt, 2 rolling); one
   more is a `400`. So before each turn whose view passes the first mark (50k characters), a
   priming request goes out: the same arguments, Claude Code's own marks off
   (`DISABLE_PROMPT_CACHING=1`), ours on each view piece. It is killed as soon as the API
   accepts it; the real call then reads the whole view back (measured: read 30,003 of 30,003
   tokens, wrote 350). One per turn, right before it, writing only what changed: not one of
   the "renewal pings" §8 forbids.
2. *The compactor sets its own marks (§4.2, §8).* Claude Code's marks are off; ours go on the
   view pieces and the end of the context, and a spare one on the step, so the size retries
   read everything.
3. *The chain.* `</chat>` opens the step block instead of closing the context block, and the
   context's tail after its last mark goes as one block per call increment. The next call's
   end mark then finds the previous call's whole context within the API's 20-block lookback.
   The text the model sees is identical. Measured: each call after the first reads ~22k tokens
   and writes ~500.
4. *The gate.* Compactor calls whose marked prefixes are not cached yet wait (until its
   first response) for the call already writing them, instead of all writing the same tens of
   thousands of tokens in parallel. JOBS stays 8.
5. *Mid-run messages (§7).* A message written to `claude` while the model writes its final
   reply is run by Claude Code as a follow-up turn of the same conversation, with a stale view.
   So a message is written only while a tool runs (it rides on that tool's result, in the
   same call), else held for the next tool call. A call that ends with a message written but
   not consumed (`--replay-user-messages` tells) is killed at its result; that message, and
   held ones, go to a fresh call with a new view, as the gist says.
6. *A clean environment for every `claude` process.* The environment is cleared and only
   HOME, USER, PATH, SHELL, LANG, TMPDIR passed: variables from a parent Claude Code session
   put a line with a random session id before the view, which made every call a cache miss.
   Set on top: `CLAUDE_CODE_DISABLE_CLAUDE_MDS`, `_AUTO_MEMORY`, `_GIT_INSTRUCTIONS` (no
   CLAUDE.md, verified with a canary file; no git status before the view) and
   `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` (stops a session-title request that sent the
   whole first message, the view, to Haiku, uncached, for every call).
7. *5-minute cache entries are forced (§8).* Claude Code's default for subscriptions is 1 hour;
   `CLAUDE_CODE_PROMPT_CACHE_TTL=5m` restores what the gist asks for.
8. *No `--safe-mode` on master calls.* It would be the cleanest switch, but it also drops
   `--mcp-config` servers (measured), and zoom and date are MCP tools. The compactor, which has
   no tools, uses it.
9. *The chat directory is a git repository from the start*, not from the first commit:
   Claude Code tells the model whether its working directory is one, in a block before the
   view, so that must not change.
10. *What cannot be removed (§7.2).* Claude Code puts its own blocks before the view: its
    one-line identity, its tool definitions, the working directory and OS, the model name,
    the account e-mail and today's date. All are constant except the date, so once a day, at
    midnight, the whole view is a cache miss. A Claude Code update changes its blocks and
    costs one miss.
11. *Tools* are Claude Code's own (Bash, Read, Edit, Write, Glob, Grep, WebFetch,
    WebSearch) plus zoom and date served over MCP from the engine (§9 suggests MCP over HTTP).

**Choices the gist leaves open, and safety additions**

12. *MASTER without its subagent paragraph (§7.2).* There are no spawn, tell or computer
    tools (§9 is optional), so the paragraph would describe tools that do not exist. Added:
    each turn is a fresh process, so anything started in the background dies with it.
13. *Free nodes are built at once*, without waiting for rule 3 (§4.1): they need no model call,
    so the compactor never sees them; the result is the same.
14. *Failures do not retry forever at 10 s (§4.1).* A usage-limit error pauses the compactor
    until the reset time Claude Code reports (else 5 minutes); a node that fails 5 times parks
    it for an hour (a refusal would otherwise cost a paid call every 10 s, for ever); an
    optional hourly budget (`budget_hour_eq`) parks it too. `/resume` lifts any of them. A
    compactor call with no result after 180 s counts as a failure.
15. *Messages accepted but not yet logged are durable too (§2).* Queued and held messages are
    kept in a file and queued again after a crash or restart. Cancelling the wait logs the
    waiting messages, unanswered (§6).
16. *Browse (§10)* is one tree from a single root, each entry opening into its two halves
    down to the messages, with scaffold entries above the view where nothing is summarized,
    rather than separate listings of the view, the log and each level.
17. *Import (§10)* takes files the user chooses: each becomes one note, its path then its
    whole content, dated by the file's modification time, appended after the conversation so
    far (the reference imported OptMem notes first, keeping their ids).
18. *Model switching (§10)* costs one cold cache write of the view for the first turn on the
    new model (prompt caches are per model).

Not implemented: subagents and computer use (§9).

## logs

Everything the system writes, for whoever works on it next (person or agent), on any machine.
Paths are the defaults; `facet status` prints the actual chat directory (`dir`) and engine
state directory (`state_dir`). The state directory is named after a hash of the chat
directory's path, so tests and other chats never share one.

**The memory** (`~/.optchat`, or `$OPTCHAT_DIR`; a git repository, committed after each turn).
Append-only, one JSON object per line, fsynced; never edit or delete a line.

    chat/main/YYYY-MM-DD.jsonl   {i, kind, text, size, date}   every message; i is the permanent id,
                                 kind is user | talk | tool | echo | note, date is UTC ISO
    chat/tree/YYYY-MM-DD.jsonl   {l, i, text, size}            summary node (l, i) covers messages
                                 [i·2^l, (i+1)·2^l); shown as id+n with id = i·2^l, n = 2^l
    usage.jsonl                  {date, kind, model, usage}    one line per API request the engine
                                 caused; kind is turn | prime | compact; usage is the API's own
                                 usage object (input, cache read, cache write by TTL, output)

Cost in this README is in "eq", input-token equivalents at API price ratios:
`input + 0.1 cache_read + 1.25 cache_write_5m + 2 cache_write_1h + 5 output` (per model;
`facet stats` tabulates it per day and kind).

**Engine state** (`~/.local/share/facet/engine-<hash>/`): not memory, safe to delete when the
engine is stopped, except `queue.json`, `later.json` and `notes.json` (accepted but not yet logged).

    events.jsonl   introspection, below
    queue.json     accepted messages not yet in the log; queued again at start
    later.json     the same, for messages waiting for turns of their own
    notes.json     notes imported during a turn, not yet in the log; added at start
    limits.json    the last rate_limit_info Claude Code reported
    model          the /model choice, if any (wins over chat.model)
    system.txt     the master system prompt as last sent; compact.txt the compactor's
    prompts/       every distinct system prompt ever sent, named <name>-<hash>.txt
    memory.html    the last /tree page written by `facet chat`
    engine.log     stdout/stderr of an engine started by `facet chat` (under launchd, the
                   plist's StandardOutPath/StandardErrorPath instead)

**events.jsonl**: one object per line, every one with `t` (UTC ISO) and `ev`. Written next to
the engine's work, never read back by it; a write that fails is dropped silently.

    engine   start: pid, exe, dir, messages, nodes, view_lines, view_bytes, startup_ms,
             load_notes (torn lines etc. found at load), conf (the settings in force)
    req      one API request: kind (turn | prime | compact), model, usage, eq, ms, cc (Claude
             Code version), and what it was for: turn + step | turn + pieces (prime) |
             node ("id+n") + try
    turn     one fresh call: first, last (message ids), messages_in, settle_ms (waiting for
             summaries), ms, steps, primed, prime_read, prime_write, midrun_delivered,
             queue_after, outcome (done | cancelled | error), model, effort, view_bytes,
             view_lines, view_marks, shared_bytes (prefix shared with the previous turn's
             view), shared_to_mark (the last cache mark inside that prefix)
    node     one compactor node: node, l, i, ok, kept (bytes), tries (bytes of every try),
             error, limit, requests, ms, gate_ms (waiting for another call's cache write),
             context_bytes, step_bytes, blocks, marks
    limits   info: Claude Code's rate_limit_info (status, five_hour / seven_day utilization
             and resetsAt), each time it changes
    input    how (starting | queued | held | delivered), bytes; never the text
    model    model: a /model switch
    system   name (master | compact), hash, bytes: a new system prompt version
    notice   text: every notice shown to the user

Questions it answers, for example:

    jq -c 'select(.ev=="turn") | {first, ms, steps, outcome, settle_ms}' events.jsonl
    jq -s '[.[] | select(.ev=="req")] | group_by(.kind) | map({kind: .[0].kind, eq: (map(.eq) | add)})' events.jsonl
    jq -c 'select(.ev=="node" and (.tries | length) > 1) | {node, tries}' events.jsonl
    jq -c 'select(.ev=="limits") | {t, s: .info.unifiedWindows.five_hour.utilization}' events.jsonl

**Service logs** (LaunchAgents; paths set in the plists): `facet serve` →
`~/Library/Logs/facet.log`, the engine → `~/.local/share/facet/engine.{out,err}.log` (the
rendered view at each start on stdout, notices on stderr), the terminal →
`~/Library/Logs/facet-term.log`, vault-phone → `~/Library/Logs/vault-phone.log`.

**Surface state** (`~/.config/facet/`): `facet.json` (settings and the two secrets, chmod 600),
`state.json` (Telegram cursors), `buffer.jsonl` (Telegram commands riding along).

## tests

`cargo test`; `python3 tests/engine_test.py` (the whole engine against a fake `claude`, no
tokens); `python3 tests/live_test.py` (real `claude -p`, Sonnet, ~100k eq: checks every
cache claim above from request usage; with `ANTHROPIC_BASE_URL` at a logging proxy and
`WIRE_LOG`, also that nothing else goes on the wire). Tests use their own chat directories
(`OPTCHAT_DIR`) and `FACET_CHAT_*` overrides; they never touch `~/.optchat` or facet.json.

## use

    facet post <file> [slug]   publish a note (writes `facet: <slug>` into its frontmatter)
    facet review <note>        write a batch of comments from a spec on stdin:
                                 @ verbatim anchor (unique; may wrap across lines)
                                 ! [severity] message
                                 ? long explanation, markdown + math
                                 + replacement for the anchored line(s)
    facet diag                 open comments; apply/dismiss by code
    facet send [--later] <text>  put a message into the conversation (--later: a turn of its own)
    facet push <text>          push to Telegram
    facet status               where everything stands

## state

Settings and secrets in `~/.config/facet/`, the memory in `~/.optchat`, engine state in
`~/.local/share/facet/` (see **logs**). Everything else lives in the vault or in the memory,
on purpose: delete this program and nothing is lost but a port number.
