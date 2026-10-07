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
    doc.rs    output addressed by a slug — a vault note, published via its own frontmatter,
              and the vault read by name: its notes, their sections, their embeds
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
    com.facet.vault    bin/facet-vault-bridge  vault-phone (a separate note viewer, kept
                                               running but no longer needed: `/n/` and
                                               `/m/` do the same reading and the same
                                               tap-to-comment now)             127.0.0.1:8765

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
messages link to them. `vault_phone` is a link in the nav and on the home page only:
wikilinks resolve on `/n/`, against the vault in `vault` — the vault-phone script used to be
pointed at a vault of its own, and a link out of a doc answered 404 whenever the two differed.

There is one viewer, not two: vault-phone's own reading and tap-to-comment are still there (the
job keeps running, and nothing stops it), but `/n/` and `/m/` now do both themselves, so facet
no longer depends on it for either. Every block of a rendered note carries where it came from —
`data-line`, and `data-note` for the note itself, which through an embed is not the page's own
note but the one the embed quotes (see **a longform**, below). A double-click (double-tap) on
one asks what to say about it and sends it through the ordinary `POST /x/send`, shaped
`[[Note]] L<line>: "quoted line text"` followed by what was typed, so it reads the same as a
reply typed by hand.

Web pages (`facet serve`, all under `/<token>`):

    /                  home: every page below, with live state and usage left
    /home              the same (old links)
    /chat              the conversation: live log (HTMX), and a box to send a message
    /tree              the memory tree: one root down to every message, searchable
    /m/                published notes; /m/<slug> one note
    /n/<note>          any note of the vault by its own name, read-only; `?h=<section>` one
                       section of it. Where every `[[wikilink]]` goes
                       both carry `data-line`/`data-note` on every block (a double-click
                       comments on it, into `POST /x/send`, below)
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

A message typed on a phone is usually the next thing to deal with, not an interruption of the
turn it lands in, so this route sends it with `later` by default (`telegram.queue`, default
true): it waits for the running turn and then starts one of its own, instead of arriving
mid-turn at the next tool call. Both ways stay one message away — `/now <text>` delivers into
the running turn, `/later <text>` makes it wait, and `/queue on|off` moves the default (with
`/queue` alone reporting it). With no turn running the flag changes nothing: the text starts a
turn either way.

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
secret path, for the engine's own `claude` calls and the subagents they spawn only.

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
a plain Enter, unless set up to send Alt-Enter, as Claude Code's `/terminal-setup` does), and
such a message is printed again where it goes into the chat, since that is long after it was typed;
Ctrl-J is a new line; Ctrl-C or Ctrl-D leaves (the engine and a running turn carry on). `/help` lists the commands: `/usage`, `/tree`,
`/view`, `/zoom <id+n>`, `/model [opus|sonnet]`, `/import <file>...`, `/cancel`,
`/resume`, `/stats`, `/status`, `/quit`. A mistyped `/command` is refused, never sent.

Settings live under `chat` in facet.json (all optional): `model` (opus), `effort` (high),
`compact_model` (sonnet), `compact_effort` (medium), `agent_model` (sonnet), `tools`, `permission`
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
   more is a `400`. So before each turn whose view passes the first mark (3/8 of the budget,
   48k characters), a
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
   held ones, go to a fresh call with a new view, as the gist says. A follow-up turn that
   starts before that result reaches us — Claude Code hands a backgrounded subagent's report
   over that way (see 19) — announces itself with a second `init`, and the call is killed
   there instead; nothing it says is logged, since the view it is working from is this
   call's.
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
    WebSearch, Task) plus zoom and date served over MCP from the engine (§9 suggests MCP
    over HTTP). A subagent gets zoom and date too, named in its definition (see 19).

**Choices the gist leaves open, and safety additions**

12. *MASTER's subagent line keeps the gist's free hand, with a nudge (§7.2).* There is no
    spawn, tell or computer tool, so the gist's wording would describe tools that do not
    exist, and its "use subagents only when the user asks for them" is a caution about
    delegating blind, not about price. A spawn here carries the whole view, as the gist's
    does (see 19), but a subagent's steps stay out of the log, which makes delegating cheaper
    here than elsewhere and is the whole of the price: what the subagent read and did is gone,
    only its report survives, and it cannot be asked again. The paragraph states both halves
    and gives the one quantity the choice turns on: how much of the work will be worth
    remembering. Little, send it out (a search, a survey, a fact, a contained piece of
    programming); much, do it here. Cost alone must not decide it, since the saving and the
    loss are the same fact. The choice is left to the agent's judgement rather than made a
    rule of. Added:
    each turn is a fresh process, so anything started in the background dies with it — a
    backgrounded Task subagent included, the moment the reply ends, so the paragraph tells it
    to spawn in the foreground when it needs what the subagent finds. For work that should
    outlive the turn, it names `facet spawn` (see 21) instead of backgrounding a Task call.
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

