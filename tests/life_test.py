#!/usr/bin/env python3
"""Tests for bin/life_linux.py: ICS recurrence/tz/exdate, mail via fake IMAP/SMTP. No network."""
import importlib.util, io, re, os, sys, json, tempfile, unittest, argparse, contextlib
from datetime import datetime, timedelta, timezone
from unittest import mock
from zoneinfo import ZoneInfo

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("life_linux", os.path.join(HERE, "..", "bin", "life_linux.py"))
L = importlib.util.module_from_spec(spec); spec.loader.exec_module(L)

ICS = """BEGIN:VCALENDAR
BEGIN:VEVENT
UID:a1
SUMMARY:Weekly seminar
DTSTART;TZID=America/Vancouver:20261005T140000
DTEND;TZID=America/Vancouver:20261005T150000
RRULE:FREQ=WEEKLY;BYDAY=MO,WE;COUNT=6
EXDATE;TZID=America/Vancouver:20261007T140000
LOCATION:Room 1\\, ESB
END:VEVENT
BEGIN:VEVENT
UID:a1
RECURRENCE-ID;TZID=America/Vancouver:20261012T140000
SUMMARY:Seminar (moved)
DTSTART;TZID=America/Vancouver:20261012T160000
DTEND;TZID=America/Vancouver:20261012T170000
END:VEVENT
BEGIN:VEVENT
UID:b2
SUMMARY:Daily UTC
DTSTART:20261001T090000Z
DTEND:20261001T093000Z
RRULE:FREQ=DAILY;INTERVAL=2;UNTIL=20261009T235959Z
END:VEVENT
BEGIN:VEVENT
UID:c3
SUMMARY:Birthday
DTSTART;VALUE=DATE:20261010
DTEND;VALUE=DATE:20261011
RRULE:FREQ=YEARLY
END:VEVENT
BEGIN:VEVENT
UID:d4
SUMMARY:Second Tuesday
DTSTART;TZID=America/Vancouver:20261013T100000
DTEND;TZID=America/Vancouver:20261013T110000
RRULE:FREQ=MONTHLY;BYDAY=2TU
END:VEVENT
BEGIN:VEVENT
UID:e5
SUMMARY:Long
 folded line
DTSTART:20261020T100000Z
DURATION:PT90M
END:VEVENT
END:VCALENDAR
"""
VAN = ZoneInfo("America/Vancouver")


def rows(lo, hi):
    return L.events_between(L.parse_ics(ICS, "t"), lo, hi)


class Cal(unittest.TestCase):
    def test_weekly_byday_count_exdate_override(self):
        r = [x for x in rows(datetime(2026, 10, 1, tzinfo=VAN), datetime(2026, 11, 30, tzinfo=VAN)) if "eminar" in x[2]]
        got = [(x[0].astimezone(VAN).strftime("%m-%d %H:%M"), x[2]) for x in r]
        # Mon5, (Wed7 excluded), Mon12->moved 16:00, Wed14, Mon19, Wed21; COUNT=6 counts the excluded ones
        self.assertEqual(got, [("10-05 14:00", "Weekly seminar"), ("10-12 16:00", "Seminar (moved)"),
                               ("10-14 14:00", "Weekly seminar"), ("10-19 14:00", "Weekly seminar"),
                               ("10-21 14:00", "Weekly seminar")])
        self.assertEqual(r[0][5], "Room 1, ESB")

    def test_daily_interval_until_utc(self):
        r = [x for x in rows(datetime(2026, 9, 1, tzinfo=timezone.utc), datetime(2026, 12, 1, tzinfo=timezone.utc)) if x[2] == "Daily UTC"]
        self.assertEqual([x[0].strftime("%d") for x in r], ["01", "03", "05", "07", "09"])

    def test_allday_yearly(self):
        r = [x for x in rows(datetime(2026, 10, 9, tzinfo=VAN), datetime(2028, 10, 11, tzinfo=VAN)) if x[2] == "Birthday"]
        self.assertEqual([x[0].year for x in r], [2026, 2027, 2028])
        self.assertTrue(all(x[4] for x in r))

    def test_monthly_nth_weekday(self):
        r = [x for x in rows(datetime(2026, 10, 1, tzinfo=VAN), datetime(2027, 1, 31, tzinfo=VAN)) if x[2] == "Second Tuesday"]
        self.assertEqual([x[0].astimezone(VAN).strftime("%m-%d") for x in r], ["10-13", "11-10", "12-08", "01-12"])

    def test_dst_wall_clock(self):  # 2026-11-01 ends DST in Los Angeles (BC kept UTC-7 for good, tzdata 2026c); weekly 14:00 stays 14:00 local
        ics = ("BEGIN:VEVENT\nUID:z\nSUMMARY:W\nDTSTART;TZID=America/Los_Angeles:20261019T140000\n"
               "DTEND;TZID=America/Los_Angeles:20261019T150000\nRRULE:FREQ=WEEKLY\nEND:VEVENT\n")
        LA = ZoneInfo("America/Los_Angeles")
        r = L.events_between(L.parse_ics(ics), datetime(2026, 10, 26, tzinfo=LA), datetime(2026, 11, 12, tzinfo=LA))
        self.assertEqual(len(r), 3)
        self.assertEqual({x[0].astimezone(LA).hour for x in r}, {14})
        self.assertEqual({x[0].utcoffset() for x in r}, {timedelta(hours=-7), timedelta(hours=-8)})

    def test_folded_and_duration(self):
        r = [x for x in rows(datetime(2026, 10, 20, tzinfo=timezone.utc), datetime(2026, 10, 21, tzinfo=timezone.utc)) if "Long" in x[2]]
        self.assertEqual(r[0][2], "Longfolded line")
        self.assertEqual(r[0][1] - r[0][0], timedelta(minutes=90))

    def test_format(self):
        out = L.format_cal(rows(datetime(2026, 10, 9, tzinfo=VAN), datetime(2026, 10, 11, tzinfo=VAN)))
        self.assertIn("Birthday", out); self.assertIn("all day", out)


