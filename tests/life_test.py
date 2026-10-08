#!/usr/bin/env python3
"""Tests for bin/life_linux.py: ICS recurrence/tz/exdate, mail via fake IMAP/SMTP. No network."""
import importlib.util, io, os, sys, json, tempfile, unittest, argparse, contextlib
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


if __name__ == "__main__":
    unittest.main()
