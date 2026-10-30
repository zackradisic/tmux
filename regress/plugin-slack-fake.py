#!/usr/bin/env python3
"""A fake Slack web API for regress/plugin-slack.sh: one workspace, three
conversations, enough state to answer what the slack plugin asks.
`users.list` comes in two pages (the cursor must be followed) and leaves
out U3, a Slack Connect user from another workspace whom only `users.info`
knows; U9 is nobody.

    python3 plugin-slack-fake.py PORT     (0 = pick a free port)

The first line on stdout is the port, then the server runs until killed.
Every request is logged to stderr as `METHOD field=value ...`.

  POST /api/<method>   form-encoded, JSON back (Slack's `ok` envelope)
  GET  /               the workspace page, with `"api_token":"xoxc-fake"`
  GET  /_calls         JSON list of [method, fields] seen so far
  GET  /_posted        JSON list of the messages chat.postMessage took

Standard library only.
"""

import json
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import parse_qs

ME = "U1"
LOCK = threading.Lock()
CALLS = []   # [method, {field: value}]
POSTED = []  # {"channel", "ts", "text", "thread_ts"}
REPLIES = []  # thread replies, as messages plus "channel": what conversations.replies returns
MARKS = {}   # channel -> ts

USERS = [
    {"id": "U1", "name": "zack", "real_name": "Zack R", "tz_offset": -25200,
     "profile": {"display_name": "zack", "real_name": "Zack R"}},
    {"id": "U2", "name": "tejas", "real_name": "Tejas K", "tz_offset": -25200,
     "profile": {"display_name": "Tejas", "real_name": "Tejas K"}},
]

# Not in users.list: another workspace's member in a shared channel.
STRANGER = {"id": "U3", "team_id": "T2", "name": "gregor", "real_name": None, "tz_offset": 3600,
            "profile": {"display_name": "gregor", "real_name": None}}

CHANNELS = [
    {"id": "C1", "name": "general", "is_channel": True, "is_private": False,
     "is_member": True, "topic": {"value": "the town square"}, "purpose": {"value": ""}},
    {"id": "G1", "name": "secret", "is_channel": False, "is_group": True, "is_private": True,
     "is_member": True, "topic": {"value": ""}, "purpose": {"value": ""}},
    {"id": "D1", "is_im": True, "user": "U2", "is_member": True, "is_open": True},
]

# Newest first, as Slack returns a history page.
HISTORY = {
    "C1": [
        # Emoji: shortcodes in the text (a standard one, a custom alias to a
        # standard one, an image-only custom one), a reaction by name.
        {"type": "message", "user": "U2", "text": "shipped :raised_hands: :party: :morph:", "ts": "1700000400.000100",
         "reactions": [{"name": "fire", "count": 1, "users": ["U1"]}]},
        {"type": "message", "user": "U2", "text": "hey <@U1> look at *this*", "ts": "1700000300.000100"},
        {"type": "message", "user": "U1", "text": "fine", "ts": "1700000200.000100"},
        {"type": "message", "user": "U2", "text": "morning", "ts": "1700000100.000100"},
        # A bot card with no text: a fields-only legacy attachment.
        {"type": "message", "subtype": "bot_message", "bot_id": "B1", "username": "Demo App", "text": "",
         "ts": "1700000075.000100",
         "attachments": [{"fallback": "[no preview available]", "id": 1, "mrkdwn_in": ["fields"],
                          "fields": [{"title": "Status", "value": "deploy green", "short": True}]}]},
        {"type": "message", "user": "U3", "text": "hello from gregor", "ts": "1700000050.000100"},
        {"type": "message", "user": "U9", "text": "ghost", "ts": "1700000025.000100"},
    ],
    "G1": [],
    "D1": [],
}

# What client.counts says; conversations.mark and chat.postMessage move it.
COUNTS = {
    # Read up to "morning": the "fine" after it is ours, the "hey" is news.
    "C1": {"has_unreads": True, "mention_count": 1, "latest": "1700000400.000100", "last_read": "1700000100.000100"},
    "G1": {"has_unreads": False, "mention_count": 0, "latest": "1699990000.000100", "last_read": "1699990000.000100"},
    "D1": {"has_unreads": True, "mention_count": 0, "latest": "1700000050.000100", "last_read": "1700000000.000000"},
}

TS_SEQ = [0]


def fresh_ts():
    TS_SEQ[0] += 1
    return "%d.%06d" % (int(time.time()), TS_SEQ[0])


def latest_of(channel):
    msgs = HISTORY.get(channel, [])
    return msgs[0]["ts"] if msgs else COUNTS[channel]["latest"]


def ok(**kw):
    kw["ok"] = True
    return kw


def err(code):
    return {"ok": False, "error": code}


def count_row(cid):
    c = COUNTS[cid]
    return {"id": cid, "has_unreads": c["has_unreads"], "mention_count": c["mention_count"],
            "latest": c["latest"], "last_read": c["last_read"], "is_muted": False, "is_open": True}


