# facet

Facet is the agent: a memory, and the input and output through which it meets the world.
This repository is the second half — the senses and the voice. The memory is the OptChat
log and the engine that writes it (`~/.optchat`, `~/Prog/tools/optchat`), which this never
writes to and never needs to understand.

    log.rs    the memory, read-only: a fold over ~/.optchat/chat/main/*.jsonl
    doc.rs    output addressed by a slug — a vault note, published via its own frontmatter
    diag.rs   output addressed by a code, anchored to a live line — a comment, triaged
    tell.rs   input: one funnel, from any route, with one adapter underneath
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

## use

    facet post <file> [slug]   publish a note (writes `facet: <slug>` into its frontmatter)
    facet diag                 open comments; apply/dismiss by code
    facet send <text>          put a message into the conversation
    facet push <text>          push to Telegram
    facet status               where everything stands

## state

Config and cursors in `~/.config/facet/` (`facet.json` is chmod 600 and holds the two
secrets; `state.json` and `buffer.jsonl` are throwaway). Everything else lives in the vault
or in the memory, on purpose: delete this program and nothing is lost but a port number.
