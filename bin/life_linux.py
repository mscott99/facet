"""life, Linux backend: mail over IMAP/SMTP, calendar from secret iCal URLs (+ optional Google API for adds).

Stdlib only. Config: ~/.config/life/accounts.json (override with $LIFE_CONFIG), chmod 600.
Loaded by bin/life when not on macOS; the macOS paths live in bin/life itself.
"""
import email, imaplib, json, os, re, smtplib, ssl, subprocess, sys, urllib.error, urllib.parse, urllib.request
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
            cfg = json.load(f)
    except ValueError as e:
        sys.exit(f"life: {p} is not valid JSON: {e}")
    # The server runs on UTC; "timezone" in the config names the user's own.
    if cfg.get("timezone") and os.environ.get("TZ") != cfg["timezone"]:
        import time
        os.environ["TZ"] = cfg["timezone"]
        time.tzset()
    return cfg


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


SENT_NAMES = ["Sent", "Sent Items", "Sent Messages", "Sent Mail", "sent-mail", "INBOX.Sent", "[Gmail]/Sent Mail"]
_LIST_RE = re.compile(r'\((?P<fl>[^)]*)\)\s+(?:"(?P<d>(?:[^"\\]|\\.)*)"|NIL)\s+(?P<n>"(?:[^"\\]|\\.)*"|\S+)')


def special_folders(c, acct):
    """-> {'s': sent folder name, 'a': all-mail folder name} (either may be missing), via RFC 6154 flags."""
    if "_special" in acct:
        return acct["_special"]
    out, names = {}, []
    try:
        typ, data = c.list()
    except Exception:
        typ, data = "NO", []
    for item in data or []:
        if not isinstance(item, (bytes, bytearray)):
            continue
        m = _LIST_RE.match(item.decode("utf-8", "replace"))
        if not m:
            continue
        name = m.group("n")
        if name.startswith('"'):
            name = re.sub(r"\\(.)", r"\1", name[1:-1])
        names.append(name)
        fl = m.group("fl").lower()
        if "\\sent" in fl:
            out.setdefault("s", name)
        if "\\all" in fl:
            out.setdefault("a", name)
    if acct.get("sent_folder"):
        out["s"] = acct["sent_folder"]
    elif "s" not in out:
        out["s"] = next((n for n in SENT_NAMES if n in names), None)
        if out["s"] is None:
            del out["s"]
    if "a" not in out and acct["gmail"]:
        out["a"] = "[Gmail]/All Mail"
    acct["_special"] = out
    return out


def select_kind(c, acct, kind):
    """kind: '' inbox, 's' sent, 'a' all mail. -> True if selected."""
    name = "INBOX" if kind == "" else special_folders(c, acct).get(kind)
    if not name:
        return False
    typ, _ = c.select('"' + name.replace("\\", "\\\\").replace('"', '\\"') + '"', readonly=True)
    return typ == "OK"


META_FIELDS = "FROM TO CC SUBJECT DATE MESSAGE-ID IN-REPLY-TO REFERENCES"


def _addrs(v):
    from email.utils import getaddresses
    return [(_dec(n), a.lower()) for n, a in getaddresses([_dec(str(v or ""))]) if a]


def _mid(v):
    m = re.search(r"<([^>]+)>", str(v or ""))
    return m.group(1).strip().lower() if m else None


def fetch_meta(c, uids):
    """-> {uid: dict(dt, frm=(name,addr), to=[(name,addr)], subj, seen, mid, irt, refs)}"""
    out = {}
    if not uids:
        return out
    typ, data = c.uid("fetch", b",".join(uids), f"(FLAGS BODY.PEEK[HEADER.FIELDS ({META_FIELDS})])")
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
        fr = (_addrs(msg["From"]) or [("?", "?")])[0]
        irt = re.findall(r"<([^>]+)>", str(msg["In-Reply-To"] or ""))
        refs = [r.lower() for r in re.findall(r"<([^>]+)>", str(msg["References"] or ""))]
        out[int(m.group(1))] = dict(
            dt=dt, frm=fr, to=_addrs(msg["To"]) + _addrs(msg["Cc"]), subj=_dec(str(msg["Subject"] or "")),
            seen="\\Seen" in meta, mid=_mid(msg["Message-ID"]),
            irt=[i.lower() for i in irt], refs=refs)
    return out


def my_addrs(accts):
    s = set()
    for x in accts:
        s.add(x["user"].lower())
        if "@" not in x["user"] and x.get("imap_host", "").count(".") >= 2:   # bare login -> user@domain of the IMAP host
            s.add(x["user"].lower() + "@" + x["imap_host"].split(".", 1)[1].lower())
        s.update(y.lower() for y in x.get("aliases", []))
    return s


def build_criteria(acct, a, everywhere_gmail=False):
    crit = []
    if getattr(a, "unread", False):
        crit.append(b"UNSEEN")
    if getattr(a, "sender", None):
        crit += [b"FROM", _q(a.sender)]
    if getattr(a, "since", None):
        d = datetime.now() - timedelta(days=int(a.since))
        crit += [b"SINCE", d.strftime("%d-%b-%Y").encode()]
    q = getattr(a, "query", None) or ""
    w = getattr(a, "with_", None)
    if acct["gmail"]:
        raw = (f"({q})" if q else "") + (f" (from:{w} OR to:{w} OR cc:{w})" if w else "")
        if raw.strip():
            crit += [b"X-GM-RAW", _q(raw.strip())]
    else:
        if q:
            crit += [b"OR", b"SUBJECT", _q(q), b"OR", b"FROM", _q(q), b"TEXT", _q(q)]
        if w:
            crit += [b"OR", b"FROM", _q(w), b"OR", b"TO", _q(w), b"CC", _q(w)]
    return crit or [b"ALL"]


def _dec(v):
    try:
        return str(make_header(decode_header(v or "")))
    except Exception:
        return v or ""


def fetch_headers(c, uids):
    """-> {uid: (datetime|None, from, subject, seen)}"""
    return {u: (m["dt"], m["frm"][0] or m["frm"][1], m["subj"], m["seen"]) for u, m in fetch_meta(c, uids).items()}


def mail_rows(a, accts=None):
    """Rows (dt, id, who, subject, seen). Searches inbox + sent (+ All Mail with --everywhere on Gmail);
    deduped by Message-ID within an account; `who` is the sender, or 'me→recipient' for sent mail."""
    rows = []
    allaccts = accts or accounts()
    mine = my_addrs(allaccts)
    for acct in allaccts:
        c = imap_connect(acct)
        seen_mid = set()
        try:
            kinds = [""]
            if getattr(a, "sent", False):
                kinds.append("s")
            if a.everywhere and acct["gmail"]:
                kinds.append("a")
            for kind in kinds:
                if not select_kind(c, acct, kind):
                    continue
                typ, data = c.uid("search", *([b"CHARSET", b"UTF-8"] + build_criteria(acct, a)))
                uids = (data[0].split() if data and data[0] else [])[-a.n:]
                for uid, mt in fetch_meta(c, uids).items():
                    if mt["mid"]:
                        if mt["mid"] in seen_mid:
                            continue
                        seen_mid.add(mt["mid"])
                    who = mt["frm"][0] or mt["frm"][1]
                    if mt["frm"][1] in mine:
                        t = mt["to"][0] if mt["to"] else ("?", "?")
                        who = "me→" + (t[0] or t[1])
                    rows.append((mt["dt"], f"{acct['name']}:{uid}" + (f":{kind}" if kind else ""), who, mt["subj"], mt["seen"]))
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
    if len(parts) < 2 or not parts[1].isdigit() or (len(parts) > 2 and parts[2] not in ("a", "s")):
        sys.exit(f"life: bad mail id {rid!r}; expected <account>:<uid>[:s|:a] as printed by `life mail inbox|search`")
    for acct in accts:
        if acct["name"] == parts[0]:
            return acct, parts[1], (parts[2] if len(parts) > 2 else "")
    sys.exit(f"life: no account named {parts[0]!r} in {config_path()}")