def handle(method, f):
    """`f` is the form as {field: first value}."""
    if method == "auth.test":
        return ok(url="https://test.slack.com/", team="Morph", user="zack", team_id="T1", user_id=ME)
    if method == "users.conversations":
        return ok(channels=CHANNELS, response_metadata={"next_cursor": ""})
    if method == "users.channelSections.list":
        return ok(channel_sections=[
            {"channel_section_id": "S1", "name": "Work", "type": "standard", "emoji": "",
             "channel_ids_page": {"channel_ids": ["C1"]}},
            {"channel_section_id": "S2", "name": "Channels", "type": "channels", "emoji": "",
             "channel_ids_page": {"channel_ids": []}},
            {"channel_section_id": "S3", "name": "Direct messages", "type": "direct_messages", "emoji": "",
             "channel_ids_page": {"channel_ids": []}},
        ])
    if method == "users.list":
        # Two pages: the second only to a caller that follows the cursor.
        if f.get("cursor") == "p2":
            return ok(members=USERS[1:], response_metadata={"next_cursor": ""})
        return ok(members=USERS[:1], response_metadata={"next_cursor": "p2"})
    if method == "users.info":
        uid = f.get("user", "")
        for u in USERS + [STRANGER]:
            if u["id"] == uid:
                return ok(user=u)
        return err("user_not_found")
    if method == "emoji.list":
        # The workspace's custom emoji: an image, an alias to a standard
        # emoji, an alias to that alias.
        return ok(emoji={"morph": "https://emoji.slack-edge.com/T1/morph/abc.png",
                         "party": "alias:tada", "yay": "alias:party"})
    if method == "client.counts":
        return ok(channels=[count_row("C1"), count_row("G1")], mpims=[], ims=[count_row("D1")],
                  threads={"has_unreads": False, "mention_count": 0})
    if method == "conversations.history":
        cid = f.get("channel", "")
        if cid not in HISTORY:
            return err("channel_not_found")
        msgs = HISTORY[cid]
        oldest = f.get("oldest")
        if oldest:
            msgs = [m for m in msgs if m["ts"] > oldest]
        latest = f.get("latest")
        if latest:
            msgs = [m for m in msgs if m["ts"] < latest]
        try:
            limit = int(f.get("limit", "100"))
        except ValueError:
            limit = 100
        return ok(messages=msgs[:limit], has_more=len(msgs) > limit)
    if method == "conversations.replies":
        cid, ts = f.get("channel", ""), f.get("ts", "")
        root = [m for m in HISTORY.get(cid, []) if m["ts"] == ts]
        if not root:
            return err("thread_not_found")
        replies = sorted((m for m in REPLIES if m["channel"] == cid and m["thread_ts"] == ts), key=lambda m: m["ts"])
        return ok(messages=root + [{k: v for k, v in m.items() if k != "channel"} for m in replies], has_more=False)
    if method == "conversations.mark":
        cid, ts = f.get("channel", ""), f.get("ts", "")
        if cid not in COUNTS:
            return err("channel_not_found")
        MARKS[cid] = ts
        c = COUNTS[cid]
        c["last_read"] = ts
        c["has_unreads"] = latest_of(cid) > ts
        c["mention_count"] = 0
        return ok()
    if method == "chat.postMessage":
        cid = f.get("channel", "")
        if cid not in HISTORY:
            return err("channel_not_found")
        m = {"type": "message", "user": ME, "text": f.get("text", ""), "ts": fresh_ts()}
        if f.get("thread_ts"):
            m["thread_ts"] = f["thread_ts"]
            REPLIES.append(dict(m, channel=cid))
            # As Slack does: the root in the history grows a reply count.
            for root in HISTORY[cid]:
                if root["ts"] == f["thread_ts"]:
                    root["reply_count"] = root.get("reply_count", 0) + 1
                    root["latest_reply"] = m["ts"]
        else:
            HISTORY[cid].insert(0, m)
            COUNTS[cid]["latest"] = m["ts"]
        POSTED.append({"channel": cid, "ts": m["ts"], "text": m["text"], "thread_ts": m.get("thread_ts")})
        return ok(channel=cid, ts=m["ts"], message=m)
    if method == "reactions.add":
        return ok()
    if method == "chat.getPermalink":
        return ok(channel=f.get("channel", ""),
                  permalink="https://test.slack.com/archives/%s/p%s" % (f.get("channel", ""), f.get("message_ts", "").replace(".", "")))
    if method == "rtm.connect":
        # No websocket here: the plugin's feed backs off and the poll stays.
        return err("not_allowed_token_type")
    return err("unknown_method")


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):  # quiet; we log our own line
        pass

    def send_json(self, obj, status=200):
        body = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def send_html(self, html, status=200):
        body = html.encode()
        self.send_response(status)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        path = self.path.split("?", 1)[0]
        with LOCK:
            if path == "/":
                sys.stderr.write("GET / (boot page)\n"); sys.stderr.flush()
                self.send_html('<html><body><script>var boot={"api_token":"xoxc-fake","team_id":"T1"};</script></body></html>')
            elif path == "/_calls":
                self.send_json(CALLS)
            elif path == "/_posted":
                self.send_json(POSTED)
            elif path == "/_marks":
                self.send_json(MARKS)
            else:
                self.send_json(err("unknown_path"), 404)

    def do_POST(self):
        path = self.path.split("?", 1)[0]
        n = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(n).decode("utf-8", "replace") if n else ""
        ctype = self.headers.get("Content-Type", "")
        if "json" in ctype:
            try:
                form = {k: str(v) for k, v in json.loads(raw or "{}").items()}
            except ValueError:
                form = {}
        else:
            form = {k: v[0] for k, v in parse_qs(raw, keep_blank_values=True).items()}
        if not path.startswith("/api/"):
            self.send_json(err("unknown_path"), 404)
            return
        method = path[len("/api/"):]
        with LOCK:
            CALLS.append([method, form])
            sys.stderr.write("%s %s\n" % (method, " ".join("%s=%s" % (k, v) for k, v in sorted(form.items()))))
            sys.stderr.flush()
            if not self.headers.get("Authorization", "").startswith("Bearer xox"):
                self.send_json(err("not_authed"))
                return
            self.send_json(handle(method, form))


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 0
    srv = HTTPServer(("127.0.0.1", port), Handler)
    sys.stdout.write("%d\n" % srv.server_address[1])
    sys.stdout.flush()
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
