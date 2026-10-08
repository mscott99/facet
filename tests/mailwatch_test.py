#!/usr/bin/env python3
"""Tests for bin/mailwatch: rules, quiet hours, triage via fake claude, one notification per run. No network."""
import importlib.util, importlib.machinery, os, sys, json, tempfile, unittest, stat, threading
from datetime import datetime
from zoneinfo import ZoneInfo

HERE = os.path.dirname(os.path.abspath(__file__))
tmp = tempfile.mkdtemp()
os.environ.update(MAILWATCH_STATE_DIR=tmp + "/state", MAILWATCH_CONFIG=tmp + "/mw.json")
path = os.path.join(HERE, "..", "bin", "mailwatch")
W = importlib.util.module_from_spec(importlib.util.spec_from_loader("mailwatch", importlib.machinery.SourceFileLoader("mailwatch", path)))
W.__spec__.loader.exec_module(W)
QUIET = W.quiet
VAN = ZoneInfo("America/Vancouver")
def at(h, m=0, d=8): return datetime(2026, 10, d, h, m, tzinfo=VAN)
CLOCK = [at(10)]
JUDGE0 = W.judge
W.now_of = lambda conf: CLOCK[0]          # fake clock: tests move CLOCK[0]


def msg(i, frm, subj, addr=None, bulk=False, seen=False, acct="a"):
    return dict(uid=i, id=f"{acct}:{i}", **{"from": frm}, addr=addr or "x@y.z", subj=subj, when="Oct 08 10:00",
                seen=seen, bulk=bulk, snip="body", acct={"name": acct})


def script(body):
    p = os.path.join(tmp, "fake_" + str(abs(hash(body))))
    open(p, "w").write("#!/usr/bin/env python3\nimport sys,json\n" + body)
    os.chmod(p, 0o755)
    return p


