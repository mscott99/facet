#!/usr/bin/env python3
# A stand-in for `claude -p --input-format stream-json --output-format stream-json`, for
# testing the engine without spending tokens. It mimics the behaviour measured on the real
# Claude Code (see DEVIATIONS.md): a message written to stdin while a tool runs is delivered
# with that tool's result (and replayed); one that arrives during the final reply is run as a
# follow-up turn of the same conversation after the first `result`.
#
# Turn script, taken from the last text block of the first user message:
#   "TOOLS n"  -> n tool steps of 0.6 s each, then a reply "done"
#   otherwise  -> one reply "ok"
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
note(kind="compact" if compactor else ("prime" if prime else "turn"), argv=argv,
     env={k: os.environ.get(k) for k in ["DISABLE_PROMPT_CACHING", "CLAUDE_CODE_PROMPT_CACHE_TTL"]},
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

# master turn
def run_turn(msg):
    if replay: out({"type": "user", "isReplay": True, "message": msg["message"]})
    t = text_of(msg)
    n = int(t.split("TOOLS ")[1].split()[0]) if "TOOLS " in t else 0
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
