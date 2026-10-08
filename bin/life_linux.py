"""life, Linux backend: mail over IMAP/SMTP, calendar from secret iCal URLs (+ optional Google API for adds).

Stdlib only. Config: ~/.config/life/accounts.json (override with $LIFE_CONFIG), chmod 600.
Loaded by bin/life when not on macOS; the macOS paths live in bin/life itself.
"""
import email, imaplib, json, os, re, smtplib, ssl, sys, urllib.parse, urllib.request
from datetime import date, datetime, timedelta, timezone
from email import policy
from email.message import EmailMessage
from email.utils import parsedate_to_datetime, parseaddr, formatdate, make_msgid
from email.header import decode_header, make_header

try:
    from zoneinfo import ZoneInfo
except ImportError:  # pragma: no cover
    ZoneInfo = None

EXAMPLE = """{
  "accounts": [
    {"name": "gmail", "user": "you@gmail.com", "password": "<16-char app password>"},
    {"name": "math", "user": "you@dept.edu", "password": "...",
     "imap_host": "imap.dept.edu", "imap_port": 993, "smtp_host": "smtp.dept.edu", "smtp_port": 465}
  ],
  "calendars": [{"name": "gmail", "ics_url": "https://calendar.google.com/calendar/ical/.../private-.../basic.ics"}],
  "oauth": {"client_id": "...", "client_secret": "...", "refresh_token": "...", "calendar_id": "primary"}
}"""


def config_path():
    return os.environ.get("LIFE_CONFIG") or os.path.expanduser("~/.config/life/accounts.json")


def load_config():
    p = config_path()
    if not os.path.exists(p):
        sys.exit(f"life: no config at {p}\nCreate it (chmod 600). Example (see bin/life.accounts.example.json):\n{EXAMPLE}")
    try:
        with open(p) as f:
            return json.load(f)
    except ValueError as e:
        sys.exit(f"life: {p} is not valid JSON: {e}")


def _secret(path):
    """Read a secret kept in its own file, so accounts.json holds no password."""
    p = os.path.expanduser(path)
    if not os.path.isabs(p):
        p = os.path.join(os.path.dirname(config_path()), p)
    try:
        return open(p).read().strip()
    except OSError as e:
        sys.exit(f"life: cannot read secret {p}: {e}")


def accounts(cfg=None):
    cfg = cfg or load_config()
    out = []
    for a in cfg.get("accounts", []):
        a = dict(a)
        if "password_file" in a and "password" not in a:
            a["password"] = _secret(a["password_file"])
        if "user" not in a or "password" not in a:
            sys.exit(f"life: account {a.get('name', '?')} in {config_path()} needs 'user' and 'password'")
        a.setdefault("name", a["user"])
        gm = a["user"].lower().endswith(("@gmail.com", "@googlemail.com")) or "gmail" in a.get("imap_host", "")
        a["gmail"] = gm
        a.setdefault("imap_host", "imap.gmail.com" if gm else None)
        a.setdefault("smtp_host", "smtp.gmail.com" if gm else None)
        a.setdefault("imap_port", 993)
        a.setdefault("smtp_port", 465)
        if not a["imap_host"]:
            sys.exit(f"life: account {a['name']} needs 'imap_host' (and 'smtp_host') in {config_path()}")
        out.append(a)
    if not out:
        sys.exit(f"life: no accounts in {config_path()}. Example:\n{EXAMPLE}")
    return out


# ------------------------------------------------------------------ mail
def imap_connect(acct):
    c = imaplib.IMAP4_SSL(acct["imap_host"], acct["imap_port"])
    c.login(acct["user"], acct["password"])
    return c


def _q(s):
    return b'"' + s.replace("\\", "\\\\").replace('"', '\\"').encode("utf-8") + b'"'


def build_criteria(acct, a, everywhere_gmail=False):
    crit = []
    if getattr(a, "unread", False):
        crit.append(b"UNSEEN")
    if getattr(a, "sender", None):
        crit += [b"FROM", _q(a.sender)]
    if getattr(a, "since", None):
        d = datetime.now() - timedelta(days=int(a.since))
        crit += [b"SINCE", d.strftime("%d-%b-%Y").encode()]
    q = getattr(a, "query", None)
    if q:
        if acct["gmail"]:
            crit += [b"X-GM-RAW", _q(q)]
        else:
            crit += [b"OR", b"SUBJECT", _q(q), b"OR", b"FROM", _q(q), b"TEXT", _q(q)]
    return crit or [b"ALL"]


def _dec(v):
    try:
        return str(make_header(decode_header(v or "")))
    except Exception:
        return v or ""