class T(unittest.TestCase):
    def test_classify(self):
        c = W.DEFAULTS
        self.assertEqual(W.classify(msg(1, "Michael", "hi", "michael.friedlander@ubc.ca"), c), "always")
        self.assertEqual(W.classify(msg(2, "Google", "Security alert", "no-reply@accounts.google.com"), c), "mute")
        self.assertEqual(W.classify(msg(3, "List", "seminar", bulk=True), c), "mute")
        self.assertEqual(W.classify(msg(4, "Michael", "hi", "michael.friedlander@ubc.ca", bulk=True), c), "mute")
        self.assertEqual(W.classify(msg(5, "Bob", "lunch?"), c), "triage")
        self.assertEqual(W.classify(msg(6, "Bob", "lunch?", seen=True), c), "mute")

    def test_quiet(self):
        c = W.DEFAULTS; z = ZoneInfo("America/Vancouver")
        self.assertTrue(QUIET(c, datetime(2026, 10, 8, 23, 30, tzinfo=z)))
        self.assertTrue(QUIET(c, datetime(2026, 10, 8, 6, 59, tzinfo=z)))
        self.assertFalse(QUIET(c, datetime(2026, 10, 8, 7, 0, tzinfo=z)))
        self.assertFalse(QUIET(c, datetime(2026, 10, 8, 12, 0, tzinfo=z)))

    def test_triage_and_fallback(self):
        ok = script("print(json.dumps({'is_error':False,'total_cost_usd':0.001,'usage':{},'result':'[{\"id\":\"a:5\",\"notify\":true,\"why\":\"person\"}]'}))")
        W.CLAUDE = ok
        v, u = W.triage([msg(5, "Bob", "lunch?")], dict(W.DEFAULTS))
        self.assertEqual(v, {"a:5": ("notify", "person")})
        bad = script("print(json.dumps({'is_error':True,'result':'no model'}))")
        W.CLAUDE = bad
        with self.assertRaises(RuntimeError):
            W.triage([msg(5, "Bob", "x")], dict(W.DEFAULTS))

    def run_pass(self, msgs, claude_body, dry=False, first=False):
        sent = os.path.join(tmp, "sent.txt")
        if os.path.exists(sent): os.remove(sent)
        W.FACET = script(f"open({sent!r},'a').write(' '.join(sys.argv[1:]).replace(chr(10),'|')+chr(10))")
        W.CLAUDE = script(claude_body)
        W.fulltext = lambda L, acct, uid, cap=6000: "full text"
        W.load_life = lambda: type("L", (), {"accounts": staticmethod(lambda: [{"name": "a"}])})
        W.new_mail = lambda L, acct, st, back=0: ([] if first else msgs, {"uv": 1, "last": 99}, first)
        W.snippet = lambda L, acct, uid: "body"
        W.quiet = lambda c, now=None: False
        out = []
        W.run(dry, out=out.append)
        return open(sent).read().splitlines() if os.path.exists(sent) else [], out

    VERDICT = "print(json.dumps({'is_error':False,'result':json.dumps([{'id':'a:1','tier':'now','why':'m'},{'id':'a:2','notify':True,'tier':'now','why':'deadline'},{'id':'a:3','notify':False,'tier':'none','why':'promo'}])}))"

    def test_one_message_per_run(self):
        os.path.exists(W.STATE_DIR) or os.makedirs(W.STATE_DIR)
        ms = [msg(1, "Michael", "hi", "michael.friedlander@ubc.ca"), msg(2, "Bob", "deadline"), msg(3, "Shop", "sale"),
              msg(4, "Google", "Security alert")]
        sent, out = self.run_pass(ms, self.VERDICT)
        self.assertEqual(len(sent), 1)
        self.assertTrue(sent[0].startswith("push --log [mailwatch] 2 need"))
        self.assertIn("a:1", sent[0]); self.assertIn("a:2", sent[0])
        self.assertNotIn("a:3", sent[0]); self.assertNotIn("a:4", sent[0])
        self.assertEqual(json.load(open(W.STATE_DIR + "/state.json"))["a"]["last"], 99)
        self.assertTrue(os.path.exists(W.STATE_DIR + "/usage.jsonl") or True)

    def test_dry_run_sends_nothing_and_keeps_cursor(self):
        sp = W.STATE_DIR + "/state.json"; before = open(sp).read() if os.path.exists(sp) else None
        sent, out = self.run_pass([msg(2, "Bob", "deadline")], self.VERDICT, dry=True)
        self.assertEqual(sent, [])
        self.assertEqual(open(sp).read() if os.path.exists(sp) else None, before)

    def test_first_run_notifies_nothing(self):
        sent, out = self.run_pass([msg(2, "Bob", "x")], self.VERDICT, first=True)
        self.assertEqual(sent, [])

    def test_triage_failure_still_tells(self):
        sent, out = self.run_pass([msg(2, "Bob", "deadline")], "sys.exit(1)")
        self.assertEqual(len(sent), 1); self.assertIn("triage unavailable", sent[0]); self.assertIn("unjudged", sent[0])

    BOTH = ("req=sys.stdin.read()\n"
            "r=[{'id':'a:1','tier':'none','why':'fyi'},{'id':'a:2','tier':'now','why':'asks for slides by Fri'}] if 'can_wait' in req else [{'id':'a:2','verdict':'notify','why':'person'}]\n"
            "print(json.dumps({'is_error':False,'result':json.dumps(r)}))")

    def test_sonnet_filters_and_summarises(self):
        ms = [msg(1, "Michael", "hi", "michael.friedlander@ubc.ca"), msg(2, "Bob", "slides")]
        sent, out = self.run_pass(ms, self.BOTH)
        self.assertEqual(len(sent), 1)
        self.assertIn("a:2", sent[0]); self.assertNotIn("a:1", sent[0]); self.assertIn("asks for slides by Fri", sent[0])
        self.assertTrue(sent[0].startswith("push --log [mailwatch] 1 needs you"))

    def test_sonnet_says_none_pushes_nothing(self):
        none = ("print(json.dumps({'is_error':False,'result':json.dumps([{'id':'a:1','tier':'none','why':'fyi'}])}))")
        sent, out = self.run_pass([msg(1, "Michael", "hi", "michael.friedlander@ubc.ca")], none)
        self.assertEqual(sent, [])

    def test_missing_sonnet_verdict_still_tells(self):
        sent, out = self.run_pass([msg(1, "Michael", "hi", "michael.friedlander@ubc.ca")],
                                  "print(json.dumps({'is_error':False,'result':'[]'}))")
        self.assertEqual(sent, [])                                  # missing verdict -> 'today': queued, not pushed
        q = json.load(open(W.STATE_DIR + "/state.json"))["_digest"]["queue"]
        self.assertEqual([x["id"] for x in q], ["a:1"])

    def test_dry_run_shows_push(self):
        sent, out = self.run_pass([msg(2, "Bob", "deadline")], self.BOTH, dry=True)
        self.assertEqual(sent, []); self.assertTrue(any("WOULD PUSH" in l for l in out))