19. *Subagents are Claude Code's Task tool, not the gist's spawn and tell (§9).* The engine
    does not start them and cannot talk to one while it runs. Every event carrying a
    `parent_tool_use_id` is dropped, so a subagent's own calls, results and prose never enter
    the log: what is remembered is the report it hands back, logged as one message of kind
    `work` (the gist's own kind for it) when it comes back inside the turn, and as the gist's
    own `[id] ` user message when it comes back after the turn is over (below).
    Exploration that would have been twenty tool/echo pairs in the log costs one line.
    MASTER therefore keeps the gist's free hand and states the one quantity the choice turns
    on (see 12): how much of the work will be worth remembering. Little, and it is sent out
    (a search, a survey, a fact, a contained piece of programming); much, and it stays here,
    as does anything whose next step depends on the last or that the user is waiting on. The
    saving and the loss are the same fact, so cost alone must not decide it. The price of a logged message, measured over this chat (1206 messages,
    $270 at per-model rates): 11.9 cents of compaction, 22.4 cents all in. An average run of
    four tool calls is eight messages, so delegating it saves about a dollar of compaction and
    pays the subagent's own reads once, out of a fresh context instead of the turn's growing
    one. The report is a message too: a transcript handed back buys nothing.

    A backgrounded subagent reports on its own, and that report starts a turn. Claude Code
    (2.1.268) backgrounds a spawn unless the call passes `run_in_background: false`, and then
    its tool result is not a report at all but a receipt: an internal agent id with a warning
    never to quote it, so it is not logged. The report comes later, in the `task_notification`
    for the task, whose `summary` is the subagent's own last words, word for word (measured:
    202 bytes, byte-identical to its last `assistant` text). It arrives after the reply that
    sent it, so it cannot be that turn's: it goes in as the gist has it, one message of kind
    `user` whose text starts `[id] `, queued the way a message sent for a turn of its own is
    (so it is durable too, see 15) and answered by a fresh call whose view already has it.
    Backgrounding buys the turn nothing, which is why MASTER steers away from it: Claude Code
    withholds the call's `result` until every background task has ended (measured: the reply
    at 9 s, the notification at 49 s, `result` and exit at 51 s), so the call waits for the
    subagent whether the master does or not. All it adds is the follow-up turn Claude Code
    opens to deliver the notification, which is killed (see 5). Nothing of the subagent's own
    account is lost: its record and its requests are closed from the notification instead of
    from a tool result.

    A subagent runs on the cheap model by default (`agent_model`, sonnet): doing one stated
    job and writing one short report is work a smaller model does well, and paying the big
    model's rate for it would undo the saving. `--agents` redefines Claude Code's own
    `general-purpose` and `Explore` rather than adding a name, so whichever one the master
    picks is the cheap one, with the AGENT prompt (no follow-up, do only what the
    task names, report that stands alone). `Explore` gets the reading tools; `general-purpose`
    also gets `Edit` and `Write`, since a contained programming task is worth delegating too.
    The default is not a ceiling: the master may pass `model` with the call, and the CLI
    honours that over the definition, so a hard task can go out on opus. Both halves verified
    against the CLI: without the parameter the turn stays on opus while the subagent's
    requests come back as sonnet; with `model: opus` the subagent's own requests come back as
    opus.

    A spawn carries the view, as the gist's does: the view as it stands at the call is
    appended to each agent definition's prompt, which is the only channel a Task subagent
    has for context, since the master writes its task but not its system prompt. It can also
    open a line of that view: both definitions name `mcp__optchat__zoom` and
    `mcp__optchat__date`, so a subagent reads the chat by the same two tools the master has
    (§7.1). Nothing is compacted for it — no node is built from its reading, and its dropped
    steps leave no line to summarize — but every node the compactor has already built is
    there, so the depth of the chat is open to it while only the summary is pushed on it.
    Because that reading is free, the `AGENT` prompt tells the subagent so and tells it to
    zoom freely rather than guess from a summary line — the same stance `MASTER` takes, and
    with the same rule for a conflict: the later line is the sharper memory.
    Measured against the CLI: a subagent given those names in its definition called
    `mcp__optchat__zoom` with `{id: 0, n: 1}` and got `0+0|user: Can you put on my alarm?`
    back from the live engine, with no permission prompt. The MCP tool names need not be in
    the top-level `--tools` at all — a connected `--mcp-config` server is exposed to the
    session, and the definition's `tools` array is what grants it. What the subagent still
    cannot do is ask: it cannot be told more once sent, which is what AGENT says to it and
    what MASTER tells the master to expect. The master's own prefix is untouched by
    this: an agent definition's prompt never enters the master's request at all — the same
    call with a 60k-character agent prompt and with a 29-byte one hit one cache entry, byte
    for byte, 10468 tokens written then read — so a view that changes every turn cannot move
    the marks. Verified the other way too: a planted line in the view of a generated
    definition came back verbatim from a real subagent that was only asked to read it.
    The price is the subagent's, paid once in a fresh context: a full 128k view is about 32k
    tokens, 12 cents at sonnet's cache-write rate, and later spawns in the same turn read it
    back at a tenth of that. Against 11.9 cents per logged message of compaction, a delegated
    run pays for its view as soon as it keeps two messages of steps out of the chat.

    The subagent pays for itself in log bytes, and that is exactly why its own spending has to
    be counted: dropping its events would otherwise drop its tokens too. A subagent's requests
    are not streamed (no `stream_event` ever carries a `parent_tool_use_id`; its usage arrives
    on the `assistant` message instead), so the turn's meter never saw them and the usage log
    understated them completely. Each of its messages is now metered as one request of kind
    `agent`, so `/usage` prices delegation apart from the turn that sent it, and each subagent
    leaves one `agent` record in events.jsonl: what it was asked (`ask_bytes`), what it spent
    (`reqs`, `eq`, and Claude Code's own `tokens`, `tools`, `task_ms`), what the chat now
    carries (`report_bytes`), and whether it finished. The turn record totals the same per turn
    (`agents`, `agent_reqs`, `agent_eq`, `agent_bytes`). So the question the default answers by
    assumption — how much to send out versus do here — becomes measurable: the subagent's own
    eq against the compaction its report avoided, over real turns.

20. *The view has two limits, and the cache marks sit under the budget (§5.2, §8).* The gist
    collapses whenever the view is over its budget, which in practice is nearly every append:
    each collapse merges a pair at the front, every cache mark moves, and the compactor's
    context is written from scratch again. Instead, appends may carry the view up to 8% past
    the budget (`over`) and a collapse then takes it back to the budget in one batch, so the
    marked prefix is byte-identical across the appends in between. The marks moved with it:
    they are now fractions of the budget (3/8, 5/8, 15/16) rather than fixed offsets, so a view
    sitting at its budget has nearly all of itself inside the cacheable prefix instead of 28 kB
    of it past the last mark. Replayed over this chat's own logs (1100 messages, 2.15 compactor
    calls each, per-token Sonnet rates): the old rule invalidates the cached prefix on 39% of
    calls, collapses 592 times and costs $83; the two limits with the marks moved invalidate on
    17%, collapse 24 times and cost $40. The view keeps its whole budget of history — it averages
    more of it than before (108 kB against 105 kB), which a floor below the budget would not have
    done (97 kB for the same $37). Nothing else changes: the merge order, the tiling and the
    equality of a folded and a grown view are all as they were.

