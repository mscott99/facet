#!/usr/bin/env python3
# End-to-end test of the engine against tests/fake_claude.py: no model is called.
#   python3 tests/engine_test.py [path/to/facet]
import json, os, re, socket, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "target", "debug", "facet")
D = tempfile.mkdtemp(prefix="facet-e2e-")
FAKE_LOG = os.path.join(D, "fake.jsonl")
DATA = os.path.join(D, "data")  # engine state lives here, not in the real ~/.local/share/facet
env = dict(os.environ, OPTCHAT_DIR=D, FACET_DATA_DIR=DATA, FACET_CLAUDE=os.path.join(HERE, "fake_claude.py"), FAKE_LOG=FAKE_LOG)
fails = []
REPORT = "BG REPORT: the thing is in three files"  # what the fake's backgrounded subagent says

def check(cond, what):
    print(("ok   " if cond else "FAIL ") + what)
    if not cond: fails.append(what)

def req(v):
    s = socket.socket(socket.AF_UNIX); s.connect(os.path.join(D, "lock"))
    s.sendall((json.dumps(v) + "\n").encode())
    data = b""
    while not data.endswith(b"\n"):
        c = s.recv(65536)
        if not c: break
        data += c
    s.close()
    return json.loads(data)

def log():
    out = []
    d = os.path.join(D, "chat/main")
    for f in sorted(os.listdir(d)):
        out += [json.loads(l) for l in open(os.path.join(d, f)) if l.strip()]
    return sorted(out, key=lambda m: m["i"])

def fake():
    return [json.loads(l) for l in open(FAKE_LOG)] if os.path.exists(FAKE_LOG) else []

def events():
    """The engine's introspection log: its state directory is named by a hash of D, so it is
    found by the `engine` record that names the directory it was started for."""
    import glob
    for p in glob.glob(os.path.join(DATA, "engine-*/events.jsonl")):
        for line in open(p):
            v = json.loads(line)
            if v.get("ev") != "engine": continue
            if v.get("dir") == D: return [json.loads(l) for l in open(p)]
            break  # some other engine's log
    return []

def state_dir():
    """Same lookup as `events()`, but the directory itself — for files beside events.jsonl
    (cards.json, limits.json, ...) that nothing else exposes a path to."""
    import glob
    for p in glob.glob(os.path.join(DATA, "engine-*/events.jsonl")):
        for line in open(p):
            v = json.loads(line)
            if v.get("ev") != "engine": continue
            if v.get("dir") == D: return os.path.dirname(p)
            break
    return None

def wait(pred, secs, what):
    t = time.time()
    while time.time() - t < secs:
        if pred(): return True
        time.sleep(0.1)
    check(False, "timed out: " + what)
    return False

def idle():
    s = req({"op": "status"})
    return not s["busy"] and s["unsummarized"] == 0 and s["compacting"] == 0

# seed: 300 messages, each with a 250-byte summary already built (a view of ~77k chars,
# so the master call is primed); merges are left to the compactor
os.makedirs(os.path.join(D, "chat/main")); os.makedirs(os.path.join(D, "chat/tree"))
with open(os.path.join(D, "chat/main/2020-01-01.jsonl"), "w") as f, open(os.path.join(D, "chat/tree/2020-01-01.jsonl"), "w") as g:
    for i in range(300):
        text = "seed message %d " % i + "q" * 600
        f.write(json.dumps({"i": i, "kind": "note", "text": text, "size": len(text) + 6, "date": "2020-01-01T00:00:00.000Z"}) + "\n")
        node = ("note: seed %d " % i).ljust(250, "s")
        g.write(json.dumps({"l": 0, "i": i, "text": node, "size": 250}) + "\n")