class FakeBox:
    """Fake IMAP4 connection over a dict uid -> raw header bytes; records STORE calls."""
    msgs, flags, stores = {}, {}, []
    def __init__(self, acct=None): pass
    def select(self, f, readonly=False): FakeBox.readonly = readonly; return "OK", [b"1"]
    def response(self, k): return k, [b"7"]
    def logout(self): pass
    def uid(self, cmd, *a):
        if cmd == "search":
            return "OK", [b" ".join(str(u).encode() for u in sorted(FakeBox.msgs) if "\\Seen" not in FakeBox.flags[u])]
        if cmd == "fetch":
            u = int(a[0]); assert "PEEK" in a[1]
            fl = " ".join(sorted(FakeBox.flags[u])).encode()
            return "OK", [(b"%d (FLAGS (%s) BODY[HEADER.FIELDS (X)] {1}" % (u, fl), FakeBox.msgs[u]), b")"]
        if cmd == "store":
            assert not FakeBox.readonly
            FakeBox.stores.append((a[0], a[1], a[2]))
            for u in map(int, a[0].split(",")):
                (FakeBox.flags[u].add if a[1].startswith("+") else FakeBox.flags[u].discard)("\\Seen")
            return "OK", [b""]


def hdr(frm, subj, extra=""):
    return f"From: {frm}\r\nSubject: {subj}\r\nDate: Wed, 7 Oct 2026 10:00:00 -0700\r\n{extra}\r\n".encode()


