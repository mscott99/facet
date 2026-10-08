#!/usr/bin/env python3
# A stand-in for `claude -p --input-format stream-json --output-format stream-json`, for
# testing the engine without spending tokens. It mimics the behaviour measured on the real
# Claude Code (see DEVIATIONS.md): a message written to stdin while a tool runs is delivered
# with that tool's result (and replayed); one that arrives during the final reply is run as a
# follow-up turn of the same conversation after the first `result`.
#
# Turn script, taken from the last text block of the first user message:
#   "TOOLS n"  -> n tool steps of 0.6 s each, then a reply "done"
#   "AGENT"    -> one subagent: the Agent tool call, the task events, two of the subagent's
#                 own requests (assistant messages carrying parent_tool_use_id, never
#                 streamed, as measured on the real CLI), then its report as a tool result
#   "BGAGENT"  -> the same subagent, backgrounded: the tool result is only the launch
#                 receipt, the reply comes before the subagent is done, the report arrives
#                 in the task notification's `summary`, and Claude Code then opens a
#                 follow-up turn of this conversation (a second `init`) to hand it over —
#                 all as measured on the real CLI (2.1.268)
#   "CHAT x"   -> calls the engine's MCP send_chat with "x" (as the real CLI would, over HTTP to
#                 the --mcp-config URL), then a reply "said it"
#   "CARD id"  -> calls MCP answer_card for card id with "card answer", then a reply "noted"
#   "CHATFAIL" -> calls send_chat with empty text (an error result), then a reply "ok"
#   otherwise  -> one reply "ok"
# A detached agent (`facet spawn`, FACET_SPAWN=1 in the environment): sleeps 1 s, then one
# reply "SPAWN REPORT: looked into it, done" — long enough that a turn sent right after the
# spawn call finishes first, proving the engine does not wait on it.
# Compactor (no --tools): replies 600 bytes first, then 300 bytes after the size feedback,
# unless the step contains "STUBBORN" (always 600). "REFUSE" makes it fail.
# Every invocation is appended to $FAKE_LOG as one JSON line (argv, env, inputs).
import json, os, sys, threading, time, queue

argv = sys.argv[1:]
log_path = os.environ.get("FAKE_LOG")
replay = "--replay-user-messages" in argv
tools_arg = argv[argv.index("--tools") + 1] if "--tools" in argv else "default"
compactor = tools_arg == ""
prime = os.environ.get("DISABLE_PROMPT_CACHING") == "1" and not compactor

def out(v):
    sys.stdout.write(json.dumps(v) + "\n"); sys.stdout.flush()

def note(**k):
    if log_path:
        with open(log_path, "a") as f: f.write(json.dumps(k) + "\n")

inbox = queue.Queue()
def reader():
    for line in sys.stdin:
        line = line.strip()
        if line: inbox.put(json.loads(line))
    inbox.put(None)
threading.Thread(target=reader, daemon=True).start()

def usage(read=1000, write=100):
    return {"input_tokens": 3, "cache_read_input_tokens": read, "cache_creation_input_tokens": write, "output_tokens": 1}

