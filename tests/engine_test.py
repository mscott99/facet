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
env["FACET_LIMIT_BACKOFF_MS"] = "6000"  # a usage-limit pause of 6 s, not a minute
CARDS = os.path.join(VAULT, ".claude", "cards.json")
fails = []

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
    # compactor input: the view's id+n| lines (the gist's), at most CVIEW; the gist's task, after </chat>
    c = merges[0]["content"]
    ctx = "".join(b["text"] for b in c[:-1])
    check(all(re.match(r"^\d+\+\d+\|", l) for l in ctx.split("\n")[1:] if l), "compactor context lines are id+n|text")
    check(all(len("".join(b["text"] for b in x["content"][:-1]).encode()) <= 32_000 + 600 for x in merges), "compaction view within CVIEW (32 KB)")
    check(c[-1]["text"].startswith("</chat>\n\nCompaction: ") and ("-" * 512) in c[-1]["text"] and c[-1]["text"].endswith("</input>"),
          "task block opens with </chat>, carries the 512-dash ruler and <input>")
    check(sum(1 for b in c if "cache_control" in b) <= 2 and "cache_control" in c[-1], "at most 2 cache marks per compactor call, one on the step (the gist's)")
    whole = [k for k, b in enumerate(c[:-1]) if b["text"].count("\n") == 4]
    check(all(b["text"].count("\n") == 4 for b in c[:-2]) and [k for k, b in enumerate(c[:-1]) if "cache_control" in b] == whole[-1:],
          "compactor context in blocks of 4 lines, the last whole one marked")
    check(all(x["env"]["DISABLE_PROMPT_CACHING"] == "1" for x in merges), "compactor runs with Claude Code marks off")
    retries = [x for x in fake() if x["kind"] == "compact-retry"]
    check(len(retries) == len(merges) and all(r["text"].startswith("Too long: your line is 600 bytes, over the 512-byte limit. Write\nthe whole line again for the same <input>, cutting just enough of the\nleast valuable items to fit before this cut:\n") and r["text"].endswith("| ← LIMIT") for r in retries),
          "one size retry per node, with the cut-at-limit feedback")
    tree = [json.loads(l) for f in os.listdir(os.path.join(D, "chat/tree")) for l in open(os.path.join(D, "chat/tree", f))]
    called = [n for n in tree if n["text"].startswith("S")]
    check(called and all(n["size"] == 300 for n in called), "node keeps the try that fits")

    # A: one message, one instance, primed: the user's message, the kickoff, the safety net's
    # chat (the fake says nothing to the user), the end record pointing at its kickoff
    def insts(): return [x for x in fake() if x["kind"] == "instance"]
    def inst_of(name): return [x for x in insts() if x["env"].get("FACET_INSTANCE") == name][-1]
    def name_of(m): return m["text"][1:m["text"].index("]")]
    def view_of(x): return "".join(b["text"] for b in x["content"][:-1])
    n0 = len(log())
    req({"op": "send", "text": "hello"})
    wait(lambda: any(m["kind"] == "work" for m in log()[n0:]), 30, "end record for hello")
    wait(idle, 30, "idle after hello")
    L = log()[n0:]
    check([m["kind"] for m in L] == ["user", "start", "chat", "work"] and L[0]["text"] == "hello" and L[2]["text"] == "ok",
          "an instance: user, kickoff, chat, end record: %r" % [(m["kind"], m["text"][:60]) for m in L])
    nm = name_of(L[1])
    check(L[1]["text"].startswith("[inst_") and L[1]["text"].endswith("answers message %d" % n0), "the kickoff names the message it answers: %r" % L[1]["text"])
    check(L[3]["text"].startswith("[%s] (kickoff %d) done; it sent no reply" % (nm, n0 + 1)), "the end record points at its kickoff: %r" % L[3]["text"])
    F = fake()
    primes = [x for x in F if x["kind"] == "prime"]
    turns = insts()
    check(len(primes) == 1 and not any(x["kind"] == "prime-not-killed" for x in F), "the instance was primed and the priming call killed")
    pv = [b["text"] for b in primes[0]["content"]]
    tv = [b["text"] for b in turns[0]["content"]]
    check(pv == tv[:-1] and tv[-1].startswith("You are instance [%s]; your kickoff is message %d." % (nm, n0 + 1)) and tv[-1].endswith("hello"),
          "priming sends exactly the instance's view blocks; its own block names it and carries the message: %r" % tv[-1])
    pc = primes[0]["content"]
    pm = [k for k, b in enumerate(pc) if "cache_control" in b]
    lw = max([k for k, b in enumerate(pc) if b["text"].count("\n") == 4 and b["text"].endswith("\n")], default=-1)
    check(pm == sorted({lw, len(pc) - 1} - {-1}) and not any("cache_control" in b for b in turns[0]["content"]),
          "marks on the priming call only: its last whole 4-line block and its end")
    check(all(b["text"].count("\n") == 4 for b in pc[:-1]), "the view goes out in blocks of 4 lines")
    check(primes[0]["argv"] == turns[0]["argv"], "priming and instance use identical arguments")
    check(turns[0]["env"]["CLAUDE_CODE_PROMPT_CACHE_TTL"] == "5m" and primes[0]["env"]["DISABLE_PROMPT_CACHING"] == "1"
          and turns[0]["env"]["FACET_INSTANCE"] == nm, "5m entries; priming with Claude Code marks off; the instance knows its name")
    check("hello" not in view_of(turns[0]), "the view is rendered before the new message is logged")
    av = [v.get("argv", []) for v in F if v["kind"] in ("instance", "prime")]
    check(av and all("Task" not in a[a.index("--tools") + 1].split(",") and "--agents" not in a and "--replay-user-messages" not in a for a in av)
          and all(len(x) < 100000 for a in av for x in a), "no Task tool, no Task agents, no argument carries the view")

    # B: instances run side by side; each sees the log as of its own start: a running one's
    # kickoff, an ended one's end record; both start and end are chronological
    n0 = len(log())
    req({"op": "send", "text": "SLOW TOOLS 2 first"})
    wait(lambda: any(m["kind"] == "start" for m in log()[n0:]), 30, "first kickoff")
    req({"op": "send", "text": "TOOLS 1 second"})
    wait(lambda: sum(1 for m in log()[n0:] if m["kind"] == "work") >= 2, 40, "both end records")
    wait(idle, 30, "idle after two instances")
    L = log()[n0:]
    print("     ", [(m["kind"], m["text"][:40]) for m in L])
    ix = lambda f: next(k for k, m in enumerate(L) if f(m))
    k1 = ix(lambda m: m["kind"] == "user" and m["text"] == "SLOW TOOLS 2 first"); a1 = name_of(L[k1 + 1])
    k2 = ix(lambda m: m["kind"] == "user" and m["text"] == "TOOLS 1 second"); a2 = name_of(L[k2 + 1])
    w1 = ix(lambda m: m["kind"] == "work" and m["text"].startswith("[%s] (kickoff %d)" % (a1, L[k1 + 1]["i"])))
    w2 = ix(lambda m: m["kind"] == "work" and m["text"].startswith("[%s] (kickoff %d)" % (a2, L[k2 + 1]["i"])))
    check(k2 < w1 and w2 < w1, "the second started while the first ran, and ended first: kickoffs and ends in time order")
    check(not any(m["kind"] in ("tool", "echo", "talk") for m in L), "no instance's own steps reach the log")
    v2 = view_of(inst_of(a2))
    check(("[%s] kickoff" % a1) in v2 and ("[%s] (kickoff" % a1) not in v2, "the second's view has the first's kickoff, not its end record")
    n1 = len(log())
    req({"op": "send", "text": "third"})
    wait(lambda: any(m["kind"] == "work" for m in log()[n1:]), 30, "third's end record")
    wait(idle, 30, "idle after third")
    a3 = name_of(log()[n1 + 1])
    v3 = view_of(inst_of(a3))
    check(("[%s] (kickoff" % a1) in v3 and ("[%s] (kickoff" % a2) in v3, "a later instance sees both end records")
    z = req({"op": "zoom", "id": a1})["text"]
    check("tool: Bash" in z and "echo: slept 0" in z, "zoom(\"name\") opens an instance's own steps: %r" % z[:200])
    check(not any(x["kind"] == "followup" for x in fake()), "nothing is written to an instance after its start")

    # C: cancel stops a running instance; its end record says so
    n0 = len(log())
    req({"op": "send", "text": "TOOLS 6"})
    wait(lambda: any(m["kind"] == "start" for m in log()[n0:]), 30, "kickoff before cancel")
    time.sleep(1.0)
    req({"op": "cancel"})
    wait(lambda: any(m["kind"] == "work" and "cancelled" in m["text"] for m in log()[n0:]), 10, "cancelled end record")
    wait(lambda: not req({"op": "status"})["busy"], 10, "idle after cancel")
    st = req({"op": "status"})
    check(not st["busy"] and st["queued"] == 0 and st["instances"] == 0, "cancel stops the instance")

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

    # E: a message sent while the engine is down waits in the inbox and is answered at start
    n0 = len(log())
    saved = json.load(open(os.path.join(D, "chat", "view.json")))
    eng.kill(); eng.wait()
    out = subprocess.run([BIN, "send", "SURVIVOR"], env=env, capture_output=True, text=True, timeout=10)
    check(out.returncode == 0, "facet send works with the engine down: %r" % ((out.stdout, out.stderr),))
    eng = subprocess.Popen([BIN, "engine"], env=env, stdout=subprocess.DEVNULL, stderr=open(os.path.join(D, "engine2.err"), "w"))
    wait(lambda: os.path.exists(os.path.join(D, "lock")) and any(m["text"] == "SURVIVOR" for m in log()[n0:]), 30, "survivor logged after restart")
    err2 = open(os.path.join(D, "engine2.err")).read()
    check("view of" in err2 and "folded" not in err2 and isinstance(saved, list) and len(saved) > 0 and all(len(p) == 2 for p in saved), "the view is loaded from view.json at a restart, not folded again, saved as the gist's [l, i] pairs (§3.2)")
    wait(idle, 60, "idle after restart")
    L = [(m["kind"], m["text"]) for m in log()[n0:]]
    k = L.index(("user", "SURVIVOR"))
    check([x[0] for x in L[k + 1:k + 4]] == ["start", "chat", "work"] and L[k + 2][1] == "ok", "a message sent while the engine was down is answered after the restart: %r" % L[k:])

    # G: /model and /zoom
    r = req({"op": "model", "name": "sonnet"})
    check(r["ok"] and req({"op": "status"})["model"] == "sonnet", "/model switches the model of new instances")
    check(req({"op": "model", "name": "gpt"})["ok"] is False, "/model refuses an unknown model")
    z = req({"op": "zoom", "id": 3, "n": 1})["text"]
    check(z.startswith("3+0|note: seed message 3 "), "/zoom gives a message whole")
    check(req({"op": "zoom", "id": 1, "n": 2})["text"] == "No line 1+2.", "/zoom refuses an unaligned line")
    n0 = len(log()); req({"op": "send", "text": "after switch"})
    wait(lambda: any(m["kind"] == "work" for m in log()[n0:]), 30, "instance after model switch")
    last = insts()[-1]
    check(last["argv"][last["argv"].index("--model") + 1] == "sonnet", "the next instance runs on the new model")
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
    wait(lambda: req({"op": "status"})["busy"], 10, "an instance for the import test")
    r = req({"op": "note", "text": "while an instance runs"})
    check(r["ok"] is True and "i" in r, "an import while an instance runs goes in at once: %r" % r)
    r2 = req({"op": "note", "text": "while an instance runs"})
    check(r2.get("skipped") == "already in the memory", "the same import twice is added once")
    wait(idle, 60, "idle after import tests")

    # S: handing on (`facet spawn`): an instance like any other, on agent_model (Sonnet); the
    # caller is named in its kickoff, its task logged there verbatim; it starts no user message
    n0 = len(log())
    sp = subprocess.run([BIN, "spawn", "--desc", "test spawn", "look into the thing and report back"],
                        env=dict(env, FACET_INSTANCE="inst_parent"), capture_output=True, text=True, timeout=10)
    sid = sp.stdout.strip()
    check(sp.returncode == 0 and sid.startswith("inst_"), "facet spawn returns a name at once: %r" % ((sp.stdout, sp.stderr),))
    wait(lambda: any(m["kind"] == "work" and m["text"].startswith("[%s] " % sid) for m in log()[n0:]), 30, "the spawned instance's end record")
    wait(idle, 30, "idle after spawn")
    L = [(m["kind"], m["text"]) for m in log()[n0:]]
    check([k for k, _ in L] == ["start", "work"] and L[0][1] == "[%s] kickoff (sonnet, effort high): task from [inst_parent] (test spawn):\n\nlook into the thing and report back" % sid
          and L[1][1] == "[%s] (kickoff %d) ok" % (sid, n0), "a handed-on task: its kickoff, then its end record, no user message, no chat: %r" % L)
    sv = inst_of(sid)
    check(sv["argv"][sv["argv"].index("--model") + 1] == "sonnet" and sv["content"][-1]["text"].startswith("You are instance [%s]" % sid)
          and "a task handed to you by [inst_parent]" in sv["content"][-1]["text"], "it runs on sonnet, told who handed it the task")
    n0 = len(log())
    sp = subprocess.run([BIN, "spawn", "--wait", "--desc", "waited", "-"], input="wait for me", env=env, capture_output=True, text=True, timeout=30)
    check(sp.returncode == 0 and re.match(r"^\[inst_[0-9a-f]+\] \(kickoff \d+\) ok$", sp.stdout.strip()) is not None, "spawn --wait prints the end record: %r" % ((sp.stdout, sp.stderr),))
    wait(idle, 30, "idle after spawn --wait")
    # the shell's "cwd was reset" note is not part of what a command printed
    n1 = len(log())
    req({"op": "send", "text": "TOOLS 1 CWD"})
    wait(lambda: any(m["kind"] == "work" for m in log()[n1:]), 30, "the CWD instance ends")
    wait(idle, 30, "idle after the CWD instance")
    z = req({"op": "zoom", "id": name_of(log()[n1 + 1])})["text"]
    check("echo: slept 0" in z and "Shell cwd was reset" not in z, "the cwd-reset line is stripped from a step: %r" % z[-200:])
    u = [json.loads(l) for l in open(os.path.join(D, "usage.jsonl"))]
    check({"compact", "prime", "instance"} <= {x["kind"] for x in u}, "usage logged per request for compact, prime and instance")
    ev = events()
    check(any(x["ev"] == "start" and x["instance"] == sid for x in ev) and any(x["ev"] == "instance" and x["instance"] == sid and x["status"] == "done" for x in ev),
          "start and end of each instance are in events.jsonl")

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

    # K: stream and venues. An instance's plain text and steps are not in the stream; send_chat
    # (an MCP tool) is what reaches the chat venue; answer_card reaches the card and the stream,
    # never the chat; a chat message left unanswered gets the safety net
    def ends(n0): return [x for x in events() if x["ev"] == "instance" and x["kickoff"] >= n0]
    n0 = len(log())
    req({"op": "send", "text": "CHAT hello from the chat tool"})
    wait(lambda: any(m["kind"] == "work" for m in log()[n0:]), 30, "end after send_chat")
    wait(idle, 30, "idle after send_chat")
    L = [(m["kind"], m["text"]) for m in log()[n0:]]
    nm = L[1][1][1:L[1][1].index("]")]
    check(L == [("user", "CHAT hello from the chat tool"), L[1], ("chat", "hello from the chat tool"), ("work", "[%s] (kickoff %d) said it" % (nm, n0 + 1))],
          "send_chat logs kind chat; the end record is the instance's last text: %r" % L)
    check(any(x["spoke"] and not x["net"] for x in ends(n0)), "an instance that sent to the chat gets no safety net")

    n0 = len(log())
    req({"op": "send", "text": "ZOOM please"})
    wait(lambda: any(m["kind"] == "work" for m in log()[n0:]), 30, "end after zoom")
    wait(idle, 30, "idle after zoom")
    L = [(m["kind"], m["text"]) for m in log()[n0:]]
    check([k for k, _ in L] == ["user", "start", "chat", "work"] and L[2][1] == "zoomed", "a zoom, like any step, stays out of the log: %r" % [(k, x[:60]) for k, x in L])

    c2 = req({"op": "card", "do": "new", "note": "Other Note", "line": 7, "text": "a comment"})["id"]
    n0 = len(log())
    req({"op": "send", "text": '[[Other Note]] L7 #%s: "a line" CARD %s please' % (c2, c2)})
    wait(lambda: any(m["kind"] == "work" for m in log()[n0:]), 30, "end after answer_card")
    wait(idle, 30, "idle after answer_card")
    L = [(m["kind"], m["text"]) for m in log()[n0:]]
    nm = L[1][1][1:L[1][1].index("]")] if len(L) > 1 else ""
    check(L == [("user", '[[Other Note]] L7 #%s: "a line" CARD %s please' % (c2, c2)), L[1], ("answer", "#%s on [[Other Note]] L7: card answer" % c2), ("work", "[%s] (kickoff %d) noted" % (nm, n0 + 1))],
          "answer_card logs kind answer, and a card message gets no chat: %r" % L)
    card = [c for c in json.load(open(CARDS))["cards"] if c["id"] == c2][0]
    check([m["text"] for m in card["thread"]] == ["a comment", "card answer"], "answer_card lands on the card")

    n0 = len(log())
    req({"op": "send", "text": "CHATFAIL"})
    wait(lambda: any(m["kind"] == "work" for m in log()[n0:]), 30, "end after a failed send")
    wait(idle, 30, "idle after failed send")
    L = [(m["kind"], m["text"]) for m in log()[n0:]]
    check([k for k, _ in L] == ["user", "start", "chat", "work"] and L[2][1] == "ok" and "it sent no reply" in L[3][1],
          "a failed send does not count as a reply: the safety net still answers: %r" % L)
    check(any(x["net"] and not x["spoke"] for x in ends(n0)), "the safety net is recorded")

    n0 = len(log())
    req({"op": "send", "text": "plain question"})
    wait(lambda: any(m["kind"] == "work" for m in log()[n0:]), 30, "end after plain question")
    wait(idle, 30, "idle after plain question")

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
        check("said it" not in page and "noted" not in page and "kickoff" not in page, "the chat page hides end records and kickoffs")
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
        check(L[0] == ("user", "NEWCARD at line five") and L[1][0] == "start" and L[2][0] == "answer" and L[2][1].startswith("new warn card #")
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

        # M: the usage limit cuts a turn off: the engine parks, the pause is on the chat page
        # (and in its live poll) and the home page, and pushed to Telegram once (a local Bot
        # API stand-in, named by `telegram.api`); it ends by itself, pushed once more
        import http.server
        tg = []
        class TG(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
                tg.append((self.path, json.loads(body or b"{}")))
                out = json.dumps({"ok": True, "result": {"message_id": len(tg)}}).encode()
                self.send_response(200); self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(out))); self.end_headers(); self.wfile.write(out)
            def log_message(self, *a): pass
        tgs = http.server.ThreadingHTTPServer(("127.0.0.1", 0), TG)
        threading.Thread(target=tgs.serve_forever, daemon=True).start()
        cfgp = os.path.join(HOME, ".config/facet/facet.json")
        c0 = json.load(open(cfgp))
        json.dump(dict(c0, telegram={"bot_token": "BOT", "chat_id": 42, "api": "http://127.0.0.1:%d" % tgs.server_port}), open(cfgp, "w"))
        sd = req({"op": "status"})["state_dir"]
        n0 = len(log())
        req({"op": "send", "text": "LIMITHIT please"})
        wait(lambda: req({"op": "status"}).get("pause_line"), 30, "pause after the limit")
        st = req({"op": "status"})
        check("paused until" in st["pause_line"] and "UTC" in st["pause_line"] and "usage limit" in st["pause_line"],
              "status gives the pause line: %r" % st["pause_line"])
        check(os.path.exists(os.path.join(sd, "pause.json")), "the pause is kept in pause.json")
        page = get("/chat")
        check(re.search(r'<p id=paused [^>]*>engine paused until \d\d:\d\d UTC', page) is not None and "<p id=paused class=at style=\"color:var(--err)\" hidden" not in page,
              "the chat page shows the pause: %r" % re.findall(r"<p id=paused[^<]*", page))
        frag = get("/f/log?since=%d" % (len(log()) + 5))
        check('data-paused="engine paused until' in frag, "the live poll carries the pause line: %r" % frag[-300:])
        check("paused until" in get("/"), "the home page shows the pause")
        wait(lambda: any("paused until" in m.get("text", "") for _, m in tg), 10, "telegram push on park")
        parks = [m for p, m in tg if "paused until" in m.get("text", "")]
        check(len(parks) == 1 and tg[0][0] == "/botBOT/sendMessage" and parks[0]["chat_id"] == 42 and "send that message again" in parks[0]["text"],
              "one Telegram push on park, saying the reply was cut off: %r" % tg)
        wait(lambda: not req({"op": "status"}).get("pause_line"), 30, "the pause ends by itself")
        wait(lambda: any("resumed" in m.get("text", "") for _, m in tg), 10, "telegram push on resume")
        check(sum("resumed" in m.get("text", "") for _, m in tg) == 1 and len(tg) == 2, "one Telegram push on resume, none more: %r" % tg)
        check(not os.path.exists(os.path.join(sd, "pause.json")), "pause.json is gone once resumed")
        page = get("/chat")
        check("<p id=paused class=at style=\"color:var(--err)\" hidden>" in page, "the chat page hides the pause line once resumed")
        check('data-paused=""' in get("/f/log?since=%d" % (len(log()) + 5)), "and so does the live poll")
        json.dump(c0, open(cfgp, "w"))
        tgs.shutdown()
        wait(idle, 30, "idle after the limit")
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
    wait(lambda: any(m["kind"] == "work" for m in log()[n0:]), 30, "instance after restart")
    wait(idle, 30, "idle after restart")
    check(any(m["kind"] == "chat" for m in log()[n0:]), "an instance answers after the restart")
    check(req({"op": "restart"})["ok"] is True and not (time.sleep(3) or eng.poll()), "a second restart works too")

    check(os.path.isdir(os.path.join(D, ".git")), "chat directory committed after instances")
finally:
    eng.kill()
print("\n%d failure(s); dir %s" % (len(fails), D))
sys.exit(1 if fails else 0)
