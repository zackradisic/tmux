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
#     mrkdwn resolved (<@U1> -> @zack, *this* -> this), a link whose
#     label is the url shortened drawn as the whole url and a labelled
#     link followed by its url (y copies the same), emoji drawn as
#     glyphs (:raised_hands: in the text, the `fire` reaction, a custom
#     alias from emoji.list; an image-only custom emoji stays :morph:),
#     the "new messages" rule before the first unread message that is not
#     ours; the Slack Connect author U3 (not in users.list) is named
#     through users.info, once; a bot card with no text shows its
#     fields; the shared GIF's thumbnail is fetched once (with the
#     session) and drawn under its clip line as kitty placeholder cells
#     (U+10EEEE, the image id as the foreground colour), and the
#     converted PNG sits in the plugin's data directory; a /giphy post's
#     picture (an attachment image block) is drawn under its caption;
#     `i`, typed text, Enter posts to the fake and the view shows
#     it; Enter on that message (no replies yet) opens its thread, `i`
#     then Enter posts there with its thread_ts and the reply shows;
#     Escape goes back to the timeline, Escape again to the sidebar;
#     `q` closes;
#   `n` on a selection: the prompt file is written under the plugin's
#     data directory with the `From Slack #general on <date>:` header
#     and the two messages' transcript lines, and the hand-off form
#     opens over the view (a second float) naming the channel, the
#     count and the file; `C-e` runs $EDITOR on the file in a popup and
#     the form comes back with the new line count; the ask typed in,
#     the command set to the fake launcher, Enter starts the agent
#     through the real agents plugin (loaded beside slack): a window
#     named after the channel runs `fake "$(cat '<file>')"`, the file
#     begins with the ask and then the header, the form closes by
#     itself and the view reports where the agent landed;
#   `open #general <ts>` shows the thread directly and Escape opens the
#     channel's timeline rather than closing; `menu-key Enter`/`Escape`
#     (the Space menu's road back) drive the same keys; `q` closes from
#     a thread outright;
#   Space draws the action menu, titled with the channel, on a real tty
#     client (a second server attached to this one, as in
#     plugin-agents-menu.sh), with the items that do not apply dimmed;
#   `send @Tejas ...` posts to the DM; a DM from Tejas injected down the
#     feed's road (`rtm-inject`) is toasted once (its desktop_notification
#     is a repeat) and bumps @slack_dms; `mark #general` reaches
#     conversations.mark; `bind`/`unbind` answer; the plugin is still
#     alive at the end.
#
# The fake refuses rtm.connect, so the websocket feed backs off and the
# plugin polls (the websocket half is the host unit test).
#
# Needs the wasm examples built (the agents plugin answers the `n` form):
#   cargo build -p slack -p agents --target wasm32-unknown-unknown --release

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
AGENTS=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm
FAKE=$(dirname "$0")/plugin-slack-fake.py
FAKE=$(cd "$(dirname "$FAKE")" && pwd -P)/$(basename "$FAKE")

HOME=$(mktemp -d /tmp/slack.XXXXXX); HOME=$(cd "$HOME" && pwd -P); export HOME
XDG_DATA_HOME="$HOME/.local/share"; export XDG_DATA_HOME
# The wasm alone: with no sidecar next to it the `net` host allowlist
# does not apply and 127.0.0.1 is allowed.
DEPLOY=$(mktemp -d); cp "$WASM" "$DEPLOY/slack.wasm"
# The agents plugin beside it, with its own sidecar (as in
# plugin-agents-new-cli.sh, plus service-serve for the `new` service
# the hand-off form calls). Its store lands under this run's HOME.
cp "$AGENTS" "$DEPLOY/agents.wasm" 2>/dev/null
cat >"$DEPLOY/agents.toml" <<'TOML'
[caps]
requests = ["capture-pane","run-command","mode","db","env-read","pane-fds","send-keys","run-process","fs-list","fs-read-any","service-serve"]
[caps.env-read]
names = ["AI_AGENT","OPENCODE"]
TOML

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
	echo "--- server: $($TMUX list-panes -a -F '#{pane_id} #{pane_mode} #{window_name}' 2>&1 | tr '\n' ';')" >&2
	echo "--- plugin log (tail):" >&2
	$TMUX plugin-log slack 2>&1 | tail -25 >&2
	$TMUX show-messages 2>/dev/null | tail -8 >&2
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
# The hand-off form: a second plugin-mode pane beside the view's.
form_pane() { $TMUX list-panes -a -F '#{pane_id} #{pane_mode}' | awk -v skip="$FORM" '/plugin-mode/ && $1 != skip { print $1; exit }'; }
form_open() { [ -n "$(form_pane)" ]; }
form_closed() { [ -z "$(form_pane)" ]; }
fscreen() { $TMUX capture-pane -M -p -t "$HF" | sed '/^ *$/d'; }
fscreen_has() { fscreen | grep -q -- "$1"; }
fkeys() { $TMUX send-keys -t "$HF" "$@"; sleep 0.4; }
FORM=
VIEW=
HF=