def fetch_headers(c, uids):
    """-> {uid: (datetime|None, from, subject, seen)}"""
    out = {}
    if not uids:
        return out
    typ, data = c.uid("fetch", b",".join(uids), "(FLAGS BODY.PEEK[HEADER.FIELDS (FROM SUBJECT DATE)])")
    for item in data:
        if not isinstance(item, tuple):
            continue
        meta = item[0].decode("ascii", "replace")
        m = re.search(r"UID (\d+)", meta)
        if not m:
            continue
        msg = email.message_from_bytes(item[1], policy=policy.default)
        try:
            dt = parsedate_to_datetime(str(msg["Date"])).astimezone()
        except Exception:
            dt = None
        name, addr = parseaddr(_dec(str(msg["From"] or "")))
        out[int(m.group(1))] = (dt, name or addr or "?", _dec(str(msg["Subject"] or "")), "\\Seen" in meta)
    return out


def folder_for(acct, everywhere):
    return '"[Gmail]/All Mail"' if (everywhere and acct["gmail"]) else "INBOX"


def mail_rows(a, accts=None):
    rows = []
    for acct in (accts or accounts()):
        c = imap_connect(acct)
        try:
            folder = folder_for(acct, a.everywhere)
            c.select(folder, readonly=True)
            crit = build_criteria(acct, a)
            typ, data = c.uid("search", *( [b"CHARSET", b"UTF-8"] + crit))
            uids = data[0].split() if data and data[0] else []
            uids = uids[-a.n:]
            tag = "a" if folder != "INBOX" else ""
            for uid, (dt, who, subj, seen) in fetch_headers(c, uids).items():
                rows.append((dt, f"{acct['name']}:{uid}" + (f":{tag}" if tag else ""), who, subj, seen))
        finally:
            try:
                c.logout()
            except Exception:
                pass
    rows.sort(key=lambda r: r[0] or datetime.min.replace(tzinfo=timezone.utc), reverse=True)
    return rows[:a.n]


def cmd_mail_list(a):
    rows = mail_rows(a)
    out = []
    for dt, rid, who, subj, seen in rows:
        when = dt.strftime("%Y-%m-%d %H:%M") if dt else "?" * 16
        out.append(f"{rid:>16} {'' if seen else '*':<2} {when}  {who[:34]:<34}  {(subj or '(no subject)')[:70]}")
    print("\n".join(out) or "(nothing)")


def body_text(msg):
    part = msg.get_body(preferencelist=("plain", "html")) if msg.is_multipart() else msg
    if part is None:
        return ""
    try:
        body = part.get_content()
    except Exception:
        body = part.get_payload(decode=True).decode("utf-8", "replace")
    if isinstance(body, bytes):
        body = body.decode("utf-8", "replace")
    if part.get_content_type() == "text/html" or re.search(r"<(p|div|br|html)\b", body, re.I):
        body = re.sub(r"(?is)<(script|style).*?</\1>", "", body)
        body = re.sub(r"(?i)<br\s*/?>|</p>|</div>|</tr>|</li>", "\n", body)
        body = re.sub(r"<[^>]+>", "", body)
        import html as _h
        body = _h.unescape(body)
    return re.sub(r"\n{3,}", "\n\n", re.sub(r"[ \t]+\n", "\n", body)).strip()


def parse_id(rid, accts):
    parts = str(rid).split(":")
    if len(parts) < 2 or not parts[1].isdigit():
        sys.exit(f"life: bad mail id {rid!r}; expected <account>:<uid> as printed by `life mail inbox`")
    for acct in accts:
        if acct["name"] == parts[0]:
            return acct, parts[1], (len(parts) > 2 and parts[2] == "a")
    sys.exit(f"life: no account named {parts[0]!r} in {config_path()}")


def cmd_mail_show(a):
    acct, uid, allmail = parse_id(a.id, accounts())
    c = imap_connect(acct)
    try:
        c.select(folder_for(acct, allmail), readonly=True)
        typ, data = c.uid("fetch", uid.encode(), "(BODY.PEEK[])")
        raw = next((i[1] for i in data if isinstance(i, tuple)), None)
    finally:
        try:
            c.logout()
        except Exception:
            pass
    if raw is None:
        sys.exit(f"life: no message {a.id}")
    msg = email.message_from_bytes(raw, policy=policy.default)
    for h in ("From", "To", "Cc", "Date", "Subject"):
        if msg[h]:
            print(f"{h}: {msg[h]}")
    print("-" * 72)
    t = body_text(msg)
    print(t if a.full else t[:a.chars])
    if not a.full and len(t) > a.chars:
        print(f"\n[... {len(t) - a.chars} more chars; --full for all]")
    att = [p.get_filename() for p in msg.walk() if p.get_filename()]
    if att:
        print("\nattachments: " + ", ".join(att))


