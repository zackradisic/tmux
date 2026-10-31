#!/bin/sh
# The slack plugin against a fake Slack (plugin-slack-fake.py, python3's
# standard library): no real workspace, no Keychain. The plugin is loaded
# with `-o api_base=http://127.0.0.1:PORT -o token=... -o cookie=...`, so
# every web API call lands on the fake and the identity comes from
# `auth.test` alone. Check:
#
#   boot: `status` names the user and team and counts the 4 channels;
#   `unread` lists the channel with a mention and the DM, not the read
#     private channel; the @slack_unread/@slack_mentions/@slack_dms
#     options carry the badges;
#   `help`, `open --help`, an unknown verb answer on the caller's
#     terminal;
#   `pick`: the sidebar float shows the "Work" section, #general with
#     its mention badge (`!` first, `@1` after the name), the DM by
#     display name with its unread dot, the read private channel with a
#     dim dot, "by sections", the newest message in the preview, a
#     footer that points at the Space menu and not at refresh; Space
#     draws the action menu (a client overlay: seen on a real tty client,
#     which a second server's pane provides by attaching) titled with
#     the channel, "mark read" offered with its key on an unread row;
#     `menu-key v` (the road every item takes back) switches to the
#     unread view (no read channel in it); `l` hands the keyboard to the
#     preview (the channel view embedded in the column: the footer swaps,
#     the separator lights), `k` moves its cursor bar, `y` copies that
#     message, `-`/`+` keep the column inside the separator, Enter opens
#     the message's thread in the full view, Escape twice is back on the
#     list; `q` closes;
#   `open #general`: the channel view with the topic, the history with
#     mrkdwn resolved (<@U1> -> @zack, *this* -> this), emoji drawn as
#     glyphs (:raised_hands: in the text, the `fire` reaction, a custom
#     alias from emoji.list; an image-only custom emoji stays :morph:),
#     the "new messages" rule before the first unread message that is not
#     ours; the Slack Connect author U3 (not in users.list) is named
#     through users.info, once; a bot card with no text shows its
#     fields; `i`, typed text, Enter posts to the fake and the view shows
#     it; Enter on that message (no replies yet) opens its thread, `i`
#     then Enter posts there with its thread_ts and the reply shows;
#     Escape goes back to the timeline, Escape again to the sidebar;
#     `q` closes;
#   `open #general <ts>` shows the thread directly and Escape opens the
#     channel's timeline rather than closing; `menu-key Enter`/`Escape`
#     (the Space menu's road back) drive the same keys; `q` closes from
#     a thread outright;
#   Space draws the action menu, titled with the channel, on a real tty
#     client (a second server attached to this one, as in
#     plugin-agents-menu.sh), with the items that do not apply dimmed;
#   `send @Tejas ...` posts to the DM; `mark #general` reaches
#     conversations.mark; `bind`/`unbind` answer; the plugin is still
#     alive at the end.
#
# The fake refuses rtm.connect, so the websocket feed backs off and the
# plugin polls (the websocket half is the host unit test).
#
# Needs the wasm example built:
#   cargo build -p slack --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin:/usr/local/bin:/opt/homebrew/bin
TERM=screen
unset TMUX TMUX_PANE AI_AGENT

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
# SLACK_TEST_SOCKET names the servers: two checkouts running this suite
# at once on the default name kill each other's servers.
SOCK=${SLACK_TEST_SOCKET:-slack-test}
TMUX="$TEST_TMUX -L$SOCK"
OUTER="$TEST_TMUX -L$SOCK-outer"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/slack.wasm
FAKE=$(dirname "$0")/plugin-slack-fake.py
FAKE=$(cd "$(dirname "$FAKE")" && pwd -P)/$(basename "$FAKE")

HOME=$(mktemp -d /tmp/slack.XXXXXX); HOME=$(cd "$HOME" && pwd -P); export HOME
XDG_DATA_HOME="$HOME/.local/share"; export XDG_DATA_HOME
# The wasm alone: with no sidecar next to it the `net` host allowlist
# does not apply and 127.0.0.1 is allowed.
DEPLOY=$(mktemp -d); cp "$WASM" "$DEPLOY/slack.wasm"