def step(blocks, stop="end_turn"):
    out({"type": "stream_event", "event": {"type": "message_start", "message": {"model": "fake-model", "usage": usage()}}})
    for b in blocks:
        if b["type"] == "text":
            out({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": b["text"]}}})
        out({"type": "assistant", "message": {"content": [b]}})
    out({"type": "stream_event", "event": {"type": "message_delta", "usage": {"output_tokens": 7}, "delta": {"stop_reason": stop}}})

def text_of(msg):
    c = msg["message"]["content"]
    if isinstance(c, str): return c
    return c[-1].get("text", "")

def result(text, err=False):
    out({"type": "result", "subtype": "error_during_execution" if err else "success", "is_error": err, "result": text})

out({"type": "system", "subtype": "init", "tools": ["Bash", "mcp__optchat__zoom", "mcp__optchat__date"]})
first = inbox.get()
if first is None: sys.exit(0)
spawned = os.environ.get("FACET_SPAWN") == "1"
note(kind="spawn" if spawned else ("compact" if compactor else ("prime" if prime else "turn")), argv=argv,
     env={k: os.environ.get(k) for k in ["DISABLE_PROMPT_CACHING", "CLAUDE_CODE_PROMPT_CACHE_TTL"]},
     content=first["message"]["content"])

# a detached agent (`facet spawn`): no Task tool, no engine-side stdin left open after this
# one message, and no engine thread tied to the turn is waiting on it — a deliberate delay
# proves the engine (and a turn sent meanwhile) does not wait for it either
if spawned:
    time.sleep(1.0)
    step([{"type": "text", "text": "SPAWN REPORT: looked into it, done"}])
    result("SPAWN REPORT: looked into it, done")
    sys.exit(0)

if prime:
    out({"type": "stream_event", "event": {"type": "message_start", "message": {"model": "fake-model", "usage": usage(0, 5000)}}})
    time.sleep(30)  # the engine must kill us
    note(kind="prime-not-killed")
    sys.exit(0)

if compactor:
    stepblock = text_of(first)
    if "REFUSE" in stepblock:
        step([{"type": "text", "text": ""}]); result("refused", err=True); sys.exit(0)
    sizes = [600, 300]
    n = 0
    msg = first
    while msg is not None:
        size = 600 if "STUBBORN" in stepblock else sizes[min(n, 1)]
        line = ("S%d " % n) + "z" * (size - 3)
        step([{"type": "text", "text": line}]); result(line)
        n += 1
        msg = inbox.get()
        if msg is not None: note(kind="compact-retry", text=text_of(msg))
    sys.exit(0)

# master turn
def run_agent():
    tid = "toolu_agent_%f" % time.time()
    step([{"type": "tool_use", "id": tid, "name": "Agent",
           "input": {"description": "look it up", "prompt": "find the thing" + "." * 50,
                     "subagent_type": "general-purpose"}}], stop="tool_use")
    out({"type": "system", "subtype": "task_started", "task_id": "task_fake_1", "tool_use_id": tid,
         "description": "look it up", "subagent_type": "general-purpose", "spawn_depth": 1,
         "task_type": "local_agent", "prompt": "find the thing"})
    # the subagent's own two requests: its usage rides on `assistant`, not on a stream_event
    out({"type": "assistant", "parent_tool_use_id": tid,
         "message": {"model": "fake-model", "usage": usage(0, 4000),
                     "content": [{"type": "tool_use", "id": "sub1", "name": "Bash", "input": {"command": "grep -r thing"}}]}})
    out({"type": "user", "parent_tool_use_id": tid,
         "message": {"content": [{"type": "tool_result", "tool_use_id": "sub1", "content": "found in three files"}]}})
    out({"type": "assistant", "parent_tool_use_id": tid,
         "message": {"model": "fake-model", "usage": usage(4000, 20),
                     "content": [{"type": "text", "text": "SUBAGENT PROSE, not the chat's business"}]}})
    out({"type": "system", "subtype": "task_notification", "task_id": "task_fake_1", "tool_use_id": tid,
         "status": "completed", "summary": "found it",
         "usage": {"total_tokens": 4321, "tool_uses": 1, "duration_ms": 1234}})
    time.sleep(0.2)
    out({"type": "user", "message": {"content": [{"tool_use_id": tid, "type": "tool_result",
         "content": [{"type": "text", "text": "REPORT: the thing is in three files"}]}]}})

ACK = ("Async agent launched successfully. (This tool result is internal metadata — never quote"
       " or paste any part of it, including the agentId below, into a user-facing reply.)\n"
       "agentId: task_fake_bg (internal ID - do not mention to user.)")

def run_bg_agent():
    tid = "toolu_agent_bg_%f" % time.time()
    step([{"type": "tool_use", "id": tid, "name": "Agent",
           "input": {"description": "look it up", "prompt": "find the thing" + "." * 50,
                     "subagent_type": "general-purpose", "run_in_background": True}}], stop="tool_use")
    out({"type": "system", "subtype": "task_started", "task_id": "task_fake_bg", "tool_use_id": tid,
         "description": "look it up", "subagent_type": "general-purpose", "is_backgrounded": True,
         "spawn_depth": 1, "task_type": "local_agent", "prompt": "find the thing"})
    out({"type": "user", "message": {"content": [{"tool_use_id": tid, "type": "tool_result",
         "content": [{"type": "text", "text": ACK}]}]}})
    step([{"type": "text", "text": "launched, I will hear back"}])  # the reply, agent still out
    out({"type": "assistant", "parent_tool_use_id": tid,
         "message": {"model": "fake-model", "usage": usage(0, 4000),
                     "content": [{"type": "tool_use", "id": "sub1", "name": "Bash", "input": {"command": "grep -r thing"}}]}})
    out({"type": "user", "parent_tool_use_id": tid,
         "message": {"content": [{"type": "tool_result", "tool_use_id": "sub1", "content": "found in three files"}]}})
    out({"type": "assistant", "parent_tool_use_id": tid,
         "message": {"model": "fake-model", "usage": usage(4000, 20),
                     "content": [{"type": "text", "text": "BG REPORT: the thing is in three files"}]}})
    out({"type": "system", "subtype": "task_notification", "task_id": "task_fake_bg", "tool_use_id": tid,
         "status": "completed", "summary": "BG REPORT: the thing is in three files",
         "output_file": "/tmp/nowhere.output",
         "usage": {"total_tokens": 4321, "tool_uses": 1, "duration_ms": 1234}})
    # the follow-up turn, with this call's own stale view: the engine must end the call here
    out({"type": "system", "subtype": "init", "tools": ["Bash"]})
    step([{"type": "text", "text": "STALE FOLLOW-UP, not the chat's business"}])
    result("launched, I will hear back")

def mcp_call(name, args):
    import urllib.request
    url = json.loads(argv[argv.index("--mcp-config") + 1])["mcpServers"]["optchat"]["url"]
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}).encode()
    r = json.loads(urllib.request.urlopen(urllib.request.Request(url, body, {"Content-Type": "application/json"})).read())
    return r["result"]["content"][0]["text"]