def build_message(acct, a):
    body = open(a.body_file).read() if a.body_file else (a.body or "")
    m = EmailMessage()
    m["From"] = acct.get("from") or acct["user"]
    m["To"] = ", ".join(a.to)
    if a.cc:
        m["Cc"] = ", ".join(a.cc)
    m["Subject"] = a.subject
    m["Date"] = formatdate(localtime=True)
    m["Message-ID"] = make_msgid()
    m.set_content(body)
    return m


def smtp_send(acct, m, rcpts):
    port = int(acct["smtp_port"])
    if port == 465:
        s = smtplib.SMTP_SSL(acct["smtp_host"], port, context=ssl.create_default_context())
    else:
        s = smtplib.SMTP(acct["smtp_host"], port)
        s.starttls(context=ssl.create_default_context())
    try:
        s.login(acct["user"], acct["password"])
        s.send_message(m, to_addrs=rcpts)
    finally:
        s.quit()


def cmd_mail_send(a):
    accts = accounts()
    name = getattr(a, "from_account", None)
    acct = next((x for x in accts if x["name"] == name), None) if name else accts[0]
    if acct is None:
        sys.exit(f"life: --from {name!r}: no such account; have {', '.join(x['name'] for x in accts)}")
    m = build_message(acct, a)
    if not a.send:
        print(f"[draft, not sent — add --send]\n{m.as_string()[:2000]}")
        return
    smtp_send(acct, m, list(a.to) + list(a.cc or []))
    if not acct["gmail"] and acct.get("sent_folder"):      # Gmail files its own copy; others only if asked
        c = imap_connect(acct)
        try:
            c.append(acct["sent_folder"], "\\Seen", imaplib.Time2Internaldate(datetime.now().timestamp()), m.as_bytes())
        finally:
            c.logout()
    print(f"sent from {acct['name']}")


# ------------------------------------------------------------------ calendar
WD = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"]


def local_tz():
    return datetime.now().astimezone().tzinfo


def unfold(text):
    return re.sub(r"\r?\n[ \t]", "", text).splitlines()


def split_prop(line):
    head, _, val = line.partition(":")
    # a ':' inside a quoted param is rare; handle quotes minimally
    if head.count('"') % 2:
        i = line.index('"', line.index('"') + 1)
        j = line.index(":", i)
        head, val = line[:j], line[j + 1:]
    bits = head.split(";")
    params = {}
    for p in bits[1:]:
        k, _, v = p.partition("=")
        params[k.upper()] = v.strip('"')
    return bits[0].upper(), params, val


def unescape(v):
    return v.replace("\\n", "\n").replace("\\N", "\n").replace("\\,", ",").replace("\\;", ";").replace("\\\\", "\\")


def parse_dt(params, val):
    """-> (datetime aware, allday)"""
    val = val.strip()
    if params.get("VALUE") == "DATE" or (len(val) == 8 and val.isdigit()):
        d = datetime.strptime(val[:8], "%Y%m%d")
        return d.replace(tzinfo=local_tz()), True
    if val.endswith("Z"):
        return datetime.strptime(val, "%Y%m%dT%H%M%SZ").replace(tzinfo=timezone.utc), False
    d = datetime.strptime(val, "%Y%m%dT%H%M%S")
    tz = None
    if params.get("TZID") and ZoneInfo:
        try:
            tz = ZoneInfo(params["TZID"])
        except Exception:
            tz = None
    return d.replace(tzinfo=tz or local_tz()), False


def parse_dur(v):
    m = re.fullmatch(r"([+-])?P(?:(\d+)W)?(?:(\d+)D)?(?:T(?:(\d+)H)?(?:(\d+)M)?(?:(\d+)S)?)?", v.strip())
    if not m:
        return timedelta(0)
    s, w, d, h, mi, se = m.groups()
    td = timedelta(weeks=int(w or 0), days=int(d or 0), hours=int(h or 0), minutes=int(mi or 0), seconds=int(se or 0))
    return -td if s == "-" else td


