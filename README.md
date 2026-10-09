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
    cards.rs  cards: a conversation anchored to a line of a note — a comment, a review's
              warning, a fix to apply — whoever opened it (see **cards**, below)
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
    (com.facet.vault was vault-phone, a separate note viewer; retired once `/n/` and `/m/`
     did the same reading and tap-to-comment — see "one viewer" below. The job is stopped
     and its plist disabled; the code is archived, not deleted, at
     `~/Prog/archive/vault-phone`.)

    bin/life           mail / calendar / web (used by Telegram /cal and /mail)
    bin/facet-scrub    length-preserving secret redaction over the chat log

    cargo build --release
    facet init          # writes ~/.config/facet/facet.json (token, Telegram bot token, paths)

The engine needs Claude Code (`claude`) logged in with a subscription; set `chat.claude` to
its full path if launchd's PATH does not find it.

## running on Linux

The `linux` branch carries what macOS-only code needed: `src/main.rs`'s `input.tmux` default
and `bin/facet-term`'s `ttyd` now resolve from `PATH` instead of a hardcoded
`/opt/homebrew/bin/...`, and `bin/life` (AppleScript Mail/Calendar + the macOS SQLite stores)
now exits with a clear message instead of a raw stack trace when it is not on Darwin. Nothing
else in `src/` is platform-specific — the crate is plain Rust (`comrak`, `ureq`, `tiny_http`,
`rustyline`, `serde_json`, `chrono`), no macOS frameworks.