[ -f "$WASM" ] || fail "slack.wasm not built"
[ -f "$AGENTS" ] || fail "agents.wasm not built"
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
$TMUX load-plugin -s server -o trust_env=1 -o "launch=fake = sh -c 'exec cat'" \
    -c capture-pane -c run-command -c mode -c db -c env-read -c pane-fds -c send-keys \
    -c run-process -c fs-list -c fs-read-any -c service-serve "$DEPLOY/agents.wasm" || fail "load-plugin agents"

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
# The Unread section comes first: every unread row, the DM before the
# channel; #general is there and in Work. The cursor opens on the DM.
screen | grep -q '▪ Unread  2' || fail "no Unread section of 2 rows: $(screen | grep Unread)"
TEJ=$(screen | grep -n '● @Tejas' | head -1 | cut -d: -f1)
GEN=$(screen | grep -n '! #general' | head -1 | cut -d: -f1)
[ "$TEJ" -lt "$GEN" ] || fail "the DM ($TEJ) is not before the channel ($GEN) in Unread"
[ "$(screen | grep -c '#general')" = 2 ] || fail "#general is not in Unread and in Work: $(screen | grep -c '#general')"
until_ok 60 screen_has 'are you around' || fail "the DM preview never loaded"
keys j
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
keys g; keys g; keys j
cursor_row | grep -q '#general' || fail "gg j is not on #general in Unread: $(cursor_row)"
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
# A resize any way round is remembered: after - the saved width is the
# float's.
until_ok 20 sh -c "[ \"\$($TMUX show-options -sqv @slack-width)\" = \"\$($TMUX display -p -t $FORM '#{pane_width}')\" ]" || fail "@slack-width ($($TMUX show-options -sqv @slack-width)) is not the float's width ($($TMUX display -p -t "$FORM" '#{pane_width}'))"
# Enter opens the message's thread in the preview itself (the sidebar
# stays, the keyboard too); Escape brings the channel back, still in
# the preview; Escape again hands the keyboard to the list.
keys Enter
until_ok 40 screen_has '· thread' || fail "Enter in the preview did not open the thread there: $(screen | head -2)"
mode_shows 'slack Morph' || fail "Enter left the sidebar"
until_ok 40 screen_has 'look at this' || fail "the thread lacks its root"
screen_lacks 'morning' || fail "the thread preview shows the timeline"
sep_lit || fail "the thread took the keyboard from the preview"
screen_has 'Esc back to channel' || fail "the footer does not say Esc back to channel: $(screen | tail -1)"
keys Escape
until_ok 40 screen_has 'morning' || fail "Escape did not bring the channel back"
screen_lacks '· thread' || fail "the header still says thread"
sep_lit || fail "Escape out of the thread dropped the preview focus"
keys Escape
sep_lit && fail "Escape in the timeline did not hand the keyboard back to the list"
screen_has 'l preview' || fail "the footer does not offer l: $(screen | tail -1)"
# l then Escape: the keyboard comes back to the list, where j moves the cursor.
keys l
until_ok 20 sep_lit || fail "l did not light the separator the second time"
keys Escape
sep_lit && fail "Escape did not hand the keyboard back to the list"
ROW=$(cursor_row)
keys j
[ "$(cursor_row)" != "$ROW" ] || fail "j after Escape did not move the list cursor: $ROW"
# n on a row is the hand-off, not compose: the form opens over the
# sidebar with the previewed DM's message; Escape puts it away.
keys g; keys g
until_ok 40 screen_has 'are you around' || fail "the DM preview is not showing before n"
keys n
until_ok 40 form_open || fail "n in the sidebar opened no hand-off form"
HF=$(form_pane)
until_ok 40 fscreen_has '1 message from @Tejas' || fail "the form is not about the previewed DM: $(fscreen | head -3)"
mode_shows 'slack Morph' || fail "n opened the channel view instead of keeping the sidebar"
fkeys Escape
until_ok 20 form_closed || fail "Escape did not close the hand-off form"
HF=
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
# - shrinks the float by a step and the size is remembered; + puts it back.
CW=$($TMUX display -p -t "$FORM" '#{pane_width}')
keys -
until_ok 20 sh -c "[ \"\$($TMUX display -p -t $FORM '#{pane_width}')\" = $((CW - 12)) ]" || fail "- did not shrink the channel view: $($TMUX display -p -t "$FORM" '#{pane_width}') from $CW"
until_ok 20 sh -c "[ \"\$($TMUX show-options -sqv @slack-chat-width)\" = $((CW - 12)) ]" || fail "@slack-chat-width was not saved: $($TMUX show-options -sqv @slack-chat-width)"
keys +
until_ok 20 sh -c "[ \"\$($TMUX display -p -t $FORM '#{pane_width}')\" = $CW ]" || fail "+ did not restore the width: $($TMUX display -p -t "$FORM" '#{pane_width}')"
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
# Pictures: the GIF's static thumbnail is fetched from the fake once, as
# the session (bearer + cookie), converted to a PNG in the data
# directory, and drawn under the clip line as 5 rows of placeholder
# cells (400x50 at cell aspect 2 in a 156-cell body: 80 columns, the
# widest block, 5 rows), their foreground the kitty image id (0x530000
# is the first in a fresh process). The /giphy post's picture (an attachment image
# block, 400x50 too) is drawn under its caption the same way: 10
# placeholder rows in all, two fetches, two ids.
PH=$(printf '\364\216\273\256')
screen_has '📎 Nervous K and P.gif' || fail "the file message shows no clip line"
screen_has '│ \[image: meerkat GIF\]' || fail "the giphy post shows no caption"
until_ok 80 sh -c "[ \"\$($TMUX capture-pane -M -p -t $FORM | grep -c '$PH')\" = 10 ]" || fail "placeholder rows: $(screen | grep -c "$PH"), not 10: $(tail -5 "$HOME/fake.log")"
CLIP=$(screen | grep -n 'Nervous K and P' | head -1 | cut -d: -f1)
screen | sed -n "$((CLIP + 1))p" | grep -q "$PH" || fail "no picture right under the clip line (line $CLIP)"
screen | sed -n "$((CLIP + 5))p" | grep -q "$PH" || fail "the picture under the clip line is shorter than 5 rows"
screen | sed -n "$((CLIP + 6))p" | grep -q "$PH" && fail "the picture under the clip line is taller than 5 rows"
CAP=$(screen | grep -n 'meerkat GIF' | head -1 | cut -d: -f1)
screen | sed -n "$((CAP + 1))p" | grep -q "$PH" || fail "no picture right under the giphy caption (line $CAP)"
# The foreground carries the kitty id: in the plugin's range (0x53xxxx),
# one id per picture. Ids are never reused in the process, so the view
# opened from the sidebar's preview above has already used some.
IDS=$($TMUX capture-pane -M -p -e -t "$FORM" | grep "$PH" | grep -o "$(printf '\033')\[38;2;83;[0-9]*;[0-9]*m" | sort -u | wc -l | tr -d ' ')
[ "$IDS" = 2 ] || fail "the placeholder rows carry $IDS image ids in the plugin's range, not 2"
[ "$(ncalls _img)" = 2 ] || fail "the pictures were fetched $(ncalls _img) times, not 2"
[ "$(ncalls _img path /_img/F1.png)" = 1 ] || fail "the thumbnail was not fetched once"
[ "$(ncalls _img path /_img/A1.png)" = 1 ] || fail "the giphy picture was not fetched once"
[ "$(ncalls _img auth bearer)" = 2 ] || fail "a picture was fetched without the session token"
[ "$(ncalls _img cookie yes)" = 2 ] || fail "a picture was fetched without the session cookie"
PNG=$(find "$HOME" -name F1.png | head -1)
[ -n "$PNG" ] || fail "no converted F1.png under $HOME"
find "$HOME" -name '*.src' | grep -q . && fail "a downloaded source was not removed"
file "$PNG" | grep -q 'PNG image data, 400 x 50' || fail "F1.png is not the 400x50 picture: $(file "$PNG")"
find "$HOME" -name 'a1700000028000100-0.png' | grep -q . || fail "the giphy picture was not converted under its own id"
# Scrolling redraws the rows as text: no further fetch.
keys k; keys k; keys j; keys j
[ "$(ncalls _img)" = 2 ] || fail "scrolling fetched a picture again"
[ "$(screen | grep -c "$PH")" = 10 ] || fail "the picture rows did not survive a scroll"
# Links: g goes to the oldest message, whose abbreviated label
# ("example.com/.../docu…") is drawn as the whole url and whose "the
# docs" label is followed by its url in brackets; y copies the same; G
# comes back to the newest.
LONG='https://example.com/some/very/long/path/to/the/document.html'
keys g
until_ok 20 screen_has "read $LONG and" || fail "the abbreviated link is not drawn as the full url: $(screen | grep -n 'example.com')"
screen_has 'docu…' && fail "the shortened label is still on screen"
screen_has 'the docs <https://docs.example.com/x>' || fail "the labelled link lacks its url: $(screen | grep 'the docs')"
keys y
until_ok 40 buffer_has "read $LONG and the docs <https://docs.example.com/x>" || fail "y did not copy the full urls: $($TMUX show-buffer)"
keys G
until_ok 20 sh -c "$TMUX capture-pane -M -p -t $FORM | grep '▎' | grep -q shipped" || fail "G did not return to the newest message: $(screen | grep '▎')"
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
# n hands the same two messages to a new agent. The transcript goes to
# a file under the plugin's data directory and the hand-off form opens
# over the view as a second float; the view stays.
keys '?'
screen_has 'n agent' || fail "the footer does not offer n: $(screen | tail -1)"
printf '#!/bin/sh\nprintf "edited in the popup\\n" >>"$1"\n' >"$HOME/ed.sh"; chmod +x "$HOME/ed.sh"
$TMUX set-environment -g EDITOR "$HOME/ed.sh"
# From the newest message: the selection is the two newest, whatever the
# copy checks above left the cursor on.
keys G
keys v
screen_has 'n agent' || fail "the selecting status does not offer n: $(screen | tail -1)"
keys k
keys n
until_ok 40 form_open || fail "n opened no form"
HF=$(form_pane)
mode_open || fail "the view closed under the form"
until_ok 40 fscreen_has 'Agent from Slack' || fail "the form has no title: $(fscreen)"
fscreen_has '2 messages from #general' || fail "the banner does not count the selection: $(fscreen)"
fscreen_has 'file: prompts/general-' || fail "the prompt field does not name the file: $(fscreen)"
fscreen_has 'C-e edit the prompt' || fail "the hint does not offer C-e: $(fscreen | tail -1)"
fscreen | grep -q '^  name  *general' || fail "the name does not default to the channel: $(fscreen | grep name)"
fscreen | grep -q '^  session  *main' || fail "the session does not default to the pane's: $(fscreen | grep session)"
fscreen | grep -q '^  command  *claude' || fail "the command does not default to claude: $(fscreen | grep command)"
PROMPT=$(find "$XDG_DATA_HOME" -path '*/slack/prompts/general-*.md' | head -1)
[ -n "$PROMPT" ] || fail "no prompt file under $XDG_DATA_HOME"
head -1 "$PROMPT" | grep -q '^From Slack #general on 20[0-9][0-9]-[0-9][0-9]-[0-9][0-9]:$' || fail "the file has no header: $(head -1 "$PROMPT")"
[ -z "$(sed -n 2p "$PROMPT")" ] || fail "no blank line under the header: $(sed -n 2p "$PROMPT")"
grep -q '^Tejas (20[0-9-]* [0-9:]*): .*look at this' "$PROMPT" || fail "the file lacks the first message: $(cat "$PROMPT")"
grep -q '^Tejas (20[0-9-]* [0-9:]*): shipped' "$PROMPT" || fail "the file lacks the second message: $(cat "$PROMPT")"
LINES_BEFORE=$(fscreen | sed -n 's/.*(\([0-9]*\) lines).*/\1/p' | head -1)
[ -n "$LINES_BEFORE" ] || fail "the prompt field shows no line count: $(fscreen | grep prompt)"
# C-e: $EDITOR on the file in a popup (on the tty client); the form
# waits for it and shows the new count.
fkeys C-e
until_ok 60 fscreen_has "($((LINES_BEFORE + 1)) lines)" || fail "C-e did not come back with the edited file: $(fscreen)"
grep -q '^edited in the popup$' "$PROMPT" || fail "the editor's line is not in the file: $(cat "$PROMPT")"
# The ask goes above the transcript; the command is the fake launcher.
$TMUX send-keys -t "$HF" -l 'summarise this for me'; sleep 0.4
fscreen_has 'summarise this for me' || fail "the ask was not typed: $(fscreen)"
fkeys C-j; fkeys C-j; fkeys C-j; fkeys C-j
fkeys C-u
$TMUX send-keys -t "$HF" -l 'fake'; sleep 0.4
fscreen | grep -q '^  command  *fake' || fail "the command was not replaced: $(fscreen | grep command)"
# The prompt field is a toggle: Tab swaps to the clipboard and back.
fkeys C-j; fkeys C-j
fkeys Tab
fscreen_has 'clipboard' || fail "Tab did not toggle the prompt to the clipboard: $(fscreen | grep prompt)"
fkeys Tab
fscreen_has 'file: prompts/general-' || fail "Tab did not toggle the prompt back to the file: $(fscreen | grep prompt)"
fkeys Enter
until_ok 60 fscreen_has 'started in main' || fail "Enter did not start the agent: $(fscreen)"
NP=$($TMUX list-panes -t main:general -F '#{pane_id}' 2>/dev/null | head -1)
[ -n "$NP" ] || fail "no window named general in main: $($TMUX list-windows -t main)"
SC=$($TMUX display-message -p -t "$NP" '#{pane_start_command}')
echo "$SC" | grep -qF -- "exec cat" || fail "the window does not run the fake launcher: $SC"
echo "$SC" | grep -qF -- "\$(cat '$PROMPT')" || fail "the window's command does not read the prompt file: $SC"
[ "$(head -1 "$PROMPT")" = "summarise this for me" ] || fail "the ask is not the file's first line: $(head -3 "$PROMPT")"
[ -z "$(sed -n 2p "$PROMPT")" ] || fail "no blank line under the ask"
sed -n 3p "$PROMPT" | grep -q '^From Slack #general on ' || fail "the header does not follow the ask: $(sed -n 3p "$PROMPT")"
grep -q '^edited in the popup$' "$PROMPT" || fail "the edit was lost at submit: $(cat "$PROMPT")"
# The form closes by itself after a beat; the view reports the landing.
until_ok 40 form_closed || fail "the form did not close after the agent started"
HF=
mode_open || fail "the view closed with the form"
screen_has 'agent started in main' || fail "the view does not report the agent: $(screen | tail -1)"
screen_has 'selecting' && fail "the selection survived n"
$TMUX kill-window -t main:general
keys j
# Compose: i opens the box in insert mode; Enter sends and the box stays
# open, empty, for the next message.
keys i
screen_has '› ' || fail "i did not open the compose box"
screen_has -- '-- INSERT --' || fail "the box does not say insert mode: $(screen | tail -3)"
screen_has 'C-j newline' || fail "the foot does not carry the compose hint: $(screen | tail -1)"
$TMUX send-keys -t "$FORM" -l 'hello there'; sleep 0.4
screen_has 'hello there' || fail "the typed text is not in the compose box"
keys Enter
until_ok 40 posted C1 'hello there' || fail "the post did not reach the fake: $(fetch "$BASE/_posted")"
until_ok 40 screen_has 'hello there' || fail "the posted message is not shown"
screen | grep -q 'posting' && fail "the posting status stayed"
screen_has '› ' || fail "Enter closed the compose box"
screen_has -- '-- INSERT --' || fail "the box left insert mode after Enter: $(screen | tail -3)"
# The next message needs no i; C-j puts a newline inside it, and the
# box grows a row for it.
$TMUX send-keys -t "$FORM" -l 'line one'; keys C-j
$TMUX send-keys -t "$FORM" -l 'line two'; sleep 0.4
screen_has '› line one' || fail "the first line of the draft is not shown: $(screen | tail -4)"
screen_has '  line two' || fail "C-j did not start a second line: $(screen | tail -4)"
keys Enter
TWO=$(printf 'line one\nline two')
until_ok 40 posted C1 "$TWO" || fail "the two-line post did not keep its newline: $(fetch "$BASE/_posted")"
until_ok 40 screen_has 'line two' || fail "the two-line post is not shown"
# Escape: normal mode, where vim keys edit the draft; Escape again closes.
$TMUX send-keys -t "$FORM" -l 'draft words'; sleep 0.4
keys Escape
screen_has -- '-- NORMAL --' || fail "Escape did not switch the box to normal mode: $(screen | tail -3)"
screen_has '› draft words' || fail "Escape dropped the draft"
keys b
keys d
keys w
screen_lacks 'draft words' || fail "dw did not delete the word: $(screen | tail -3)"
screen_has '› draft' || fail "dw took too much: $(screen | tail -3)"
keys 0
keys x
screen_has '› raft' || fail "0 x did not delete the first character: $(screen | tail -3)"
keys d
keys d
screen_lacks 'raft' || fail "dd did not empty the draft: $(screen | tail -3)"
screen_has '› ' || fail "dd closed the box"
keys Escape
screen | grep -q '› ' && fail "the second Escape did not close the box"
screen | grep -q -- '-- NORMAL --' && fail "the mode tag outlived the box"
mode_open || fail "Escape while composing left the view"
screen_has 'Space menu' || fail "the foot did not go back to the hints: $(screen | tail -1)"
ROOT=$(posted_ts C1 "$TWO")
# Enter opens the thread of the message under the cursor - the two-line
# post, the newest, which has no replies - and does not compose.
keys Enter
until_ok 40 screen_has '· thread' || fail "Enter did not open the thread"
until_ok 40 screen_lacks 'morning' || fail "the thread still shows the timeline"
screen_has 'line two' || fail "the thread lacks its root"
screen | grep -q '› ' && fail "Enter opened the compose box"
# i then Enter is "reply in this thread": the post carries the root's ts.
keys i
$TMUX send-keys -t "$FORM" -l 'in the thread'; sleep 0.4
screen_has 'reply › in the thread' || fail "the thread compose box is not a reply box"
keys Enter
until_ok 40 posted C1 'in the thread' "$ROOT" || fail "the reply did not carry thread_ts=$ROOT: $(fetch "$BASE/_posted")"
until_ok 40 screen_has 'in the thread' || fail "the reply is not shown in the thread"
# The reply box stays open too: Escape to normal mode, Escape closes it.
screen_has 'reply › ' || fail "Enter closed the reply box"
keys Escape
screen_has -- '-- NORMAL --' || fail "Escape did not switch the reply box to normal mode"
keys Escape
screen | grep -q 'reply › ' && fail "the second Escape did not close the reply box"
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
# While composing, Space types a space; Escape keeps the draft and goes
# to normal mode, where q is a vim key and not "close"; Escape closes.
keys i
keys Space
$TMUX send-keys -t "$FORM" -l 'x'; sleep 0.4
screen_has '›  x' || fail "Space while composing did not type a space: $(screen | tail -2)"
keys Escape
screen_has '›  x' || fail "Escape dropped the draft instead of switching modes"
keys q
mode_open || fail "q in the box's normal mode closed the view"
keys Escape
screen | grep -q '›  x' && fail "the second Escape did not close the box"
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
# The DM we just sent is read: no unread DM. A message from Tejas in it,
# injected down the feed's road (the fake has no websocket), is told as
# a toast titled with the sender (the plugin logs the toast; no toast
# plugin is loaded here) and bumps @slack_dms at once; the fake is told
# too, so the next badge poll agrees. Slack's desktop_notification for
# the same message is a repeat and stays silent; our own message in the
# DM, and a message in a channel, are not toasts of their own.
until_ok 40 sh -c "[ \"\$($TMUX show-options -sv @slack_dms)\" = 0 ]" || fail "@slack_dms after our DM: $($TMUX show-options -sv @slack_dms)"
toasts() { $TMUX plugin-log slack 2>/dev/null | grep -c 'slack: toast:'; }
T0=$(toasts)
R=$(slack 'rtm-inject {"type":"message","channel":"D1","user":"U1","text":"me again","ts":"1700000900.000100"}')
[ "$R" = "injected" ] || fail "rtm-inject (ours) did not answer: $R"
R=$(slack 'rtm-inject {"type":"message","channel":"C1","user":"U2","text":"a channel message","ts":"1700000901.000100"}')
[ "$R" = "injected" ] || fail "rtm-inject (channel) did not answer: $R"
R=$(slack 'rtm-inject {"type":"message","channel":"D1","user":"U2","text":"ping from the *feed*","ts":"1700000902.000100"}')
[ "$R" = "injected" ] || fail "rtm-inject did not answer: $R"
fetch "$BASE/_unread?channel=D1" >/dev/null
until_ok 40 sh -c "$TMUX plugin-log slack | grep -q 'slack: toast: Tejas: ping from the feed'" || fail "the DM was not toasted: $($TMUX plugin-log slack | tail -5)"
until_ok 40 sh -c "[ \"\$($TMUX show-options -sv @slack_dms)\" = 1 ]" || fail "@slack_dms after the DM: $($TMUX show-options -sv @slack_dms)"
R=$(slack 'rtm-inject {"type":"desktop_notification","title":"Morph","subtitle":"Tejas","msg":"1700000902.000100","channel":"D1","content":"ping from the feed"}')
[ "$R" = "injected" ] || fail "rtm-inject (notification) did not answer: $R"
R=$(slack 'rtm-inject {"type":"desktop_notification","title":"Morph","subtitle":"#general","msg":"1700000901.000100","channel":"C1","content":"Tejas: a channel message"}')
[ "$R" = "injected" ] || fail "rtm-inject (channel notification) did not answer: $R"
until_ok 40 sh -c "$TMUX plugin-log slack | grep -q 'slack: toast: Morph (#general): Tejas: a channel message'" || fail "the channel notification was not toasted: $($TMUX plugin-log slack | tail -5)"
[ "$(toasts)" = "$((T0 + 2))" ] || fail "$(( $(toasts) - T0 )) toasts, not 2 (the DM once, the channel once): $($TMUX plugin-log slack | grep 'toast:')"
# A mention in a channel toasts on the message itself too; Slack's
# notification for it, when it comes, is a repeat.
R=$(slack 'rtm-inject {"type":"message","channel":"C1","user":"U2","text":"<@U1> can you look?","ts":"1700000903.000100"}')
[ "$R" = "injected" ] || fail "rtm-inject (mention) did not answer: $R"
until_ok 40 sh -c "$TMUX plugin-log slack | grep -q 'slack: toast: #general · Tejas: @zack can you look?'" || fail "the mention was not toasted: $($TMUX plugin-log slack | grep 'toast:' | tail -3)"
R=$(slack 'rtm-inject {"type":"desktop_notification","title":"Morph","subtitle":"#general","msg":"1700000903.000100","channel":"C1","content":"Tejas: @zack can you look?"}')
[ "$R" = "injected" ] || fail "rtm-inject (mention notification) did not answer: $R"
sleep 0.5
[ "$(toasts)" = "$((T0 + 3))" ] || fail "$(( $(toasts) - T0 )) toasts, not 3 (the mention once): $($TMUX plugin-log slack | grep 'toast:')"
$TMUX plugin-log slack | grep -q 'desktop_notification C already told' || fail "the repeated notification left no evidence line"
R=$(slack 'rtm-inject nonsense')
echo "$R" | grep -q '^rtm-inject:' || fail "rtm-inject with bad json did not complain: $R"
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