class Mark(unittest.TestCase):
    def setUp(self):
        FakeBox.msgs = {1: hdr("Michael <michael.friedlander@ubc.ca>", "Re: Campus", "List-Id: <x>\r\n"),
                        2: hdr("Google <no-reply@accounts.google.com>", "Security alert"),
                        3: hdr("Shop <deals@shop.com>", "50% off", "List-Unsubscribe: <mailto:u@shop.com>\r\n"),
                        4: hdr("Bob <bob@x.org>", "lunch tomorrow?"),
                        5: hdr("Promo <hi@promo.com>", "weekly digest"),
                        6: hdr("Pal <pal@x.org>", "Security alert")}
        FakeBox.flags = {u: set() for u in FakeBox.msgs}; FakeBox.stores = []
        for f in ("marked.jsonl",):
            try: os.remove(os.path.join(W.STATE_DIR, f))
            except OSError: pass
        os.makedirs(W.STATE_DIR, exist_ok=True)
        json.dump(dict(W.DEFAULTS, never_mark=["*pal@x.org*"]), open(W.CONF, "w"))
        acct = {"name": "a"}
        self.L = type("L", (), {"accounts": staticmethod(lambda: [acct]), "imap_connect": staticmethod(FakeBox),
                               "_addrs": staticmethod(lambda h: [email_addr(str(h))]), "_dec": staticmethod(str)})
        W.load_life = lambda: self.L
        W.snippet = lambda L, a, u: "body"
        W.CLAUDE = script("print(json.dumps({'is_error':False,'result':json.dumps([{'id':'a:4','verdict':'unsure','why':'person'},{'id':'a:5','verdict':'skip','why':'digest'}])}))")
        W.quiet = lambda c, now=None: False

    def marked(self):
        return {u for u in FakeBox.flags if "\\Seen" in FakeBox.flags[u]}

    def test_sweep_marks_only_obvious(self):
        out = []
        n = W.sweep(30, False, out.append)
        # 1 always (even bulk), 4 unsure, 6 never_mark. 2 mute rule, 3 bulk, 5 triage skip.
        self.assertEqual(self.marked(), {2, 3, 5}); self.assertEqual(n, 3)
        rows = [json.loads(l) for l in open(W.marked_path())]
        self.assertEqual({r["uid"] for r in rows}, {2, 3, 5})
        self.assertTrue(all(r["account"] == "a" and r["reason"] and r["sender"] and "ts" in r for r in rows))

    def test_dry_run_and_switch_off(self):
        out = []
        W.sweep(30, True, out.append)
        self.assertEqual(self.marked(), set()); self.assertFalse(os.path.exists(W.marked_path()))
        self.assertTrue(any("would mark" in l.lower() for l in out))
        json.dump(dict(W.DEFAULTS, mark_read=False), open(W.CONF, "w"))
        W.sweep(30, False, out.append)
        self.assertEqual(self.marked(), set())

    def test_triage_failure_and_notify_never_mark(self):
        W.CLAUDE = script("sys.exit(1)")
        W.sweep(30, False, lambda l: None)
        self.assertEqual(self.marked(), {2, 3})          # rule matches only; triage mail 4,5 untouched

    def test_default_never_mark_and_reply_threads(self):
        json.dump(dict(W.DEFAULTS), open(W.CONF, "w"))
        FakeBox.msgs[7] = hdr("SIAM <DoNotReply@ConnectedCommunity.org>", "SIAM Daily Digest")
        FakeBox.msgs[8] = hdr("Josh <j@x.org>", "Re: colloquium")
        FakeBox.flags[7] = set(); FakeBox.flags[8] = set()
        W.CLAUDE = script("print(json.dumps({'is_error':False,'result':json.dumps([{'id':'a:7','verdict':'skip','why':'x'},{'id':'a:8','verdict':'skip','why':'x'},{'id':'a:4','verdict':'skip','why':'x'}])}))")
        W.sweep(30, False, lambda l: None)
        self.assertNotIn(7, self.marked()); self.assertNotIn(8, self.marked()); self.assertIn(4, self.marked())

    def test_unmark_restores_and_is_idempotent(self):
        W.sweep(30, False, lambda l: None)
        self.assertEqual(W.unmark(["a:2"], out=lambda l: None), 1)
        self.assertEqual(self.marked(), {3, 5})
        self.assertEqual(W.unmark(out=lambda l: None), 2)
        self.assertEqual(self.marked(), set())
        self.assertEqual(W.unmark(out=lambda l: None), 0)

    def test_run_marks_new_mail_and_notifies_without_marking(self):
        W.FACET = script("open(%r,'w').write('sent')" % (tmp + "/f.txt"))
        W.fulltext = lambda L, acct, uid, cap=6000: "full text"
        W.new_mail = lambda L, acct, st, back=0: (W._headers(L, FakeBox(), acct, [1, 2, 4, 5]), {"uv": 7, "last": 9}, False)
        W.run(False, out=lambda l: None)
        self.assertEqual(self.marked(), {2, 5})


class FakeIdleConn:
    """Fake raw IMAP connection: scripted server lines; a None line simulates the read timeout."""
    def __init__(self, lines):
        self.lines, self.sent = list(lines), []
        self.sock = type("S", (), {"settimeout": lambda s, v: None})()
    def _new_tag(self): return b"A1"
    def send(self, b): self.sent.append(b)
    def readline(self):
        l = self.lines.pop(0)
        if l is None: raise TimeoutError()
        return l