FAKEPID=
WATCHDOG=
CTL=
cleanup() {
	exec 3>&- 2>/dev/null
	[ -n "$CTL" ] && kill "$CTL" 2>/dev/null
	$OUTER kill-server 2>/dev/null
	$TMUX kill-server 2>/dev/null
	[ -n "$WATCHDOG" ] && kill "$WATCHDOG" 2>/dev/null
	[ -n "$FAKEPID" ] && kill "$FAKEPID" 2>/dev/null
	wait 2>/dev/null
	rm -rf "$HOME" "$DEPLOY"
}
trap cleanup EXIT
fail() {
	echo "FAIL: $*" >&2
	if [ -n "$FORM" ]; then
		echo "--- screen:" >&2
		$TMUX capture-pane -M -p -t "$FORM" 2>/dev/null | sed '/^ *$/d' >&2
	fi
	if [ -n "$VIEW" ]; then
		echo "--- what the tty client sees:" >&2
		$OUTER capture-pane -p -t "$VIEW" 2>/dev/null | sed '/^ *$/d' >&2
	fi
	echo "--- fake log (tail):" >&2
	tail -20 "$HOME/fake.log" >&2 2>/dev/null
	cleanup
	exit 1
}
# A GET on the fake.
fetch() {
	python3 -c 'import sys, urllib.request
sys.stdout.write(urllib.request.urlopen(sys.argv[1], timeout=5).read().decode())' "$1"
}
# How many calls of method $1 (with field $2 = $3, when given) the fake saw.
ncalls() {
	fetch "$BASE/_calls" | python3 -c 'import json, sys
m = sys.argv[1]; f = sys.argv[2] if len(sys.argv) > 2 else None; v = sys.argv[3] if len(sys.argv) > 3 else None
print(sum(1 for c in json.load(sys.stdin) if c[0] == m and (f is None or c[1].get(f) == v)))' "$@"
}
# How many messages with text $2 were posted to channel $1 (in thread $3,
# when given).
nposted() {
	fetch "$BASE/_posted" | python3 -c 'import json, sys
t = sys.argv[3] if len(sys.argv) > 3 else None
print(sum(1 for p in json.load(sys.stdin) if p["channel"] == sys.argv[1] and p["text"] == sys.argv[2] and (t is None or p["thread_ts"] == t)))' "$@"
}
# The ts of the message with text $2 posted to channel $1.
posted_ts() {
	fetch "$BASE/_posted" | python3 -c 'import json, sys
print(next(p["ts"] for p in json.load(sys.stdin) if p["channel"] == sys.argv[1] and p["text"] == sys.argv[2]))' "$1" "$2"
}
# Run `$2...` every quarter second until it succeeds, at most $1 times.
until_ok() {
	n=$1; shift
	i=0
	while [ $i -lt "$n" ]; do
		"$@" >/dev/null 2>&1 && return 0
		sleep 0.25; i=$((i + 1))
	done
	return 1
}
screen() { $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }
screen_has() { screen | grep -q -- "$1"; }
screen_lacks() { ! screen_has "$1"; }
# What the attached terminal renders, overlays (a menu) included.
seen() { $OUTER capture-pane -p -t "$VIEW" | sed '/^ *$/d'; }
seen_has() { seen | grep -q -- "$1"; }
# The pane in plugin mode, if any.
mode_pane() { $TMUX list-panes -a -F '#{pane_id} #{pane_mode}' | awk '/plugin-mode/ { print $1; exit }'; }
mode_open() { [ -n "$(mode_pane)" ]; }
# Whichever pane is in plugin mode shows $1.
mode_shows() { p=$(mode_pane); [ -n "$p" ] && $TMUX capture-pane -M -p -t "$p" | grep -q -- "$1"; }
mode_closed() { [ -z "$(mode_pane)" ]; }
keys() { $TMUX send-keys -t "$FORM" "$@"; sleep 0.4; }
slack() { $TMUX plugin-command slack "$*"; }
status_ready() { slack status | grep -q 'zack on Morph'; }
posted() { [ "$(nposted "$@")" -ge 1 ]; }
# What the tty client's terminal renders, overlays (a menu) included.
seen() { $OUTER capture-pane -p -t "$VIEW" | sed '/^ *$/d'; }
seen_has() { seen | grep -q -- "$1"; }
seen_lacks() { ! seen_has "$1"; }
VIEW=
marks_grew() { [ "$(ncalls conversations.mark channel C1)" -gt "$1" ]; }
# The row under the cursor (reverse video), its attributes stripped.
cursor_row() { $TMUX capture-pane -M -p -e -t "$FORM" | grep -E "$(printf '\033')\[([0-9]+;)*7m" | head -1 | sed "s/$(printf '\033')\[[0-9;]*m//g"; }
buffer_has() { $TMUX show-buffer 2>/dev/null | grep -q -- "$1"; }
FORM=
VIEW=

