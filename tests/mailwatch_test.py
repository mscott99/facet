#!/usr/bin/env python3
"""Tests for bin/mailwatch: rules, quiet hours, per-mail check, twice-daily batch. No network."""
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
        W.CLAUDE = script("print(json.dumps({'is_error':True,'result':'no model'}))")
        with self.assertRaises(RuntimeError):
            W.triage([msg(5, "Bob", "x")], dict(W.DEFAULTS))

    # ---- twice-daily batch
    def run_pass(self, msgs, claude_body, dry=False, first=False):
        sent = os.path.join(tmp, "sent.txt")
        if os.path.exists(sent): os.remove(sent)
        W.FACET = script(f"open({sent!r},'a').write(' '.join(sys.argv[1:]).replace(chr(10),'|')+chr(10))")
        W.CLAUDE = script(claude_body)
        W.fulltext = lambda L, acct, uid, cap=6000: "full text"
        self.seen_accts = []
        def new_mail(L, acct, st, back=0):
            self.seen_accts.append(acct["name"])
            return ([] if first else msgs, {"uv": 1, "last": 99}, first)
        W.load_life = lambda: type("L", (), {"accounts": staticmethod(lambda: [{"name": "a"}, {"name": "math"}])})
        W.new_mail = new_mail
        W.snippet = lambda L, acct, uid: "body"
        W.quiet = lambda c, now=None: False
        out = []
        W.run(dry, out=out.append)
        return open(sent).read().splitlines() if os.path.exists(sent) else [], out

    # triage says notify for a:2; summary call (prompt has 'phone message') returns text
    BOTH = ("req=sys.stdin.read()\n"
            "r='- Bob: deadline -- asks slides by Fri (a:2)' if 'phone message' in req else json.dumps([{'id':'a:2','verdict':'notify','why':'person'},{'id':'a:3','verdict':'skip','why':'promo'}])\n"
            "print(json.dumps({'is_error':False,'result':r}))")

    def test_one_summary_push_per_run_and_timer_ignores_math(self):
        os.makedirs(W.STATE_DIR, exist_ok=True)
        ms = [msg(1, "Michael", "hi", "michael.friedlander@ubc.ca"), msg(2, "Bob", "deadline"), msg(3, "Shop", "sale"),
              msg(4, "Google", "Security alert")]
        sent, out = self.run_pass(ms, self.BOTH)
        self.assertEqual(self.seen_accts, ["a"])                    # math skipped
        self.assertEqual(len(sent), 1)
        self.assertTrue(sent[0].startswith("push --log [mailwatch] 2 need"))
        self.assertIn("a:2", sent[0]); self.assertIn("a:1", sent[0])   # a:1 dropped by the summary: appended as a line
        self.assertNotIn("a:3", sent[0]); self.assertNotIn("a:4", sent[0])
        self.assertEqual(json.load(open(W.STATE_DIR + "/state.json"))["a"]["last"], 99)

    def test_dry_run_sends_nothing_and_keeps_cursor(self):
        sp = W.STATE_DIR + "/state.json"; before = open(sp).read() if os.path.exists(sp) else None
        sent, out = self.run_pass([msg(2, "Bob", "deadline")], self.BOTH, dry=True)
        self.assertEqual(sent, []); self.assertTrue(any("WOULD PUSH" in l for l in out))
        self.assertEqual(open(sp).read() if os.path.exists(sp) else None, before)

    def test_first_run_notifies_nothing(self):
        sent, out = self.run_pass([msg(2, "Bob", "x")], self.BOTH, first=True)
        self.assertEqual(sent, [])

    def test_nothing_notify_worthy_pushes_nothing(self):
        sent, out = self.run_pass([msg(3, "Shop", "sale")], "print(json.dumps({'is_error':False,'result':'[{\"id\":\"a:3\",\"verdict\":\"skip\"}]'}))")
        self.assertEqual(sent, [])

    def test_triage_failure_still_tells(self):
        sent, out = self.run_pass([msg(2, "Bob", "deadline")], "sys.exit(1)")
        self.assertEqual(len(sent), 1); self.assertIn("a:2", sent[0])

    def test_summary_failure_falls_back_to_list(self):
        body = ("req=sys.stdin.read()\n"
                "if 'phone message' in req: sys.exit(1)\n"
                "print(json.dumps({'is_error':False,'result':json.dumps([{'id':'a:2','verdict':'notify','why':'person'}])}))")
        sent, out = self.run_pass([msg(2, "Bob", "deadline")], body)
        self.assertEqual(len(sent), 1); self.assertIn("Bob: deadline -- person (a:2", sent[0])


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