def output_tool(name, args):
    tid = "toolu_out_%f" % time.time()
    step([{"type": "tool_use", "id": tid, "name": "mcp__optchat__" + name, "input": args}], stop="tool_use")
    said = mcp_call(name, args)
    out({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": tid, "content": said}]}})

def run_turn(msg):
    if replay: out({"type": "user", "isReplay": True, "message": msg["message"]})
    t = text_of(msg)
    n = int(t.split("TOOLS ")[1].split()[0]) if "TOOLS " in t else 0
    if "BGAGENT" in t: return run_bg_agent()
    if "AGENT" in t: run_agent()
    if "CHATFAIL" in t:
        output_tool("send_chat", {"text": ""})
    elif "CHAT " in t:
        output_tool("send_chat", {"text": t.split("CHAT ", 1)[1].strip()})
        time.sleep(0.2); step([{"type": "text", "text": "said it"}]); result("said it"); return
    if "CARD " in t:
        output_tool("answer_card", {"id": t.split("CARD ", 1)[1].split()[0], "text": "card answer"})
        time.sleep(0.2); step([{"type": "text", "text": "noted"}]); result("noted"); return
    for k in range(n):
        tid = "tool%d_%f" % (k, time.time())
        step([{"type": "tool_use", "id": tid, "name": "Bash", "input": {"command": "sleep %d" % k}}], stop="tool_use")
        time.sleep(0.6)  # the tool runs
        out({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": tid, "content": "slept %d" % k}]}})
        # messages that arrived while the tool ran ride on its result
        while not inbox.empty():
            m = inbox.get()
            if m is None: sys.exit(0)
            note(kind="midrun", text=text_of(m))
            if replay: out({"type": "user", "isReplay": True, "message": m["message"]})
    time.sleep(0.4)  # the final reply is being written
    step([{"type": "text", "text": "done" if n else "ok"}])
    result("done" if n else "ok")

run_turn(first)
while True:
    m = inbox.get()
    if m is None: break
    note(kind="followup", text=text_of(m))  # what the engine must prevent
    run_turn(m)