[ -f "$WASM" ] || fail "slack.wasm not built"
[ -f "$FAKE" ] || fail "no $FAKE"
command -v python3 >/dev/null || fail "python3 not found"

# --- the fake Slack on a free port -----------------------------------------
PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
BASE="http://127.0.0.1:$PORT"
python3 "$FAKE" "$PORT" >"$HOME/fake.port" 2>"$HOME/fake.log" &
FAKEPID=$!
i=0
while [ $i -lt 40 ]; do
	python3 -c "import socket; socket.create_connection(('127.0.0.1', $PORT), 1).close()" 2>/dev/null && break
	sleep 0.25; i=$((i + 1))
done
[ $i -lt 40 ] || fail "the fake did not come up"
[ "$(fetch "$BASE/_calls")" = "[]" ] || fail "the fake answers oddly: $(fetch "$BASE/_calls")"

# Watchdog: nothing below may hang.
( sleep 300; echo "FAIL: watchdog" >&2; $OUTER kill-server 2>/dev/null; $TMUX kill-server 2>/dev/null; kill $$ 2>/dev/null ) &
WATCHDOG=$!

# --- the server and the plugin ----------------------------------------------
$TMUX kill-server 2>/dev/null
$OUTER kill-server 2>/dev/null
sleep 0.5
$TMUX -f/dev/null new-session -d -s main -x 160 -y 45 'sleep 600' || fail "new-session"
PANE=$($TMUX list-panes -t main:0 -F '#{pane_id}')
# A key sent to a pane in a mode reaches the mode only through a client,
# so keep a control client attached for the whole run. Its stdin is a
# fifo this shell holds open (fd 3): no EOF, nothing to type, no sleeper
# left behind for cleanup's wait.
mkfifo "$HOME/ctl" || fail "mkfifo"
$TMUX -C attach -t main <"$HOME/ctl" >/dev/null 2>&1 &
CTL=$!
exec 3>"$HOME/ctl"
until_ok 20 sh -c "$TMUX list-clients | grep -q ." || fail "the control client did not attach"
$TMUX load-plugin -s server -o workspace=test -o "api_base=$BASE" -o token=xoxc-fake -o cookie=xoxd-fake \
    -c mode -c db -c fs-read -c fs-write -c run-process -c run-command -c write-options -c timers \
    -c net -c service-call "$DEPLOY/slack.wasm" || fail "load-plugin"

# --- boot: identity and the first sync --------------------------------------
until_ok 80 status_ready || fail "status never named the user: $(slack status)"
ST=$(slack status)
echo "$ST" | grep -q "4 channels" || fail "status does not count 4 channels: $ST"
echo "$ST" | grep -q 'test.slack.com' || fail "status does not name the workspace: $ST"
[ "$(ncalls auth.test)" -ge 1 ] || fail "auth.test was not called"
[ "$(ncalls users.conversations)" -ge 1 ] || fail "users.conversations was not called"
[ "$(ncalls emoji.list)" -ge 1 ] || fail "emoji.list was not called"
grep -q 'Authorization' "$HOME/fake.log" && fail "the fake logged a header"

# --- unread and the status-line options ------------------------------------
U=$(slack unread)
echo "$U" | grep -qx '#general (1 mention)' || fail "unread lacks #general (1 mention): $U"
echo "$U" | grep -qx '@Tejas' || fail "unread lacks @Tejas: $U"
echo "$U" | grep -q 'secret' && fail "unread lists the read channel: $U"
[ "$($TMUX show-options -sv @slack_unread)" = 2 ] || fail "@slack_unread: $($TMUX show-options -sv @slack_unread)"
[ "$($TMUX show-options -sv @slack_mentions)" = 1 ] || fail "@slack_mentions: $($TMUX show-options -sv @slack_mentions)"
[ "$($TMUX show-options -sv @slack_dms)" = 1 ] || fail "@slack_dms: $($TMUX show-options -sv @slack_dms)"