def fetch_raw(c, acct, kind, uid):
    if not select_kind(c, acct, kind):
        return None
    typ, data = c.uid("fetch", str(uid).encode(), "(BODY.PEEK[])")
    return next((i[1] for i in data if isinstance(i, tuple)), None)


def cmd_mail_show(a):
    acct, uid, kind = parse_id(a.id, accounts())
    c = imap_connect(acct)
    try:
        raw = fetch_raw(c, acct, kind, uid)
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


_QUOTE_CUT = re.compile(
    r"(?mi)^(?:On\b[^\n]{0,300}(?:\n[^\n]{0,200})?\bwrote:[ \t]*$|-{2,}\s*Original Message\s*-{2,}|From:[^\n]*\n(?:Sent|Date):[^\n]*$)")


def strip_quotes(t):
    """Drop 'On ... wrote:' tails, Outlook headers and '>' quoted lines."""
    t = t.replace("\r", "")
    m = _QUOTE_CUT.search(t)
    if m:
        t = t[:m.start()]
    t = "\n".join(l for l in t.split("\n") if not l.lstrip().startswith(">"))
    return re.sub(r"\n{3,}", "\n\n", t).strip()


def norm_subject(s):
    s = re.sub(r"\s+", " ", s or "").strip()
    while True:
        n = re.sub(r"(?i)^\s*((re|fwd?|aw|sv)\s*(\[\d+\])?\s*:\s*)", "", s)
        if n == s:
            return s.lower()
        s = n


def thread_messages(acct, seed_kind, seed_uid, accts):
    """-> (list of (kind, uid, meta) chronologically, seed meta). Same-account only."""
    c = imap_connect(acct)
    try:
        if not select_kind(c, acct, seed_kind):
            sys.exit(f"life: cannot open folder for {acct['name']}:{seed_uid}:{seed_kind}")
        seed = fetch_meta(c, [str(seed_uid).encode()]).get(int(seed_uid))
        if seed is None:
            sys.exit(f"life: no message {acct['name']}:{seed_uid}")
        ns = norm_subject(seed["subj"])
        cand, seen_mid = [], set()
        kinds = ["", "s"] + (["a"] if acct["gmail"] else [])
        for kind in kinds:
            if not select_kind(c, acct, kind):
                continue
            uids = set()
            if ns:
                typ, d = c.uid("search", b"CHARSET", b"UTF-8", b"SUBJECT", _q(ns))
                uids.update(d[0].split() if d and d[0] else [])
            for m in [seed["mid"]] + seed["irt"] + seed["refs"][-1:]:
                if m:
                    mb = b"<" + m.encode() + b">"
                    typ, d = c.uid("search", b"OR", b"HEADER", b"MESSAGE-ID", mb, b"OR", b"HEADER", b"REFERENCES", mb,
                                   b"HEADER", b"IN-REPLY-TO", mb)
                    uids.update(d[0].split() if d and d[0] else [])
            uids = sorted(uids, key=int)[-300:]
            for uid, mt in fetch_meta(c, uids).items():
                if mt["mid"]:
                    if mt["mid"] in seen_mid:
                        continue
                    seen_mid.add(mt["mid"])
                cand.append((kind, uid, mt))
    finally:
        try:
            c.logout()
        except Exception:
            pass
    mine = my_addrs(accts)
    def parts(mt):
        return ({mt["frm"][1]} | {a for _, a in mt["to"]}) - mine
    par = list(range(len(cand)))
    def find(i):
        while par[i] != i:
            par[i] = par[par[i]]; i = par[i]
        return i
    for i, (_, _, x) in enumerate(cand):
        for j in range(i):
            y = cand[j][2]
            xi = ({x["mid"]} | set(x["irt"]) | set(x["refs"])) - {None}
            yi = ({y["mid"]} | set(y["irt"]) | set(y["refs"])) - {None}
            linked = (x["mid"] and x["mid"] in yi - {y["mid"]} | set()) or (y["mid"] and y["mid"] in xi - {x["mid"]})
            if not linked and xi & yi - {None}:
                # share a reference ancestor (siblings) -> same thread
                linked = bool((set(x["irt"]) | set(x["refs"])) & (set(y["irt"]) | set(y["refs"])))
            if not linked and norm_subject(x["subj"]) == norm_subject(y["subj"]) and ns and parts(x) & parts(y):
                linked = True
            if linked:
                par[find(i)] = find(j)
    si = next((i for i, (k, u, _) in enumerate(cand) if k == seed_kind and u == int(seed_uid)), None)
    if si is None:   # seed was deduped against another copy
        si = next((i for i, (_, _, m) in enumerate(cand) if m["mid"] == seed["mid"]), 0)
    comp = [cand[i] for i in range(len(cand)) if find(i) == find(si)]
    comp.sort(key=lambda r: r[2]["dt"] or datetime.min.replace(tzinfo=timezone.utc))
    return comp, seed


def cmd_mail_thread(a):
    accts = accounts()
    acct, uid, kind = parse_id(a.id, accts)
    comp, seed = thread_messages(acct, kind, uid, accts)
    mine = my_addrs(accts)
    def nm(p):
        return "me" if p[1] in mine else (p[0] or p[1])
    print(f"== {seed['subj'] or '(no subject)'} [{len(comp)} msgs, {acct['name']}]")
    c = imap_connect(acct)
    try:
        for k, u, mt in comp:
            raw = fetch_raw(c, acct, k, u)
            body = ""
            if raw:
                body = body_text(email.message_from_bytes(raw, policy=policy.default))
                if not a.full:
                    body = strip_quotes(body)
                    if a.chars and len(body) > a.chars:
                        body = body[:a.chars] + f"\n[... {len(body) - a.chars} more chars; mail show {acct['name']}:{u}{':' + k if k else ''}]"
            when = mt["dt"].strftime("%Y-%m-%d %H:%M") if mt["dt"] else "?"
            to = ", ".join(nm(p) for p in mt["to"][:4]) + (f" +{len(mt['to']) - 4}" if len(mt["to"]) > 4 else "")
            print(f"-- {acct['name']}:{u}{':' + k if k else ''} {when} {nm(mt['frm'])} -> {to}")
            print(body or "(empty)")
    finally:
        try:
            c.logout()
        except Exception:
            pass


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


SA_MINT = """import sys
from google.oauth2 import service_account
from google.auth.transport.requests import Request
c = service_account.Credentials.from_service_account_file(sys.argv[1], scopes=["https://www.googleapis.com/auth/calendar"])
c.refresh(Request())
print(c.token)
"""