RAW = (b"From: Ann <ann@x.org>\r\nTo: me@gmail.com\r\nSubject: =?utf-8?q?Caf=C3=A9_plans?=\r\n"
       b"Date: Tue, 06 Oct 2026 10:00:00 -0700\r\nContent-Type: text/html\r\n\r\n<html><body><p>Hello <b>there</b></p></body></html>")


class FakeIMAP:
    log = []
    def __init__(self, host, port): self.host = host
    def login(self, u, p): FakeIMAP.log.append(("login", self.host, u))
    def select(self, f, readonly=False): FakeIMAP.log.append(("select", f)); return "OK", [b"1"]
    def logout(self): pass
    def list(self): return "OK", [b'(\\HasNoChildren) "/" "INBOX"', b'(\\HasNoChildren \\Sent) "/" "sent-mail"']
    def uid(self, cmd, *a):
        FakeIMAP.log.append((cmd,) + a)
        if cmd == "search": return "OK", [b"41 42"]
        if cmd == "fetch" and b"," in a[0] or (cmd == "fetch" and "HEADER" in a[1]):
            hdr = RAW.split(b"\r\n\r\n")[0] + b"\r\n\r\n"
            return "OK", [(b"1 (UID 42 FLAGS (\\Seen) BODY[HEADER.FIELDS] {1}", hdr), b")",
                          (b"2 (UID 41 FLAGS () BODY[HEADER.FIELDS] {1}", hdr.replace(b"10:00", b"09:00")), b")"]
        return "OK", [(b"1 (UID 42 BODY[] {1}", RAW), b")"]


class FakeSMTP:
    sent = []
    def __init__(self, h, p, **k): self.h = h
    def login(self, u, p): pass
    def send_message(self, m, to_addrs=None): FakeSMTP.sent.append((self.h, m, to_addrs))
    def quit(self): pass