# --- help on the caller's terminal -----------------------------------------
slack help | grep -q 'pick' || fail "help printed nothing"
slack 'open --help' | grep -q 'j/k' || fail "open --help printed nothing"
slack bogus | grep -q 'unknown verb' || fail "an unknown verb did not answer"

# --- the sidebar -------------------------------------------------------------
# The Space menu is a client overlay on a real tty, which the control
# client cannot draw. A second server's pane runs a genuine attach to
# this one: capturing that pane shows what the user's terminal would,
# menu and all. The sidebar is opened by a key pressed there, so the
# plugin knows which client asked and the menu goes to it by name.
$OUTER -f/dev/null new-session -d -x 160 -y 45 "$TEST_TMUX -L$SOCK attach -t main" || fail "outer new-session"
VIEW=$($OUTER list-panes -F '#{pane_id}' | head -1)
[ -n "$VIEW" ] || fail "no viewer pane"
until_ok 40 sh -c "$TMUX list-clients -F '#{client_tty}' | grep -q /dev" || fail "no tty client attached"
$TMUX bind-key -n F5 'plugin-command slack pick' || fail "bind-key"
$OUTER send-keys -t "$VIEW" F5
until_ok 40 mode_open || fail "pick opened no plugin mode"
FORM=$(mode_pane)
until_ok 40 screen_has 'Work' || fail "the sidebar shows no Work section"
screen_has '#general' || fail "the sidebar lacks #general"
# The row look: the badge first, then the name, the mention count after it.
screen | grep -q '! #general  @1' || fail "the mention row is not '! #general  @1'"
screen | grep -q '● @Tejas' || fail "the unread DM lacks its dot before the name"
screen | grep -q '· \*secret' || fail "the read private channel lacks its dim dot"
screen_has '●1' && fail "the old right-hand badge is still drawn"
screen_has 'by sections' || fail "the sidebar is not tagged by sections"
# The preview of the selected channel names the stranger, lazily.
until_ok 60 screen_has 'hello from gregor' || fail "the preview never showed the history"
until_ok 60 screen_has 'gregor  ' || fail "the preview did not name U3"
screen_lacks 'U3 ' || fail "the preview shows the raw id U3"
[ "$(ncalls users.info user U3)" -ge 1 ] || fail "users.info was not asked for U3"
screen_has 'Space menu' || fail "the footer does not point at the menu"
screen_has 'refresh' && fail "the footer still lists refresh"
# The preview opens at its end: the newest message is on screen.
until_ok 40 screen_has 'look at this' || fail "the preview lacks the newest message"
# Space draws the menu over the sidebar, titled with the channel; an
# unread row offers "mark read" with its key.
keys Space
until_ok 20 seen_has 'mark read' || fail "Space drew no menu"
seen_has 'open in the Slack app' || fail "the menu is missing its later items"
# The title: the channel's name (menu_safe drops the `#`, which tmux
# would format-expand).
seen_has '─ general ─' || fail "the menu is not titled with the channel"
seen | grep 'mark read' | grep -q '(m)' || fail "mark read has no key on an unread row"
seen | grep 'refresh' | grep -q '(R)' || fail "refresh is not in the menu with its key"
$OUTER send-keys -t "$VIEW" Escape; sleep 0.5
seen_has 'open in the Slack app' && fail "Escape did not put the menu away"
# Every item hands its key back through menu-key: the view switch by that road.
$TMUX plugin-command slack 'menu-key v'
until_ok 20 screen_has 'by unread' || fail "menu-key v did not switch to the unread view"
screen_lacks 'secret' || fail "the unread view lists the read channel"
screen_has '#general' || fail "the unread view lacks #general"
screen_has '@Tejas' || fail "the unread view lacks @Tejas"
# Back to sections. m on #general (the first row of Work): the row loses
# its badge and, read now, sorts below #random, which spoke later. The
# cursor does not follow it down: it stays on the first row, #random.
$TMUX plugin-command slack 'menu-key v'; sleep 0.3
$TMUX plugin-command slack 'menu-key v'
until_ok 20 screen_has 'by sections' || fail "menu-key v twice did not return to sections"
keys g; keys g
BEFORE_M=$(ncalls conversations.mark channel C1)
keys m
until_ok 40 marks_grew "$BEFORE_M" || fail "m did not mark #general read"
until_ok 40 sh -c "$TMUX capture-pane -M -p -t $FORM | grep -q '· #general'" || fail "#general kept its badge: $(screen | grep general)"
RANDOM_AT=$(screen | grep -n '#random' | head -1 | cut -d: -f1)
GENERAL_AT=$(screen | grep -n '#general' | head -1 | cut -d: -f1)
[ "$RANDOM_AT" -lt "$GENERAL_AT" ] || fail "#general ($GENERAL_AT) did not drop below #random ($RANDOM_AT)"
cursor_row | grep -q '#random' || fail "the cursor followed the marked row: $(cursor_row)"
# --- the preview has the keyboard --------------------------------------------
# The preview is the channel view embedded in the column, drawn by the
# plugin at the column's width. l hands it the keyboard: the footer
# swaps to the channel view's hints and the separator lights up (┃);
# its keys then work on the messages; Escape hands the keyboard back.
# Nothing the column draws may cross the separator, at any width.
SEP_PY='
import sys
lines = sys.stdin.read().split("\n")
cols = [i for i, ch in enumerate(lines[0]) if ch in "│┃"]
if len(cols) != 1:
    print("no single separator in the title row: %r" % lines[0]); sys.exit(1)
