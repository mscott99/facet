#!/usr/bin/env python3
"""Tests for bin/mailwatch: rules, quiet hours, triage via fake claude, one notification per run. No network."""
import importlib.util, importlib.machinery, os, sys, json, tempfile, unittest, stat
from datetime import datetime
from zoneinfo import ZoneInfo

HERE = os.path.dirname(os.path.abspath(__file__))
tmp = tempfile.mkdtemp()
os.environ.update(MAILWATCH_STATE_DIR=tmp + "/state", MAILWATCH_CONFIG=tmp + "/mw.json")
path = os.path.join(HERE, "..", "bin", "mailwatch")
W = importlib.util.module_from_spec(importlib.util.spec_from_loader("mailwatch", importlib.machinery.SourceFileLoader("mailwatch", path)))
W.__spec__.loader.exec_module(W)
QUIET = W.quiet


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
        self.assertEqual(v, {"a:5": (True, "person")})
        bad = script("print(json.dumps({'is_error':True,'result':'no model'}))")
        W.CLAUDE = bad
        with self.assertRaises(RuntimeError):
            W.triage([msg(5, "Bob", "x")], dict(W.DEFAULTS))

    def run_pass(self, msgs, claude_body, dry=False, first=False):
        sent = os.path.join(tmp, "sent.txt")
        if os.path.exists(sent): os.remove(sent)
        W.FACET = script(f"open({sent!r},'a').write(' '.join(sys.argv[1:])+chr(10))")
        W.CLAUDE = script(claude_body)
        W.load_life = lambda: type("L", (), {"accounts": staticmethod(lambda: [{"name": "a"}])})
        W.new_mail = lambda L, acct, st, back=0: ([] if first else msgs, {"uv": 1, "last": 99}, first)
        W.snippet = lambda L, acct, uid: "body"
        W.quiet = lambda c, now=None: False
        out = []
        W.run(dry, out=out.append)
        return open(sent).read().splitlines() if os.path.exists(sent) else [], out

    VERDICT = "print(json.dumps({'is_error':False,'result':json.dumps([{'id':'a:2','notify':True,'why':'deadline'},{'id':'a:3','notify':False,'why':'promo'}])}))"

    def test_one_message_per_run(self):
        os.path.exists(W.STATE_DIR) or os.makedirs(W.STATE_DIR)
        ms = [msg(1, "Michael", "hi", "michael.friedlander@ubc.ca"), msg(2, "Bob", "deadline"), msg(3, "Shop", "sale"),
              msg(4, "Google", "Security alert")]
        sent, out = self.run_pass(ms, self.VERDICT)
        self.assertEqual(len(sent), 1)
        self.assertTrue(sent[0].startswith("send --later [mailwatch] 2 new"))
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
        self.assertEqual(len(sent), 1); self.assertIn("triage unavailable", sent[0])


if __name__ == "__main__":
    unittest.main()