class Mail(unittest.TestCase):
    def setUp(self):
        d = tempfile.mkdtemp()
        self.cfg = os.path.join(d, "a.json")
        json.dump({"accounts": [{"name": "gmail", "user": "me@gmail.com", "password": "pw"},
                                {"name": "math", "user": "m@dept.edu", "password": "p", "imap_host": "imap.dept.edu", "smtp_host": "smtp.dept.edu"}]},
                  open(self.cfg, "w"))
        os.environ["LIFE_CONFIG"] = self.cfg
        FakeIMAP.log.clear(); FakeSMTP.sent.clear()

    def run_cmd(self, fn, **kw):
        buf = io.StringIO()
        with mock.patch.object(L.imaplib, "IMAP4_SSL", FakeIMAP), mock.patch.object(L.smtplib, "SMTP_SSL", FakeSMTP), contextlib.redirect_stdout(buf):
            fn(argparse.Namespace(**kw))
        return buf.getvalue()

    def test_inbox_across_accounts(self):
        out = self.run_cmd(L.cmd_mail_list, n=5, unread=False, sender=None, since=None, query=None, everywhere=False, verbose=False)
        self.assertIn("gmail:42", out); self.assertIn("math:42", out); self.assertIn("Café plans", out)
        self.assertEqual({l[1] for l in FakeIMAP.log if l[0] == "login"}, {"imap.gmail.com", "imap.dept.edu"})

    def test_search_gmail_uses_xgmraw_other_uses_or(self):
        self.run_cmd(L.cmd_mail_list, n=5, unread=True, sender=None, since="3", query="grant", everywhere=False, verbose=False)
        searches = [l for l in FakeIMAP.log if l[0] == "search"]
        self.assertIn(b"X-GM-RAW", searches[0]); self.assertIn(b"OR", searches[1]); self.assertIn(b"UNSEEN", searches[0])

    def test_show_strips_html(self):
        out = self.run_cmd(L.cmd_mail_show, id="gmail:42", full=False, chars=4000)
        self.assertIn("Subject: Café plans", out); self.assertIn("Hello there", out); self.assertNotIn("<b>", out)

    def test_show_bad_id(self):
        with self.assertRaises(SystemExit): self.run_cmd(L.cmd_mail_show, id="nope", full=False, chars=10)

    def test_send(self):
        kw = dict(to=["a@b.c"], cc=None, subject="Hi", body="yo", body_file=None, from_account="math")
        self.assertIn("draft", self.run_cmd(L.cmd_mail_send, send=False, **kw)); self.assertEqual(FakeSMTP.sent, [])
        self.assertIn("sent from math", self.run_cmd(L.cmd_mail_send, send=True, **kw))
        h, m, to = FakeSMTP.sent[0]
        self.assertEqual((h, m["From"], to), ("smtp.dept.edu", "m@dept.edu", ["a@b.c"]))

    def test_missing_config_message(self):
        os.environ["LIFE_CONFIG"] = "/nonexistent/x.json"
        with self.assertRaises(SystemExit) as e: L.load_config()
        self.assertIn("/nonexistent/x.json", str(e.exception))

    def test_cal_add_needs_oauth(self):
        with self.assertRaises(SystemExit) as e:
            L.cmd_cal_add(argparse.Namespace(title="t", start="2026-10-10 10:00", minutes=60, calendar=None, notes=None))
        self.assertIn("oauth", str(e.exception))


def _msg(frm, to, subj, date, mid, irt=None, body="hi"):
    h = f"From: {frm}\r\nTo: {to}\r\nSubject: {subj}\r\nDate: {date}\r\nMessage-ID: <{mid}>\r\n"
    if irt:
        h += f"In-Reply-To: <{irt}>\r\nReferences: <{irt}>\r\n"
    return (h + "Content-Type: text/plain\r\n\r\n" + body).encode()


QUOTED = ("Sure, 2pm.\n\nOn Wed, 7 Oct 2026 at 11:58, Ann <ann@x.org> wrote:\n> Are you on campus?\n> yes\n")
BOX = {  # folder -> {uid: raw}
    "INBOX": {7: _msg("Bob <bob@x.org>", "m@dept.edu", "Re: Plans", "Wed, 07 Oct 2026 13:05:00 -0700", "b1@x", "a1@d", QUOTED),
              9: _msg("Zed <zed@x.org>", "m@dept.edu", "Unrelated", "Wed, 07 Oct 2026 14:00:00 -0700", "z1@x")},
    "sent-mail": {3: _msg("m@dept.edu", "Bob <bob@x.org>", "Plans", "Wed, 07 Oct 2026 11:58:00 -0700", "a1@d", None, "Are you around?"),
                  5: _msg("m@dept.edu", "Bob <bob@x.org>", "Re: Plans", "Wed, 07 Oct 2026 13:38:00 -0700", "a2@d", "b1@x", "OK, 3pm.\n\n> Sure, 2pm.\n")},
}


class BoxIMAP:
    def __init__(self, host, port): self.cur = None
    def login(self, u, p): pass
    def logout(self): pass
    def list(self): return "OK", [b'(\\HasNoChildren) "/" "INBOX"', b'(\\HasNoChildren \\Sent) "/" "sent-mail"', b'(\\HasNoChildren) "/" "Sent"']
    def select(self, f, readonly=False):
        self.cur = f.strip('"'); return ("OK" if self.cur in BOX else "NO"), [b"1"]
    def uid(self, cmd, *a):
        box = BOX[self.cur]
        if cmd == "search":
            m = re.search(rb"SUBJECT \"([^\"]*)\"", b" ".join(x if isinstance(x, bytes) else x.encode() for x in a))
            ids = [u for u, r in box.items() if not m or m.group(1).lower() in re.search(rb"Subject: ([^\r]*)", r).group(1).lower()]
            return "OK", [b" ".join(str(u).encode() for u in ids)]
        uids = [int(x) for x in a[0].split(b",")] if isinstance(a[0], bytes) else [int(a[0])]
        out = []
        for u in uids:
            raw = box[u]
            if "HEADER" in a[1]:
                raw = raw.split(b"\r\n\r\n")[0] + b"\r\n\r\n"
            out += [(f"1 (UID {u} FLAGS (\\Seen) BODY[] {{1}}".encode(), raw), b")"]
        return "OK", out