c = cols[0]
bad = [l for l in lines if len(l) > c and l[c] not in "│┃"]
print("\n".join(bad)); sys.exit(1 if bad else 0)
'
sep_ok() { $TMUX capture-pane -M -p -t "$FORM" | python3 -c "$SEP_PY"; }
sep_lit() { screen | grep -q '┃'; }
# The cursor is on #random; down to #general, whose messages the sidebar
# already holds.
i=0
while [ $i -lt 4 ] && ! cursor_row | grep -q '#general'; do keys j; i=$((i + 1)); done
cursor_row | grep -q '#general' || fail "j did not reach #general: $(cursor_row)"
until_ok 40 screen_has 'look at this' || fail "the preview does not show #general"
sep_ok || fail "the preview column crosses the separator: $(sep_ok)"
sep_lit && fail "the separator is lit before l"
keys l
until_ok 20 screen_has 'Esc back to list' || fail "l did not hand the keyboard to the preview: $(screen | tail -1)"
sep_lit || fail "the separator did not light up"
sep_ok || fail "the focused column crosses the separator: $(sep_ok)"
screen | grep '▎' | grep -q 'shipped' || fail "the cursor bar is not on the newest message: $(screen | grep '▎')"
keys k
screen | grep '▎' | grep -q 'look at this' || fail "k did not move the cursor bar up: $(screen | grep '▎')"
screen | grep '▎' | grep -q 'shipped' && fail "the cursor bar stayed on the newest message too"
keys y
until_ok 40 buffer_has 'Tejas (2023-11-1[45] [0-9:]*): hey @zack look at this' || fail "y did not copy the message under the preview cursor: $($TMUX show-buffer)"
until_ok 20 screen_has 'copied 1 message' || fail "no copied status in the column: $(screen | tail -1)"
# - and + resize the float with the keyboard on the preview; the column
# is laid out again at each width.
keys -
until_ok 20 sep_ok || fail "after - the column crosses the separator: $(sep_ok)"
screen_has 'look at this' || fail "after - the preview lost its messages"
sep_lit || fail "after - the preview lost the keyboard"
keys +
until_ok 20 sep_ok || fail "after + the column crosses the separator: $(sep_ok)"
# Enter opens the message's thread in the full channel view.
keys Enter
until_ok 40 mode_shows '· thread' || fail "Enter in the preview did not open the thread"
FORM=$(mode_pane)
until_ok 40 screen_has 'look at this' || fail "the thread lacks its root"
screen_lacks 'morning' || fail "the thread shows the timeline"
# Escape: the timeline, then the sidebar again, the keyboard on the list.
keys Escape
until_ok 40 screen_has 'morning' || fail "Escape did not return to the timeline"
keys Escape
until_ok 40 mode_shows 'slack Morph' || fail "Escape in the timeline did not open the sidebar"
FORM=$(mode_pane)
until_ok 40 screen_has 'Work' || fail "the reopened sidebar shows no Work section"
sep_lit && fail "the reopened sidebar has the keyboard on the preview"
screen_has 'l preview' || fail "the footer does not offer l: $(screen | tail -1)"
# l then Escape: the keyboard comes back to the list, where j moves the cursor.
until_ok 40 screen_has 'random chatter' || fail "the preview of the first row never loaded"
keys l
until_ok 20 sep_lit || fail "l did not light the separator the second time"
keys Escape
sep_lit && fail "Escape did not hand the keyboard back to the list"
ROW=$(cursor_row)
keys j
[ "$(cursor_row)" != "$ROW" ] || fail "j after Escape did not move the list cursor: $ROW"
keys q
until_ok 40 mode_closed || fail "q did not close the sidebar"
FORM=
# Undo for the channel-view checks below (the new-messages rule): the
# fake gives #general its unread back, a refresh picks it up.
fetch "$BASE/_unread?channel=C1" >/dev/null
slack refresh
until_ok 40 sh -c "$TMUX plugin-command slack unread | grep -q general" || fail "#general did not get its unread back: $(slack unread)"

