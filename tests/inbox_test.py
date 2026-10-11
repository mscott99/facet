#!/usr/bin/env python3
# Messages sent while the engine is down: kept in the inbox, shown as "queued" after a reload,
# taken by the engine when it starts, shown as "working" while held, then ordinary messages.
#   python3 tests/inbox_test.py [path/to/facet]
import json, os, re, subprocess, sys, tempfile, time, urllib.parse, urllib.request, glob, random

HERE = os.path.dirname(os.path.abspath(__file__))
BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "target", "debug", "facet")
D = tempfile.mkdtemp(prefix="facet-inbox-e2e-")
DATA = os.path.join(D, "data"); HOME = os.path.join(D, "home"); VAULT = os.path.join(D, "vault"); PORT = random.randint(20000, 40000)
os.makedirs(os.path.join(HOME, ".config/facet")); os.makedirs(VAULT); os.makedirs(os.path.join(D, "chat/main")); os.makedirs(os.path.join(D, "chat/tree"))
json.dump({"store": D, "port": PORT, "token": "tk", "vault": VAULT}, open(os.path.join(HOME, ".config/facet/facet.json"), "w"))
env = dict(os.environ, OPTCHAT_DIR=D, FACET_DATA_DIR=DATA, FACET_CLAUDE=os.path.join(HERE, "fake_claude.py"), FAKE_LOG=os.path.join(D, "fake.jsonl"), HOME=HOME)
fails = []
def check(c, w):
    print(("ok   " if c else "FAIL ") + w, flush=True)
    if not c: fails.append(w)
def get(u): return urllib.request.urlopen("http://127.0.0.1:%d/tk%s" % (PORT, u), timeout=40).read().decode()
def post(u, **f): return urllib.request.urlopen(urllib.request.Request("http://127.0.0.1:%d/tk%s" % (PORT, u), urllib.parse.urlencode(f).encode()), timeout=10).read().decode()
def wait(p, s, w):
    t = time.time()
    while time.time() - t < s:
        if p(): return True
        time.sleep(0.1)
    check(False, "timed out: " + w); return False
def log():
    out = []
    for f in sorted(os.listdir(os.path.join(D, "chat/main"))): out += [json.loads(l) for l in open(os.path.join(D, "chat/main", f)) if l.strip()]
    return out
def outbox(page):  # the server-rendered outbox on /chat
    m = re.search(r'<div id=outbox>(.*)</div></div>', page, re.S); return m.group(1) if m else ""

srv = subprocess.Popen([BIN, "serve"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
eng = None
try:
    for _ in range(50):
        try: get("/chat"); break
        except Exception: time.sleep(0.1)
    check("not running" in get("/chat") and 'id=down class=at style="color:var(--err)">' in get("/chat"), "engine is down, page says so")
    check(post("/x/send", text="first while down", later="0") == "queued", "send with the engine down is answered 'queued'")
    check(post("/x/send", text="second while down", later="1") == "queued", "a second one too")
    ob = outbox(get("/chat"))
    check("first while down" in ob and "second while down" in ob and ob.count("ob queued") == 2 and ob.index("first") < ob.index("second"),
          "a reload shows both as queued, in order: %r" % ob[:300])
    srv.kill(); srv.wait()
    srv = subprocess.Popen([BIN, "serve"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(50):
        try: get("/chat"); break
        except Exception: time.sleep(0.1)
    check(outbox(get("/chat")).count("ob queued") == 2, "still queued after serve restarts")
    check(len(glob.glob(os.path.join(DATA, "engine-*/inbox/*.json"))) == 2, "two files in the inbox")
    # the live poll wakes on an outbox change: ask with the current hash, send, get the answer
    h = re.search(r'data-obh="(\d+)"', get("/f/log?since=-1")).group(1)
    import threading; held = {}
    def hold(): held["r"] = get("/f/log?wait=1&since=-1&obh=" + h)
    th = threading.Thread(target=hold); th.start(); time.sleep(1)
    check("r" not in held, "live request is held while nothing changes")
    post("/x/send", text="third while down", later="0"); th.join(10)
    check("third while down" in held.get("r", ""), "the live poll is woken by a new queued message")
    eng = subprocess.Popen([BIN, "engine"], env=env, stdout=subprocess.DEVNULL, stderr=open(os.path.join(D, "engine.err"), "w"))
    wait(lambda: len([m for m in log() if m["kind"] == "user"]) >= 1, 60, "engine takes the inbox")
    wait(lambda: not glob.glob(os.path.join(DATA, "engine-*/inbox/*.json")), 30, "inbox empties")
    wait(lambda: not outbox(get("/chat")).strip(), 60, "outbox empties once logged")
    time.sleep(2)
    users = [m["text"] for m in log() if m["kind"] == "user"]
    check(sorted(users) == ["first while down", "second while down", "third while down"], "each reaches the log once: %r" % users)
    allseen = " ".join(users + [m["text"] for m in log()])
    check(all(x in allseen for x in ["first while down", "second while down", "third while down"]), "none lost")
    check(sum(m["text"].count("first while down") for m in log() if m["kind"] == "user") <= 1, "none duplicated")
finally:
    srv.kill()
    if eng: eng.kill()
print("\n%d failure(s); dir %s" % (len(fails), D))
sys.exit(1 if fails else 0)