class Thread(unittest.TestCase):
    setUp = Mail.setUp

    def run_cmd(self, fn, **kw):
        buf = io.StringIO()
        with mock.patch.object(L.imaplib, "IMAP4_SSL", BoxIMAP), contextlib.redirect_stdout(buf):
            fn(argparse.Namespace(**kw))
        return buf.getvalue()

    def test_thread_spans_inbox_and_sent_and_strips_quotes(self):
        out = self.run_cmd(L.cmd_mail_thread, id="math:7", full=False, chars=2500)
        self.assertIn("3 msgs", out)
        self.assertLess(out.index("Are you around?"), out.index("Sure, 2pm"))
        self.assertLess(out.index("Sure, 2pm"), out.index("OK, 3pm"))
        self.assertNotIn("Are you on campus", out); self.assertNotIn("wrote:", out)
        self.assertIn("math:3:s", out); self.assertNotIn("Unrelated", out)
        self.assertEqual(out.count("Sure, 2pm"), 1)

    def test_thread_from_sent_id_and_full(self):
        out = self.run_cmd(L.cmd_mail_thread, id="math:5:s", full=True, chars=2500)
        self.assertIn("math:7 ", out); self.assertIn("> Are you on campus?", out)

    def test_search_includes_sent_and_marks_direction(self):
        out = self.run_cmd(L.cmd_mail_list, n=10, unread=False, sender=None, since=None, query="Plans", everywhere=False,
                           verbose=False, sent=True, with_="bob@x.org")
        self.assertIn("math:3:s", out); self.assertIn("me→Bob", out); self.assertIn("math:7 ", out)
        out = self.run_cmd(L.cmd_mail_list, n=10, unread=False, sender=None, since=None, query="Plans", everywhere=False,
                           verbose=False, sent=False)
        self.assertNotIn(":s", out)

    def test_show_sent_id(self):
        self.assertIn("OK, 3pm", self.run_cmd(L.cmd_mail_show, id="math:5:s", full=False, chars=4000))

    def test_strip_quotes(self):
        self.assertEqual(L.strip_quotes("Hi\n\nOn Mon, X <a@b>\nwrote:\n> q\n"), "Hi")
        self.assertEqual(L.norm_subject("RE: Fwd: Re: Hello"), "hello")


class SecretAndDelete(unittest.TestCase):
    def setUp(self):
        self.d = tempfile.mkdtemp()
        self.cfgp = os.path.join(self.d, "cfg", "accounts.json")
        os.makedirs(os.path.dirname(self.cfgp))
        json.dump({"accounts": [{"name": "g", "user": "x@gmail.com", "password_file": "pw"}]}, open(self.cfgp, "w"))
        os.environ["LIFE_CONFIG"] = self.cfgp
        self.home = mock.patch.dict(os.environ, {"HOME": self.d})
        self.home.start()

    def tearDown(self):
        self.home.stop()

    def run_secret(self, **kw):
        ns = dict(name="pw", src=None, nospace=False, check=None); ns.update(kw)
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            L.cmd_secret(argparse.Namespace(**ns))
        return out.getvalue()

    def test_env_saved_shredded_not_printed(self):
        open(os.path.join(self.d, ".env"), "w").write('  "abcd efgh ijkl"\n')
        out = self.run_secret(nospace=True)
        dest = os.path.join(self.d, "cfg", "pw")
        self.assertEqual(open(dest).read(), "abcdefghijkl")
        self.assertEqual(oct(os.stat(dest).st_mode & 0o777), "0o600")
        self.assertFalse(os.path.exists(os.path.join(self.d, ".env")))
        self.assertIn("saved pw (len 12)", out)
        self.assertNotIn("abcd", out)

    def test_from_file_kept_and_spaces_kept(self):
        f = os.path.join(self.d, "in.txt"); open(f, "w").write("a b\n")
        self.run_secret(src=f)
        self.assertEqual(open(os.path.join(self.d, "cfg", "pw")).read(), "a b")
        self.assertTrue(os.path.exists(f))

    def test_check(self):
        class C:
            def logout(self): pass
        with mock.patch.object(L, "imap_connect", return_value=C()) as m:
            out = self.run_secret(src=None, check="gmail") if False else None
            f = os.path.join(self.d, "in.txt"); open(f, "w").write("sekret")
            out = self.run_secret(src=f, check="gmail")
            self.assertEqual(m.call_args[0][0]["password"], "sekret")
        self.assertIn("check gmail: OK", out)
        self.assertNotIn("sekret", out)
        with mock.patch.object(L, "imap_connect", side_effect=L.imaplib.IMAP4.error("bad sekret")):
            out = self.run_secret(src=f, check="imap:g")
        self.assertIn("FAIL error", out)
        self.assertNotIn("sekret", out)

    def test_delete_dry_and_yes(self):
        evs = [{"id": "e1", "summary": "Aaronson talk", "start": {"dateTime": "2026-10-08T15:30:00-07:00"}},
               {"id": "e2", "summary": "other", "start": {"dateTime": "2026-10-09T15:30:00-07:00"}}]
        ns = dict(match="aaronson", start="2026-10-08", days=3, calendar=None, yes=False)
        with mock.patch.object(L, "google_auth", return_value=("tok", "cal@x")), \
             mock.patch.object(L, "google_get", return_value={"items": evs}) as g, \
             mock.patch.object(L, "google_delete") as dl:
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                L.cmd_cal_delete(argparse.Namespace(**ns))
            self.assertIn("would delete", out.getvalue()); self.assertNotIn("other", out.getvalue())
            dl.assert_not_called()
            self.assertIn("q=aaronson", g.call_args[0][1])
            ns["yes"] = True
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                L.cmd_cal_delete(argparse.Namespace(**ns))
            self.assertIn("deleted: 2026-10-08T15:30", out.getvalue())
            dl.assert_called_once()
            self.assertTrue(dl.call_args[0][1].endswith("/e1"))