21. *A subagent that outlives its turn (§9, partial spawn).* A Task subagent lives and dies
    inside the master's own `claude -p` process: when the call's reply ends, Claude Code kills
    whatever it was still running in the background, no matter how it was told to behave.
    Measured: three backgrounded Task agents, 831k eq between them (one alone 564k eq, 21
    requests), logged "unfinished" with no report the instant their parent's process exited —
    the work, and its cost, both lost. `facet spawn [--model] [--kind] [--desc] <task>` is a
    second way to send one out, not a Task call at all: the CLI op reaches the engine over its
    socket (the same one `facet send` uses) and `src/optchat/agent.rs::spawn` starts its own
    `claude -p`, as a child of the engine process — which already outlives any one turn, since
    a turn is only a thread inside it — and in its own process group besides
    (`Proc::spawn_detached`, a `setsid`-equivalent), so nothing a turn does, including ending,
    can reach it. It is given the same AGENT prompt a Task subagent gets, as its
    `--system-prompt-file`, with the view at the moment of the call and the task as its first
    message (mirroring the master's own call shape), the cheap model by default
    (`agent_model`), and the same read tools plus `mcp__optchat__zoom`/`date` (`--kind explore`
    withholds `Edit`/`Write`). A thread the engine owns — not the turn, which may have long
    since ended — reads its stream-json, metering each request the way a turn meters its own
    and writing every event to `<state dir>/agents/<id>.jsonl` (so its usage can be reconstructed
    even if the thread died first). When it ends, it is logged as one `agent` event exactly like
    a Task subagent's (`status` now also "failed" or "timed out", and `"detached": true`), and
    its report — its last assistant text, or the call's own `result` text, whichever there is —
    is delivered the same way a backgrounded Task subagent's report is: `turn::input`, a message
    starting `[id] `, `later = true`, so it reaches the chat and starts a turn whether or not one
    is running, and whether or not the turn that spawned it is still alive. Verified against
    the fake CLI: `facet spawn` returns an id while the engine's `status` still shows no turn
    running; a turn sent immediately afterward settles and goes idle on its own; the spawned
    process's report (after a deliberate delay past that) still arrives, as `[id] ...`, kind
    `user`, starting a fresh turn, with one `agent` event (`status: "done"`) and `agent`-kind
    rows in usage.jsonl. Open: if the engine itself is killed while a spawn is still running,
    the detached process (being in its own process group) keeps running, but nothing is left to
    read its pipe or deliver its report — a restart does not reconnect to it. The master is told
    about both paths (12), and which to use for what: a Task call when the result is needed
    inside the turn, `facet spawn` for anything that should still be working after the reply.

Not implemented: computer use, and `tell` — a running `facet spawn` cannot be messaged once
sent, only awaited for its report (§9); `facet spawn` itself (21) is this engine's answer to
the gist's `spawn`.

## measured and rejected

Three cheaper-looking ideas, each measured against this chat's own logs (1098 messages,
$240 of model time at list prices) and each dropped. Kept here so they are not tried twice.