def google_auth(cfg):
    """(access token, default calendar id) from a service account or an OAuth block, else None."""
    g = cfg.get("google") or {}
    if g.get("service_account_file"):
        key = os.path.join(os.path.dirname(config_path()), os.path.expanduser(g["service_account_file"]))
        py = os.path.expanduser(g.get("python", sys.executable))
        r = subprocess.run([py, "-I", "-c", SA_MINT, key], capture_output=True, text=True, timeout=60)
        if r.returncode:
            sys.exit("life: service account token failed: " + (r.stderr.strip().splitlines() or ["?"])[-1])
        return r.stdout.strip(), g.get("calendar_id") or "primary"
    o = cfg.get("oauth")
    if o and all(o.get(k) for k in ("client_id", "client_secret", "refresh_token")):
        return access_token(o), o.get("calendar_id") or "primary"
    return None


def google_get(token, url):
    req = urllib.request.Request(url, headers={"Authorization": "Bearer " + token})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.load(r)


def cmd_cal_add(a):
    cfg = load_config()
    auth = google_auth(cfg)
    if not auth:
        sys.exit(f"life: adding events on Linux needs Google access in {config_path()}, either\n"
                 '  "google": {"service_account_file": "...json", "python": "<venv with google-auth>", "calendar_id": "you@gmail.com"}\n'
                 '  or "oauth": {"client_id": "...", "client_secret": "...", "refresh_token": "...", "calendar_id": "primary"}\n'
                 "See README 'life on Linux'. (Reading the calendar needs only the iCal URL.)")
    token, cal = auth
    cal = a.calendar or cal
    req = urllib.request.Request(
        f"https://www.googleapis.com/calendar/v3/calendars/{urllib.parse.quote(cal)}/events",
        data=json.dumps(event_body(a)).encode(),
        headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=30) as r:
        res = json.load(r)
    print(f"added: {a.title} {a.start} ({res.get('htmlLink', '')})")


def cmd_cal_list(a):
    cfg = load_config()
    for c in cfg.get("calendars") or []:
        print(c.get("name", "?") + "  (iCal, read)")
    auth = google_auth(cfg)
    if auth:
        for it in google_get(auth[0], "https://www.googleapis.com/calendar/v3/users/me/calendarList").get("items", []):
            print(f"{it.get('summary')}  (id: {it.get('id')}, {it.get('accessRole')})")


# ------------------------------------------------------------ secrets
def _shred(path):
    try:
        if subprocess.run(["shred", "-u", path], capture_output=True).returncode == 0:
            return
    except OSError:
        pass
    n = os.path.getsize(path)
    with open(path, "r+b") as f:
        f.write(b"\0" * n)
        f.flush()
        os.fsync(f.fileno())
    os.unlink(path)


def clean_secret(raw, nospace=False):
    v = raw.strip()
    if len(v) >= 2 and v[0] == v[-1] and v[0] in "\"'":
        v = v[1:-1].strip()
    return re.sub(r"\s+", "", v) if nospace else v


def check_login(cfg, kind, name, value):
    """OK/FAIL line for an IMAP login with the new secret; never shows the secret."""
    accts = cfg.get("accounts", [])
    if kind == "gmail":
        pick = [x for x in accts if x.get("password_file") == name] or \
               [x for x in accts if x.get("user", "").lower().endswith(("@gmail.com", "@googlemail.com"))]
    else:
        pick = [x for x in accts if x.get("name", x.get("user")) == kind]
    if not pick:
        return f"FAIL no matching account ({kind})"
    a = dict(pick[0])
    a["password"] = value
    gm = a["user"].lower().endswith(("@gmail.com", "@googlemail.com"))
    a.setdefault("imap_host", "imap.gmail.com" if gm else None)
    a.setdefault("imap_port", 993)
    try:
        c = imap_connect(a)
        try:
            c.logout()
        except Exception:
            pass
        return "OK"
    except Exception as e:
        return "FAIL " + type(e).__name__