class ZoteroTest(unittest.TestCase):
    def item(self, key, title, cite="", doi="", year="2020", last="Smith", extra=""):
        d = {"key": key, "itemType": "journalArticle", "title": title, "date": year, "DOI": doi, "extra": extra,
             "creators": [{"creatorType": "author", "firstName": "A", "lastName": last}], "tags": [{"tag": "cs"}]}
        if cite:
            d["citationKey"] = cite
        return {"key": key, "data": d, "meta": {}, "bibtex": "@article{zoteroKey,\n title = {%s}\n}" % title}

    def setUp(self):
        self.d = tempfile.mkdtemp()
        os.environ["LIFE_ZOTERO_CACHE"] = os.path.join(self.d, "z.json")
        os.environ["LIFE_CONFIG"] = os.path.join(self.d, "accounts.json")
        json.dump({"zotero": {"vault": self.d}}, open(os.environ["LIFE_CONFIG"], "w"))
        open(os.path.join(self.d, "zotero_api_key"), "w").write("SECRETKEY\n")
        open(os.path.join(self.d, "zotero_user_id"), "w").write("1\n")
        self.lib = [self.item("K1", "Gaussian recovery", cite="smithGaussianRecovery2020", doi="10.1/x"),
                    self.item("K2", "A Note on Cones", extra="Citation Key: jonesNoteCones2019\nfoo", year="2019", last="Jones"),
                    self.item("K3", "Unkeyed paper", year="2018", last="Lee")]
        self.calls = []

    def fake_get(self, path, params=None, key=None):
        self.calls.append((path, dict(params or {}), key))
        self.assertNotIn("SECRETKEY", path + json.dumps(params or {}))
        h = {"Last-Modified-Version": "5", "Total-Results": str(len(self.lib))}
        if path.endswith("/deleted"):
            return {"items": ["K3"]}, h
        if params.get("format") == "versions":
            return {}, h
        s, n = params["start"], params["limit"]
        return self.lib[s:s + n], h

    def run_cmd(self, fn, **kw):
        out = io.StringIO()
        with mock.patch.object(L, "zot_get", self.fake_get), contextlib.redirect_stdout(out):
            fn(argparse.Namespace(**kw))
        return out.getvalue()

    def test_search_show_cache(self):
        out = self.run_cmd(L.cmd_zot_search, query=["gaussian"], author=None, title=None, tag=None, year=None, n=10)
        self.assertIn("smithGaussianRecovery2020  Smith  2020  Gaussian recovery  [K1]", out)
        self.assertNotIn("Unkeyed", out)
        n = len(self.calls)
        out = self.run_cmd(L.cmd_zot_search, query=[], author="jones", title=None, tag=None, year=None, n=10)
        self.assertIn("jonesNoteCones2019", out)          # BBT key read from `extra`
        self.assertEqual(len(self.calls), n + 1)            # only the version probe: cache is used
        for c in self.calls:
            self.assertEqual(c[2], "SECRETKEY")             # key travels as a parameter to the header sender only
        out = self.run_cmd(L.cmd_zot_search, query=[], author=None, title=None, tag="CS", year="2018", n=10)
        self.assertIn("Unkeyed", out)

    def test_incremental_and_deleted(self):
        self.run_cmd(L.cmd_zot_sync, force=False)
        self.lib = [self.item("K4", "New one")]
        with mock.patch.object(L, "zot_get", self.fake_get) as _:
            pass
        def bump(path, params=None, key=None):
            r, h = self.fake_get(path, params, key); h["Last-Modified-Version"] = "6"; return r, h
        with mock.patch.object(L, "zot_get", bump):
            c = L.zot_sync()
        self.assertIn("K4", c["items"]); self.assertNotIn("K3", c["items"]); self.assertEqual(c["version"], 6)
        self.assertEqual([x for x in self.calls if "since" in x[1] and "include" in x[1]][0][1]["since"], 5)

    def test_show_bibtex_keeps_bbt_key(self):
        out = self.run_cmd(L.cmd_zot_show, id="smithgaussianrecovery2020", bibtex=True, chars=100)
        self.assertTrue(out.startswith("@article{smithGaussianRecovery2020,"))
        out = self.run_cmd(L.cmd_zot_show, id="K2", bibtex=False, chars=100)
        self.assertIn("bibkey: jonesNoteCones2019", out)

    def test_match(self):
        open(os.path.join(self.d, "mybib.bib"), "w").write(
            "@article{smithGaussianRecovery2020,\n title = {Gaussian recovery},\n year = {2020}\n}\n"
            "@article{oldKey,\n title = {A note on {C}ones},\n year = {2019}\n}\n"
            "@article{viaDoi,\n title = {zzz},\n doi = {10.1/X}\n}\n"
            "@article{ghost,\n title = {Nothing}, year = {1999}\n}\n")
        os.makedirs(os.path.join(self.d, "References", "LinkFiles"))
        open(os.path.join(self.d, "References", "LinkFiles", "@leeUnkeyed.md"), "w").write(
            '---\ntitle: "Unkeyed paper"\nyear: 2018\n---\n')
        out = self.run_cmd(L.cmd_zot_match, bib=None, n=5)
        self.assertIn("bib entries matched: 3/4", out)
        self.assertIn("bib, no library item: ghost", out)
        self.assertIn("oldKey -> K2 (by title+year", out)
        self.assertIn("@citekey notes (not in bib) matched: 1/1", out)
        self.assertNotIn("SECRETKEY", out)