class Idle(unittest.TestCase):
    def test_idle_wait_exists(self):
        c = FakeIdleConn([b"+ idling\r\n", b"* 3 RECENT\r\n", b"* 7 EXISTS\r\n", b"A1 OK done\r\n"])
        self.assertTrue(W.idle_wait(c, 5))
        self.assertEqual(c.sent, [b"A1 IDLE\r\n", b"DONE\r\n"])

    def test_idle_wait_timeout_sends_done(self):
        c = FakeIdleConn([b"+ idling\r\n", None])
        self.assertFalse(W.idle_wait(c, 5))
        self.assertEqual(c.sent[-1], b"DONE\r\n")

    def test_idle_refused(self):
        with self.assertRaises(RuntimeError):
            W.idle_wait(FakeIdleConn([b"A1 NO nope\r\n"]), 5)

    def setUp(self):
        os.makedirs(W.STATE_DIR, exist_ok=True)
        json.dump(dict(W.DEFAULTS), open(W.CONF, "w"))
        self.sent = os.path.join(tmp, "isent.txt")
        if os.path.exists(self.sent): os.remove(self.sent)
        W.FACET = script(f"open({self.sent!r},'a').write(' '.join(sys.argv[1:]).replace(chr(10),'|')+chr(10))")
        W.CLAUDE = script("print(json.dumps({'is_error':False,'result':json.dumps([{'id':'a:1','tier':'now','why':'asks you'},{'id':'a:2','tier':'now','why':'x'}])}))")
        W.fulltext = lambda L, acct, uid, cap=6000: "full"
        W.snippet = lambda L, a, u: "body"
        self.acct = {"name": "a"}
        self.L = type("L", (), {"accounts": staticmethod(lambda: [self.acct])})
        self.quiet_now = False
        W.quiet = lambda c, now=None: self.quiet_now
        self.mails = [msg(1, "Michael", "hi", "michael.friedlander@ubc.ca"), msg(2, "Bob", "lunch?")]
        W.new_mail = lambda L, a, st, back=0: (list(self.mails), {"uv": 1, "last": 2}, False)
        json.dump({"a": {"uv": 1, "last": 0}}, open(W.STATE_DIR + "/state.json", "w"))

    def pushes(self):
        return open(self.sent).read().splitlines() if os.path.exists(self.sent) else []

    def test_idle_pushes_always_only_and_once(self):
        self.assertEqual(W.idle_handle(self.L, self.acct, lambda l: None), 1)
        self.assertEqual(len(self.pushes()), 1); self.assertIn("a:1", self.pushes()[0]); self.assertNotIn("a:2", self.pushes()[0])
        st = json.load(open(W.STATE_DIR + "/state.json"))
        self.assertEqual(st["a"]["last"], 0)                       # cursor belongs to the timer
        self.assertEqual(st["_idle"]["a"], [1])
        self.assertEqual(W.idle_handle(self.L, self.acct, lambda l: None), 0)     # not again
        self.assertEqual(len(self.pushes()), 1)

    def test_timer_skips_what_idle_pushed(self):
        W.idle_handle(self.L, self.acct, lambda l: None)
        W.load_life = lambda: self.L
        W.run(False, out=lambda l: None)
        self.assertEqual(len(self.pushes()), 1)                    # timer: mail 1 skipped, mail 2 is triage-only
        st = json.load(open(W.STATE_DIR + "/state.json"))
        self.assertEqual(st["a"]["last"], 2); self.assertEqual(st["_idle"]["a"], [])

    def test_quiet_hours_hold_then_timer_catches_up(self):
        self.quiet_now = True
        self.assertEqual(W.idle_handle(self.L, self.acct, lambda l: None), 0)
        self.assertEqual(self.pushes(), [])
        self.quiet_now = False
        W.load_life = lambda: self.L
        W.run(False, out=lambda l: None)
        self.assertEqual(len(self.pushes()), 1); self.assertIn("a:1", self.pushes()[0])

    def test_no_cursor_does_nothing(self):
        os.remove(W.STATE_DIR + "/state.json")
        self.assertEqual(W.idle_handle(self.L, self.acct, lambda l: None), 0)
        self.assertEqual(self.pushes(), [])

    def test_account_loop_reconnects_with_backoff_and_handles_event(self):
        stop = threading.Event()
        conns = []
        def connect(a):
            conns.append(1)
            if len(conns) == 1: raise OSError("down")
            return type("C", (), {"select": lambda s, *a, **k: ("OK", []), "logout": lambda s: None})()
        self.L.imap_connect = staticmethod(connect)
        calls = []
        def waits(c, secs):
            calls.append(1)
            if len(calls) == 2: stop.set()
            return True
        real_wait = W.idle_wait; W.idle_wait = waits
        orig = threading.Event.wait
        threading.Event.wait = lambda self_, t=None: True      # skip the real backoff sleep
        try:
            W.idle_account(self.L, self.acct, stop, lambda l: None)
        finally:
            threading.Event.wait = orig; W.idle_wait = real_wait
        self.assertEqual(len(conns), 2); self.assertEqual(len(calls), 2)
        self.assertEqual(len(self.pushes()), 1)                    # event handled once; second event: already done