# --- the channel view ---------------------------------------------------------
$TMUX plugin-command -t "$PANE" slack 'open #general' || fail "plugin-command open"
until_ok 40 mode_open || fail "open opened no plugin mode"
FORM=$(mode_pane)
until_ok 60 screen_has 'morning' || fail "the channel view never showed the history"
screen_has '#general' || fail "the channel view lacks its title"
screen_has 'the town square' || fail "the channel view lacks the topic"
screen_has '@zack look at this' || fail "mrkdwn was not resolved"
screen_has '── new messages ──' || fail "no new-messages rule"
until_ok 60 screen_has 'gregor  ' || fail "the channel view did not name U3"
screen_lacks 'U3 ' || fail "the channel view shows the raw id U3"
screen_has 'Demo App' || fail "the bot post lacks its username"
screen_has 'Status: deploy green' || fail "the fields-only attachment shows no field"
screen_lacks 'no preview' || fail "the useless fallback is shown"
# Emoji: the shortcode and the reaction name as glyphs, the custom alias
# through emoji.list, the image-only custom emoji literal.
screen_has 'shipped 🙌 🎉 :morph:' || fail "emoji shortcodes were not rendered: $(screen | grep shipped)"
screen_has '🔥 1' || fail "the fire reaction was not rendered: $(screen | grep -n 1 | grep -v shipped | tail -3)"
screen | grep -q -e ':raised_hands:' -e ':fire:' -e ':party:' && fail "a shortcode stayed literal"
NEW=$(screen | grep -n 'new messages' | head -1 | cut -d: -f1)
HEY=$(screen | grep -n 'look at this' | head -1 | cut -d: -f1)
FINE=$(screen | grep -n 'fine' | head -1 | cut -d: -f1)
[ "$NEW" -lt "$HEY" ] || fail "the rule ($NEW) is not before the hey message ($HEY)"
[ "$FINE" -lt "$NEW" ] || fail "our own message ($FINE) counts as new (rule at $NEW)"
# y copies the message under the cursor (the newest) as a transcript
# line: author, stamp, text with the emoji as glyphs; v then k then y
# copies the two newest, oldest first.
keys y
until_ok 40 buffer_has 'Tejas (2023-11-1[45] [0-9:]*): shipped 🙌 🎉 :morph:' || fail "y did not copy the message: $($TMUX show-buffer)"
keys v
screen_has 'selecting' || fail "v did not open a selection"
keys k
keys y
until_ok 40 buffer_has 'look at this' || fail "y did not copy the selection: $($TMUX show-buffer)"
[ "$($TMUX show-buffer | grep -c '^Tejas (')" = 2 ] || fail "the selection copy is not two messages: $($TMUX show-buffer)"
[ "$($TMUX show-buffer | head -1 | grep -c 'look at this')" = 1 ] || fail "the selection is not oldest first: $($TMUX show-buffer)"
screen_has 'copied 2 messages' || fail "no copied status: $(screen | tail -2)"
keys j
# Compose: i, the text, Enter.
keys i
$TMUX send-keys -t "$FORM" -l 'hello there'; sleep 0.4
screen_has 'hello there' || fail "the typed text is not in the compose box"
keys Enter
until_ok 40 posted C1 'hello there' || fail "the post did not reach the fake: $(fetch "$BASE/_posted")"
until_ok 40 screen_has 'hello there' || fail "the posted message is not shown"
screen | grep -q 'posting' && fail "the posting status stayed"
ROOT=$(posted_ts C1 'hello there')
# Enter opens the thread of the message under the cursor - the one we
# just posted, which has no replies - and does not compose.
keys Enter
until_ok 40 screen_has '· thread' || fail "Enter did not open the thread"
until_ok 40 screen_lacks 'morning' || fail "the thread still shows the timeline"
screen_has 'hello there' || fail "the thread lacks its root"
screen | grep -q '› ' && fail "Enter opened the compose box"
# i then Enter is "reply in this thread": the post carries the root's ts.
keys i
$TMUX send-keys -t "$FORM" -l 'in the thread'; sleep 0.4
screen_has 'reply › in the thread' || fail "the thread compose box is not a reply box"
keys Enter
until_ok 40 posted C1 'in the thread' "$ROOT" || fail "the reply did not carry thread_ts=$ROOT: $(fetch "$BASE/_posted")"
until_ok 40 screen_has 'in the thread' || fail "the reply is not shown in the thread"
# Escape: back to the timeline, then back to the sidebar.
keys Escape
until_ok 40 screen_has 'morning' || fail "Escape did not return to the timeline"
screen_lacks '· thread' || fail "the title still says thread after Escape"
screen_has '1 reply' || fail "the root shows no reply marker in the timeline"
# The sidebar is a float of its own: find the mode pane again.
keys Escape
until_ok 40 mode_shows 'slack Morph' || fail "Escape in the timeline did not open the sidebar"
FORM=$(mode_pane)
keys q
until_ok 40 mode_closed || fail "q did not close the sidebar"
FORM=