ARXIV_XML = """<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom" xmlns:arxiv="http://arxiv.org/schemas/atom">
<entry><id>http://arxiv.org/abs/2010.02264v2</id><title>Subspace Embeddings
 Under Nonlinear Transformations</title><summary> We study  embeddings. </summary>
<published>2020-10-05T12:00:00Z</published><author><name>Aarshvi Gajjar</name></author><author><name>Cameron Musco</name></author>
<arxiv:primary_category term="cs.LG"/><arxiv:doi>10.1000/jj</arxiv:doi></entry></feed>"""

CROSSREF = {"message": {"type": "journal-article", "title": ["On <i>Cones</i> and Hulls"], "DOI": "10.1000/abc",
            "container-title": ["J. Conv. An."], "volume": "3", "issue": "2", "page": "1-9", "URL": "http://dx.doi.org/10.1000/abc",
            "issued": {"date-parts": [[2019, 5]]}, "author": [{"given": "Jo", "family": "Jones"}]}}

PAGE = """<html><head><title>T page</title><meta name="citation_title" content="A Fine Paper">
<meta name="citation_author" content="Lee, Ann"><meta name="citation_author" content="Bob Ray">
<meta name="citation_publication_date" content="2021/03/04"><meta name="citation_journal_title" content="JJ">
<meta name="citation_doi" content="10.5000/zz"></head></html>"""


