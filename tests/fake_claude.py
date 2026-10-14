#!/usr/bin/env python3
# A stand-in for `claude -p --input-format stream-json --output-format stream-json`, for
# testing the engine without spending tokens. The engine writes one message to an instance
# and nothing after it; anything more would be answered as a follow-up (logged, `followup`).
#
# An instance's script, taken from the last text block of its first user message (its own
# block after the view: "You are instance [x]; ..." then the message or task):
#   "TOOLS n"  -> n tool steps of 0.6 s each, then a reply "done"
#   "SLOW"     -> waits 2 s before its first step
#   "CHAT x"   -> calls the engine's MCP send_chat with "x" (as the real CLI would, over HTTP to
#                 the --mcp-config URL), then a reply "said it"
#   "ZOOM"     -> calls MCP zoom (id 0), then a reply "zoomed"
#   "LIMITHIT" -> the call fails with Claude Code's usage-limit message
#   "IMAGE"    -> one Read call whose result carries an image block, then a reply "seen"
#   "CARD id"  -> calls MCP answer_card for card id with "card answer", then a reply "noted"
#   "NEWCARD at x" -> calls MCP new_card on "Other Note" (anchor x, warn, with a fix "LINE FIVE"),
#                 then a reply "opened"
#   "CHATFAIL" -> calls send_chat with empty text (an error result), then a reply "ok"
#   otherwise  -> one reply "ok"
# Compactor (no --tools): replies 600 bytes first, then 300 bytes after the size feedback,
# unless the step contains "STUBBORN" (always 600). "REFUSE" makes it fail.
# Every invocation is appended to $FAKE_LOG as one JSON line (argv, env, inputs).
import json, os, sys, threading, time, queue

argv = sys.argv[1:]
log_path = os.environ.get("FAKE_LOG")
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
note(kind="compact" if compactor else ("prime" if prime else "instance"), argv=argv,
     env={k: os.environ.get(k) for k in ["DISABLE_PROMPT_CACHING", "CLAUDE_CODE_PROMPT_CACHE_TTL", "FACET_INSTANCE"]},
     content=first["message"]["content"])

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

# an instance
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
    t = text_of(msg)
    n = int(t.split("TOOLS ")[1].split()[0]) if "TOOLS " in t else 0
    if "SLOW" in t: time.sleep(2.0)
    if "IMAGE" in t:
        tid = "toolu_img_%f" % time.time()
        step([{"type": "tool_use", "id": tid, "name": "Read", "input": {"file_path": "/tmp/pic.png"}}], stop="tool_use")
        out({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": tid, "content": [
            {"type": "text", "text": "a picture"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgoFAKE"}}]}]}})
        time.sleep(0.2); step([{"type": "text", "text": "seen"}]); result("seen"); return
    if "LIMITHIT" in t:  # the usage limit cuts the reply off, as Claude Code says it
        step([{"type": "text", "text": ""}]); result("You've hit your session limit", err=True); return
    if "ZOOM" in t:
        output_tool("zoom", {"id": 0, "n": 1})
        time.sleep(0.2); step([{"type": "text", "text": "zoomed"}]); result("zoomed"); return
    if "CHATFAIL" in t:
        output_tool("send_chat", {"text": ""})
    elif "CHAT " in t:
        output_tool("send_chat", {"text": t.split("CHAT ", 1)[1].strip()})
        time.sleep(0.2); step([{"type": "text", "text": "said it"}]); result("said it"); return
    if "NEWCARD" in t:
        output_tool("new_card", {"note": "Other Note", "anchor": t.split("NEWCARD at ", 1)[1].strip(), "text": "server card on $x^2$",
                                 "kind": "warn", "fix": "LINE FIVE"})
        time.sleep(0.2); step([{"type": "text", "text": "opened"}]); result("opened"); return
    if "CARD " in t:
        output_tool("answer_card", {"id": t.split("CARD ", 1)[1].split()[0], "text": "card answer"})
        time.sleep(0.2); step([{"type": "text", "text": "noted"}]); result("noted"); return
    for k in range(n):
        tid = "tool%d_%f" % (k, time.time())
        step([{"type": "tool_use", "id": tid, "name": "Bash", "input": {"command": "sleep %d" % k}}], stop="tool_use")
        time.sleep(0.6)  # the tool runs
        out({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": tid, "content": "slept %d" % k + ("\nShell cwd was reset to /home/fake" if "CWD" in t else "")}]}})
    time.sleep(0.4)  # the final reply is being written
    step([{"type": "text", "text": "done" if n else "ok"}])
    result("done" if n else "ok")

run_turn(first)
while True:
    m = inbox.get()
    if m is None: break
    note(kind="followup", text=text_of(m))  # nothing may be written to an instance after its start