1. *Freezing the view for the length of a turn.* The hope was that holding the view still
   between turns would stop the compactor's prefix being reshaped under it. Replayed: the
   current collapse-to-budget rule invalidates the cached prefix on 75.8% of appends and
   costs $111 of compaction; freezing to the turn's start brings that to 38.2% and $81, but
   the view overshoots its own budget (153 kB against VIEW = 128 kB) because nothing may
   collapse while a turn runs. Collapsing in one batch (§20) does better on both counts
   (31.1%, $52) and freezing on top of it adds nothing (31.1%, $54).
   Worse than useless: nodes are built on top of nodes built in the same turn, so a parent
   and its own children would both be written against the turn's starting view, and the
   parent's line would summarize children it cannot see. Batching the collapse is the fix;
   freezing is not.
2. *Batching tool calls to shorten the log.* Merging every run of consecutive tool calls
   into one message does cut compaction ($53 to $21 simulated, 1100 messages to 435), but
   the saving is not batching: it is having fewer, larger messages, and a subagent buys the
   same amount ($21 at runs of 3 or more) while keeping the steps out of the log instead of
   inside it. Batching also costs what it saves twice over: the agent already bundles shell
   commands itself (365 of 440 tool calls in this chat ran several), runs average 4 calls, so
   the headroom is small, and any batching rule has to hold back a call whose result the next
   call needs. Use subagents; leave the tool loop alone.