def parse_ics(text, cal=""):
    evs, cur = [], None
    for line in unfold(text):
        if line == "BEGIN:VEVENT":
            cur = {"cal": cal, "ex": [], "props": {}}
        elif line == "END:VEVENT" and cur is not None:
            p = cur["props"]
            if "DTSTART" in p:
                st, allday = parse_dt(*p["DTSTART"])
                if "DTEND" in p:
                    en = parse_dt(*p["DTEND"])[0]
                elif "DURATION" in p:
                    en = st + parse_dur(p["DURATION"][1])
                else:
                    en = st + (timedelta(days=1) if allday else timedelta(0))
                rr = None
                if "RRULE" in p:
                    rr = dict(kv.split("=", 1) for kv in p["RRULE"][1].split(";") if "=" in kv)
                ex = set()
                for params, v in cur["ex"]:
                    for one in v.split(","):
                        if one.strip():
                            ex.add(ex_key(*parse_dt(params, one)))
                evs.append({"summary": unescape(p.get("SUMMARY", ({}, ""))[1]), "start": st, "end": en,
                            "allday": allday, "where": unescape(p.get("LOCATION", ({}, ""))[1]),
                            "rrule": rr, "ex": ex, "uid": p.get("UID", ({}, ""))[1], "cal": cal,
                            "recid": ex_key(*parse_dt(*p["RECURRENCE-ID"])) if "RECURRENCE-ID" in p else None,
                            "cancelled": p.get("STATUS", ({}, ""))[1].strip().upper() == "CANCELLED"})
            cur = None
        elif cur is not None and ":" in line:
            k, params, v = split_prop(line)
            if k == "EXDATE":
                cur["ex"].append((params, v))
            elif k not in cur["props"]:
                cur["props"][k] = (params, v)
    return evs


def ex_key(dt, allday):
    return ("d", dt.date()) if allday else ("t", dt.astimezone(timezone.utc))


def _add_months(d, n):
    m = d.month - 1 + n
    return m // 12 + d.year, m % 12 + 1


def _nth_weekday(year, month, wd, n):
    import calendar
    days = [d for d in range(1, calendar.monthrange(year, month)[1] + 1) if date(year, month, d).weekday() == wd]
    try:
        return days[n - 1] if n > 0 else days[n]
    except IndexError:
        return None


def occurrences(ev, hi):
    """Start datetimes (aware, event tz) of the event up to `hi`, honouring RRULE basics."""
    st = ev["start"]
    rr = ev["rrule"]
    if not rr:
        yield st
        return
    freq = rr.get("FREQ", "")
    interval = int(rr.get("INTERVAL", 1))
    count = int(rr["COUNT"]) if "COUNT" in rr else None
    until = None
    if "UNTIL" in rr:
        u, ad = parse_dt({}, rr["UNTIL"])
        until = u + timedelta(days=1) - timedelta(seconds=1) if ad else u
    byday = []
    for tok in rr.get("BYDAY", "").split(","):
        m = re.fullmatch(r"([+-]?\d+)?(MO|TU|WE|TH|FR|SA|SU)", tok.strip())
        if m:
            byday.append((int(m.group(1)) if m.group(1) else 0, WD.index(m.group(2))))
    bymd = [int(x) for x in rr.get("BYMONTHDAY", "").split(",") if x]
    wall = st.replace(tzinfo=None)
    tz = st.tzinfo
    n = 0
    k = 0
    while k < 5000:
        cands = []
        if freq == "DAILY":
            cands = [wall + timedelta(days=k * interval)]
        elif freq == "WEEKLY":
            monday = wall - timedelta(days=wall.weekday()) + timedelta(weeks=k * interval)
            days = sorted(w for _, w in byday) or [wall.weekday()]
            cands = [monday + timedelta(days=w) for w in days]
        elif freq == "MONTHLY":
            y, mo = _add_months(wall, k * interval)
            ds = []
            if byday:
                for nth, w in byday:
                    if nth:
                        d = _nth_weekday(y, mo, w, nth)
                        if d:
                            ds.append(d)
                    else:
                        ds += [d for d in range(1, 32) if _valid(y, mo, d) and date(y, mo, d).weekday() == w]
            else:
                ds = [d for d in (bymd or [wall.day]) if _valid(y, mo, d)]
            cands = [wall.replace(year=y, month=mo, day=d) for d in sorted(set(ds))]
        elif freq == "YEARLY":
            y = wall.year + k * interval
            if _valid(y, wall.month, wall.day):
                cands = [wall.replace(year=y)]
        else:
            yield st
            return
        k += 1
        for c in cands:
            if c < wall:
                continue
            aware = c.replace(tzinfo=tz)
            if until and aware > until:
                return
            n += 1
            if count and n > count:
                return
            if aware > hi:
                return
            yield aware
        if cands and cands[0].replace(tzinfo=tz) > hi:
            return


def _valid(y, m, d):
    try:
        date(y, m, d)
        return True
    except ValueError:
        return False