class ZoteroAddTest(ZoteroTest):
    test_search_show_cache = test_match = None
    def test_parsers(self):
        it = L.parse_arxiv(ARXIV_XML, "2010.02264")
        self.assertEqual((it["itemType"], it["title"], it["date"], it["DOI"]), ("preprint", "Subspace Embeddings Under Nonlinear Transformations", "2020-10-05", "10.1000/jj"))
        self.assertEqual(it["archiveID"], "arXiv:2010.02264")
        self.assertEqual(it["creators"][1], {"creatorType": "author", "firstName": "Cameron", "lastName": "Musco"})
        c = L.parse_crossref(CROSSREF, "10.1000/abc")
        self.assertEqual((c["itemType"], c["title"], c["date"], c["publicationTitle"]), ("journalArticle", "On Cones and Hulls", "2019-05", "J. Conv. An."))
        h, hint = L.parse_html(PAGE, "http://x/y")
        self.assertEqual((h["title"], h["date"], hint["_doi"]), ("A Fine Paper", "2021-03-04", "10.5000/zz"))
        self.assertEqual(h["creators"][1]["lastName"], "Ray")
        w, _ = L.parse_html("<html><title> Plain  page </title></html>", "http://x/")
        self.assertEqual((w["itemType"], w["title"], w["url"]), ("webPage", "Plain page", "http://x/"))

    def test_ids(self):
        self.assertEqual(L.arxiv_id_of("https://arxiv.org/abs/2010.02264v3"), "2010.02264")
        self.assertEqual(L.arxiv_id_of("arXiv:2010.02264"), "2010.02264")
        self.assertIsNone(L.arxiv_id_of("10.1000/abc"))
        self.assertEqual(L.doi_of("https://doi.org/10.1000/abc."), "10.1000/abc")

    def test_citekey(self):
        k = lambda t, y="2017", a="Bora": L.zot_citekey({"title": t, "date": y, "creators": [{"creatorType": "author", "lastName": a}]})
        self.assertEqual(k("Compressed Sensing Using Generative Models"), "boraCompressedSensingUsing2017")
        self.assertEqual(k("On Oracle-Type Local Recovery", "2021", "Adcock"), "adcockOracletypeLocalRecovery2021")
        self.assertEqual(k("Fighting the Curse of Dimensionality", "2012", "Herrmann"), "herrmannFightingCurseDimensionality2012")
        self.assertEqual(k("Stable and Robust Sampling Strategies", "2014", "Krahmer"), "krahmerStableRobustSampling2014")

    def post_run(self, what, yes, find_rows=(), **kw):
        posts = []
        def fake_get(path, params=None, key=None):
            self.assertNotIn("SECRETKEY", json.dumps(params or {}))
            return list(find_rows), {}
        with mock.patch.object(L, "zot_get", fake_get), mock.patch.object(L, "resolve_any", lambda s, **k: L.parse_arxiv(ARXIV_XML, "2010.02264")), \
             mock.patch.object(L, "zot_post", lambda items, tok: posts.append((items, tok)) or {"successful": {"0": {"key": "NEWKEY1"}}}):
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                L.cmd_zot_add(argparse.Namespace(what=what, yes=yes, tag=["t1"], collection=None))
        return out.getvalue(), posts

    def ts_run(self, responses, what):
        """resolve_any with a mocked translation-server; responses: list of (code, text) or an exception."""
        calls = []
        def fake_ts(path, body, ctype, timeout=60):
            calls.append((path, body, ctype))
            r = responses[len(calls) - 1]
            if isinstance(r, Exception):
                raise r
            return r
        with mock.patch.object(L, "ts_post", fake_ts):
            return L.resolve_any(what), calls

    def test_translation_server_search_and_web(self):
        item = {"key": "ABC", "version": 0, "itemType": "conferencePaper", "title": "T", "date": "2021-03-01",
                "creators": [], "tags": [{"tag": "x"}], "attachments": [{"x": 1}], "notes": [], "seeAlso": []}
        it, calls = self.ts_run([(200, json.dumps([item]))], "10.1000/abc")
        self.assertEqual(calls[0][0:3], ("/search", "10.1000/abc", "text/plain"))
        self.assertEqual((it["itemType"], it["tags"]), ("conferencePaper", []))
        for k in ("key", "version", "attachments", "notes", "seeAlso"):
            self.assertNotIn(k, it)
        it, calls = self.ts_run([(200, json.dumps([item]))], "https://arxiv.org/abs/2010.02264v2")
        self.assertEqual(calls[0][0:2], ("/search", "arXiv:2010.02264"))
        it, calls = self.ts_run([(200, json.dumps([item]))], "https://journal.example/paper/1")
        self.assertEqual(calls[0][0], "/web")
        self.assertEqual(json.loads(calls[0][1])["url"], "https://journal.example/paper/1")
        self.assertIn("source: translation-server", L.NOTES)

    def test_translation_server_multiple_choice(self):
        item = {"itemType": "journalArticle", "title": "First"}
        choice = (300, json.dumps({"url": "u", "session": "s", "items": {"u1": {"title": "First"}, "u2": {"title": "Second"}}}))
        it, calls = self.ts_run([choice, (200, json.dumps([item]))], "https://journal.example/toc")
        self.assertEqual(it["title"], "First")
        self.assertEqual(list(json.loads(calls[1][1])["items"]), ["u1"])
        self.assertTrue(any("Second" in n for n in L.NOTES))

    def test_translation_server_down_falls_back(self):
        for resp in ([ConnectionRefusedError("down")], [(500, "boom")], [(501, "No items returned from any translator")]):
            with mock.patch.object(L, "resolve_old", lambda s: {"itemType": "preprint", "title": "old"}):
                it, _ = self.ts_run(resp, "2010.02264")
            self.assertEqual(it["title"], "old")
        with mock.patch.object(L, "resolve_old", lambda s: {"title": "old"}), mock.patch.object(L, "ts_post", side_effect=AssertionError):
            self.assertEqual(L.resolve_any("2010.02264", ts=False)["title"], "old")

    def test_dry_run_never_posts(self):
        out, posts = self.post_run("2010.02264", False)
        self.assertEqual(posts, [])
        self.assertIn("dry run", out)
        self.assertIn("suggested citekey: gajjarSubspaceEmbeddingsNonlinear2020", out)
        self.assertIn('"tag": "t1"', out)

    def test_yes_posts_once_with_token(self):
        out, posts = self.post_run("2010.02264", True)
        self.assertEqual(len(posts), 1)
        self.assertEqual(len(posts[0][1]), 32)
        self.assertIn("created item key: NEWKEY1", out)

    def test_dedupe_blocks_post(self):
        for d in ({"title": "Other", "DOI": "10.1000/JJ"}, {"title": "x", "extra": "arXiv:2010.02264v1"},
                  {"title": "Subspace embeddings under nonlinear transformations."}):
            out, posts = self.post_run("2010.02264", True, find_rows=[{"key": "OLD1", "data": d}])
            self.assertEqual(posts, [], d)
            self.assertIn("already in library", out)
            self.assertIn("OLD1", out)