def cmd_secret(a):
    if not re.fullmatch(r"[A-Za-z0-9_.-]+", a.name) or a.name.startswith("."):
        sys.exit("life: secret NAME must be a plain file name")
    env = os.path.expanduser("~/.env")
    src = os.path.expanduser(a.src) if a.src else None
    if src is None and os.path.exists(env):
        src = env
    if src:
        try:
            raw = open(src).read()
        except OSError as e:
            sys.exit(f"life: cannot read {src}: {e.strerror}")
    elif sys.stdin.isatty():
        import getpass
        raw = getpass.getpass(f"{a.name}: ")
    else:
        sys.exit("life: no ~/.env, no --from FILE and no terminal for a hidden prompt")
    value = clean_secret(raw, a.nospace)
    if not value:
        sys.exit("life: secret is empty; nothing written")
    if a.check and a.check != "gmail" and not a.check.startswith("imap:"):
        sys.exit("life: --check takes gmail or imap:ACCOUNT")
    d = os.path.dirname(config_path())
    os.makedirs(d, mode=0o700, exist_ok=True)
    dest = os.path.join(d, a.name)
    old = os.umask(0o077)
    try:
        fd = os.open(dest, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w") as f:
            f.write(value)
    finally:
        os.umask(old)
    os.chmod(dest, 0o600)
    print(f"saved {a.name} (len {len(value)})")
    if src and os.path.realpath(src) == os.path.realpath(env):
        _shred(src)
        print("shredded ~/.env")
    if a.check:
        kind = a.check[5:] if a.check.startswith("imap:") else "gmail"
        print("check " + a.check + ": " + check_login(load_config(), kind, a.name, value))


# ------------------------------------------------------- calendar delete
def google_delete(token, url):
    req = urllib.request.Request(url, method="DELETE", headers={"Authorization": "Bearer " + token})
    with urllib.request.urlopen(req, timeout=30):
        pass


def cmd_cal_delete(a):
    if not a.match.strip():
        sys.exit("life: --match must not be empty")
    cfg = load_config()
    auth = google_auth(cfg)
    if not auth:
        sys.exit(f"life: deleting events needs Google access in {config_path()} ('google' block); see README")
    token, cal = auth
    cal = a.calendar or cal
    lo = datetime.fromisoformat(a.start).astimezone() if a.start else datetime.now().astimezone().replace(hour=0, minute=0, second=0, microsecond=0)
    hi = lo + timedelta(days=a.days)
    base = f"https://www.googleapis.com/calendar/v3/calendars/{urllib.parse.quote(cal)}/events"
    q = urllib.parse.urlencode({"timeMin": lo.isoformat(), "timeMax": hi.isoformat(), "singleEvents": "true",
                                "orderBy": "startTime", "maxResults": 250, "q": a.match})
    hits = [e for e in google_get(token, base + "?" + q).get("items", [])
            if a.match.lower() in (e.get("summary") or "").lower()]
    if not hits:
        print("(no matching events)")
        return
    for e in hits:
        st = e.get("start", {})
        when = st.get("dateTime") or st.get("date") or "?"
        if a.yes:
            google_delete(token, base + "/" + urllib.parse.quote(e["id"], safe=""))
        print(("deleted: " if a.yes else "would delete: ") + f"{when}  {e.get('summary') or '(untitled)'}")
    if not a.yes:
        print(f"{len(hits)} match(es); dry run, add --yes to delete")


# ---------------------------------------------------------------- zotero (read-only)
ZOT = "https://api.zotero.org"
ZOT_NOTE = re.compile(r"<[^>]+>")


def zot_cache_path():
    return os.environ.get("LIFE_ZOTERO_CACHE") or os.path.expanduser("~/.cache/life/zotero.json")


def zot_creds():
    d = os.path.dirname(config_path())
    return _secret(os.path.join(d, "zotero_api_key")), _secret(os.path.join(d, "zotero_user_id"))


def zot_get(path, params=None, key=None):
    """GET one page from the Zotero API; the key goes in a header only. Returns (json, headers)."""
    q = ("?" + urllib.parse.urlencode(params)) if params else ""
    req = urllib.request.Request(ZOT + path + q, headers={"Zotero-API-Key": key, "Zotero-API-Version": "3"})
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return json.loads(r.read().decode("utf-8")), r.headers
    except urllib.error.HTTPError as e:
        sys.exit(f"life: zotero {path} -> HTTP {e.code}")


def zot_sync(force=False):
    """Bring the local copy of the library (top-level items, with BibTeX) up to date.
    Uses `since=<last version>`; items deleted remotely are dropped via /deleted."""
    key, uid = zot_creds()
    p = zot_cache_path()
    cache = {"version": 0, "items": {}}
    if not force and os.path.exists(p):
        try:
            cache = json.load(open(p))
        except ValueError:
            pass
    base = f"/users/{uid}"
    _, h = zot_get(base + "/items/top", {"limit": 1, "format": "versions"}, key)
    remote = int(h.get("Last-Modified-Version") or 0)
    if cache["version"] and remote == cache["version"]:
        return cache
    start = 0
    while True:
        params = {"limit": 100, "start": start, "include": "data,bibtex"}
        if cache["version"]:
            params["since"] = cache["version"]
        rows, h = zot_get(base + "/items/top", params, key)
        for r in rows:
            cache["items"][r["key"]] = {"data": r["data"], "bibtex": (r.get("bibtex") or "").strip(),
                                        "meta": r.get("meta", {})}
        start += len(rows)
        if not rows or start >= int(h.get("Total-Results") or 0):
            break
    if cache["version"]:
        gone, _ = zot_get(base + "/deleted", {"since": cache["version"]}, key)
        for k in gone.get("items", []):
            cache["items"].pop(k, None)
    cache["version"] = remote
    os.makedirs(os.path.dirname(p), exist_ok=True)
    tmp = p + ".tmp"
    with open(tmp, "w") as f:
        json.dump(cache, f)
    os.chmod(tmp, 0o600)
    os.replace(tmp, p)
    return cache


def zot_bibkey(d):
    """Better BibTeX key of an item: the `citationKey` field, else `Citation Key: x` in extra."""
    if d.get("citationKey"):
        return d["citationKey"]
    m = re.search(r"^\s*Citation Key:\s*(\S+)", d.get("extra", ""), re.M | re.I)
    return m.group(1) if m else ""


def zot_authors(d):
    out = []
    for c in d.get("creators", []):
        if c.get("creatorType") in ("author", "editor", None, ""):
            out.append(c.get("lastName") or c.get("name") or "")
    return [x for x in out if x]


def zot_year(d):
    m = re.search(r"(1[5-9]|20)\d\d", d.get("date", ""))
    return m.group(0) if m else ""


def zot_line(k, it):
    d = it["data"]
    au = zot_authors(d)
    first = (au[0] + (" et al." if len(au) > 1 else "")) if au else "-"
    return f"{zot_bibkey(d) or '-'}  {first}  {zot_year(d) or '-'}  {d.get('title', '')}  [{k}]"


def zot_match_item(items, ident):
    """Item by Zotero key or bibkey (exact, then case-insensitive)."""
    if ident in items:
        return ident
    for k, it in items.items():
        if zot_bibkey(it["data"]) == ident:
            return k
    for k, it in items.items():
        if zot_bibkey(it["data"]).lower() == ident.lower():
            return k
    return None


def zot_search_items(items, query="", author=None, title=None, tag=None, year=None):
    """Free-text search over the cache (title, creators, tags, abstract, venue, bibkey, DOI), AND of the terms."""
    out = []
    for k, it in items.items():
        d = it["data"]
        if d.get("itemType") in ("attachment", "note"):
            continue
        hay = " ".join([d.get("title", ""), " ".join(zot_authors(d)), d.get("abstractNote", ""),
                        d.get("publicationTitle", ""), zot_bibkey(d), d.get("DOI", ""),
                        " ".join(t.get("tag", "") for t in d.get("tags", []))]).lower()
        if query and not all(w in hay for w in query.lower().split()):
            continue
        if author and author.lower() not in " ".join(
                (c.get("lastName", "") + " " + c.get("firstName", "") + " " + c.get("name", "")) for c in d.get("creators", [])).lower():
            continue
        if title and title.lower() not in d.get("title", "").lower():
            continue
        if tag and tag.lower() not in [t.get("tag", "").lower() for t in d.get("tags", [])]:
            continue
        if year and zot_year(d) != str(year):
            continue
        out.append((k, it))
    out.sort(key=lambda kv: (zot_year(kv[1]["data"]), kv[0]), reverse=True)
    return out


def cmd_zot_search(a):
    cache = zot_sync(force=getattr(a, "refresh", False))
    rows = zot_search_items(cache["items"], " ".join(a.query or []), a.author, a.title, a.tag, a.year)
    for k, it in rows[:a.n]:
        print(zot_line(k, it))
    if len(rows) > a.n:
        print(f"... {len(rows) - a.n} more (-n)")
    if not rows:
        print("(no matches)")


def zot_children(key_id, ident):
    key, uid = zot_creds()
    rows, _ = zot_get(f"/users/{uid}/items/{ident}/children", {"limit": 100}, key)
    return rows


def zot_note_text(html):
    t = re.sub(r"</(p|div|li|h\d)>|<br\s*/?>", "\n", html)
    t = ZOT_NOTE.sub("", t)
    return re.sub(r"\n{3,}", "\n\n", t.replace("&nbsp;", " ").replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">")).strip()


def cmd_zot_show(a):
    cache = zot_sync()
    k = zot_match_item(cache["items"], a.id)
    if not k:
        sys.exit(f"life: no Zotero item with key or bibkey {a.id!r}")
    it = cache["items"][k]
    d = it["data"]
    if a.bibtex:
        print(zot_bibtex(it))
        return
    print(f"{d.get('title', '')}")
    print(f"  key: {k}   bibkey: {zot_bibkey(d) or '-'}   type: {d.get('itemType')}")
    print(f"  authors: {'; '.join((c.get('lastName', '') + ', ' + c.get('firstName', '')).strip(', ') or c.get('name', '') for c in d.get('creators', [])) or '-'}")
    for f, lab in (("date", "date"), ("publicationTitle", "venue"), ("DOI", "doi"), ("url", "url"),
                   ("volume", "volume"), ("issue", "issue"), ("pages", "pages")):
        if d.get(f):
            print(f"  {lab}: {d[f]}")
    if d.get("tags"):
        print("  tags: " + ", ".join(t.get("tag", "") for t in d["tags"]))
    if d.get("abstractNote"):
        print("  abstract: " + d["abstractNote"])
    if it["meta"].get("numChildren"):
        for c in zot_children(None, k):
            cd = c["data"]
            if cd.get("itemType") == "note":
                print(f"  note [{cd['key']}]: " + zot_note_text(cd.get("note", ""))[:a.chars].replace("\n", "\n    "))
            elif cd.get("itemType") == "attachment":
                print(f"  attachment [{cd['key']}]: {cd.get('title') or cd.get('filename') or ''} ({cd.get('contentType', '?')})"
                      + (f" {cd['url']}" if cd.get("url") else ""))