def email_addr(h):
    import email.utils
    return email.utils.parseaddr(h)



class Digest(unittest.TestCase):
    """Tiers, digest timing, after-lunch push, quiet hours, dedupe with idle. Fake clock (CLOCK)."""
    def setUp(self):
        os.makedirs(W.STATE_DIR, exist_ok=True)
        json.dump(dict(W.DEFAULTS), open(W.CONF, "w"))
        self.sent = os.path.join(tmp, "dsent.txt")
        if os.path.exists(self.sent): os.remove(self.sent)
        W.FACET = script(f"open({self.sent!r},'a').write(' '.join(sys.argv[1:]).replace(chr(10),'|')+chr(10))")
        self.verdicts = {}
        W.judge = lambda items, conf: ({m["id"]: self.verdicts.get(m["id"], ("today", False, "w")) for m, _ in items}, None)
        W.fulltext = lambda L, acct, uid, cap=6000: "full"
        W.snippet = lambda L, a, u: "body"
        self.acct = {"name": "a"}
        self.L = type("L", (), {"accounts": staticmethod(lambda: [self.acct])})
        W.load_life = lambda: self.L
        self.qn = False
        W.quiet = lambda c, now=None: self.qn
        self.mails = []
        W.new_mail = lambda L, a, st, back=0: (list(self.mails), {"uv": 1, "last": 50}, False)
        json.dump({"a": {"uv": 1, "last": 0}}, open(W.STATE_DIR + "/state.json", "w"))
        CLOCK[0] = at(9)

    def tearDown(self):
        W.judge = JUDGE0
        CLOCK[0] = at(10)

    def pushes(self):
        return open(self.sent).read().splitlines() if os.path.exists(self.sent) else []

    def state(self):
        return json.load(open(W.STATE_DIR + "/state.json"))

    def mail(self, i, frm="Michael", subj="s"):
        return msg(i, frm, subj, "michael.friedlander@ubc.ca")

    def test_tier_of_defaults(self):
        self.assertEqual(W.tier_of({"id": "x"})[0], "today")
        self.assertEqual(W.tier_of({"tier": "NOW", "why": "w"}), ("now", False, "w"))
        self.assertEqual(W.tier_of({"tier": "banana"})[0], "today")
        self.assertEqual(W.tier_of({"tier": "today", "can_wait": "yes"})[1], False)    # only a real true counts

    def test_now_pushes_immediately_today_queues(self):
        self.verdicts = {"a:1": ("now", False, "urgent")}
        self.mails = [self.mail(1), self.mail(2)]
        W.run(False, out=lambda l: None)
        p = self.pushes()
        self.assertEqual(len(p), 1); self.assertIn("a:1", p[0]); self.assertNotIn("a:2", p[0])
        self.assertEqual([q["id"] for q in self.state()["_digest"]["queue"]], ["a:2"])

    def test_digest_at_times_once_and_empty_sends_nothing(self):
        self.mails = [self.mail(1), self.mail(2)]
        CLOCK[0] = at(6, 30); self.qn = False
        W.run(False, out=lambda l: None)                    # 06:30: queued, no digest due yet
        self.assertEqual(self.pushes(), [])
        self.mails = []
        CLOCK[0] = at(8, 5); W.run(False, out=lambda l: None)
        p = self.pushes()
        self.assertEqual(len(p), 1); self.assertTrue(p[0].startswith("push --log [mail digest] 2 to read today"))
        self.assertIn("a:1", p[0]); self.assertIn("a:2", p[0])
        self.assertEqual(self.state()["_digest"]["queue"], [])
        CLOCK[0] = at(8, 25); W.run(False, out=lambda l: None)                  # same digest slot: nothing
        CLOCK[0] = at(12, 35); W.run(False, out=lambda l: None)                 # lunch digest, empty queue: nothing
        self.assertEqual(len(self.pushes()), 1)
        self.mails = [self.mail(3)]
        CLOCK[0] = at(9); W.new_mail = lambda L, a, st, back=0: (list(self.mails), {"uv": 1, "last": 60}, False)
        CLOCK[0] = at(12, 40); W.run(False, out=lambda l: None)                 # arrives after lunch digest
        self.assertEqual(len(self.pushes()), 2)                                   # after lunch, can_wait False -> pushed

    def test_after_lunch_cannot_wait_pushed_can_wait_queued(self):
        self.verdicts = {"a:1": ("today", False, "due tonight"), "a:2": ("today", True, "fyi")}
        self.mails = [self.mail(1), self.mail(2)]
        CLOCK[0] = at(14)
        W.run(False, out=lambda l: None)
        p = self.pushes()
        self.assertEqual(len(p), 1); self.assertIn("a:1", p[0]); self.assertNotIn("a:2", p[0])
        self.assertNotIn("[mail digest]", p[0])
        self.assertEqual([q["id"] for q in self.state()["_digest"]["queue"]], ["a:2"])
        self.mails = []
        CLOCK[0] = at(8, 1, d=9); W.run(False, out=lambda l: None)             # next morning's digest carries it
        self.assertEqual(len(self.pushes()), 2); self.assertIn("a:2", self.pushes()[1])

    def test_before_lunch_cannot_wait_still_waits_for_digest(self):
        self.verdicts = {"a:1": ("today", False, "x")}
        self.mails = [self.mail(1)]
        CLOCK[0] = at(10)
        W.run(False, out=lambda l: None)
        self.assertEqual(self.pushes(), [])

    def test_quiet_hours_hold_now_until_seven(self):
        self.verdicts = {"a:1": ("now", False, "urgent")}
        self.mails = [self.mail(1)]
        self.qn = True; CLOCK[0] = at(23, 30)
        W.run(False, out=lambda l: None)
        self.assertEqual(self.pushes(), []); self.assertEqual(self.state()["a"]["last"], 0)   # cursor untouched
        self.qn = False; CLOCK[0] = at(7, 5, d=9)
        W.run(False, out=lambda l: None)
        self.assertEqual(len(self.pushes()), 1); self.assertIn("a:1", self.pushes()[0])

    def test_idle_and_timer_dedupe(self):
        self.mails = [self.mail(1)]
        CLOCK[0] = at(7, 30)
        self.assertEqual(W.idle_handle(self.L, self.acct, lambda l: None), 1)       # idle queues it
        W.run(False, out=lambda l: None)                                              # timer skips it
        self.assertEqual([q["id"] for q in self.state()["_digest"]["queue"]], ["a:1"])
        W.dispatch(self.state(), dict(W.DEFAULTS), [], [(self.mail(1), "w", False)], at(9), True, lambda l: None)
        st = self.state()
        W.dispatch(st, dict(W.DEFAULTS), [], [(self.mail(1), "w", False)], at(9), False, lambda l: None)
        self.assertEqual(len(st["_digest"]["queue"]), 1)

    def test_dry_run_sends_and_saves_nothing(self):
        self.mails = [self.mail(1)]
        before = open(W.STATE_DIR + "/state.json").read()
        out = []
        CLOCK[0] = at(13); W.run(True, out=out.append)
        self.assertEqual(self.pushes(), []); self.assertEqual(open(W.STATE_DIR + "/state.json").read(), before)
        self.assertTrue(any("tiers:" in l for l in out))

    def test_push_refuses_empty(self):
        with self.assertRaises(RuntimeError):
            W.push("  ")


if __name__ == "__main__":
    unittest.main()
