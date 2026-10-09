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
# Both processes (engine and serve) read facet.json from this HOME, never the real one: a
# vault of two small notes, where the cards (`.claude/cards.json`) are written.
HOME = os.path.join(D, "home"); VAULT = os.path.join(D, "vault"); PORT = __import__("random").randint(20000, 40000)
os.makedirs(os.path.join(HOME, ".config/facet")); os.makedirs(VAULT)
json.dump({"store": D, "port": PORT, "token": "tk", "vault": VAULT}, open(os.path.join(HOME, ".config/facet/facet.json"), "w"))
open(os.path.join(VAULT, "Some Note.md"), "w").write("first\nsecond\nthird line\n")
open(os.path.join(VAULT, "Other Note.md"), "w").write("Other\n\nline three\n\nline five\n\nline seven\n")
env["HOME"] = HOME
CARDS = os.path.join(VAULT, ".claude", "cards.json")
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

    # J: cards (cards.rs). The agent opens and answers them through the engine (op `card`, the
    # same call as its MCP card tools); what it does on one is logged as kind `answer`, never a
    # chat message. The vault had no .claude directory: the first card must make it, not vanish.
    n0 = len(log())
    r = req({"op": "card", "do": "new", "note": "Some Note", "line": 3, "text": "is this right?", "kind": "info"})
    check(r["ok"] is True and r["line"] == 3, "the engine opens a card: %r" % r)
    c1 = r["id"]
    check(os.path.isfile(CARDS), "the card is on file, its directory made")
    r = req({"op": "answer", "id": c1, "text": "looks right to me"})
    check(r["ok"] is True and r.get("fix") is False, "answering with no fix attaches none: %r" % r)
    time.sleep(0.5)
    L = [(m["kind"], m["text"]) for m in log()[n0:]]
    check(L == [("answer", "new info card #%s on [[Some Note]] L3: is this right?" % c1), ("answer", "#%s on [[Some Note]] L3: looks right to me" % c1)],
          "what the agent does on a card is in the stream as kind answer, naming its card: %r" % L)
    card = [c for c in json.load(open(CARDS))["cards"] if c["id"] == c1][0]
    check([(m["by"], m["text"]) for m in card["thread"]] == [("server", "is this right?"), ("server", "looks right to me")] and card["fix"] is None,
          "the thread is kept on the card: %r" % card)
    r = req({"op": "card", "do": "fix", "id": c1, "fix": "third LINE"})
    card = [c for c in json.load(open(CARDS))["cards"] if c["id"] == c1][0]
    check(r["ok"] and card["fix"] == [{"start_line": 3, "end_line": 3, "old_text": "third line", "new_text": "third LINE"}],
          "a fix is set on the card's own line, old text read from the note: %r" % card["fix"])
    r2 = req({"op": "answer", "id": "nosuchcard", "text": "hi"})
    check(r2["ok"] is False and "no card" in r2["error"], "an unknown id is refused, not silently dropped")
    check(req({"op": "answer", "id": c1, "text": ""})["ok"] is False, "an empty answer is refused")
    check(req({"op": "card", "do": "new", "note": "No Such Note", "line": 1, "text": "x"})["ok"] is False, "a card on a missing note is refused")
    out = subprocess.run([BIN, "card", "list"], env=env, capture_output=True, text=True, timeout=10).stdout
    check(("#" + c1) in out and "[fix]" in out, "`facet card list` shows it: %r" % out)
    r = req({"op": "card", "do": "close", "id": c1})
    check(r["ok"] and not any(c["id"] == c1 and not c["closed"] for c in json.load(open(CARDS))["cards"]), "a card closes")

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

    c2 = req({"op": "card", "do": "new", "note": "Other Note", "line": 7, "text": "a comment"})["id"]
    n0 = len(log())
    req({"op": "send", "text": '[[Other Note]] L7 #%s: "a line" CARD %s please' % (c2, c2), "later": True})
    wait(lambda: any(m["kind"] in ("talk", "chat") and m["text"] == "noted" for m in log()[n0:]), 30, "reply after answer_card")
    wait(idle, 30, "idle after answer_card")
    L = [(m["kind"], m["text"]) for m in log()[n0:]]
    check(L == [("user", '[[Other Note]] L7 #%s: "a line" CARD %s please' % (c2, c2)), ("answer", "#%s on [[Other Note]] L7: card answer" % c2), ("talk", "noted")],
          "answer_card logs kind answer, and a card-only turn gets no chat fallback: %r" % L)
    card = [c for c in json.load(open(CARDS))["cards"] if c["id"] == c2][0]
    check([m["text"] for m in card["thread"]] == ["a comment", "card answer"], "answer_card lands on the card")

    n0 = len(log())
    req({"op": "send", "text": "CHATFAIL"})
    wait(lambda: any(m["kind"] == "chat" for m in log()[n0:]), 30, "fallback after a failed send")
    wait(idle, 30, "idle after failed send")
    L = [(m["kind"], m["text"]) for m in log()[n0:]]
    check(L[1][0] == "echo" and "send_chat failed" in L[1][1] and L[-1] == ("chat", "ok"),
          "a failed send is remembered as an echo, and the fallback still answers: %r" % L)

    # what the chat venue shows (the web chat page and Telegram share `Msg::in_chat`; Telegram
    # pushes kind chat alone, unit-tested in log.rs): the real binary's page, served against D
    import urllib.request, urllib.parse, threading, shutil
    port = PORT
    srv = subprocess.Popen([BIN, "serve"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
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
        get = lambda u: urllib.request.urlopen("http://127.0.0.1:%d/tk%s" % (port, u), timeout=40).read().decode()
        def post(u, **f):
            body = urllib.parse.urlencode(f).encode()
            return json.loads(urllib.request.urlopen(urllib.request.Request("http://127.0.0.1:%d/tk%s" % (port, u), body), timeout=10).read())
        q = "/f/cards?notes=" + urllib.parse.quote(json.dumps(["Other Note"]))
        cs = json.loads(get(q))
        check([c["id"] for c in cs["cards"]] == [c2] and [m["by"] for m in cs["cards"][0]["msgs"]] == ["server", "server"]
              and "card answer" in cs["cards"][0]["msgs"][1]["html"] and cs["cards"][0]["note"] == "Other Note",
              "a page gets its notes' cards, each message rendered with its index: %r" % cs)
        check(c1 not in get("/f/cards?all=1"), "a closed card is on no page")

        # a page already open is waiting on its cards (held at version v); the agent opens a new
        # card (MCP new_card), and the held request answers with it — no reload
        held = {}
        def hold():
            t = time.time()
            held["r"] = json.loads(get(q + "&wait=1&v=%d" % cs["v"])); held["s"] = time.time() - t
        th = threading.Thread(target=hold); th.start()
        time.sleep(1.5)
        check("r" not in held, "with nothing new, the page's request is held")
        n0 = len(log())
        req({"op": "send", "text": "NEWCARD at line five"})
        th.join(35)
        new = [c for c in held.get("r", {}).get("cards", []) if c["id"] != c2]
        check(len(new) == 1 and new[0]["kind"] == "warn" and new[0]["line"] == 5 and new[0]["by"] == "server"
              and new[0]["fix"]["text"] == "LINE FIVE" and "katex" not in new[0]["msgs"][0]["html"] and "x^2" in new[0]["msgs"][0]["html"],
              "a card the agent opens reaches the open page through the held request (%.1fs): %r" % (held.get("s", -1), new))
        wait(idle, 30, "idle after new_card")
        L = [(m["kind"], m["text"]) for m in log()[n0:]]
        check(L[0] == ("user", "NEWCARD at line five") and L[1][0] == "answer" and L[1][1].startswith("new warn card #")
              and not any(k in ("tool", "echo") for k, _ in L), "new_card is logged as kind answer, its call quiet: %r" % L)
        s3 = new[0]["id"] if new else ""

        # the page's own side: the first word on a new card opens it and goes to the
        # conversation as a card message (queued); the X closes; apply puts the fix in
        r = post("/x/card", **{"do": "say", "id": "cweb1", "note": "Other Note", "line": "3", "quote": "line three", "text": "why three?"})
        check(r == {"ok": True, "k": 0}, "a word on a new card opens it: %r" % r)
        wait(lambda: any(m["kind"] == "user" and m["text"].startswith('[[Other Note]] L3 #cweb1: "line three"\nwhy three?') for m in log()), 10, "card message logged")
        r = post("/x/card", **{"do": "say", "id": "cweb1", "text": "and four?"})
        check(r == {"ok": True, "k": 1}, "the next word on it is its next message: %r" % r)
        r = post("/x/card", **{"do": "say", "id": s3, "text": "really?"})
        wait(lambda: any(m["kind"] == "user" and "on your warn card: server card" in m["text"] and m["text"].endswith("really?") for m in log()), 10,
             "a reply on the agent's card carries what it had said")
        r = post("/x/card", **{"do": "apply", "id": s3})
        check(r["ok"] and open(os.path.join(VAULT, "Other Note.md")).read().split("\n")[4] == "LINE FIVE", "apply puts the fix in: %r" % r)
        check(s3 not in get(q), "an applied card closes")
        check(post("/x/card", **{"do": "close", "id": "cweb1"})["ok"] and "cweb1" not in get(q), "the X closes a card")
        check(post("/x/card", **{"do": "say", "id": "cweb2", "note": "No Such", "line": "1", "text": "x"})["ok"] is False,
              "a card that cannot be kept says so")
        wait(idle, 30, "idle after card messages")

        # the same, in a page's DOM (jsdom, if there is a node with it: FACET_JSDOM=<node_modules>)
        node = shutil.which("node")
        if node and os.environ.get("FACET_JSDOM"):
            p = subprocess.Popen([node, os.path.join(HERE, "cards_dom.js"), "http://127.0.0.1:%d" % port, "tk", "Other Note"],
                                 env=dict(os.environ, NODE_PATH=os.environ["FACET_JSDOM"]), stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
            ready = ""
            while True:
                line = p.stdout.readline()
                if not line or line.startswith("READY"): ready = line.strip(); break
                if line.startswith(("ok ", "FAIL ")): check(line.startswith("ok "), "dom: " + line.split(" ", 1)[1].strip())
            check(ready.startswith("READY"), "the page loads in jsdom: %r" % ready)
            req({"op": "send", "text": "NEWCARD at line seven"})
            p.stdin.write("go\n"); p.stdin.flush()
            res = p.stdout.read(); p.wait(60)
            for line in res.splitlines():
                if line.startswith(("ok ", "FAIL ")): check(line.startswith("ok "), "dom: " + line.split(" ", 1)[1].strip())
            check(p.returncode == 0, "the DOM test ran through: %r" % res[-500:])
            wait(idle, 30, "idle after the DOM test")
        else:
            print("skip DOM test (no node, or FACET_JSDOM unset)")
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
