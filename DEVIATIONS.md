# OptChat engine: deviations from the gist, and why

The engine in `src/optchat/` follows Taelin's spec
(gist.github.com/VictorTaelin/91837951a5ce5b38f341ec1ba1df6449) exactly where it can. Every place
it does not is listed here, with the reason the gist does not already refute. All measurements:
Claude Code 2.1.268, OAuth subscription, Sonnet 5.5, through a structure-only logging proxy
(`tests/live_test.py` with `WIRE_LOG`).

## Forced by `claude -p`

1. **Priming instead of view breakpoints (§8).** Claude Code spends all 4 cache breakpoints itself
   from the second step of a call on (2 on the system prompt, 2 rolling); one more is a `400`. So
   the view cannot carry the gist's 50k/80k/100k marks in the real call. Before each turn whose
   view has a mark (> 50k chars), one priming request goes out: the same arguments, Claude Code's
   marks off (`DISABLE_PROMPT_CACHING=1`), our marks on each view piece. It is killed at
   `message_start`. The real call's first request then reads the whole view back (measured:
   read 30,003 of 30,003, wrote 350). This is not a keep-alive ping: one per turn, right before
   it, and it writes only the part of the view that changed.

2. **Compactor marks are ours (§4.2).** `DISABLE_PROMPT_CACHING=1`, 4 marks: the view pieces, the
   end of the chat, and (if one is left) the end of the step, so the size retries read everything.

3. **The chain (compactor).** `</chat>` opens the step block instead of closing the chat block,
   and the chat's tail after its last mark is sent as one block per call increment. The next
   call's end mark then finds the previous call's whole context within the API's 20-block
   lookback. Measured: each compactor call after the first reads ~22k and writes ~500-600
   tokens. The text the model sees is identical.

4. **The gate (compactor).** Calls whose marked prefixes are not cached yet wait for the call
   already writing them (until its `message_start`), instead of all writing the same tens of
   thousands of tokens in parallel. JOBS stays 8.

5. **Mid-run messages (§7).** A message written to `claude`'s stdin while the model writes its
   final reply is run by Claude Code as a follow-up turn of the same conversation, with a stale
   view. So a message is written only while a tool is running (it rides on that tool's result,
   in the same call, cached normally), else held until the next tool call. A call that ends with
   a written-but-unconsumed message (`--replay-user-messages` tells) is killed at its `result`;
   such messages, and held ones, go to a fresh call with a new view, as the gist says.

6. **A clean environment for every `claude` process.** The environment is cleared and only
   HOME, USER, PATH, SHELL, LANG, TMPDIR (and ANTHROPIC_BASE_URL) passed. Measured: variables
   inherited from a parent Claude Code session add a "scratchpad" line with a random session id
   to a block *before* the view, making every process a cache miss. Also set:
   `CLAUDE_CODE_DISABLE_CLAUDE_MDS`, `_AUTO_MEMORY`, `_GIT_INSTRUCTIONS` (no CLAUDE.md, verified
   with a canary file; no git status before the view) and `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC`,
   which stops a session-title request that sent the whole first user message (the view) to Haiku,
   uncached, for every process.

7. **No `--safe-mode` on master calls.** It would be the cleanest switch, but it also drops
   `--mcp-config` servers (measured), and zoom/date are MCP tools. The compactor (no tools) uses it.

8. **The chat directory is `git init`ed at engine start**, not at the first commit: Claude Code
   states "Is a git repository" in a block before the view, so it must not flip.

## Choices the gist leaves open, or safety additions

9. **MASTER without the subagent paragraph (§7.2).** This engine has no spawn/tell tools (§9 is
   optional); the paragraph would describe tools that do not exist. Added instead: each turn is a
   fresh process, so background jobs die with it (true of `claude -p`).

10. **Free nodes are built eagerly**, without waiting for rule 3: they need no model call, so
    the compactor never sees them; the result is the same.

11. **Usage limits park the compactor** for 5 minutes instead of a 10 s retry (the previous
    engine made 5,077 failed calls in one afternoon at the limit). **A node failing 5 times parks
    it for an hour** (a refusal would otherwise cost a paid call every 10 s, forever); `/resume`
    lifts either. Optional hourly budget: `chat.budget_hour_eq`.

12. **Cancel during the wait** logs the waiting messages, unanswered (§6, "their message then
    stays in the log").

13. **Persisting** commits the chat directory after each turn (§10). `usage.jsonl` (one line per
    API request, every request the engine causes) lives in the chat directory too.

Not implemented (optional in the gist): subagents (§9), the HTML browser (§10; `facet serve`
already reads the log), importing history.