def events_between(evs, lo, hi):
    """Expanded (start, end, summary, cal, allday, where) overlapping [lo, hi], sorted."""
    overridden = {(e["uid"], e["recid"]) for e in evs if e["recid"] is not None}
    out = []
    for ev in evs:
        if ev["cancelled"]:
            continue
        dur = ev["end"] - ev["start"]
        starts = [ev["start"]] if ev["recid"] is not None else occurrences(ev, hi)
        for s in starts:
            k = ex_key(s, ev["allday"])
            if k in ev["ex"] or (ev["recid"] is None and (ev["uid"], k) in overridden):
                continue
            e = s + dur
            if e >= lo and s <= hi:
                out.append((s, e, ev["summary"], ev["cal"], ev["allday"], ev["where"]))
    return sorted(set(out), key=lambda r: (r[0], r[2]))


def fetch_ics(url):
    req = urllib.request.Request(url, headers={"User-Agent": "life/1"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return r.read().decode("utf-8", "replace")


def load_events(cfg=None):
    cfg = cfg or load_config()
    cals = cfg.get("calendars") or []
    if not cals:
        sys.exit(f"life: no 'calendars' in {config_path()}. Add the secret iCal URL, e.g.\n"
                 '  "calendars": [{"name": "gmail", "ics_url": "https://calendar.google.com/calendar/ical/.../basic.ics"}]')
    evs = []
    for c in cals:
        evs += parse_ics(fetch_ics(c["ics_url"] if "ics_url" in c else _secret(c["ics_url_file"])), c.get("name", ""))
    return evs


def format_cal(rows):
    lines, day = [], None
    for s, e, summ, cal, allday, where in rows:
        s, e = s.astimezone(), e.astimezone()
        if s.strftime("%F") != day:
            day = s.strftime("%F")
            lines.append(f"\n{s.strftime('%a %d %b')}")
        clock = "all day" if allday else s.strftime("%H:%M") + e.strftime("-%H:%M")
        lines.append(f"  {clock:<12} {(summ or '(untitled)')[:56]:<56} [{(cal or '?')[:20]}]" + (f" @ {where[:28]}" if where else ""))
    return "\n".join(lines).strip() or "(no events)"


def cmd_cal(a, now=None):
    now = now or datetime.now().astimezone()
    rows = events_between(load_events(), now - timedelta(days=a.past), now + timedelta(days=a.days))
    print(format_cal(rows))


def access_token(o):
    data = urllib.parse.urlencode({"client_id": o["client_id"], "client_secret": o["client_secret"],
                                   "refresh_token": o["refresh_token"], "grant_type": "refresh_token"}).encode()
    with urllib.request.urlopen(urllib.request.Request("https://oauth2.googleapis.com/token", data=data), timeout=30) as r:
        return json.load(r)["access_token"]


def event_body(a):
    st = datetime.fromisoformat(a.start).astimezone()
    en = st + timedelta(minutes=a.minutes)
    body = {"summary": a.title,
            "start": {"dateTime": st.isoformat()},
            "end": {"dateTime": en.isoformat()}}
    if a.notes:
        body["description"] = a.notes
    return body


def cmd_cal_add(a):
    cfg = load_config()
    o = cfg.get("oauth")
    if not o or not all(o.get(k) for k in ("client_id", "client_secret", "refresh_token")):
        sys.exit(f"life: adding events on Linux needs a Google OAuth block in {config_path()}:\n"
                 '  "oauth": {"client_id": "...", "client_secret": "...", "refresh_token": "...", "calendar_id": "primary"}\n'
                 "See README 'life on Linux' for how to get these. (Reading the calendar needs only the iCal URL.)")
    cal = a.calendar or o.get("calendar_id") or "primary"
    req = urllib.request.Request(
        f"https://www.googleapis.com/calendar/v3/calendars/{urllib.parse.quote(cal)}/events",
        data=json.dumps(event_body(a)).encode(),
        headers={"Authorization": "Bearer " + access_token(o), "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=30) as r:
        res = json.load(r)
    print(f"added: {a.title} {a.start} ({res.get('htmlLink', '')})")


def cmd_cal_list(a):
    cfg = load_config()
    for c in cfg.get("calendars") or []:
        print(c.get("name", c.get("ics_url", "?")[:40]))
    o = cfg.get("oauth")
    if o and all(o.get(k) for k in ("client_id", "client_secret", "refresh_token")):
        req = urllib.request.Request("https://www.googleapis.com/calendar/v3/users/me/calendarList",
                                     headers={"Authorization": "Bearer " + access_token(o)})
        with urllib.request.urlopen(req, timeout=30) as r:
            for it in json.load(r).get("items", []):
                print(f"{it.get('summary')}  (id: {it.get('id')})")
