# facet

Facet is the agent: a memory, and the input and output through which it meets the world.
Both halves live in this crate. The memory is the OptChat engine in `src/optchat/` (a Rust
implementation of Taelin's OptChat gist, driving `claude -p`; deviations in DEVIATIONS.md),
which owns `~/.optchat`. The rest is the senses and the voice; it only reads the log, and
reaches the engine through its Unix socket.

    optchat/  the engine: store, view fold, compactor, turn loop, zoom/date over MCP,
              and the lock socket `~/.optchat/lock` every route talks to
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

Design, in full: `myVault/tmp/Facet core design.md`, published at `/m/design`.

## run

    cargo build --release
    facet init          # takes token, bot token and paths from what is already configured
    facet serve         # LaunchAgent: launchd/com.facet.plist

Loopback only; published over the tailnet by `tailscale serve --bg --https=10443
127.0.0.1:8730`. One token in the path gates every route.

## chat

    facet chat          the conversation in this terminal; starts the engine if needed
                        (Enter sends, Alt-Enter/Ctrl-J new line, Ctrl-C/Ctrl-D leave while the engine
                        carries on; /help lists the commands)
    facet engine        the engine in the foreground (LaunchAgent: launchd/com.facet.engine.plist)
    facet view          the view the model sees
    facet stats         usage per day and kind, from ~/.optchat/usage.jsonl
    facet browse [f]    the whole memory tree as one page (live at /tree in the web route)
    facet import <f>... add files to the memory, one note each (also /import in the chat)

Settings live under `chat` in facet.json (all optional): `model` (opus), `effort` (high),
`compact_model` (sonnet), `compact_effort` (medium), `tools`, `permission`
(bypassPermissions), `cwd` (~), `instructions` (~/.optchat/instructions.md, appended to the
system prompt), `cache_ttl` (5m), `prime` (true), `budget_hour_eq` (0 = none), `claude`
(path to the binary; set it for launchd).

Introspection: `~/.local/share/facet/engine-*/events.jsonl` (outside the chat directory,
never read back by the engine): what each API request was for and how long it took, a
trace per turn and per compactor node, every change in the subscription's limits, every
system prompt version (texts beside it in `prompts/`). Format in `src/optchat/events.rs`.

Tests: `cargo test`; `python3 tests/engine_test.py` (the whole engine against a fake
`claude`, no tokens); `python3 tests/live_test.py` (real `claude -p`, Sonnet, ~100k eq:
checks every cache claim from request usage; with `ANTHROPIC_BASE_URL` at a logging proxy and
`WIRE_LOG`, also that nothing else goes on the wire).

## use

    facet post <file> [slug]   publish a note (writes `facet: <slug>` into its frontmatter)
    facet review <note>        write a batch of comments from a spec on stdin:
                                 @ verbatim anchor (unique; may wrap across lines)
                                 ! [severity] message
                                 ? long explanation, markdown + math
                                 + replacement for the anchored line(s)
    facet diag                 open comments; apply/dismiss by code
    facet send <text>          put a message into the conversation
    facet push <text>          push to Telegram
    facet status               where everything stands

## state

Config and cursors in `~/.config/facet/` (`facet.json` is chmod 600 and holds the two
secrets; `state.json` and `buffer.jsonl` are throwaway). Everything else lives in the vault
or in the memory, on purpose: delete this program and nothing is lost but a port number.