def zot_bibtex(it):
    """The API's BibTeX for the item, with the BBT key swapped in when known."""
    bt = it.get("bibtex", "")
    key = zot_bibkey(it["data"])
    if key:
        bt = re.sub(r"^(@\w+\{)[^,]*,", lambda m: m.group(1) + key + ",", bt, count=1)
    return bt


# --- bibkey <-> library matching
def norm(s):
    s = re.sub(r"[{}\\$]", "", s or "").lower()
    s = re.sub(r"\b(the|a|an|of|on|in|for|and)\b", " ", s)
    return re.sub(r"[^a-z0-9]+", "", s)


def parse_bib(path):
    """Minimal .bib reader: [{key, title, year, doi}]. Handles nested braces/quotes in simple field values."""
    text = open(path, encoding="utf-8", errors="replace").read()
    out = []
    for m in re.finditer(r"^@(\w+)\s*\{\s*([^,\s]+)\s*,", text, re.M):
        if m.group(1).lower() in ("comment", "string", "preamble"):
            continue
        i, depth = m.end(), 1
        while i < len(text) and depth:
            depth += {"{": 1, "}": -1}.get(text[i], 0)
            i += 1
        body = text[m.end():i]
        def field(n):
            fm = re.search(r"^\s*" + n + r"\s*=\s*", body, re.M | re.I)
            if not fm:
                return ""
            j = fm.end()
            if body[j] == "{":
                dp, k2 = 1, j + 1
                while k2 < len(body) and dp:
                    dp += {"{": 1, "}": -1}.get(body[k2], 0)
                    k2 += 1
                return body[j + 1:k2 - 1]
            if body[j] == '"':
                return body[j + 1:body.index('"', j + 1)]
            return re.match(r"[^,\n]*", body[j:]).group(0).strip()
        out.append({"key": m.group(2), "title": field("title"), "year": field("year"), "doi": field("doi").lower()})
    return out


def link_keys(vault):
    """Citekeys that have a literature note: References/**/@key.md."""
    keys = {}
    for root, _, files in os.walk(os.path.join(vault, "References")):
        for f in files:
            if f.startswith("@") and f.endswith(".md"):
                keys[f[1:-3]] = os.path.join(root, f)
    return keys


def note_meta(path):
    """title/year/doi from a LinkFile's frontmatter (best effort)."""
    try:
        head = open(path, encoding="utf-8", errors="replace").read(2000)
    except OSError:
        return {}
    fm = re.match(r"---\n(.*?)\n---", head, re.S)
    out = {}
    for k in ("title", "year", "DOI", "doi"):
        m = re.search(r"^" + k + r':\s*"?(.*?)"?\s*$', fm.group(1) if fm else "", re.M)
        if m:
            out[k.lower()] = m.group(1)
    return out


def zot_vault(cfg):
    return os.path.expanduser(cfg.get("zotero", {}).get("vault", "~/Obsidian/myVault"))


def zot_index(items):
    by_key, by_doi, by_ty = {}, {}, {}
    for k, it in items.items():
        d = it["data"]
        if d.get("itemType") in ("attachment", "note"):
            continue
        if zot_bibkey(d):
            by_key.setdefault(zot_bibkey(d), k)
        if d.get("DOI"):
            by_doi.setdefault(d["DOI"].lower(), k)
        by_ty.setdefault((norm(d.get("title")), zot_year(d)), k)
    return by_key, by_doi, by_ty


def zot_match(items, entries):
    """entries: [{key,title,year,doi}] -> ({entry key: item key}, [unmatched entry keys], how)."""
    by_key, by_doi, by_ty = zot_index(items)
    got, miss, how = {}, [], {}
    for e in entries:
        k, w = by_key.get(e["key"]), "key"
        if not k and e.get("doi"):
            k, w = by_doi.get(e["doi"].lower()), "doi"
        if not k and e.get("title"):
            k, w = by_ty.get((norm(e["title"]), e.get("year", ""))), "title+year"
        if k:
            got[e["key"]], how[e["key"]] = k, w
        else:
            miss.append(e["key"])
    return got, miss, how


def cmd_zot_match(a):
    cfg = load_config()
    cache = zot_sync()
    items = cache["items"]
    vault = zot_vault(cfg)
    bib = a.bib or os.path.join(vault, "mybib.bib")
    entries = parse_bib(bib)
    lk = link_keys(vault)
    seen = {e["key"] for e in entries}
    link_entries = [dict(key=k, **{f: v for f, v in note_meta(p).items() if f in ("title", "year", "doi")})
                    for k, p in lk.items() if k not in seen]
    got, miss, how = zot_match(items, entries)
    lgot, lmiss, lhow = zot_match(items, link_entries)
    n_lib = sum(1 for it in items.values() if it["data"].get("itemType") not in ("attachment", "note"))
    print(f"library: {n_lib} items (v{cache['version']}); {bib}: {len(entries)} entries; "
          f"{len(lk)} @citekey notes under References/")
    print(f"bib entries matched: {len(got)}/{len(entries)}  (by key {sum(1 for v in how.values() if v == 'key')}, "
          f"doi {sum(1 for v in how.values() if v == 'doi')}, title+year {sum(1 for v in how.values() if v == 'title+year')})")
    for k in miss:
        print(f"  bib, no library item: {k}")
    for k, v in how.items():
        if v != "key":
            print(f"  bib {k} -> {got[k]} (by {v}; library bibkey {zot_bibkey(items[got[k]]['data']) or '-'})")
    print(f"@citekey notes (not in bib) matched: {len(lgot)}/{len(link_entries)}")
    for k in lmiss:
        print(f"  note, no library item: {k}")
    used = set(got.values()) | set(lgot.values())
    unused = [(k, it) for k, it in items.items() if k not in used and it["data"].get("itemType") not in ("attachment", "note")]
    print(f"library items with no bib entry or @citekey note: {len(unused)}")
    for k, it in unused[:a.n]:
        print("  " + zot_line(k, it))
    if len(unused) > a.n:
        print(f"  ... {len(unused) - a.n} more (-n)")


def cmd_zot_sync(a):
    c = zot_sync(force=a.force)
    print(f"zotero cache: {len(c['items'])} top-level items, library version {c['version']} ({zot_cache_path()})")


# ---------------------------------------------------------------- zotero (add / find)
import html as _html
import difflib
import uuid
from html.parser import HTMLParser

ARXIV_RE = re.compile(r"(?:arxiv\.org/(?:abs|pdf)/|arxiv:\s*)?((?:\d{4}\.\d{4,5}|[a-z\-]+(?:\.[A-Z]{2})?/\d{7})(?:v\d+)?)(?:\.pdf)?$", re.I)
DOI_RE = re.compile(r"10\.\d{4,9}/[^\s\"<>]+", re.I)
SKIPWORDS = set("""a ab aboard about above across after against al along amid among an and anti around as at before behind below beneath beside besides between beyond but by d da das de del dell dello dei degli della delle dem den der des despite die do down du during ein eine einem einen einer eines el en et except for from gli i il in inside into is l la las le les like lo los near nor of off on onto or over per plus round save since so some sur than the through to toward towards un una unas under underneath une unlike uno unos until up upon versus via von while with within without yet zu zum""".split())  # Better BibTeX's title skip list ("using" is not on it)