# --- a thread opened directly, the menu-key door, q from a thread -----------
$TMUX plugin-command -t "$PANE" slack "open #general $ROOT" || fail "plugin-command open with a ts"
until_ok 40 mode_open || fail "open with a ts opened no plugin mode"
FORM=$(mode_pane)
until_ok 60 screen_has 'in the thread' || fail "open with a ts did not show the thread"
screen_has '· thread' || fail "the directly opened thread is not titled as one"
# Escape here opens the channel's timeline instead of closing.
keys Escape
until_ok 40 screen_has 'morning' || fail "Escape from a directly opened thread did not open the timeline"
mode_open || fail "Escape from a directly opened thread closed the view"
# The Space menu's items come back through `menu-key <key>`: the same
# keys, the same view. The cursor follows the bottom: the root.
slack 'menu-key Enter'
until_ok 40 screen_has '· thread' || fail "menu-key Enter did not open the thread"
slack 'menu-key Escape'
until_ok 40 screen_has 'morning' || fail "menu-key Escape did not return to the timeline"
slack 'menu-key Enter'
until_ok 40 screen_has '· thread' || fail "menu-key Enter did not reopen the thread"
# q closes outright, thread or not.
keys q
until_ok 40 mode_closed || fail "q did not close the channel view from a thread"
FORM=