class ZoteroPdfTest(ZoteroTest):
    test_search_show_cache = test_match = None

    def setUp(self):
        super().setUp()
        os.environ["LIFE_ZOTERO_FILES"] = os.path.join(self.d, "files")
        open(os.path.join(self.d, "koofr_user"), "w").write("u@x\n")
        open(os.path.join(self.d, "koofr_app_password"), "w").write("a b c\n")
        import zipfile
        b = io.BytesIO()
        with zipfile.ZipFile(b, "w") as z:
            z.writestr("sub/paper.pdf", b"%PDF-1.4 fake")
        self.zip = b.getvalue()
        self.dav = {"ATT00001.zip": self.zip, "ATT00001.prop": b'<properties><mtime>1</mtime><hash>h1</hash></properties>',
                    "": b"<D:href>/dav/Koofr/zotero/ATT00001.zip</D:href><D:href>/dav/Koofr/zotero/ATT00001.prop</D:href>"}
        self.gets = []
        self.kids = [{"key": "ATT00001", "data": {"itemType": "attachment", "linkMode": "imported_file", "filename": "paper.pdf", "contentType": "application/pdf"}},
                     {"key": "ATT00002", "data": {"itemType": "attachment", "linkMode": "imported_file", "filename": "gone.pdf", "contentType": "application/pdf"}},
                     {"key": "NOTE0001", "data": {"itemType": "note"}}]

    def run_pdf(self, **kw):
        ns = dict(what=["K1"], all=False, path_only=False, list=False); ns.update(kw)
        out = io.StringIO()
        def dav(name, method="GET"):
            self.gets.append((name, method))
            return self.dav.get(name)
        with mock.patch.object(L, "zot_get", lambda path, params=None, key=None: self.fake_get(path, params, key) if "children" not in path else (self.kids, {})), \
                mock.patch.object(L, "dav_request", dav), contextlib.redirect_stdout(out):
            L.cmd_zot_pdf(argparse.Namespace(**ns))
        return out.getvalue()

    def test_fetch_and_cache(self):
        out = self.run_pdf(what=["smithGaussianRecovery2020"], path_only=True).strip()
        self.assertTrue(out.endswith("ATT00001/paper.pdf"))
        self.assertEqual(open(out, "rb").read()[:4], b"%PDF")
        n = len([g for g in self.gets if g[0].endswith(".zip")])
        self.run_pdf(path_only=True)
        self.assertEqual(len([g for g in self.gets if g[0].endswith(".zip")]), n)   # same hash: no re-download
        self.dav["ATT00001.prop"] = b"<hash>h2</hash>"
        self.run_pdf(path_only=True)
        self.assertEqual(len([g for g in self.gets if g[0].endswith(".zip")]), n + 1)
        self.assertTrue(all(m in ("GET", "PROPFIND") for _, m in self.gets))

    def test_all_and_missing(self):
        out = io.StringIO()
        with contextlib.redirect_stderr(out):
            res = self.run_pdf(all=True)
        self.assertIn("ATT00001", res)
        self.assertIn("ATT00002", out.getvalue())
        self.assertIn("not on the WebDAV share", out.getvalue())
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            self.dav.pop("ATT00001.zip")
            self.kids = self.kids[1:2]
            self.run_pdf()

    def test_list_and_errors(self):
        self.assertEqual(self.run_pdf(list=True, what=[]).split(), ["ATT00001"])
        with self.assertRaises(SystemExit):
            self.run_pdf(what=["nonexistentxyz"])

    def test_zip_slip(self):
        import zipfile
        b = io.BytesIO()
        with zipfile.ZipFile(b, "w") as z:
            z.writestr("../../evil.pdf", b"%PDF")
        self.dav["ATT00001.zip"] = b.getvalue()
        out = self.run_pdf(path_only=True).strip()
        self.assertTrue(out.startswith(os.path.join(self.d, "files", "ATT00001")))


if __name__ == "__main__":
    unittest.main()