def http_get(url, headers=None, timeout=30):
    req = urllib.request.Request(url, headers={"User-Agent": "life-zot/1.0 (mailto:m.valckescott@gmail.com)", **(headers or {})})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.read().decode(r.headers.get_content_charset() or "utf-8", "replace")
    except urllib.error.HTTPError as e:
        sys.exit(f"life: {url} -> HTTP {e.code}")
    except urllib.error.URLError as e:
        sys.exit(f"life: {url}: {e.reason}")


def arxiv_id_of(s):
    """Bare arXiv id (version stripped) from an id or arxiv.org url, else None."""
    s = s.strip()
    m = re.search(r"arxiv\.org/(?:abs|pdf)/([^?#\s]+?)(?:\.pdf)?(?:v\d+)?(?:[?#].*)?$", s, re.I)
    t = m.group(1) if m else re.sub(r"^arxiv:\s*", "", s, flags=re.I)
    t = re.sub(r"v\d+$", "", t)
    return t if re.fullmatch(r"\d{4}\.\d{4,5}|[a-z\-]+(\.[A-Za-z]{2})?/\d{7}", t) else None


def doi_of(s):
    m = DOI_RE.search(s.strip())
    return m.group(0).rstrip(".,;)") if m else None


def split_name(n):
    n = " ".join(n.split())
    if "," in n:
        last, first = [x.strip() for x in n.split(",", 1)]
    else:
        parts = n.split(" ")
        last, first = parts[-1], " ".join(parts[:-1])
    return {"creatorType": "author", "firstName": first, "lastName": last}


def parse_arxiv(xml_text, aid):
    import xml.etree.ElementTree as ET
    ns = {"a": "http://www.w3.org/2005/Atom", "x": "http://arxiv.org/schemas/atom"}
    e = ET.fromstring(xml_text).find("a:entry", ns)
    if e is None or e.find("a:title", ns) is None:
        raise ValueError("arXiv: no entry for " + aid)
    g = lambda p: " ".join((e.findtext(p, "", ns) or "").split())
    if g("a:title").lower() == "error":
        raise ValueError("arXiv error: " + g("a:summary"))
    it = {"itemType": "preprint", "title": g("a:title"),
          "creators": [split_name(" ".join((a.findtext("a:name", "", ns) or "").split())) for a in e.findall("a:author", ns)],
          "abstractNote": g("a:summary"), "date": g("a:published")[:10], "repository": "arXiv",
          "archiveID": "arXiv:" + aid, "url": "https://arxiv.org/abs/" + aid}
    cat = e.find("x:primary_category", ns)
    if cat is not None:
        it["extra"] = "arXiv:%s [%s]" % (aid, cat.get("term"))
    d = e.findtext("x:doi", "", ns)
    if d:
        it["DOI"] = d.strip()
    return it


def resolve_arxiv(aid):
    return parse_arxiv(http_get("https://export.arxiv.org/api/query?id_list=" + urllib.parse.quote(aid)), aid)


CROSSREF_TYPES = {"journal-article": "journalArticle", "proceedings-article": "conferencePaper", "book": "book",
                  "monograph": "book", "edited-book": "book", "book-chapter": "bookSection", "posted-content": "preprint",
                  "dissertation": "thesis", "report": "report"}


def parse_crossref(js, doi):
    m = js["message"] if "message" in js else js
    ty = CROSSREF_TYPES.get(m.get("type"), "journalArticle")
    clean = lambda s: " ".join(_html.unescape(re.sub(r"<[^>]+>", "", s or "")).split())
    first = lambda k: clean((m.get(k) or [""])[0])
    dp = (m.get("issued") or m.get("published") or m.get("published-print") or {}).get("date-parts", [[]])[0]
    date = "-".join("%02d" % x if i else str(x) for i, x in enumerate(dp)) if dp and dp[0] else ""
    cre = []
    for a in m.get("author", []):
        if a.get("family"):
            cre.append({"creatorType": "author", "firstName": a.get("given", ""), "lastName": a["family"]})
        elif a.get("name"):
            cre.append({"creatorType": "author", "name": a["name"]})
    for a in m.get("editor", []) if ty in ("book", "bookSection") else []:
        if a.get("family"):
            cre.append({"creatorType": "editor", "firstName": a.get("given", ""), "lastName": a["family"]})
    it = {"itemType": ty, "title": first("title"), "creators": cre, "date": date, "DOI": m.get("DOI", doi),
          "url": m.get("URL", "https://doi.org/" + doi), "abstractNote": clean(m.get("abstract"))}
    venue = first("container-title")
    if ty == "journalArticle":
        it.update(publicationTitle=venue, volume=m.get("volume", ""), issue=m.get("issue", ""), pages=m.get("page", ""),
                  ISSN=(m.get("ISSN") or [""])[0])
    elif ty == "conferencePaper":
        it.update(proceedingsTitle=venue, volume=m.get("volume", ""), pages=m.get("page", ""),
                  publisher=m.get("publisher", ""))
    elif ty == "bookSection":
        it.update(bookTitle=venue, publisher=m.get("publisher", ""), pages=m.get("page", ""))
    elif ty == "book":
        it.update(publisher=m.get("publisher", ""), ISBN=(m.get("ISBN") or [""])[0])
    elif ty == "preprint":
        it.update(repository=m.get("institution", [{}])[0].get("name", "") if m.get("institution") else m.get("publisher", ""))
    return it


def resolve_doi(doi):
    js = json.loads(http_get("https://api.crossref.org/works/" + urllib.parse.quote(doi)))
    return parse_crossref(js, doi)


class MetaScraper(HTMLParser):
    def __init__(self):
        super().__init__()
        self.meta = []
        self.title = ""
        self._t = False

    def handle_starttag(self, tag, attrs):
        a = dict(attrs)
        if tag == "meta":
            k = (a.get("name") or a.get("property") or "").strip().lower()
            if k and a.get("content") is not None:
                self.meta.append((k, a["content"].strip()))
        elif tag == "title":
            self._t = True

    def handle_endtag(self, tag):
        if tag == "title":
            self._t = False

    def handle_data(self, data):
        if self._t:
            self.title += data


def parse_html(text, url):
    """Item from page meta tags (citation_*, Dublin Core, og:), else webPage. May carry hints _doi/_arxiv."""
    p = MetaScraper()
    p.feed(text)
    allv = lambda *ks: [v for k, v in p.meta if k in ks and v]
    one = lambda *ks: (allv(*ks) or [""])[0]
    hint = {}
    d = doi_of(one("citation_doi", "dc.identifier", "dc.identifier.doi", "prism.doi") or "")
    if d:
        hint["_doi"] = d
    ax = one("citation_arxiv_id")
    if ax and arxiv_id_of(ax):
        hint["_arxiv"] = arxiv_id_of(ax)
    title = one("citation_title", "dc.title", "og:title") or " ".join(p.title.split())
    authors = allv("citation_author", "dc.creator")
    date = one("citation_publication_date", "citation_date", "citation_online_date", "dc.date", "article:published_time")
    date = re.sub(r"/", "-", date)[:10]
    today = datetime.now(timezone.utc).strftime("%Y-%m-%d")
    journal = one("citation_journal_title")
    if journal:
        it = {"itemType": "journalArticle", "publicationTitle": journal, "volume": one("citation_volume"),
              "issue": one("citation_issue"), "pages": "-".join(x for x in (one("citation_firstpage"), one("citation_lastpage")) if x)}
    else:
        it = {"itemType": "webPage", "websiteTitle": one("og:site_name", "citation_publisher")}
    it.update(title=title, creators=[split_name(a) for a in authors], date=date, url=one("citation_abstract_html_url") or url,
              abstractNote=one("citation_abstract", "dc.description", "og:description", "description"))
    if d and journal:
        it["DOI"] = d
    if it["itemType"] == "webPage":
        it["accessDate"] = today
    return it, hint