eng = subprocess.Popen([BIN, "engine"], env=env, stdout=subprocess.DEVNULL, stderr=open(os.path.join(D, "engine.err"), "w"))
try:
    wait(lambda: os.path.exists(os.path.join(D, "lock")) and req({"op": "status"}) is not None, 10, "engine up")
    second = subprocess.run([BIN, "engine"], env=env, capture_output=True, timeout=10)
    check(second.returncode == 1 and b"another engine" in second.stderr, "a second engine exits (lock socket)")
    wait(idle, 120, "seed compaction")
    st = req({"op": "status"})
    check(st["unsummarized"] == 0, "seeded chat settles (%d messages)" % st["messages"])
    merges = [x for x in fake() if x["kind"] == "compact"]
    check(len(merges) > 0, "compactor built merges with a model call (%d calls)" % len(merges))
    # compactor input: no ids in the context; the context's last block marked; </chat> in the step
    c = merges[0]["content"]
    check(all("|" not in b["text"].split("\n")[1][:12] for b in c[:-1] if b["text"].startswith("<chat>\n") and len(b["text"]) > 8), "no ids in compactor context")
    check(c[-1]["text"].startswith("</chat>\n\nFor length only, here is an invented example line about no real chat, exactly 512 bytes;"), "step block opens with </chat> and SCALE")
    check(sum(1 for b in c if "cache_control" in b) <= 4, "at most 4 cache marks per compactor call")
    check(all(x["env"]["DISABLE_PROMPT_CACHING"] == "1" for x in merges), "compactor runs with Claude Code marks off")
    retries = [x for x in fake() if x["kind"] == "compact-retry"]
    check(len(retries) == len(merges) and all(r["text"].startswith("That line is 600 bytes; the limit is 512. It must end where it is cut here:\n") and r["text"].endswith("| ← LIMIT") for r in retries),
          "one size retry per node, with the cut-at-limit feedback")
    tree = [json.loads(l) for f in os.listdir(os.path.join(D, "chat/tree")) for l in open(os.path.join(D, "chat/tree", f))]
    called = [n for n in tree if n["text"].startswith("S")]
    check(called and all(n["size"] == 300 for n in called), "node keeps the try that fits")

    # A: a plain turn, primed
    n0 = len(log())
    req({"op": "send", "text": "hello"})
    wait(lambda: any(m["kind"] in ("talk", "chat") for m in log()[n0:]), 30, "reply to hello")
    wait(idle, 30, "idle after hello")
    L = log()[n0:]
    check([m["kind"] for m in L[:2]] == ["user", "chat"] and L[0]["text"] == "hello" and L[1]["text"] == "ok", "turn: user, chat")
    F = fake()
    primes = [x for x in F if x["kind"] == "prime"]
    turns = [x for x in F if x["kind"] == "turn"]
    check(len(primes) == 1 and not any(x["kind"] == "prime-not-killed" for x in F), "the call was primed and the priming call killed")
    pv = [b["text"] for b in primes[0]["content"]]
    tv = [b["text"] for b in turns[0]["content"]]
    check(pv == tv[:-1] and tv[-1] == "hello", "priming sends exactly the real call's view blocks")
    check(all("cache_control" in b for b in primes[0]["content"]) and not any("cache_control" in b for b in turns[0]["content"]), "marks on the priming call only")
    check(primes[0]["argv"] == turns[0]["argv"], "priming and real call use identical arguments")
    check(turns[0]["env"]["CLAUDE_CODE_PROMPT_CACHE_TTL"] == "5m" and primes[0]["env"]["DISABLE_PROMPT_CACHING"] == "1", "5m entries; priming with Claude Code marks off")
    check("hello" not in "".join(tv[:-1]), "the view is rendered before the new message is logged")

    # B: mid-run messages
    n0 = len(log())
    req({"op": "send", "text": "TOOLS 3"})
    wait(lambda: any(m["kind"] == "tool" for m in log()[n0:]), 30, "first tool")
    time.sleep(0.2)
    req({"op": "send", "text": "MID during tool"})
    wait(lambda: sum(1 for m in log()[n0:] if m["kind"] == "echo") >= 3, 30, "third tool result")
    time.sleep(0.15)
    req({"op": "send", "text": "LATE during reply"})
    wait(lambda: any(m["text"] == "LATE during reply" for m in log()[n0:]), 30, "late message logged")
    wait(idle, 30, "idle after mid-run")
    L = [(m["kind"], m["text"]) for m in log()[n0:]]
    print("     ", [k if k != "user" else "user:" + t for k, t in L])
    ki = [k for k, _ in L]
    mid = L.index(("user", "MID during tool"))
    check(ki[mid - 1] == "echo" and ki[mid - 2] == "tool", "mid-run message logged right after the tool result it rode on")
    late = L.index(("user", "LATE during reply"))
    check(("chat", "done") in L[:late] and L[late + 1:] and L[late + 1] == ("chat", "ok"), "a message during the final reply starts a fresh call")
    F = fake()
    check(not any(x["kind"] == "followup" for x in F), "no follow-up turn in a stale conversation")
    check(any(x["kind"] == "midrun" and x["text"] == "MID during tool" for x in F), "mid-run message was delivered to the running call")

    # B2: messages sent for turns of their own (`later`) wait, then start one call each, in
    # order; a plain message sent meanwhile joins whichever turn is running
    n0, f0 = len(log()), len(fake())
    req({"op": "send", "text": "TOOLS 3"})
    wait(lambda: any(m["kind"] == "tool" for m in log()[n0:]), 30, "first tool (later)")
    req({"op": "send", "text": "TOOLS 2 own turn A", "later": True})
    req({"op": "send", "text": "own turn B", "later": True})
    check(req({"op": "status"})["later"] == 2, "later messages wait apart from the running turn")
    time.sleep(0.2)
    req({"op": "send", "text": "MID one"})
    wait(lambda: any(m["text"] == "TOOLS 2 own turn A" for m in log()[n0:]), 30, "turn A starts")
    wait(lambda: any(m["kind"] == "tool" for m in log()[[m["text"] for m in log()].index("TOOLS 2 own turn A"):]), 30, "turn A's first tool")
    time.sleep(0.2)
    req({"op": "send", "text": "MID two"})
    wait(lambda: any(m["text"] == "own turn B" for m in log()[n0:]), 30, "turn B starts")
    wait(idle, 30, "idle after later")
    T = [x["content"][-1]["text"] for x in fake()[f0:] if x["kind"] == "turn"]
    check(T == ["TOOLS 3", "TOOLS 2 own turn A", "own turn B"], "each later message starts a call of its own, in order: %r" % T)
    M = [x["text"] for x in fake()[f0:] if x["kind"] == "midrun"]
    check(M == ["MID one", "MID two"], "plain messages between them go into the running turn: %r" % M)
    U = [m["text"] for m in log()[n0:] if m["kind"] == "user"]
    check(U == ["TOOLS 3", "MID one", "TOOLS 2 own turn A", "MID two", "own turn B"], "logged in that order: %r" % U)

    # C: cancel during a tool
    n0 = len(log())
    req({"op": "send", "text": "TOOLS 6"})
    wait(lambda: any(m["kind"] == "tool" for m in log()[n0:]), 30, "tool before cancel")
    req({"op": "cancel"})
    wait(lambda: not req({"op": "status"})["busy"], 10, "idle after cancel")
    st = req({"op": "status"})
    check(not st["busy"] and st["queued"] == 0, "cancel stops the call")
    n1 = len(log()); time.sleep(1.5)
    check(len(log()) == n1, "nothing is logged after the cancel")

    # D: a stubborn node keeps the shortest of TRIES tries
    req({"op": "send", "text": "STUBBORN " + "w" * 700})
    wait(idle, 60, "idle after stubborn")
    tries = [x for x in fake() if x["kind"] == "compact-retry" and "STUBBORN" not in x["text"]]
    F = fake()
    last_stub = max(k for k, x in enumerate(F) if x["kind"] == "compact" and "STUBBORN" in x["content"][-1]["text"])
    n_retry = 0
    for x in F[last_stub + 1:]:
        if x["kind"] == "compact-retry": n_retry += 1
        elif x["kind"] == "compact": break
    check(n_retry == 4, "TRIES = 5: four feedback rounds for a node that never fits (%d)" % n_retry)

    # E: a crash loses no accepted message: kill -9 with one written mid-run, not yet consumed
    n0 = len(log())
    req({"op": "send", "text": "TOOLS 4"})
    wait(lambda: any(m["kind"] == "tool" for m in log()[n0:]), 30, "tool before crash")
    req({"op": "send", "text": "SURVIVOR"})
    eng.kill(); eng.wait()
    eng = subprocess.Popen([BIN, "engine"], env=env, stdout=subprocess.DEVNULL, stderr=open(os.path.join(D, "engine2.err"), "w"))
    wait(lambda: os.path.exists(os.path.join(D, "lock")) and any(m["text"] == "SURVIVOR" for m in log()[n0:]), 30, "survivor logged after restart")
    wait(idle, 60, "idle after restart")
    L = [(m["kind"], m["text"]) for m in log()[n0:]]
    k = L.index(("user", "SURVIVOR"))
    check(L[k + 1:k + 2] == [("chat", "ok")], "a message accepted before a crash is answered after the restart")

    # G: /model and /zoom
    r = req({"op": "model", "name": "sonnet"})
    check(r["ok"] and req({"op": "status"})["model"] == "sonnet", "/model switches the master model")
    check(req({"op": "model", "name": "gpt"})["ok"] is False, "/model refuses an unknown model")
    z = req({"op": "zoom", "id": 3, "n": 1})["text"]
    check(z.startswith("3+0|note: seed message 3 "), "/zoom gives a message whole")
    check(req({"op": "zoom", "id": 1, "n": 2})["text"] == "No line 1+2.", "/zoom refuses an unaligned line")
    n0 = len(log()); req({"op": "send", "text": "after switch"})
    wait(lambda: any(m["kind"] in ("talk", "chat") for m in log()[n0:]), 30, "turn after model switch")
    last = [x for x in fake() if x["kind"] == "turn"][-1]
    check(last["argv"][last["argv"].index("--model") + 1] == "sonnet", "the next turn runs on the new model")
    wait(idle, 60, "idle after model switch")

    # F: importing notes
    nf = os.path.join(D, "a note.md")
    open(nf, "w").write("# Title\nsome remembered fact\n")
    os.utime(nf, (1700000000, 1700000000))
    out = subprocess.run([BIN, "import", nf], env=env, capture_output=True, text=True, timeout=30).stdout
    m = log()[-1]
    check("message" in out and m["kind"] == "note" and m["text"].endswith("# Title\nsome remembered fact") and "a note.md" in m["text"].split("\n")[0],
          "a file becomes one note: its path, then its content")
    check(m["date"].startswith("2023-11-14"), "the note keeps the file's date")
    out2 = subprocess.run([BIN, "import", nf], env=env, capture_output=True, text=True, timeout=30).stdout
    check("skipped" in out2 and log()[-1]["i"] == m["i"], "importing the same file again adds nothing")
    bf = os.path.join(D, "bin.dat"); open(bf, "wb").write(b"\xff\xfe\x00\x01")
    out3 = subprocess.run([BIN, "import", bf], env=env, capture_output=True, text=True, timeout=30).stdout
    check("NOT imported: not a text file" in out3, "a binary file is refused")
    n0 = len(log())
    req({"op": "send", "text": "TOOLS 3"})
    wait(lambda: req({"op": "status"})["busy"], 10, "turn for queued import")
    r = req({"op": "note", "text": "during a turn"})
    check(r["ok"] is True and r.get("queued") is True, "an import during a turn is queued")
    r2 = req({"op": "note", "text": "during a turn"})
    check(r2.get("skipped") == "already queued", "the same import twice during a turn is queued once")
    wait(idle, 60, "idle after import tests")
    new = log()[n0:]
    notes = [x for x in new if x["kind"] == "note"]
    check(len(notes) == 1 and notes[0]["text"] == "during a turn" and new[-1]["kind"] == "note",
          "a queued import is logged once, after the turn's messages")

    # G: a subagent: its steps stay out of the chat, its cost does not stay out of the log
    n0 = len(log())
    req({"op": "send", "text": "AGENT please"})
    wait(lambda: any(m["kind"] in ("talk", "chat") for m in log()[n0:]), 30, "reply after the subagent")
    wait(idle, 60, "idle after the subagent turn")
    L = log()[n0:]
    work = [m for m in L if m["kind"] == "work"]
    check(len(work) == 1 and work[0]["text"].startswith("REPORT:"), "the subagent's report is logged, as kind work")
    check(not any("SUBAGENT PROSE" in m["text"] or "grep -r thing" in m["text"] for m in L),
          "the subagent's own steps and prose are not logged")
    ev = events()
    a = [x for x in ev if x["ev"] == "agent"]
    check(len(a) == 1 and a[0]["reqs"] == 2 and a[0]["eq"] > 0 and a[0]["status"] == "completed",
          "one agent record, with the requests we counted: %r" % (a[-1] if a else None))
    check(a and a[0]["tokens"] == 4321 and a[0]["tools"] == 1 and a[0]["task_ms"] == 1234
          and a[0]["report_bytes"] == len(work[0]["text"]) and a[0]["ask_bytes"] > 0 and a[0]["task"] == "task_fake_1",
          "the agent record keeps what it was asked, what it cost and what it left behind")
    ar = [x for x in ev if x["ev"] == "req" and x["kind"] == "agent"]
    check(len(ar) == 2 and all(x["tool_use_id"].startswith("toolu_agent") for x in ar),
          "each subagent request is logged on its own, under the agent that made it (%d)" % len(ar))
    t = [x for x in ev if x["ev"] == "turn"][-1]
    check(t["agents"] == 1 and t["agent_reqs"] == 2 and t["agent_eq"] > 0 and t["agent_bytes"] == len(work[0]["text"]),
          "the turn record totals what its subagents cost")
    av = [v.get("argv", []) for v in fake()]
    # --agents is a path: the view is too big for one argv string on Linux (128 KiB)
    paths = [a[a.index("--agents") + 1] for a in av if "--agents" in a]
    check(paths and all(len(x) < 1000 and os.path.isfile(x) for x in paths) and all(len(x) < 100000 for a in av for x in a),
          "every call names an agents file, and no argument carries the view")
    defs = [json.load(open(paths[-1]))] if paths else []
    check(defs and all(set(d) == {"general-purpose", "Explore"} for d in defs)
          and all("<chat>" in a["prompt"] and a["model"] for d in defs for a in d.values()),
          "every call defines the subagents, and each definition carries the view")
    p = defs[-1]["general-purpose"]["prompt"] if defs else ""
    check(p.rstrip().endswith("</chat>") and re.search(r"\n\d+\+\d+\|", p),
          "the view in a definition is the whole current one, lines and all")
    check(defs and all({"mcp__optchat__zoom", "mcp__optchat__date"} <= set(a["tools"])
                       for d in defs for a in d.values()),
          "a subagent may open a line of the view, as the master may")

    u = [json.loads(l) for l in open(os.path.join(D, "usage.jsonl"))]
    check({"compact", "prime", "turn", "agent"} <= {x["kind"] for x in u}, "usage logged per request for compact, prime, turn and agent")
    check(sum(1 for x in u if x["kind"] == "agent") == 2, "a subagent's own requests are priced apart from the turn's")
    # H: a backgrounded subagent reports after the reply, so its report cannot be that
    # turn's: it comes in as a message of its own and starts another turn (§9)
    n0 = len(log())
    req({"op": "send", "text": "BGAGENT please"})
    wait(lambda: any(m["kind"] == "work" and m["text"].startswith("[task_fake_bg]") for m in log()[n0:]),
         30, "the background report joins the chat")
    wait(idle, 60, "idle after the background report's turn")
    L = log()[n0:]
    rep = [m for m in L if m["kind"] == "work" and m["text"].startswith("[task_fake_bg]")]
    check(len(rep) == 1 and REPORT in rep[0]["text"],
          "a backgrounded subagent's report joins the chat as one message starting \"[id] \"")
    check(not any("Async agent launched" in m["text"] for m in L),
          "the launch receipt is not logged as a report")
    check(not any("STALE FOLLOW-UP" in m["text"] for m in L),
          "nothing the follow-up turn says is logged")
    check(any(m["kind"] in ("talk", "chat") and m["i"] > rep[0]["i"] for m in L), "the report starts a turn")
    turns = [v for v in fake() if v.get("kind") == "turn"]
    said = turns[-1]["content"][-1]["text"] if turns else ""
    check(said.startswith("[task_fake_bg]"), "that turn is a fresh call, with the report as its message")
    check(not any(v.get("kind") == "followup" for v in fake()), "no message was answered by a follow-up turn")
    ab = [x for x in events() if x["ev"] == "agent" and x["task"] == "task_fake_bg"]
    check(len(ab) == 1 and ab[0]["status"] == "completed" and ab[0]["reqs"] == 2
          and ab[0]["report_bytes"] == len(REPORT) and ab[0]["tokens"] == 4321,
          "the backgrounded agent's record is finished, with what it cost and left: %r" % (ab[0] if ab else None))
    u = [json.loads(l) for l in open(os.path.join(D, "usage.jsonl"))]
    check(sum(1 for x in u if x["kind"] == "agent") == 4, "its own two requests are priced apart too")

    # I: a detached subagent (`facet spawn`) is not a Task call and not owned by any turn: it
    # must survive the turn that launched it ending, and report later as its own turn (§9)
    n0 = len(log())
    u0 = sum(1 for x in (json.loads(l) for l in open(os.path.join(D, "usage.jsonl"))) if x["kind"] == "agent")
    sp = subprocess.run([BIN, "spawn", "--model", "sonnet", "--kind", "general-purpose", "--desc", "test spawn",
                          "look into the thing and report back"], env=env, capture_output=True, text=True, timeout=10)
    check(sp.returncode == 0 and sp.stdout.strip() != "", "facet spawn returns an id at once: %r" % ((sp.stdout, sp.stderr),))
    sid = sp.stdout.strip()
    check(not req({"op": "status"})["busy"], "the engine is not busy right after a spawn (it is not a turn)")
    # a normal turn, started right after the spawn, runs and finishes well inside the 1s the
    # fake's detached agent sleeps before it reports: proof the engine does not wait on it
    req({"op": "send", "text": "hi again"})
    wait(lambda: any(m["kind"] in ("talk", "chat") for m in log()[n0:]), 30, "an ordinary turn still runs fine right after a spawn")
    wait(idle, 10, "idle right after that turn, before the spawn has reported")
    logged = [m for m in log()[n0:] if m["kind"] == "tool" and m["text"].startswith("spawn " + sid)]
    check(len(logged) == 1 and logged[0]["text"].endswith("(sonnet, general-purpose, test spawn): look into the thing and report back"),
          "the spawn's task is logged once, verbatim: %r" % (logged,))
    check(not any(sid in m["text"] for m in log()[n0:] if m["kind"] != "tool"), "the spawn's report has not arrived yet at that point")
    wait(lambda: any(m["kind"] == "work" and m["text"].startswith("[%s] " % sid) for m in log()[n0:]),
         15, "the detached agent's report joins the chat, after the turn that spawned it is long done")
    wait(idle, 30, "idle after the spawn's own turn")
    # the shell's "cwd was reset" note is not part of what the command printed
    n1 = len(log())
    req({"op": "send", "text": "TOOLS 1 CWD"})
    wait(lambda: any(m["kind"] in ("talk", "chat") for m in log()[n1:]), 30, "the CWD turn finishes")
    ec = [m["text"] for m in log()[n1:] if m["kind"] == "echo"]
    check(ec == ["slept 0"], "the cwd-reset line is stripped from an echo: %r" % (ec,))
    wait(idle, 30, "idle after the CWD turn")
    L = log()[n0:]
    rep = [m for m in L if m["kind"] == "work" and m["text"].startswith("[%s] " % sid)]
    check(len(rep) == 1 and "SPAWN REPORT" in rep[0]["text"],
          "the detached agent's report starts a turn of its own, the same shape as a backgrounded Task agent's")
    check(any(m["kind"] in ("talk", "chat") and m["i"] > rep[0]["i"] for m in L), "that turn replies")
    ev = events()
    asp = [x for x in ev if x["ev"] == "agent" and x.get("tool_use_id") == sid]
    check(len(asp) == 1 and asp[0]["status"] == "done" and asp[0]["reqs"] >= 1 and asp[0]["eq"] > 0
          and asp[0].get("detached") is True and asp[0]["report_bytes"] == len(rep[0]["text"]) - len("[%s] " % sid),
          "the detached agent's own cost is recorded, kind agent, status done: %r" % (asp[0] if asp else None))
    u1 = sum(1 for x in (json.loads(l) for l in open(os.path.join(D, "usage.jsonl"))) if x["kind"] == "agent")
    check(u1 > u0, "its own requests are priced as kind agent too (%d -> %d)" % (u0, u1))

    # J: a deliberate answer to a line-comment card (§ the viewer's cards) — never a talk
    # reply its poll merely happens to catch. The card itself is registered the way web.rs's
    # `/x/send` does it (a file beside events.jsonl, not through the socket): this engine
    # never learns of the vault, so `--apply` is exercised as a unit test in diag.rs instead.
    sd = state_dir()
    check(sd is not None, "the engine's state directory is found")
    cards_f = os.path.join(sd, "cards.json")
    json.dump({"c1": {"note": "Some Note", "line": 3, "answers": []}}, open(cards_f, "w"))
    n0 = len(log())
    r = req({"op": "answer", "id": "c1", "text": "looks right to me"})
    check(r["ok"] is True and r.get("code") is None, "answering with no --apply attaches no fix: %r" % r)
    time.sleep(0.5)
    L = log()[n0:]
    check([(m["kind"], m["text"]) for m in L] == [("answer", "#c1 on [[Some Note]] L3: looks right to me")],
          "the answer is in the stream as kind answer, naming its card: %r" % L)
    card = json.load(open(cards_f))["c1"]
    check(len(card["answers"]) == 1 and card["answers"][0]["text"] == "looks right to me" and card["answers"][0]["code"] is None,
          "the answer is recorded against its card, not just the chat: %r" % card)
    r2 = req({"op": "answer", "id": "no-such-card", "text": "hi"})
    check(r2["ok"] is False and "no card" in r2["error"], "an unregistered id is refused, not silently dropped")
    r3 = req({"op": "answer", "id": "c1", "text": ""})
    check(r3["ok"] is False, "an empty answer is refused")

    # K: stream and venues. The master's plain text is the stream's only; send_chat (an MCP
    # tool) is what reaches the chat venue; answer_card reaches the card and the stream, never
    # the chat; a chat message left unanswered in the chat gets the turn's final text (fallback)
    n0 = len(log())
    req({"op": "send", "text": "CHAT hello from the chat tool"})
    wait(lambda: any(x["ev"] == "recap_dropped" for x in events()), 30, "recap after send_chat dropped")
    wait(idle, 30, "idle after send_chat")
    L = log()[n0:]
    check([(m["kind"], m["text"]) for m in L] == [("user", "CHAT hello from the chat tool"), ("chat", "hello from the chat tool")],
          "send_chat logs kind chat, its call and result leave no tool/echo lines, and the recap after it is dropped: %r" % [(m["kind"], m["text"]) for m in L])
    check(not any(x.get("fallback") for x in events() if x["ev"] == "turn" and x["first"] == n0), "a turn that sent to the chat gets no fallback")

    n0 = len(log())
    req({"op": "send", "text": "plain question"})
    wait(lambda: any(m["kind"] == "chat" for m in log()[n0:]), 30, "fallback chat")
    wait(idle, 30, "idle after fallback")
    L = [(m["kind"], m["text"]) for m in log()[n0:]]
    check(L == [("user", "plain question"), ("chat", "ok")], "with no send, the final text goes to the chat as kind chat: %r" % L)
    check(any(x["ev"] == "fallback" and x["turn"] == n0 for x in events()) and
          any(x["ev"] == "turn" and x["first"] == n0 and x["fallback"] for x in events()), "the fallback is recorded")

    n0 = len(log())
    req({"op": "send", "text": "ZOOM please"})
    wait(lambda: any(m["text"] == "zoomed" for m in log()[n0:]), 30, "reply after zoom")
    wait(idle, 30, "idle after zoom")
    L = [(m["kind"], m["text"]) for m in log()[n0:]]
    check(not any(k in ("tool", "echo") for k, _ in L) and not any("zoom" in x.lower() for k, x in L if k not in ("user", "talk", "chat")),
          "a zoom leaves no tool or echo line in the log, only the reply: %r" % L)

    json.dump({"c2": {"note": "Other Note", "line": 7, "answers": []}}, open(cards_f, "w"))
    n0 = len(log())
    req({"op": "send", "text": '[[Other Note]] L7 #c2: "a line" CARD c2 please', "later": True})
    wait(lambda: any(m["kind"] in ("talk", "chat") and m["text"] == "noted" for m in log()[n0:]), 30, "reply after answer_card")
    wait(idle, 30, "idle after answer_card")
    L = [(m["kind"], m["text"]) for m in log()[n0:]]
    check(L == [("user", '[[Other Note]] L7 #c2: "a line" CARD c2 please'), ("answer", "#c2 on [[Other Note]] L7: card answer"), ("talk", "noted")],
          "answer_card logs kind answer, and a card-only turn gets no chat fallback: %r" % L)
    card = json.load(open(cards_f))["c2"]
    check([a["text"] for a in card["answers"]] == ["card answer"], "answer_card lands on the card")

    n0 = len(log())
    req({"op": "send", "text": "CHATFAIL"})
    wait(lambda: any(m["kind"] == "chat" for m in log()[n0:]), 30, "fallback after a failed send")
    wait(idle, 30, "idle after failed send")
    L = [(m["kind"], m["text"]) for m in log()[n0:]]
    check(L[1][0] == "echo" and "send_chat failed" in L[1][1] and L[-1] == ("chat", "ok"),
          "a failed send is remembered as an echo, and the fallback still answers: %r" % L)

    # what the chat venue shows (the web chat page and Telegram share `Msg::in_chat`; Telegram
    # pushes kind chat alone, unit-tested in log.rs): the real binary's page, served against D
    import random, urllib.request, urllib.parse
    home = os.path.join(D, "home"); os.makedirs(os.path.join(home, ".config/facet"))
    port = random.randint(20000, 40000)
    json.dump({"store": D, "port": port, "token": "tk"}, open(os.path.join(home, ".config/facet/facet.json"), "w"))
    srv = subprocess.Popen([BIN, "serve"], env=dict(env, HOME=home), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        page = ""
        for _ in range(50):
            try:
                page = urllib.request.urlopen("http://127.0.0.1:%d/tk/f/log?since=-1" % port, timeout=2).read().decode(); break
            except Exception: time.sleep(0.1)
        check("hello from the chat tool" in page and "plain question" in page, "the chat page shows user messages and chat sends")
        check("said it" not in page and "noted" not in page, "the chat page hides the agent's plain text")
        check("card answer" not in page and "CARD c2" not in page, "the chat page hides card comments and card answers")
        check("send_chat failed" not in page and "seed message" not in page, "the chat page hides steps and notes")
        # a card's answers, as its page asks for them: by id, saying where they start; and the
        # whole card, thread and all, as a reload puts it back
        get = lambda u: urllib.request.urlopen("http://127.0.0.1:%d/tk%s" % (port, u), timeout=5).read().decode()
        rp = get("/f/reply?id=c2&since=0")
        check('data-from="0" data-high="1"' in rp and "card answer" in rp, "a card's answers come by id, from where it asked: %r" % rp)
        check("card answer" not in get("/f/reply?id=c2&since=1"), "an answer already shown is not sent again")
        cs = json.loads(get("/f/cards?notes=" + urllib.parse.quote(json.dumps(["Other Note"]))))
        check(len(cs) == 1 and cs[0]["id"] == "c2" and cs[0]["n"] == 1 and "card answer" in cs[0]["thread"],
              "a reload puts the card back with its thread: %r" % cs)
    finally:
        srv.kill()

    # L: a queued restart re-execs the engine (same pid) once idle, taking the lock over again
    ne0 = sum(1 for x in events() if x["ev"] == "engine")
    n0 = len(log())
    r = req({"op": "restart"})
    check(r["ok"] is True and r.get("queued") is True, "restart is queued at once: %r" % r)
    wait(lambda: sum(1 for x in events() if x["ev"] == "engine") > ne0, 30, "engine re-execs")
    wait(lambda: os.path.exists(os.path.join(D, "lock")) and req({"op": "status"}) is not None, 10, "engine back up")
    check(eng.poll() is None, "same process (pid %d) after the restart" % eng.pid)
    ev = events()
    check(any(x["ev"] == "restart" and x["pid"] == eng.pid for x in ev), "restart recorded in events.jsonl")
    check(any(m["kind"] == "echo" and "restarting" in m["text"] for m in log()[n0:]), "restart noted in the chat log")
    n0 = len(log())
    req({"op": "send", "text": "after restart"})
    wait(idle, 30, "turn after restart")
    check(any(m["kind"] in ("talk", "chat") for m in log()[n0:]), "a turn works after the restart")
    check(req({"op": "restart"})["ok"] is True and not (time.sleep(3) or eng.poll()), "a second restart works too")

    check(os.path.isdir(os.path.join(D, ".git")), "chat directory committed after turns")
finally:
    eng.kill()
print("\n%d failure(s); dir %s" % (len(fails), D))
sys.exit(1 if fails else 0)