# --- Space draws the menu -------------------------------------------------------
# A menu is a client overlay on a real tty: the control client cannot
# draw one. A second server's pane attaches to this one as a genuine
# pty client, and capturing that pane is what the user would see.
$OUTER kill-server 2>/dev/null
$OUTER -f/dev/null new-session -d -x 160 -y 45 "$TEST_TMUX -L$SOCK attach -t main" || fail "outer new-session"
until_ok 40 sh -c "$TMUX list-clients -F '#{client_tty}' | grep -q ." || fail "no tty client attached"
VIEW=$($OUTER list-panes -F '#{pane_id}' | head -1)
[ -n "$VIEW" ] || fail "no viewer pane"
$TMUX plugin-command -t "$PANE" slack 'open #general' || fail "plugin-command open (menu)"
until_ok 40 mode_open || fail "open (menu) opened no plugin mode"
FORM=$(mode_pane)
until_ok 60 screen_has 'morning' || fail "the channel view never showed the history (menu)"
screen_has 'Space menu' || fail "the footer does not point at the menu"
keys Space
until_ok 40 seen_has 'open thread' || fail "Space drew no menu"
seen_has 'general' || fail "the menu is not titled with the channel"
seen_has 'back to sidebar' || fail "the menu lacks the back-to-sidebar item"
seen | grep 'open thread' | grep -q '(Enter)' || fail "open thread shows no key"
seen | grep 'load older' | grep -q '(L)' || fail "load older shows no key"
screen | grep -q '› ' && fail "Space opened the compose box"
# The menu is the client's: dismiss it by typing on the tty, not the
# pane. Every item's key is also its shortcut (Escape and q would run
# "back" and "close"), so C-g is the plain dismissal.
$OUTER send-keys -t "$VIEW" C-g
until_ok 40 seen_lacks 'open thread' || fail "C-g did not dismiss the menu"
mode_open || fail "dismissing the menu closed the view"
screen_has 'morning' || fail "dismissing the menu changed the view"
# In a thread the thread item is dimmed (tmux draws it without its key)
# and back goes to the channel.
keys Enter
until_ok 40 screen_has '· thread' || fail "Enter did not open the thread (menu)"
keys Space
until_ok 40 seen_has 'back to channel' || fail "the thread menu lacks back-to-channel"
seen | grep 'open thread' | grep -q '(Enter)' && fail "open thread offered a key inside a thread"
seen | grep 'load older' | grep -q '(L)' && fail "load older offered a key inside a thread"
# An item's key typed in the menu runs it: Escape is "back to channel",
# so the menu agrees with the keymap by construction.
$OUTER send-keys -t "$VIEW" Escape
until_ok 40 seen_lacks 'back to channel' || fail "Escape did not take the thread menu down"
until_ok 40 screen_has 'morning' || fail "Escape chosen in the menu did not return to the timeline"
# While composing, Space types a space and Escape drops the text.
keys i
keys Space
$TMUX send-keys -t "$FORM" -l 'x'; sleep 0.4
screen_has '›  x' || fail "Space while composing did not type a space: $(screen | tail -2)"
keys Escape
screen | grep -q '›  x' && fail "Escape did not drop the composed text"
mode_open || fail "Escape while composing left the view"
keys q
until_ok 40 mode_closed || fail "q did not close the channel view (menu)"
FORM=
$OUTER kill-server 2>/dev/null
VIEW=

# --- send, mark, bind ---------------------------------------------------------
R=$(slack 'send @Tejas see you at 3')
[ "$R" = "posting to @Tejas…" ] || fail "send did not answer: $R"
until_ok 40 posted D1 'see you at 3' || fail "the DM did not reach the fake: $(fetch "$BASE/_posted")"
BEFORE=$(ncalls conversations.mark channel C1)
slack 'mark #general'
until_ok 40 marks_grew "$BEFORE" || fail "mark did not reach conversations.mark (had $BEFORE)"
R=$(slack 'bind #general claude:abc')
[ "$R" = "bound #general" ] || fail "bind did not answer: $R"
R=$(slack 'unbind #general')
[ "$R" = "unbound #general" ] || fail "unbind did not answer: $R"
R=$(slack 'send #nowhere x')
[ "$R" = "send: no channel #nowhere" ] || fail "send to a missing channel: $R"

# --- still alive ---------------------------------------------------------------
slack status | grep -q 'zack on Morph' || fail "the plugin stopped answering"
# Each unknown id was asked of users.info once, found or not, through
# every refetch above.
[ "$(ncalls users.info user U3)" -eq 1 ] || fail "users.info for U3 was asked $(ncalls users.info user U3) times"
[ "$(ncalls users.info user U9)" -eq 1 ] || fail "users.info for U9 was asked $(ncalls users.info user U9) times"
[ "$(ncalls users.list cursor p2)" -ge 1 ] || fail "the users.list cursor was not followed"
$TMUX show-plugins | grep -q slack || fail "show-plugins lost the plugin"

echo "PASS"
exit 0