class PerMail(unittest.TestCase):
    """Math account, per mail: haiku check -> none / notify / act (opus spawn)."""
    def setUp(self):
        os.makedirs(W.STATE_DIR, exist_ok=True)
        json.dump(dict(W.DEFAULTS), open(W.CONF, "w"))
        self.log = os.path.join(tmp, "pm_sent.txt"); self.calls = os.path.join(tmp, "pm_calls.txt")
        for f in (self.log, self.calls):
            if os.path.exists(f): os.remove(f)
        W.FACET = script(f"open({self.log!r},'a').write(' '.join(sys.argv[1:]).replace(chr(10),'|')+chr(10))")
        self.verdict("act")
        W.fulltext = lambda L, acct, uid, cap=6000: "full"
        self.acct = {"name": "math"}
        self.L = type("L", (), {"accounts": staticmethod(lambda: [self.acct])})
        self.quiet_now = False
        W.quiet = lambda c, now=None: self.quiet_now
        self.mails = []
        W.new_mail = lambda L, a, st, back=0: ([dict(m) for m in self.mails if m["uid"] > st["last"]], {"uv": 1, "last": 9}, False)
        json.dump({"math": {"uv": 1, "last": 0}}, open(W.STATE_DIR + "/state.json", "w"))

    def verdict(self, action, fail=False):
        body = (f"open({self.calls!r},'a').write('x')\n" + ("sys.exit(1)" if fail else
                f"print(json.dumps({{'is_error':False,'result':json.dumps({{'action':{action!r},'why':'asks which slot'}})}}))"))
        W.CLAUDE = script(body)

    def m(self, i, frm="Shannon", subj="slot?", addr="s@math.ubc.ca"):
        return msg(i, frm, subj, addr, acct="math")

    def pushes(self):
        return open(self.log).read().splitlines() if os.path.exists(self.log) else []

    def ncalls(self):
        return len(open(self.calls).read()) if os.path.exists(self.calls) else 0

    def handle(self):
        return W.idle_handle(self.L, self.acct, lambda l: None)

    def test_act_spawns_opus_once(self):
        self.mails = [self.m(1)]
        self.assertEqual(self.handle(), 1)
        p = self.pushes()
        self.assertEqual(len(p), 1); self.assertTrue(p[0].startswith("spawn --model opus --desc mail: slot?"))
        self.assertEqual(self.handle(), 0)                              # cursor moved: not again
        W.spawn_act(json.load(open(W.STATE_DIR + "/state.json")), dict(self.mails[0], acct=self.acct), "w")
        self.assertEqual(len(self.pushes()), 1)                         # dedupe by id even if re-offered

    def test_notify_pushes_none_is_silent(self):
        self.mails = [self.m(1)]; self.verdict("notify"); self.handle()
        p = self.pushes(); self.assertEqual(len(p), 1); self.assertTrue(p[0].startswith("push --log [mail] Shannon: slot? -- asks which slot (math:1)"))
        self.mails.append(self.m(2, "Bob", "fyi", "b@x.org")); self.verdict("none"); self.handle()
        self.assertEqual(len(self.pushes()), 1)

    def test_mute_skips_without_model_call(self):
        self.mails = [self.m(1, "Google", "Security alert", "no-reply@accounts.google.com"),
                      dict(self.m(2, "List", "seminar"), bulk=True)]
        self.assertEqual(self.handle(), 2)
        self.assertEqual(self.ncalls(), 0); self.assertEqual(self.pushes(), [])

    def test_always_floor(self):
        self.verdict("none")
        self.mails = [self.m(1, "Michael", "hi", "michael.friedlander@ubc.ca")]; self.handle()
        p = self.pushes(); self.assertEqual(len(p), 1); self.assertIn("[mail]", p[0])

    def test_check_failure_notifies(self):
        self.verdict("act", fail=True); self.mails = [self.m(1)]; self.handle()
        p = self.pushes(); self.assertEqual(len(p), 1); self.assertIn("(check failed)", p[0])

    def test_quiet_hours_hold_then_catch_up(self):
        self.mails = [self.m(1)]; self.quiet_now = True
        self.assertEqual(self.handle(), 0); self.assertEqual(self.pushes(), []); self.assertEqual(self.ncalls(), 0)
        self.assertEqual(json.load(open(W.STATE_DIR + "/state.json"))["math"]["last"], 0)
        self.quiet_now = False
        self.assertEqual(self.handle(), 1); self.assertEqual(len(self.pushes()), 1)

    def test_first_run_records_cursor_only(self):
        os.remove(W.STATE_DIR + "/state.json")
        W.new_mail = lambda L, a, st, back=0: ([], {"uv": 1, "last": 9}, True)
        self.assertEqual(self.handle(), 0); self.assertEqual(self.pushes(), [])
        self.assertEqual(json.load(open(W.STATE_DIR + "/state.json"))["math"]["last"], 9)

    def test_push_failure_keeps_cursor_for_retry(self):
        self.verdict("notify"); self.mails = [self.m(1)]
        W.FACET = script("sys.exit(1)")
        self.assertEqual(self.handle(), 0)
        self.assertEqual(json.load(open(W.STATE_DIR + "/state.json"))["math"]["last"], 0)

    def test_account_loop_reconnects_and_handles_event(self):
        self.mails = [self.m(1)]
        stop = threading.Event(); conns = []
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
        threading.Event.wait = lambda self_, t=None: True
        try:
            W.idle_account(self.L, self.acct, stop, lambda l: None)
        finally:
            threading.Event.wait = orig; W.idle_wait = real_wait
        self.assertEqual(len(conns), 2); self.assertEqual(len(calls), 2)
        self.assertEqual(len(self.pushes()), 1)                         # handled once

    def test_idle_main_uses_only_per_mail_account(self):
        got = []
        W.load_life = lambda: type("L", (), {"accounts": staticmethod(lambda: [{"name": "gmail"}, {"name": "math"}, {"name": "scom"}])})
        real = W.idle_account; W.idle_account = lambda L, a, *x: got.append(a["name"])
        try: W.idle_main()
        finally: W.idle_account = real
        self.assertEqual(got, ["math"])


def email_addr(h):
    import email.utils
    return email.utils.parseaddr(h)


if __name__ == "__main__":
    unittest.main()