Service management is `systemd --user`, not launchd: the unit files in `systemd/` are
equivalent to the plists in `launchd/` (`facet.service` = `com.facet`/`facet serve`,
`facet-engine.service` = `com.facet.engine`/`facet engine`, `facet-term.service` =
`com.facet.term`; there is no Linux unit for the vault bridge, since that job is retired on
macOS too — see "one viewer" above). Install (as whichever user facet runs as — root, on a
single-user box):

    mkdir -p ~/.config/systemd/user
    cp systemd/*.service ~/.config/systemd/user/
    loginctl enable-linger "$USER"   # let --user units run without a login session
    systemctl --user daemon-reload
    systemctl --user enable --now facet.service facet-engine.service facet-term.service

Reach it the same way as the Mac — `tailscale serve`/`tailscale cert` for the TLS-terminated
URLs (see **routes**, below); Tailscale's Linux client does `serve`/`cert` the same as macOS.

What does not work here: `life web`'s `js`/`open`/`tabs` (AppleScript browser control) fail
loudly. `life web get` (plain `curl`) works. `bin/life` mail/calendar have a Linux backend, below.

### life on Linux

`bin/life` on Linux runs `bin/life_linux.py` (stdlib only; keep both files together): IMAP/SMTP
for mail, secret iCal URLs for the calendar. Same commands (`mail inbox|search|show|send`,
`cal next|add|calendars`). Config: `~/.config/life/accounts.json` (`chmod 600`; or `$LIFE_CONFIG`);
copy `bin/life.accounts.example.json`.

- **Gmail app password**: Google Account → Security → turn on 2-Step Verification → App passwords
  → create one → paste the 16 characters as `password`. Gmail needs no host fields
  (`imap.gmail.com:993`, `smtp.gmail.com:465`); sent mail is filed by Gmail itself. Search uses
  `X-GM-RAW`, so Gmail search syntax works.
- **Other account** (math/Exchange): give `imap_host`, `smtp_host` (+ ports) and an app password if
  the host offers one. Add `"sent_folder": "Sent"` to have a copy appended after sending. Plain
  password login only; hosts that insist on OAuth2 (e.g. Microsoft 365) won't work.
- **Calendar read**: Google Calendar on the web → Settings → your calendar → *Integrate calendar* →
  *Secret address in iCal format* → put it in `calendars[].ics_url`. Handles RRULE
  (DAILY/WEEKLY/MONTHLY/YEARLY, INTERVAL, COUNT, UNTIL, BYDAY, BYMONTHDAY), EXDATE, modified
  instances and time zones. Anyone with that URL can read the calendar; treat it as a secret.
- **Calendar add** (optional), two ways. Simplest: a Google Cloud *service account* (Calendar
  API enabled, JSON key downloaded), share your calendar with its address ("Make changes to
  events"), install `google-auth` in a venv, and fill
  `"google": {"service_account_file": "key.json", "python": "<venv>/bin/python", "calendar_id": "you@gmail.com"}`
  (paths relative to the config dir; `~` allowed). The key never expires, unlike a refresh token
  from an unpublished OAuth app (7 days). Or: an OAuth client plus refresh token for scope
  `https://www.googleapis.com/auth/calendar.events` in the `oauth` block.
  Without either, `life cal add` exits saying so. `life cal calendars` lists what each can see.
- `life secret NAME [--from FILE] [--nospace] [--check gmail|imap:ACCOUNT]`: stores a secret in
  `~/.config/life/NAME` (600). Source: `--from FILE`, else `~/.env` (whole content, whitespace and surrounding
  quotes stripped; `--nospace` also drops inner spaces, for app passwords; `~/.env` is shredded afterwards),
  else a hidden prompt on a terminal. Prints only `saved NAME (len N)`; `--check` does an IMAP login and prints OK/FAIL + exception type.
- `life cal delete --match TEXT [--from YYYY-MM-DD] [--days N] [--calendar ID] [--yes]`: events whose title contains TEXT
  (case-insensitive; default today, 30 days) on the service-account calendar. Lists them; deletes only with `--yes`.
- **Time zone**: a server usually runs in UTC; set `"timezone": "America/Vancouver"` (or yours).
- Mail ids are `<account>:<uid>` for the inbox, `:s` for Sent, `:a` for Gmail All Mail (`--everywhere`); all work with
  `life mail show`. Sent/All folders are found by IMAP special-use flags (`\\Sent`, `\\All`), falling back to
  common names; `"sent_folder"` in an account overrides. `life mail send` is a dry run unless `--send`; `--from <account>` picks the sender.
- `life mail search <query> [--with ADDR] [--no-sent]` searches inbox **and sent** (deduped by Message-ID); sent rows
  show `me→recipient`. `--with ADDR` = everything to/from/cc that address (query may be `""`). `inbox` is inbox-only.
- `life mail thread <id> [--full] [--chars N]`: whole conversation across inbox + sent (+ All Mail on Gmail), linked by
  Message-ID/In-Reply-To/References, else same normalised subject + shared non-self participant. Output:
  `== subject [N msgs, account]` then per message `-- <id> <date> <from> -> <to>` and the body with quoted replies
  (`>` lines, `On ... wrote:` tails, Outlook headers) removed; `--full` keeps them. Bare logins (e.g. `matthewscott`)
  count as "me" as `login@<imap domain>`; add `"aliases": [...]` to an account for others.
- Tests: `python3 tests/life_test.py` (fake ICS, fake IMAP/SMTP; no network).

A Linux box (the Hetzner one this branch was built against: Ubuntu 24.04 x86, cloned at
`/root/facet`) is meant to become the main host — source of truth for `~/.optchat`, run
continuously instead of sleeping with a laptop lid, agents launched there by default — with
the Mac as one more tailnet client.

## routes

Every service binds 127.0.0.1 only. To reach them from other devices, publish them on a
tailnet with `tailscale serve` (real TLS certificates, tailnet devices only), for example:

    tailscale serve --bg --https=10443 127.0.0.1:8730    https://<host>.<tailnet>.ts.net:10443/<token>/   web
    tailscale serve --bg --https=8443  127.0.0.1:8731    https://<host>.<tailnet>.ts.net:8443/<token>/    terminal

One secret token in the path gates the web pages and the terminal (`token` in facet.json; the
terminal script reads the same value from `~/.config/optchat-web/token`). Set `base_url` and
`terminal_url` in facet.json to the published addresses, so pages and messages link to them.
Wikilinks resolve on `/n/`, against the vault in `vault`.

There is one viewer, not two: vault-phone — a separate note reader and tap-to-comment page,
its own Python server on 127.0.0.1:8765/tailnet :9443 — is retired (archived at
`~/Prog/archive/vault-phone`, `com.facet.vault` stopped and its plist disabled); `/n/` and
`/m/` now do everything it did. What came from it:

  - Every block of a rendered note carries where it came from — `data-line`, and `data-note`
    for the note itself, which through an embed is not the page's own note but the one the
    embed quotes (see **a longform**, below). A double-click (double-tap; a touch-screen gets
    its own detector, since iOS Safari does not fire `dblclick` reliably) on one opens a card
    under that line (see **cards**, below).
  - A labelled embed is closed where it ends (a tombstone `∎` on a proof, a plainer `□` on
    anything else) rather than left for the reader to guess at a page boundary that is not
    there.
  - `[[@bibkey]]` — a citation, not a note — no longer sends a wikilink click at `/n/@bibkey`
    into a 404: it renders `[Author Year]` (ported from vault-phone's `render_wikilink`,
    `md::cite_label`), as an unlinked span, since neither viewer had anywhere to send such a
    click in the first place (no Zotero/BibTeX route exists in either).

What vault-phone did that was judged not worth porting: live diff-patched re-render over
server-sent events (facet's doc fragment instead asks `/f/doc/<slug>?wait=1`, held until the
note or anything it embeds changes, and swaps the whole thing — coarser, but needs no new
transport); a dot at the diagnostic's exact source line with a
bottom sheet for Apply/Dismiss/Reply (facet's card already sits right after the block it
anchors to and carries apply, close and reply, so the dot would be a second UI for the same
triage, not a new capability); whole-tree-snapshot `/undo` of the last agent turn (needs the
vault to be its own git repo — myVault is one — but recovering a bad edit by hand or through
Obsidian's own history already covers it, and the stale-text guard on `apply` already stops
the dangerous case, a fix landing on text that moved).

Web pages (`facet serve`, all under `/<token>`):

    /                  home: every page below, with live state and usage left
    /home              the same (old links)
    /chat              the chat venue: your messages and the agent's send_chat messages, live
                       (HTMX), and a box to send a message. Its plain text, steps, card
                       traffic and subagent reports are not shown (the stream has them: /tree)
    /tree              the memory tree: one root down to every message, searchable
    /m/                published notes; /m/<slug> one note
    /n/<note>          any note of the vault by its own name, read-only; `?h=<section>` one
                       section of it. Where every `[[wikilink]]` goes
                       both carry `data-line`/`data-note` on every block (a double-click
                       opens a card on it; the page places the note's cards itself)
    /d/                every open card, under its note and the lines it is about
    POST /x/send       a message into the conversation (form field `text`; `later=1`: a turn of
                       its own; in the box, Enter sends, Shift-Enter sends later, Alt-Enter is
                       a new line)
    POST /x/card       a card, from its page (`do`=say: `id`, `text`, and for the first word
                       `note`/`line`/`quote`; close; apply) -> JSON {ok, k | text | error}
    /f/log, /f/doc/<slug>, /f/note/<name>, /f/cards, /static/htmx.js
                       fragments and assets the pages ask for. With `wait=1` a request is held
                       (up to 25s, a thread of its own: the server runs one per request) until
                       there is news - a chat message, a changed note or embed, any card
                       changed - then the page asks again; a hidden tab stops asking. The log is
                       parsed once and then only from where it stopped. The memory tree is
                       folded once per change to its files. `/f/cards?notes=[..]` (or `all=1`)
                       `&v=` answers JSON {v, cards}: the open cards of those notes, each message
                       rendered, held while the cards file's version is still `v`.

Telegram (bot `telegram.username`, polled by `facet serve`; only the paired chat is heard):
plain text is a message into the conversation; what the agent sends to the chat (kind
`chat`, see **stream and venues**) is pushed as it is logged — never its plain text or a card's
answer — and new notes and new cards the agent opened are announced. Commands are answered on the spot and ride along with
the next message: `/help`, `/ping` (= `/usage`: engine state and usage left), `/last N`,
`/link`, `/notes`, `/diag` (= `/cards`), `/cal N`, `/mail [query]`, `/buffer`, `/flush`.

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
    card {do, ...}      a card (see **cards**): do = new {note, anchor | line, text, kind?, fix?}
                        | reply {id, text, fix?} | fix {id, fix} | kind {id, kind} | close {id,
                        reason?} | delete {id} | apply {id} | list {note?}; what the agent does on
                        one is logged as kind `answer` (the same call as the MCP card tools)
    answer {id, text, apply?}   the old name of card/reply
    cancel / resume     stop the turn or the wait / lift a compactor pause
    view / status       the rendered view / engine state, usage left
    zoom {id, n}        a line of the view opened, as the agent's zoom
    model {name}        the master model for the next turns
    watch               then a stream of events: msg, delta, thought, accepted, phase, notice, limits

The MCP server listens on a random loopback port with a random secret path, for the engine's
own `claude` calls and the subagents they spawn only. The master's path (`/<secret>/mcp`) serves
`zoom`, `date`, `send_chat {text}` and the card tools — `new_card {note, anchor | line, text,
kind?, fix?}`, `answer_card {id, text, fix?}`, `fix_card {id, fix}`, `close_card {id, delete?}`,
`list_cards {note?}`; a detached subagent (`facet spawn`) is given `/<secret>/agent`, which
lists and serves `zoom` and `date` only, and Task subagents' definitions leave the output tools
out of their tool lists.
Zoom and date leave no trace in the stream (no step, no result): their content is already memory.

### cards

Everything said about a line of a note is one **card**, whoever started it: a double-click in
the viewer, a review (`facet review`), the agent's `new_card`. There used to be two systems —
review *diagnostics* in `.claude/diagnostics.json` (apply/dismiss/discuss) and the viewer's
*comment cards* (words in the log, answers in the engine's `cards.json`) — and they are now one
model, in one file, `<vault>/.claude/cards.json` (`{"cards": [..]}`, git-ignored in the vault):

    { id, note, line, end, quote, kind, by, thread, fix, closed, applied?, at }
      note     vault-relative path; line..end the lines it is about (1-based)
      quote    the line as it read when the card was opened
      kind     comment | info | warn | error — the colour of its edge, and nothing else
      by       who opened it: user | server
      thread   [{by: user|server, text, at}], in order
      fix      null, or [{start_line, end_line, old_text, new_text}]: what Apply puts in.
               old_text is read from the note when the fix is set, never typed, so the
               stale-text guard is right by construction; applying shifts every other
               card's lines on the note by what the edit did
      closed   the X: off every page, kept on file (`delete` removes it outright)

Every card looks the same: a coloured edge under its line, the quote dim at the top with an X
(close) beside it, the thread, the fix with an `apply` button if there is one, and a one-row
box that grows (16px, so iOS does not zoom), always last. Enter or `send` sends, Shift-Enter is
a new line; what is sent shows grey at once and firms up when the server has it (it is the
card's message `k` from then on, so the held request bringing it back does not show it twice);
a failure takes it back out and returns the words to the box. A card nothing was said in goes
on Escape or a click away.

What the user writes on a card is kept on it and told to the conversation, queued for a turn of
its own, shaped `[[Note]] L<n> #<id>: "quote"` then the words (on a card the agent opened, also
`(on your <kind> card: <what it said>)`), so the agent answers with `answer_card`, on the card
alone — never the chat (see **stream and venues**). A send the conversation refuses is taken
back off the card. Nothing about a card fails silently: no vault, a `.claude` that cannot be
made, a note or anchor not found, an unknown id — each is an error to whoever asked.

A page places its cards itself, from `/f/cards` (held until any card changes), under the last
block of the card's note starting at or before its line — through embeds too, since every block
carries its own note. So a card the agent opens shows on a page already open, a reply lands in
its card, and one closed or applied elsewhere leaves, with no reload; the note's own markup
never has to be rendered again for a card. `/d/` is the same component over every open card.

The CLI (through the engine when it is up, so the stream has it; else straight on the file):

    facet card new <note> (--line N | --at "verbatim text") [--kind K] [--fix R] <text>|-
    facet card reply <id> <text>|- [--fix R]      (`facet answer` is the same)
    facet card fix <id> [R]                        set, or with nothing take off, the fix
    facet card close|delete|apply <id> [reason]
    facet card list [note]                         (`facet diag` is the same)
    facet review <note> [--replace] < spec         many server cards at once (see below)

Closing a server card that was not applied writes the objection to `review_memory` (as
dismissing a diagnostic did), so the next review does not raise it again. Migration is on load
and idempotent: anything in an old `diagnostics.json` (which an editor or review skill may
still write) is absorbed as server cards and the file left as `{}`; the engine's old
`cards.json` is rebuilt into threads from the log once and renamed `cards.json.migrated`.

### stream and venues

The log is the **stream**: everything lands in it, and the memory tree is built on it alone —
user messages, the agent's text, its tool calls and results, subagent reports, card comments
and card answers, chat sends. A **venue** is where some of it is also delivered. There are two:

  - **chat**: Telegram and the web chat page, one venue. It shows the user's messages and what
    the agent sent with `send_chat` (logged as kind `chat`), nothing else.
  - **card**: a card on a line of a note (`<vault>/.claude/cards.json`). It shows what the
    agent says with the card tools / `facet card` (logged as kind `answer`, text `#<id> on
    [[Note]] L<n>: ...`, or `new <kind> card #<id> on ...`), and the user's own words on it.

The agent's plain text (kind `talk`) reaches the stream only. A successful `send_chat` or card
tool call (not `list_cards`, which only reads) leaves no `tool`/`echo` lines: the `chat` or
`answer` message is the record
(a failed one is logged as an echo, so the failure is remembered). One reply, not two: plain
text written after a `send_chat` is held; if another step follows it is logged as `talk`
(working notes), but if the call ends on it, it is a recap of what was just sent and is dropped
(a `recap_dropped` event records its size) — unless no `chat` actually landed, when it is logged
and the safety net below sends it. The safety net: when a call
ends (not cancelled) and a user chat message in it — not a card comment, not a "[id] " report —
has no `chat` after it, the turn's final text (the `talk` lines after its last step, else its
last `talk`, else a line saying the turn ended without a reply) is logged as `chat`, so it
reaches the user, and a `fallback` event is written (the turn record gets `fallback: true`).
The text then stands in the stream twice, once as `talk` and once as `chat`.

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
    zoom freely rather than guess from a summary line — the same stance `MASTER` takes.
    Both prompts also say the view is true as a working rule: act on it without checking it
    over again, and zoom for what a line leaves out rather than to confirm what it says.
    The cheap call is the one that recovers a dropped detail; re-reading what the summary
    already got right buys nothing, and a wrong line is overwritten as the chat goes on.
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

    Memory hygiene (all constant per call, so the prompt cache is untouched): the task of a spawn
    is logged verbatim as a `tool` message "spawn <id> (<model>, <kind>, <desc>): <task>" (a
    plain log line, no turn), so a task passed by file is not lost; `--add-dir /tmp` on the
    master's and spawns' calls keeps work in /tmp from resetting the shell's directory, and a
    trailing "Shell cwd was reset to ..." line is stripped from a tool result before it is logged
    as `echo`; the compactor's length example (`prompts::SCALE`) is labelled invented, about no
    real chat, and COMPACT tells it that a summary is never longer than what it stands for and
    may lean on the lines before it, never the ones after.

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
                                 kind is user | talk | chat | answer | tool | echo | note | work,
                                 date is UTC ISO (talk: stream only; chat: sent to the chat
                                 venue; answer: sent to a card — see **stream and venues**)
    chat/tree/YYYY-MM-DD.jsonl   {l, i, text, size}            summary node (l, i) covers messages
                                 [i·2^l, (i+1)·2^l); shown as id+n with id = i·2^l, n = 2^l
    usage.jsonl                  {date, kind, model, usage}    one line per API request the engine
                                 caused; kind is turn | prime | compact; usage is the API's own
                                 usage object (input, cache read, cache write by TTL, output)

Cost in this README is in "eq", input-token equivalents at API price ratios:
`input + 0.1 cache_read + 1.25 cache_write_5m + 2 cache_write_1h + 5 output` (per model;
`facet stats` tabulates it per day and kind).

**Engine state** (`~/.local/share/facet/engine-<hash>/`): not memory, safe to delete when the
engine is stopped, except `queue.json`, `later.json`, `notes.json` (accepted but not yet
logged). (Cards are not here: they live in the vault, see **cards**; a `cards.json.migrated`
here is the old file, already absorbed.)

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
             subagents cost: agents, agent_reqs, agent_eq, agent_bytes (reports logged);
             fallback (its final text was sent to the chat for it)
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
    fallback turn, bytes: a call ended with a chat message unanswered in the chat venue, so
             its final text was logged as `chat` (see **stream and venues**)
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
`~/Library/Logs/facet-term.log` (vault-phone's log, `~/Library/Logs/vault-phone.log`, is now
only as current as the archived job's last run).

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
    facet review <note>        open a batch of cards from a spec on stdin:
                                 @ verbatim anchor (unique; may wrap across lines)
                                 ! [comment|info|warn|error] message
                                 ? long explanation, markdown + math
                                 + replacement for the anchored line(s)
    facet card new|reply|fix|close|delete|apply|list ...
                               cards (see **cards**); `facet diag` lists them, `facet apply` /
                                 `facet dismiss <id>` apply or close one, `facet answer <id>
                                 <text> [--fix R]` replies
    facet restart [--serve]    queue an engine restart (re-exec of the binary, same pid) for after the
                                  running reply, once no detached spawn is alive; --serve also
                                  restarts serve (systemctl --user / launchctl kickstart)
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

## mailwatch (cheap periodic mail check)

`bin/mailwatch` runs every 20 minutes from a systemd `--user` timer (`systemd/mailwatch.{service,timer}`,
`Persistent=true`) and wakes OptChat only when something deserves attention. Read-only: IMAP PEEK, never
replies, never deletes or moves mail, never prints message bodies. Its one write is the `\Seen` flag on mail
that is obviously not worth reading (see "Marking read" below).

1. **No LLM.** For each account in `~/.config/life/accounts.json`, list INBOX mail with UID above the cursor in
   `~/.local/state/mailwatch/state.json` (per account: UIDVALIDITY + last UID). The first run only records
   the cursor. Nothing new: exit silently.
2. **Rules** (`~/.config/life/mailwatch.json`, written with defaults on first run; case-insensitive globs
   against `From <addr> | Subject`): `always` (straight to notify), `mute`, plus `bulk_headers_mute`
   (List-Unsubscribe / List-Id / Precedence bulk / Auto-Submitted ⇒ mute, even if `always` would match) and
   `skip_seen` (already read elsewhere ⇒ mute). `quiet_hours` (default 23:00-07:00, `timezone`): the run
   does nothing and leaves the cursor, so the first run afterwards catches up.
3. **One batched triage** of the remainder: `claude -p --model haiku` (falls back to `fallback_model` sonnet),
   From/Subject/date + first 600 chars of the unquoted text, max `max_batch` (25) messages; strict JSON
   `{id, notify, why}`. Token/cost log: `~/.local/state/mailwatch/usage.jsonl`. If triage fails, those
   messages are reported as "triage unavailable" rather than silently dropped.

4. **One Sonnet judgement** of the candidates (`always` mail plus triage `notify`; `unsure` is never escalated, and
   stays unread). One `claude -p --model sonnet` (`judge_model`) call reads each candidate's full text (PEEK,
   text/plain, quotes stripped, 6000 chars each) and says per mail whether it truly needs his attention, with a
   short why (who, what is asked, deadline). Verdicts are tiers, see "Tiers and digests" below; a missing or garbled
   verdict counts as `today` (never `now`). If Sonnet fails, every
   candidate is pushed with its triage-level reason (`(unjudged)`), so an always-sender mail is never dropped.
   Mail Sonnet calls not needed is only noted in `~/.local/state/mailwatch/judged.jsonl` (no bodies); it is *not*
   marked read.

If anything needs him, ONE `facet push --log "[mailwatch] N need you\n- from: subject -- why (id, date)"`:
straight to Telegram, no Opus turn, and the same text is added to the chat log as a `note` (queued like an import
while a turn is running, so it never starts a turn but OptChat sees it later). `facet push --log` pushes first;
if only the logging fails it warns on stderr and still exits 0, so the push is never repeated. `--dry-run` prints
`WOULD PUSH:` plus the text. Ids work with `life mail show/thread`. Quiet hours hold everything until morning.

**Tiers and digests.** Only mail needing immediate attention notifies instantly. The one batched Sonnet call returns
per mail `tier` = `now` (action within hours: same-day meeting change, deadline today, supervisor waiting),
`today` (read it today) or `none`, and for `today` also `can_wait` (true = can wait till tomorrow morning). Both the
timer and the `--idle` path use it (`tiers()` + `dispatch()` in `bin/mailwatch`).
- `now`: pushed at once with `facet push --log` (`[mailwatch] N need you`), as before.
- `today`: appended to a queue in `state.json` under `_digest.queue` (deduped by account:uid, so idle and timer never
  double-queue). Config `digest_times` (default `["08:00","12:30"]`, in the config's `timezone`): the first timer run
  after each time sends ONE message `[mail digest] N to read today` and clears the queue; empty queue sends nothing.
  The message is a **summary**: one Sonnet call (`digest_model`, haiku `fallback_digest_model`) over the queued mails' full
  text (re-fetched with PEEK, never marked read) gives, per mail or thread, sender, subject, 1-2 sentences of what it
  says and what is asked by when, and the id, most pressing first; a dropped id is appended as a list line; if both
  models fail the old one-line-per-mail list is sent. The after-lunch "cannot wait" push is summarised the same way.
  `--dry-run` shows the summary. `_digest.last` stops a slot firing twice.
- After the day's last digest time, `today` mail with `can_wait` false is pushed at once (together with any `now`
  mail, one message); with `can_wait` true it stays queued for the next morning's digest.
- Quiet hours (23:00-07:00) stay hard. Nothing runs then: the timer and idle return before touching mail or the
  cursor, so a `now` mail that arrived overnight is simply found by the first timer run at/after 07:00 and pushed
  then (`today` mail from then waits for the 08:00 digest).
- `facet push` (and `mailwatch.push()`) refuse empty text.
- `--dry-run` prints each tier decision, `tiers: now=N today=N none=N`, queue/push decisions and the digest it would
  send; it saves nothing. Tests use a fake clock.

**Marking read** (config `mark_read`, default true). Set `\Seen` only on (a) mail matching a `mute` rule or
carrying bulk headers, and (b) mail the triage returns as `skip`. The triage verdict is 3-way
(`notify` / `skip` / `unsure`); `skip` is for confident cases only, anything else, a triage failure, or a missing
verdict leaves the mail unread. Never marked, whatever else matches: `always`-rule matches, notified mail, `unsure`
mail, senders/subjects matching `never_mark` (default: siam, yaniv, friedlander, nserc, editorialmanager, fogs,
grad@ubc.ca, manuscript, submission, springer, elsevier, referee; extend it in `mailwatch.json`), and a triage `skip`
on a `Re:`/`Fwd:` subject. Every mark is appended to `~/.local/state/mailwatch/marked.jsonl`
(`ts, account, uid, uv, sender, subject, reason`).

- `bin/mailwatch --sweep [--days N] [--dry-run]` (default 30 days): same decisions over mail that is already unread;
  never notifies; prints a summary grouped by reason. Always dry-run first.
- `bin/mailwatch --unmark [acct:uid ...] [--since DAYS]` puts logged messages back to unread (everything logged if
  no selector); undone entries are recorded in the log and skipped next time.

**Push on receipt** (`bin/mailwatch --idle`, `systemd/mailwatch-idle.service`, `Restart=always`). One IMAP IDLE
connection per account (threads; raw IDLE commands since imaplib has none), renewed every 25 minutes, reconnect with
5 s..5 min backoff. Idle costs no model calls. On a new-mail event it reads headers only (PEEK, never marks read) and,
for mail from an `always` sender (not bulk), runs the same Sonnet read -> `facet push --log` path immediately for just
those mails. Everything else waits for the 20-minute timer, which stays as the backstop (and catches up after
disconnects). Shared state: both take an exclusive file lock (`~/.local/state/mailwatch/lock`); the timer owns the
UID cursor, idle records what it pushed in `state.json` under `_idle` (per account), the timer skips those uids and
prunes them once the cursor passes. So nothing is notified twice. Quiet hours are never broken: idle does nothing
then, and the first timer run after 07:00 handles the held mail. Install: `cp systemd/mailwatch* ~/.config/systemd/user/
&& systemctl --user daemon-reload && systemctl --user enable --now mailwatch-idle.service`; log: `journalctl --user -u
mailwatch-idle` ("[acct] idle: connected").

`bin/mailwatch --dry-run [--last N]` prints decisions, sends nothing, leaves the cursor alone (`--last N`
pretends the cursor is N messages back per account, and ignores quiet hours). Install:
`cp systemd/mailwatch.* ~/.config/systemd/user/ && systemctl --user daemon-reload &&
systemctl --user enable --now mailwatch.timer`. Env overrides for tests: `MAILWATCH_{STATE_DIR,CONFIG,CLAUDE,FACET}`.
Tests: `python3 tests/mailwatch_test.py`. Cost: about $0.0005-0.002 per triage call, none when nothing needs triage.