def resolve_url(url):
    ax = arxiv_id_of(url)
    if ax:
        return resolve_arxiv(ax)
    d = doi_of(url)
    if d and re.search(r"doi\.org", url):
        return resolve_doi(d)
    it, hint = parse_html(http_get(url), url)
    if hint.get("_arxiv"):
        return resolve_arxiv(hint["_arxiv"])
    if hint.get("_doi"):
        try:
            return resolve_doi(hint["_doi"])
        except Exception:
            pass
    return it


TSERVER = os.environ.get("LIFE_TRANSLATION_URL", "http://127.0.0.1:1969")
NOTES = []  # remarks from the last resolve (which path, multiple-choice lists), printed by zot add


def ts_post(path, body, ctype, timeout=60):
    """POST to the local translation-server. Returns (status, text); raises OSError if unreachable."""
    req = urllib.request.Request(TSERVER + path, data=body.encode(), method="POST",
                                 headers={"Content-Type": ctype, "User-Agent": "life-zot/1.0"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace")


def ts_item(it):
    """Translation-server item -> Zotero item the Web API accepts (drop its bookkeeping fields)."""
    it = {k: v for k, v in it.items() if k not in ("key", "version", "attachments", "notes", "seeAlso", "id")}
    it["tags"] = []
    return it


def resolve_ts(s):
    """Resolve an identifier (DOI, arXiv id, ISBN, PMID) via /search or a URL via /web on the local
    translation-server. Returns an item, or None when the server is down or finds nothing (caller falls back)."""
    try:
        if re.match(r"https?://", s) and not re.search(r"doi\.org/10\.|arxiv\.org/(abs|pdf)/", s):
            sess = uuid.uuid4().hex
            code, txt = ts_post("/web", json.dumps({"url": s, "session": sess}), "application/json")
            if code == 300:
                ch = json.loads(txt).get("items") or {}
                titles = [(k, (v.get("title") if isinstance(v, dict) else str(v))) for k, v in ch.items()]
                NOTES.append("translation-server offered %d choices; picked the first:" % len(titles))
                NOTES.extend("  %s%s" % ("* " if i == 0 else "  ", t) for i, (k, t) in enumerate(titles[:10]))
                if not titles:
                    return None
                code, txt = ts_post("/web", json.dumps({"url": s, "session": sess, "items": {titles[0][0]: ch[titles[0][0]]}}),
                                    "application/json")
        else:
            ident = s.strip()
            if re.match(r"https?://", ident):
                ident = ("arXiv:" + arxiv_id_of(ident)) if arxiv_id_of(ident) else (doi_of(ident) or ident)
            code, txt = ts_post("/search", ident, "text/plain")
        if code != 200:
            NOTES.append("translation-server: HTTP %s; using Crossref/arXiv/meta" % code)
            return None
        items = json.loads(txt)
        if not items:
            return None
        NOTES.append("source: translation-server")
        return ts_item(items[0])
    except (OSError, ValueError, KeyError) as e:
        NOTES.append("translation-server unavailable or failed (%s); using Crossref/arXiv/meta" % (e,))
        return None


def resolve_any(s, ts=True):
    del NOTES[:]
    if ts:
        it = resolve_ts(s)
        if it:
            return it
    return resolve_old(s)


def resolve_old(s):
    ax = arxiv_id_of(s)
    if ax:
        return resolve_arxiv(ax)
    if re.match(r"https?://", s):
        return resolve_url(s)
    d = doi_of(s)
    if d:
        return resolve_doi(d)
    raise ValueError("not an arXiv id, DOI or URL: " + s)


def zot_citekey(item):
    """Better BibTeX-style key as used in mybib.bib: first author's lowercased last name + first three
    non-skipword title words in CamelCase (hyphenated compounds join, tail lowercased) + year."""
    cs = [c for c in item.get("creators", []) if c.get("creatorType") in (None, "author")] or item.get("creators", [])
    last = (cs[0].get("lastName") or cs[0].get("name") or "") if cs else ""
    last = re.sub(r"[^a-z0-9]", "", unicodedata_ascii(last).lower())
    words = []
    for w in re.split(r"[\s:,;.!?()\[\]{}\"']+", re.sub(r"[${}\\]", "", item.get("title", ""))):
        if not w or w.lower() in SKIPWORDS:
            continue
        parts = [x for x in re.split(r"-+", unicodedata_ascii(w)) if x]
        parts = [re.sub(r"[^A-Za-z0-9]", "", x) for x in parts]
        parts = [x for x in parts if x]
        if parts:
            words.append(parts[0].capitalize() + "".join(x.lower() for x in parts[1:]))
    m = re.search(r"(1[5-9]|20)\d\d", item.get("date", ""))
    return last + "".join(words[:3]) + (m.group(0) if m else "")


def unicodedata_ascii(s):
    import unicodedata
    return unicodedata.normalize("NFKD", s).encode("ascii", "ignore").decode()


def bib_keys(path):
    try:
        return set(re.findall(r"^@\w+\{([^,\s]+),", open(path, errors="replace").read(), re.M))
    except OSError:
        return set()


def norm_title(t):
    return re.sub(r"[^a-z0-9]+", " ", t.lower()).strip()


def zot_idents(d):
    """Normalised identifiers an item carries: doi:, arxiv:, url:."""
    out = set()
    blob = " ".join(str(d.get(k, "")) for k in ("DOI", "url", "extra", "archiveID"))
    for m in DOI_RE.finditer(blob):
        out.add("doi:" + m.group(0).rstrip(".,;)").lower())
    for m in re.finditer(r"arxiv(?:\.org/(?:abs|pdf)/|:\s*)([a-z\-]*[./]?\d{4,7}(?:\.\d{4,5})?)", blob, re.I):
        a = arxiv_id_of(m.group(1))
        if a:
            out.add("arxiv:" + a.lower())
    if d.get("url"):
        out.add("url:" + d["url"].rstrip("/").lower())
    return out


def zot_find(q, item=None, limit=25):
    """Live search of /items/top (qmode=everything). With `item`, keep only exact id matches or near-identical titles.
    Returns [(key, data, why)]."""
    key, uid = zot_creds()
    rows, _ = zot_get(f"/users/{uid}/items/top", {"q": q, "qmode": "everything", "limit": limit, "include": "data"}, key)
    if item is None:
        return [(r["key"], r["data"], "") for r in rows]
    mine = zot_idents(item)
    out = []
    for r in rows:
        d = r["data"]
        shared = {i for i in mine & zot_idents(d) if not i.startswith("url:") or True}
        if shared:
            out.append((r["key"], d, "same " + sorted(shared)[0]))
        elif item.get("title") and difflib.SequenceMatcher(None, norm_title(item["title"]), norm_title(d.get("title", ""))).ratio() >= 0.93:
            out.append((r["key"], d, "near-identical title"))
    return out


def zot_duplicates(item):
    seen = {}
    queries = [item.get("title", "")]
    queries += [i.split(":", 1)[1] for i in zot_idents(item) if i.split(":")[0] in ("doi", "arxiv")]
    for q in queries:
        if q:
            for k, d, why in zot_find(q, item):
                seen.setdefault(k, (d, why))
    return [(k, d, why) for k, (d, why) in seen.items()]


def zot_post(items, token):
    key, uid = zot_creds()
    req = urllib.request.Request(ZOT + f"/users/{uid}/items", data=json.dumps(items).encode(), method="POST",
                                 headers={"Zotero-API-Key": key, "Zotero-API-Version": "3", "Zotero-Write-Token": token,
                                          "Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return json.loads(r.read().decode())
    except urllib.error.HTTPError as e:
        sys.exit(f"life: zotero POST items -> HTTP {e.code}: {e.read().decode('utf-8', 'replace')[:300]}")


def cmd_zot_find(a):
    rows = zot_find(" ".join(a.query))
    for k, d, _ in rows:
        print(zot_line(k, {"data": d}))
    if not rows:
        print("no match")


def clean_item(it):
    return {k: v for k, v in it.items() if v not in ("", None, [])}


def cmd_zot_add(a):
    item = clean_item(resolve_any(a.what, ts=not getattr(a, "no_translation", False)))
    for n in NOTES:
        print(n)
    item["tags"] = [{"tag": t} for t in a.tag]
    if a.collection:
        item["collections"] = [a.collection]
    dups = zot_duplicates(item)
    ck = zot_citekey(item)
    vault = zot_vault(load_config())
    taken = ck in bib_keys(os.path.join(vault, "mybib.bib"))
    print(f"suggested citekey: {ck}  ({'ALREADY in mybib.bib' if taken else 'not in mybib.bib'})")
    if dups:
        for k, d, why in dups:
            print(f"already in library ({why}): {k}  {d.get('title', '')}  -- not added")
        return
    print(json.dumps(item, indent=2, ensure_ascii=False))
    if not a.yes:
        print("dry run; add --yes to create")
        return
    res = zot_post([item], uuid.uuid4().hex)
    ok = res.get("successful") or {}
    if not ok:
        sys.exit("life: zotero did not create the item: " + json.dumps(res.get("failed"))[:300])
    print("created item key: " + list(ok.values())[0]["key"])


# ---------------------------------------------------------------- zotero (files from WebDAV, read-only)
import base64
import io
ZOT_DAV = "https://app.koofr.net/dav/Koofr/zotero/"


def zot_files_dir():
    return os.environ.get("LIFE_ZOTERO_FILES") or os.path.expanduser("~/.cache/life/zotero-files")


def dav_request(name, method="GET"):
    """GET (or PROPFIND depth 1 on the base) from the Zotero WebDAV share. Only these two methods exist here."""
    assert method in ("GET", "PROPFIND")
    d = os.path.dirname(config_path())
    user, pw = _secret(os.path.join(d, "koofr_user")), _secret(os.path.join(d, "koofr_app_password"))
    tok = base64.b64encode(f"{user}:{pw}".encode()).decode()
    h = {"Authorization": "Basic " + tok}
    if method == "PROPFIND":
        h["Depth"] = "1"
    req = urllib.request.Request(ZOT_DAV + name, headers=h, method=method)
    try:
        with urllib.request.urlopen(req, timeout=120) as r:
            return r.read()
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return None
        sys.exit(f"life: webdav {name or '/'} -> HTTP {e.code}")


def dav_keys():
    """Attachment keys that have a .zip on the share."""
    body = (dav_request("", "PROPFIND") or b"").decode("utf-8", "replace")
    return sorted(set(re.findall(r"/?([A-Z0-9]{8})\.zip", body)))


def dav_hash(prop):
    m = re.search(r"<hash>([^<]*)</hash>", prop or "")
    return m.group(1) if m else ""


def zot_fetch_file(akey):
    """Local paths of the files in KEY.zip, downloading only when the cached copy's .prop hash differs.
    Returns None when the share has no zip for this key."""
    prop = (dav_request(akey + ".prop") or b"").decode("utf-8", "replace")
    dest = os.path.join(zot_files_dir(), akey)
    stamp = os.path.join(dest, ".hash")
    h = dav_hash(prop)
    if h and os.path.exists(stamp) and open(stamp).read() == h:
        files = [f for f in sorted(os.listdir(dest)) if not f.startswith(".")]
        if files:
            return [os.path.join(dest, f) for f in files]
    blob = dav_request(akey + ".zip")
    if blob is None:
        return None
    import zipfile
    os.makedirs(dest, mode=0o700, exist_ok=True)
    out = []
    with zipfile.ZipFile(io.BytesIO(blob)) as z:
        for i in z.infolist():
            name = os.path.basename(i.filename.replace("\\", "/"))
            if i.is_dir() or not name or name.startswith("."):
                continue
            p = os.path.join(dest, name)
            with open(p, "wb") as f:
                f.write(z.read(i))
            out.append(p)
    open(stamp, "w").write(h)
    return out


def zot_pdf_attachments(item_key):
    """[(attachment key, filename, contentType)] of file attachments under an item, via the API."""
    out = []
    for r in zot_children(None, item_key):
        d = r["data"]
        if d.get("itemType") == "attachment" and d.get("linkMode") in ("imported_file", "imported_url"):
            out.append((r["key"], d.get("filename") or d.get("title", ""), d.get("contentType", "")))
    return out


def cmd_zot_pdf(a):
    if a.list:
        for k in dav_keys():
            print(k)
        return
    if not a.what:
        sys.exit("life: zot pdf needs an item key, citekey or search text (or --list)")
    ident = " ".join(a.what)
    items = zot_sync()["items"]
    k = zot_match_item(items, ident)
    if k:
        atts, title = zot_pdf_attachments(k), items[k]["data"].get("title", "")
    elif len(a.what) == 1 and re.fullmatch(r"[A-Z0-9]{8}", ident):
        atts, title = [(ident, "", "application/pdf")], ident        # a bare attachment key
    else:
        rows = zot_search_items(items, ident)
        if not rows:
            sys.exit(f"life: nothing in the library matches {ident!r}")
        if len(rows) > 1:
            for kk, it in rows[:10]:
                print(zot_line(kk, it), file=sys.stderr)
            sys.exit(f"life: {len(rows)} items match {ident!r}; give a key or citekey")
        k = rows[0][0]
        atts, title = zot_pdf_attachments(k), rows[0][1]["data"].get("title", "")
    pdfs = [x for x in atts if x[2] == "application/pdf" or x[1].lower().endswith(".pdf")]
    if not pdfs:
        sys.exit(f"life: {title!r} has no PDF attachment")
    if not a.all:
        pdfs = pdfs[:1]
    missing = 0
    for akey, fname, ctype in pdfs:
        paths = zot_fetch_file(akey)
        if paths is None:
            missing += 1
            print(f"life: attachment {akey} ({fname or 'file'}) is not on the WebDAV share (only on the Mac, not synced)", file=sys.stderr)
            continue
        for p in paths:
            print(p if a.path_only else f"{p}  [{akey}]")
    if missing == len(pdfs):
        sys.exit(1)