3. *Describing the byte limit better in the compactor's prompt.* The prompt already carries
   an exemplar line of exactly NODE bytes, which is the strongest form of "show, don't tell"
   available: a model cannot count its own bytes, since bytes sit two layers below its
   tokens. It calibrates and does not measure. First tries fit 30.7% of the time (median
   625 bytes against a 512 limit); the retry, which shows the model its own draft cut at the
   limit, fits 80.1%; a third try fits 45%. More words about the limit will not move the
   first number, because the second is not counting either, only copying up to a visible
   cut. Shortening without a new call is the only real fix.

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
    req      one API request: kind (turn | prime | compact | agent), model, usage, eq, ms, cc
             (Claude Code version), and what it was for: turn + step | turn + pieces (prime) |
             node ("id+n") + try | agent (tool_use_id) + agent_kind + step (subagent)
    turn     one fresh call: first, last (message ids), messages_in, settle_ms (waiting for
             summaries), ms, steps, primed, prime_read, prime_write, midrun_delivered,
             queue_after, outcome (done | cancelled | error | followup, the call killed at a
             second init), model, effort, view_bytes,
             view_lines, view_marks, shared_bytes (prefix shared with the previous turn's
             view), shared_to_mark (the last cache mark inside that prefix), and what its
             subagents cost: agents, agent_reqs, agent_eq, agent_bytes (reports logged)
    agent    one subagent, from the tool call that sent it to the report it handed back:
             tool_use_id, task, agent_kind (subagent_type), description, status, ask_bytes
             (the prompt it was given), report_bytes (what the chat now carries), reqs and eq
             (ours, counted from its own messages), tokens, tools, task_ms (Claude Code's own
             account of the task), ms (wall time in the turn)
    node     one compactor node: node, l, i, ok, kept (bytes), tries (bytes of every try),
             error, limit, requests, ms, gate_ms (waiting for another call's cache write),
             context_bytes, step_bytes, blocks, marks
    limits   info: Claude Code's rate_limit_info (status, five_hour / seven_day utilization
             and resetsAt), each time it changes
    input    how (starting | queued | held | delivered | later), bytes; never the text
    model    model: a /model switch
    system   name (master | compact), hash, bytes: a new system prompt version
    notice   text: every notice shown to the user

Questions it answers, for example:

    jq -c 'select(.ev=="turn") | {first, ms, steps, outcome, settle_ms}' events.jsonl
    jq -s '[.[] | select(.ev=="req")] | group_by(.kind) | map({kind: .[0].kind, eq: (map(.eq) | add)})' events.jsonl
    jq -c 'select(.ev=="node" and (.tries | length) > 1) | {node, tries}' events.jsonl
    jq -c 'select(.ev=="agent") | {agent_kind, eq, reqs, report_bytes, ms}' events.jsonl
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
                                 a longform is assembled on the way out: see below
    facet review <note>        write a batch of comments from a spec on stdin:
                                 @ verbatim anchor (unique; may wrap across lines)
                                 ! [severity] message
                                 ? long explanation, markdown + math
                                 + replacement for the anchored line(s)
    facet diag                 open comments; apply/dismiss by code
    facet send [--later] <text>  put a message into the conversation (--later: a turn of its own)
    facet push <text>          push to Telegram
    facet spawn [--model M] [--kind general-purpose|explore] [--desc D] <task>
                                a subagent that outlives this turn (21): returns its id at
                                  once; its report arrives later, as a message of its own
    facet status               where everything stands

A longform keeps its prose in one note and every statement and proof in a note of its own,
embedded on a line of the vault's own form, `proposition::![[Range cover#Statement]]`. Markdown
renders such a line as literal text — the `!` stops even the wikilink extension — so the page
assembles it instead (`doc::assemble`): the line is replaced by the section it names, under a
bold run-in label ("Proposition."), three levels deep at most, and an embed that resolves to
nothing says so where it stands rather than vanishing. A labelled embed is also closed where it
ends — a tombstone (∎) for a proof, a plainer mark (□) for anything else — so a reader meets the
boundary an inlined block has no page break to show. Which header forms count and where a
section ends (the next header of the same or a higher level, the header line itself left out)
are the vault's own rules, from its `Scripts/read_section_rust`. A line's text is expanded only
as it is rendered, never before the blocks are cut, so a comment anchored to an embed line still
lands beside it.

Assembly keeps one more thing besides the text: which note, and which line of it, each
assembled line came from (`doc::Src`) — the embedded note's own line once a line is inside an
embed, not the longform quoting it. Rendering carries this through by asking comrak for its own
`data-sourcepos` (`render.sourcepos`) and rewriting each block's copy of it into `data-line`/
`data-note`, rather than keeping a second line-count of its own; a wikilink's `href` is found
the same way regardless of which attribute comes first in the tag, since `data-sourcepos` (or
the rewritten `data-line`) usually comes before it now.

## state

Settings and secrets in `~/.config/facet/`, the memory in `~/.optchat`, engine state in
`~/.local/share/facet/` (see **logs**). Everything else lives in the vault or in the memory,
on purpose: delete this program and nothing is lost but a port number.
