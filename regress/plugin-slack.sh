#!/bin/sh
# The slack plugin against a fake Slack (plugin-slack-fake.py, python3's
# standard library): no real workspace, no Keychain. The plugin is loaded
# with `-o api_base=http://127.0.0.1:PORT -o token=... -o cookie=...`, so
# every web API call lands on the fake and the identity comes from
# `auth.test` alone. Check:
#
#   boot: `status` names the user and team and counts the 4 channels;
#   `unread` lists the channel with a mention, the DM and the followed
#     thread with its 2 new replies (one a mention), not the read
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
TOAST=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/notify_toast.wasm
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
# The toast plugin, loaded before the feed section: its chooser shows
# the toasts and asks this plugin to draw the conversation.
cp "$TOAST" "$DEPLOY/notify_toast.wasm" 2>/dev/null
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
	$TMUX plugin-log notify_toast 2>&1 | tail -5 >&2
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
# A reaction named $1 reached the fake (reactions.add).
reacted() {
	fetch "$BASE/_reactions" | python3 -c 'import json, sys
sys.exit(0 if any(r["name"] == sys.argv[1] for r in json.load(sys.stdin)) else 1)' "$1"
}
# What the tty client's terminal renders, overlays (a menu) included.
seen() { $OUTER capture-pane -p -t "$VIEW" | sed '/^ *$/d'; }
seen_has() { seen | grep -q -- "$1"; }
seen_lacks() { ! seen_has "$1"; }
VIEW=
marks_grew() { [ "$(ncalls conversations.mark channel C1)" -gt "$1" ]; }
# The row under the cursor (reverse video), its attributes stripped.
# The engine marks the cursor row with ▸ in its first column (reverse
# video would not do: a mention badge is drawn inverted too).
cursor_row() { $TMUX capture-pane -M -p -t "$FORM" | grep '^▸' | head -1; }
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
    -c net -c service-call -c service-serve "$DEPLOY/slack.wasm" || fail "load-plugin"
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
# One mention in #general, one in the thread we follow.
[ "$($TMUX show-options -sv @slack_mentions)" = 2 ] || fail "@slack_mentions: $($TMUX show-options -sv @slack_mentions)"
[ "$($TMUX show-options -sv @slack_threads)" = 1 ] || fail "@slack_threads: $($TMUX show-options -sv @slack_threads)"
echo "$U" | grep -qx '#general ↳ planning the sprint (2 new, mention)' || fail "unread lacks the thread line: $U"
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
screen | grep -q '▪ Unread  3' || fail "no Unread section of 3 rows (the DM, #general, the thread): $(screen | grep Unread)"
TEJ=$(screen | grep -n '● @Tejas' | head -1 | cut -d: -f1)
GEN=$(screen | grep -n '! #general' | head -1 | cut -d: -f1)
[ "$TEJ" -lt "$GEN" ] || fail "the DM ($TEJ) is not before the channel ($GEN) in Unread"
[ "$(screen | grep -c '#general')" = 3 ] || fail "#general is not in Unread (its row and its thread's) and in Work: $(screen | grep -c '#general')"
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
# cursor does not follow it down: it stays where it was, on the row that
# takes the place (the unread thread's).
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
GENERAL_AT=$(screen | grep -n '· #general' | head -1 | cut -d: -f1)
[ "$RANDOM_AT" -lt "$GENERAL_AT" ] || fail "#general ($GENERAL_AT) did not drop below #random ($RANDOM_AT)"
# The row that takes its place in Unread is the thread's.
cursor_row | grep -q '↳ #general' || fail "the cursor followed the marked row: $(cursor_row)"
# The row came under the cursor without a key: its preview is fetched
# all the same (it used to say loading… until the next j/k).
until_ok 40 screen_has 'can you review the plan' || fail "the thread row that took the cursor's place was not fetched: $(screen | grep -c 'loading')"
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

# --- a thread with unread replies -------------------------------------------
# The thread we follow (Slack's Threads view) is a row of the Unread
# section: a red badge because a reply mentions us, ↳, the channel, the
# root's first words and how many replies are new. Its preview is the
# thread itself, the new-messages rule before the unread replies; @
# hands the keyboard to the preview with the cursor on the mention,
# which carries a red @ in its gutter; Escape goes back to the list (the
# row is the thread); m marks the thread read on Slack at its last reply
# and the row goes, the badges with it.
screen | grep -q '! ↳ #general  planning the sprint  +2' || fail "no thread row in Unread: $(screen | grep '↳')"
keys g; keys g; keys j
cursor_row | grep -q '↳ #general' || fail "gg j is not on the thread row: $(cursor_row)"
until_ok 40 screen_has 'can you review the plan' || fail "the thread preview did not show the unread reply"
screen_has '· thread' || fail "the preview header does not say thread: $(screen | head -1)"
screen_has 'planning the sprint' || fail "the thread preview lacks its root"
screen_has '── new messages ──' || fail "no new-messages rule before the unread replies"
NEW=$(screen | grep -n 'new messages' | head -1 | cut -d: -f1)
REV=$(screen | grep -n 'can you review' | head -1 | cut -d: -f1)
[ "$NEW" -lt "$REV" ] || fail "the rule ($NEW) is not before the unread reply ($REV)"
replies_fetched() { [ "$(ncalls conversations.replies ts 1700000022.000100)" -ge 1 ]; }
thread_marked() { [ "$(ncalls subscriptions.thread.mark thread_ts 1700000022.000100)" -ge 1 ]; }
until_ok 40 replies_fetched || fail "the thread row did not fetch the whole thread"
sep_ok || fail "the thread preview crosses the separator: $(sep_ok)"
keys @
until_ok 20 sep_lit || fail "@ did not hand the keyboard to the preview: $(screen | tail -1)"
screen | grep '▎' | grep -q 'can you review' || fail "the cursor bar is not on the mention: $(screen | grep '▎')"
screen | grep -q '@ @zack can you review the plan' || fail "the mention has no @ mark in its gutter: $(screen | grep 'review')"
screen_has 'Esc back to list' || fail "a thread row's footer does not say back to list: $(screen | tail -1)"
keys Escape
sep_lit && fail "Escape did not hand the keyboard back to the list"
screen_has '· thread' || fail "Escape dropped the thread preview"
keys m
until_ok 40 thread_marked || fail "m did not mark the thread read on Slack"
[ "$(ncalls subscriptions.thread.mark ts 1700000320.000100)" -ge 1 ] || fail "the thread mark is not at the last reply: $(fetch "$BASE/_calls" | tr ',' '\n' | grep -A3 thread.mark | head -8)"
until_ok 40 screen_lacks '↳ #general' || fail "the thread row stayed after m: $(screen | grep '↳')"
until_ok 40 sh -c "[ \"\$($TMUX show-options -sv @slack_mentions)\" = 0 ]" || fail "@slack_mentions after reading the thread: $($TMUX show-options -sv @slack_mentions)"
[ "$($TMUX show-options -sv @slack_threads)" = 0 ] || fail "@slack_threads after reading the thread: $($TMUX show-options -sv @slack_threads)"
U=$(slack unread)
echo "$U" | grep -q '↳' && fail "unread still lists the thread: $U"
# --- m in a channel's own section ---------------------------------------
# #general unread again (the fake resets it; the badge poll brings it
# back). m on its row in Work, not in Unread: its Unread copy goes, so
# every row below shifts up, and read now it sorts below #random. The
# cursor is left on the row that took its place, #random, not one row
# further down (where its old position points now).
fetch "$BASE/_unread?channel=C1" >/dev/null
until_ok 60 sh -c "$TMUX capture-pane -M -p -t $FORM | grep -q '! #general'" || fail "#general did not come back unread: $(screen | grep general)"
keys G
i=0
while [ $i -lt 12 ] && ! cursor_row | grep -q '#general'; do keys k; i=$((i + 1)); done
cursor_row | grep -q '! #general' || fail "k from the bottom did not reach Work's #general: $(cursor_row)"
BEFORE_M=$(ncalls conversations.mark channel C1)
keys m
until_ok 40 marks_grew "$BEFORE_M" || fail "m in Work did not mark #general read"
until_ok 40 sh -c "$TMUX capture-pane -M -p -t $FORM | grep -q '· #general'" || fail "#general kept its badge (Work): $(screen | grep general)"
cursor_row | grep -q '#random' || fail "the cursor is not on the row that took #general's place: $(cursor_row)"
# --- p: pinned on top ---------------------------------------------------------
# p on #random (in Work) pins it: a Pinned section appears above Unread
# with a copy of its row, the status says so and the store keeps the
# pin; the cursor stays on #random, so p again unpins it and the
# section goes.
keys p
until_ok 20 screen_has '▪ Pinned  1' || fail "p drew no Pinned section: $(screen | head -3 | tr '\n' '~')"
screen_has 'pinned #random (p again: unpin)' || fail "p said nothing: $(screen | tail -1)"
# Two rows (the preview's header and the status name it too).
[ "$(screen | grep -c '[·●!] #random')" = 2 ] || fail "#random is not in Pinned and in Work: $(screen | grep '#random' | tr '\n' '~')"
PIN_AT=$(screen | grep -n '▪ Pinned' | head -1 | cut -d: -f1)
UNR_AT=$(screen | grep -n '▪ Unread' | head -1 | cut -d: -f1)
[ "$PIN_AT" -lt "$UNR_AT" ] || fail "Pinned ($PIN_AT) is not above Unread ($UNR_AT)"
STORE=$(find "$HOME/.local/share" -name store.db -path '*/plugins/slack/*' 2>/dev/null | head -1)
[ -n "$STORE" ] || fail "no slack store.db under $HOME"
until_ok 20 sh -c "sqlite3 '$STORE' \"select value from settings where key = 'pins'\" | grep -q '\"channel\":\"C2\"'" || fail "the pin is not in the store: $(sqlite3 "$STORE" "select value from settings where key = 'pins'")"
cursor_row | grep -q '#random' || fail "p moved the cursor: $(cursor_row)"
keys p
until_ok 20 screen_lacks '▪ Pinned' || fail "p again did not unpin: $(screen | grep Pinned)"
screen_has 'unpinned #random' || fail "unpin said nothing: $(screen | tail -1)"
until_ok 20 sh -c "sqlite3 '$STORE' \"select value from settings where key = 'pins'\" | grep -qx '\[\]'" || fail "the unpin did not reach the store: $(sqlite3 "$STORE" "select value from settings where key = 'pins'")"
# --- w: a quiet channel -----------------------------------------------------
# Every channel message toasts by default; w on #random quiets it: the
# row says `quiet` before its age, the status says so, and a plain
# message there is not toasted later (the feed section below).
keys w
until_ok 20 sh -c "$TMUX capture-pane -M -p -t $FORM | grep '#random' | grep -q 'quiet '" || fail "w did not mark #random quiet: $(screen | grep random)"
screen_has '#random is quiet' || fail "w said nothing: $(screen | tail -2)"
# Down to #general, whose messages the sidebar already holds.
keys j
cursor_row | grep -q '#general' || fail "j from #random is not on #general: $(cursor_row)"
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
# sidebar with the previewed DM's message; Escape puts it away. The
# conversations read so far (l, Enter in the preview) sit in a Recent
# section on top now, newest first: gg lands there, j walks on to the
# DM in Unread.
screen_has '▪ Recent  2' || fail "no Recent section of the thread and the channel read: $(screen | grep Recent)"
REC_AT=$(screen | grep -n '▪ Recent' | head -1 | cut -d: -f1)
UNR_AT=$(screen | grep -n '▪ Unread' | head -1 | cut -d: -f1)
[ "$REC_AT" -lt "$UNR_AT" ] || fail "Recent ($REC_AT) is not above Unread ($UNR_AT)"
keys g; keys g
cursor_row | grep -q '· #general' || fail "gg is not on Recent's first row, the channel read last (l after Escape): $(cursor_row)"
keys j
cursor_row | grep -q '↳ #general' || fail "Recent's second row is not the thread read before it: $(cursor_row)"
i=0
while [ $i -lt 6 ] && ! cursor_row | grep -q '@Tejas'; do keys j; i=$((i + 1)); done
cursor_row | grep -q '@Tejas' || fail "j did not reach the DM: $(cursor_row)"
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
# @ puts the cursor on the message that mentions us and says so; the
# message carries a red @ in its gutter. G goes back to the bottom.
keys @
until_ok 20 screen_has 'your mention' || fail "@ did not find the mention: $(screen | tail -1)"
screen | grep '▎' | grep -q 'look at this' || fail "the cursor bar is not on the mention: $(screen | grep '▎')"
screen | grep -q '@ hey @zack look at this' || fail "the mention has no @ mark in its gutter: $(screen | grep 'look at this')"
screen | grep -c '^.\{0,3\}@ ' | grep -qx 1 || fail "more than one message carries the @ mark: $(screen | grep '@ ')"
keys G
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
# The reaction picker: e opens a search line and a grid under the
# messages - the recent row (Slack's quick reactions before any was
# sent), then every category under its header. Typing narrows the grid
# and the highlighted name shows at the right; Enter reacts with it and
# closes the picker. A name typed in full goes as typed (the skin tone
# included). The names sent head the recent row next time and are kept
# in the store.
keys e
until_ok 20 screen_has 'react :' || fail "e opened no reaction picker: $(screen | tail -4)"
screen_has ' recent' || fail "the picker has no recent row"
screen_has 'smileys & emotion' || fail "the picker has no category header: $(screen | tail -12)"
screen_has '👍' || fail "the recent row shows no 👍: $(screen | grep -A1 recent)"
# C-j/C-l/C-k/C-h move like the arrows (plain hjkl type into the search).
keys C-j C-l
until_ok 20 screen_has ':smiley:' || fail "C-j C-l did not land on :smiley:: $(screen | grep 'react :')"
keys C-k C-h
until_ok 20 screen_has ':+1:' || fail "C-k C-h did not come back to :+1:: $(screen | grep 'react :')"
keys f i r
until_ok 20 screen_has ':fire:' || fail "typing fir did not highlight :fire:: $(screen | grep 'react :')"
screen_lacks ' recent' || fail "the headers stayed while searching"
keys Enter
until_ok 40 reacted fire || fail "no fire reaction reached the fake: $(fetch "$BASE/_reactions")"
until_ok 20 screen_has ':fire: added' || fail "no ':fire: added' status: $(screen | tail -2)"
screen_lacks 'react :' || fail "the picker stayed open after Enter"
keys e
keys -l '+1::skin-tone-3'
until_ok 20 screen_has ':+1::skin-tone-3:' || fail "the typed tone is not the pick: $(screen | grep 'react :')"
keys Enter
until_ok 40 reacted '+1::skin-tone-3' || fail "the precise name did not go as typed: $(fetch "$BASE/_reactions")"
until_ok 20 sh -c "sqlite3 '$STORE' \"select value from settings where key = 'recent_reactions'\" | grep -q '\"+1\",\"fire\"'" || fail "the recent list is not in the store: $(sqlite3 "$STORE" "select value from settings where key = 'recent_reactions'")"
keys e
until_ok 20 screen_has 'react :' || fail "e did not reopen the picker"
screen | grep -A1 ' recent' | tail -1 | grep -q '^ *👍 *🔥' || fail "the recent row does not start 👍 🔥: $(screen | grep -A1 recent)"
keys Escape
screen_lacks 'react :' || fail "Escape did not close the picker"
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
# z on a message with a picture opens it big: a float the size of the
# window (200x50), the file itself (url_private, F1.gif) fetched once
# and converted under its own name, drawn centred as placeholder rows
# under an id of its own (400x50 at cell aspect 2: as wide as the float,
# a row per 16 cells), the name and pixel size on the last row. On a message
# without one, z says so. q closes the viewer; the view is still there.
keys z
screen_has 'no picture in this message' || fail "z on a text message did not say so: $(screen | tail -2)"
$TMUX list-panes -a -F '#{pane_id} #{pane_mode}' | awk '/plugin-mode/ { print $1 }' >"$HOME/floats"
i=0
while :; do
	keys k
	keys z
	VIEW=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' | awk '/plugin-mode/ { print $1 }' |
	    grep -vxF -f "$HOME/floats" | head -1)
	[ -n "$VIEW" ] && break
	i=$((i + 1))
	[ "$i" -lt 15 ] || fail "z never found the picture message"
done
viewer() { $TMUX capture-pane -M -p -t "$VIEW"; }
# The window's size less the border; the block's rows follow from the
# width (16 cells of width per row of height, rounded).
WANT=$($TMUX display-message -p -t "$VIEW" '#{window_width}x#{window_height}' | awk -Fx '{ printf "%dx%d", $1 - 2, $2 - 2 }')
GOT=$($TMUX display-message -p -t "$VIEW" '#{pane_width}x#{pane_height}')
[ "$GOT" = "$WANT" ] || fail "the viewer is $GOT, not the window's $WANT"
ROWS=$(echo "$GOT" | awk -Fx '{ printf "%d", $1 / 16 + 0.5 }')
until_ok 80 sh -c "[ \"\$($TMUX capture-pane -M -p -t $VIEW | grep -c '$PH')\" = $ROWS ]" ||
    fail "viewer rows: $(viewer | grep -c "$PH"), not $ROWS ($($TMUX display-message -p -t "$VIEW" 'cell #{window_cell_width}x#{window_cell_height} pane #{pane_width}x#{pane_height} win #{window_width}x#{window_height}'); first row $(viewer | grep "$PH" | head -1 | grep -o "$PH" | wc -l | tr -d ' ') cells; lines $(viewer | grep -n "$PH" | cut -d: -f1 | tr '\n' ','); ids $($TMUX capture-pane -M -p -e -t "$VIEW" | grep "$PH" | grep -o "$(printf '\033')\[38;2;83;[0-9]*;[0-9]*m" | sort | uniq -c | tr -s ' \n' ' '); timeline rows $(screen | grep -c "$PH")): $(viewer | tail -1)"
viewer | tail -1 | grep -q 'Nervous K and P.gif · 400×50' || fail "the viewer caption is off: $(viewer | tail -1)"
viewer | tail -1 | grep -q 'q/Esc/z close' || fail "the viewer foot has no hint: $(viewer | tail -1)"
[ "$(ncalls _img path /_img/F1.gif)" = 1 ] || fail "the file itself was not fetched once: $(ncalls _img path /_img/F1.gif)"
find "$HOME" -name 'F1.full.png' | grep -q . || fail "no F1.full.png under $HOME"
VID=$($TMUX capture-pane -M -p -e -t "$VIEW" | grep "$PH" | grep -o "$(printf '\033')\[38;2;83;[0-9]*;[0-9]*m" | sort -u)
[ "$(echo "$VID" | wc -l | tr -d ' ')" = 1 ] || fail "the viewer's rows carry more than one id"
$TMUX capture-pane -M -p -e -t "$FORM" | grep -q "$VID" && fail "the viewer reused a timeline picture's id"
$TMUX send-keys -t "$VIEW" q; sleep 0.6
$TMUX list-panes -a -F '#{pane_id}' | grep -qx "$VIEW" && fail "q did not close the viewer"
[ "$(screen | grep -c "$PH")" = 10 ] || fail "the timeline lost its pictures behind the viewer"
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
# @ in the box opens a dropdown of people: @te offers tejas (handle and
# name); Enter takes the handle into the draft; what is sent carries
# Slack's <@U2> form. Escape hides the dropdown and keeps insert mode.
$TMUX send-keys -t "$FORM" -l '@te'; sleep 0.4
screen | grep -q '^[^@]*@tejas  Tejas\( \{4\}\|$\)' || fail "no dropdown for @te: $(screen | tail -6 | tr '\n' '~')"
screen_has 'Enter takes' || fail "the foot does not carry the dropdown hint: $(screen | tail -1)"
keys Enter
screen | grep -q '› @tejas ' || fail "Enter did not take the handle: $(screen | tail -4 | tr '\n' '~')"
screen | grep -q '^[^@]*@tejas  Tejas\( \{4\}\|$\)' && fail "the dropdown stayed after Enter"
$TMUX send-keys -t "$FORM" -l 'see this'; keys Enter
until_ok 40 posted C1 '<@U2> see this' || fail "the mention was not sent as <@U2>: $(fetch "$BASE/_posted")"
$TMUX send-keys -t "$FORM" -l '@z'; sleep 0.4
screen | grep -q '^[^@]*@zack\( \{6\}\|$\)' || fail "no dropdown for @z: $(screen | tail -6 | tr '\n' '~')"
keys Escape
until_ok 40 sh -c "! $TMUX capture-pane -M -p -t $FORM | grep -q '^[^@]*@zack\( \{6\}\|$\)'" || fail "Escape did not hide the dropdown: $(screen | tail -6 | tr '\n' '~')"
screen_has -- '-- INSERT --' || fail "Escape with the dropdown open left insert mode: $(screen | tail -3)"
keys BSpace; keys BSpace
screen | grep -q '› @' && fail "the @z token is still in the box: $(screen | tail -3 | tr '\n' '~')"
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
# E on our own message (the reply just posted, under the cursor at the
# bottom) opens the box on its text; Enter saves it through chat.update
# and the thread shows the new text.
screen | grep '▎' | grep -q 'in the thread' || fail "the cursor is not on our reply: $(screen | grep '▎')"
keys E
until_ok 20 screen_has 'edit › in the thread' || fail "E did not open the box on the message's text: $(screen | tail -3)"
$TMUX send-keys -t "$FORM" -l ' (edited)'; sleep 0.3
keys Enter
updated() { [ "$(ncalls chat.update channel C1)" -ge 1 ]; }
until_ok 40 updated || fail "Enter did not reach chat.update"
until_ok 40 screen_has 'in the thread (edited)' || fail "the edit is not shown: $(screen | grep 'in the thread')"
screen | grep -q 'edit › ' && fail "the edit box stayed open after Enter"
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
# p in a thread pins the thread (the sidebar shows it below).
keys p
until_ok 20 screen_has 'pinned the thread in #general (p again: unpin)' || fail "p in the thread said nothing: $(screen | tail -1)"
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
# --- the history: C-o reopens what was opened before -----------------------
# The sidebar opens on the conversation opened last (the thread, in the
# preview). C-o opens it in the channel view; C-o again the one before
# it (the channel's timeline); Tab comes forward to the thread again.
slack pick
until_ok 40 mode_open || fail "pick did not reopen the sidebar"
FORM=$(mode_pane)
until_ok 40 screen_has 'by sections' || fail "the sidebar did not draw"
cursor_row | grep -q '#general' || fail "the sidebar did not open on #general, the conversation opened last: $(cursor_row)"
until_ok 40 screen_has '· thread' || fail "the preview does not show the thread opened last: $(screen | head -1)"
# The thread pinned from the channel view heads the sidebar, named by
# its root; the Recent section has it first too, as the last opened.
screen_has '▪ Pinned  1' || fail "the thread pinned in the view is not in a Pinned section: $(screen | head -4 | tr '\n' '~')"
screen | grep '↳ #general' | head -1 | grep -q 'line one line two' || fail "the pinned thread row is not named by its root: $(screen | grep '↳' | head -1)"
screen_has '▪ Recent' || fail "no Recent section when the sidebar reopens"
until_ok 40 screen_has 'in the thread (edited)' || fail "the preview's thread lacks its reply: $(screen | head -8)"
# Every jump closes the view and opens another: a new mode pane each time.
view_swapped() { [ -n "$(mode_pane)" ] && [ "$(mode_pane)" != "$OLD" ]; }
OLD=$FORM
keys C-o
until_ok 40 view_swapped || fail "C-o did not swap the sidebar for a view"
FORM=$(mode_pane)
until_ok 40 screen_has '· thread' || fail "C-o did not open the thread opened last: $(screen | head -1)"
until_ok 40 screen_has 'in the thread (edited)' || fail "the reopened thread lacks its reply"
OLD=$FORM
keys C-o
until_ok 40 view_swapped || fail "C-o again did not open another view"
FORM=$(mode_pane)
until_ok 40 screen_has 'the town square' || fail "C-o again did not go back to the timeline: $(screen | head -1)"
screen_has '· thread' && fail "the timeline view says thread: $(screen | head -1)"
OLD=$FORM
keys Tab
until_ok 40 view_swapped || fail "Tab did not open a view"
FORM=$(mode_pane)
until_ok 40 screen_has '· thread' || fail "Tab did not come forward to the thread: $(screen | head -1)"
keys Tab
until_ok 20 screen_has 'at the newest conversation opened' || fail "Tab at the newest entry said nothing: $(screen | tail -2)"
keys q
until_ok 40 mode_closed || fail "q did not close the reopened view"
FORM=
$OUTER kill-server 2>/dev/null
VIEW=

# --- send, mark, bind ---------------------------------------------------------
# The toast plugin from here on (rows never expire): the feed section's
# toasts become its rows, checked in the chooser below.
[ -f "$DEPLOY/notify_toast.wasm" ] || fail "notify_toast.wasm not built"
$TMUX load-plugin -s server -o duration_ms=0 -o chooser_width=140 -o chooser_height=40 -c run-command -c mode -c service-call -c service-serve \
    "$DEPLOY/notify_toast.wasm" || fail "load-plugin notify_toast"
R=$(slack 'send @Tejas see you at 3')
[ "$R" = "posting to @Tejas…" ] || fail "send did not answer: $R"
until_ok 40 posted D1 'see you at 3' || fail "the DM did not reach the fake: $(fetch "$BASE/_posted")"
# The DM we just sent is read: no unread DM. A message from Tejas in it,
# injected down the feed's road (the fake has no websocket), is told as
# a toast titled with the sender (the plugin logs the toast, and the
# toast plugin holds the row) and bumps @slack_dms at once; the fake is told
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
# The channel message was toasted on arrival (every channel message
# is, by default); Slack's notification for it is the repeat.
until_ok 40 sh -c "$TMUX plugin-log slack | grep -q 'slack: toast: #general · Tejas: a channel message'" || fail "the channel message was not toasted: $($TMUX plugin-log slack | grep 'toast:' | tail -3)"
sleep 0.5
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

# A message in a quiet channel (w on #random in the sidebar above) is
# not toasted: w is the exception to "every message".
R=$(slack 'rtm-inject {"type":"message","channel":"C2","user":"U2","text":"quiet words","ts":"1700000904.000100"}')
[ "$R" = "injected" ] || fail "rtm-inject (quiet) did not answer: $R"
sleep 1
$TMUX plugin-log slack | grep -q 'quiet words' && fail "the quiet channel's message was toasted: $($TMUX plugin-log slack | grep 'toast:' | tail -3)"
[ "$(toasts)" = "$((T0 + 3))" ] || fail "$(( $(toasts) - T0 )) toasts, not 3 (the quiet channel none): $($TMUX plugin-log slack | grep 'toast:')"
R=$(slack 'rtm-inject nonsense')
echo "$R" | grep -q '^rtm-inject:' || fail "rtm-inject with bad json did not complain: $R"

# --- the notifications pane ---------------------------------------------------
# The toasts are rows of the toast plugin's chooser: the newest first
# (the mention took the channel message's row, same key, and came last),
# each with its age at the right edge, and the selected row's column is
# the slack plugin's picture of the conversation (its `preview` service):
# the messages as the sidebar previews them, the unread bar included.
# One more message in #general, dated after the suite's own posts (the
# view read the channel at those): the unread bar has something to mark.
R=$(slack 'rtm-inject {"type":"message","channel":"C1","user":"U2","text":"fresh after the mark","ts":"2000000000.000100"}')
[ "$R" = "injected" ] || fail "rtm-inject (fresh) did not answer: $R"
until_ok 40 sh -c "$TMUX plugin-log slack | grep -q 'slack: toast: #general · Tejas: fresh after the mark'" || fail "the fresh message was not toasted"
$TMUX plugin-command -t "$PANE" notify_toast chooser || fail "chooser did not open"
until_ok 40 mode_open || fail "the chooser opened no plugin mode"
FORM=$(mode_pane)
until_ok 40 screen_has 'slack: 4 notifications' || fail "the chooser lacks the slack group (two rows, four pings): $(screen | head -3)"
screen | grep -q '^(1).*#general · Tejas: fresh after the mark ×3' || fail "the newest row is not first: $(screen | head -3 | tr '\n' '~')"
screen | grep -q '^(2).*Tejas: ping from the' || fail "the DM row is not second: $(screen | head -3 | tr '\n' '~')"
screen | grep -Eq '^\(1\).* (now|[0-9]+s) *│' || fail "the first row shows no age: $(screen | head -2 | tr '\n' '~')"
# The cursor opens on the group row; the column follows the cursor.
keys j
until_ok 60 screen_has 'a channel message' || fail "the column did not draw #general: $(screen | head -8 | tr '\n' '~')"
screen_has 'can you look?' || fail "the column lacks the mention: $(screen | head -8 | tr '\n' '~')"
screen_has '── new messages ──' || fail "no read mark in the column: $(screen | tail -8 | tr '\n' '~')"
screen | grep -q '▏  fresh after the mark' || fail "the unread message is not barred: $(screen | tail -8 | tr '\n' '~')"
screen | grep -q '▏.*can you look?' && fail "a message read before is barred: $(screen | tail -8 | tr '\n' '~')"
keys j
until_ok 60 screen_has 'are you around?' || fail "the column did not draw the DM: $(screen | head -8 | tr '\n' '~')"
# d on the #general row drops it and marks #general read on Slack (the
# row's plugin hears `dismissed` with its key); the DM row moves up.
keys k
BEFORE_M=$(ncalls conversations.mark channel C1)
keys d
until_ok 40 marks_grew "$BEFORE_M" || fail "d did not mark #general read on Slack"
until_ok 40 sh -c "$TMUX capture-pane -M -p -t $FORM | grep -q '^(1).*Tejas: ping from the'" || fail "the dismissed row is still there: $(screen | head -3 | tr '\n' '~')"
screen | grep -q 'fresh after the mark ×3' && fail "the #general row survived d: $(screen | head -3 | tr '\n' '~')"
keys q
until_ok 40 mode_closed || fail "q did not close the chooser"
FORM=
BEFORE=$(ncalls conversations.mark channel C1)
slack 'mark #general'
until_ok 40 marks_grew "$BEFORE" || fail "mark did not reach conversations.mark (had $BEFORE)"
# The other way round: a conversation read elsewhere (the app, the
# phone) comes down the feed as channel_marked, and the plugin takes
# the channel's row back from the toast plugin (`withdraw`), so the
# pane stops telling of a message already read; the DM's row stays.
# (The mark above withdrew too, with nothing left to remove: d had
# dropped the row.)
$TMUX plugin-log slack | grep -q 'slack: withdraw slack:C1' || fail "our own mark did not withdraw the channel's row"
R=$(slack 'rtm-inject {"type":"message","channel":"C1","user":"U2","text":"read on the phone","ts":"2000000001.000100"}')
[ "$R" = "injected" ] || fail "rtm-inject (phone) did not answer: $R"
until_ok 40 sh -c "$TMUX plugin-log slack | grep -q 'slack: toast: #general · Tejas: read on the phone'" || fail "the message was not toasted"
until_ok 40 sh -c "[ \"\$($TMUX plugin-log notify_toast | grep -c 'notification from slack: added to feed')\" -ge 5 ]" || fail "the toast plugin did not take the row: $($TMUX plugin-log notify_toast | tail -3)"
R=$(slack 'rtm-inject {"type":"channel_marked","channel":"C1","ts":"2000000001.000100","unread_count_display":0,"mention_count_display":0}')
[ "$R" = "injected" ] || fail "rtm-inject (channel_marked) did not answer: $R"
until_ok 40 sh -c "$TMUX plugin-log notify_toast | grep -q 'withdraw slack:C1 from slack: removed 1'" || fail "the read elsewhere did not withdraw the row: $($TMUX plugin-log notify_toast | tail -4)"
$TMUX plugin-command -t "$PANE" notify_toast chooser || fail "chooser did not reopen"
until_ok 40 mode_open || fail "the chooser opened no plugin mode (after withdraw)"
FORM=$(mode_pane)
until_ok 40 screen_has 'ping from the' || fail "the DM row went with the withdrawn one: $(screen | head -4 | tr '\n' '~')"
screen_has 'read on the phone' && fail "the withdrawn row is still in the chooser: $(screen | head -4 | tr '\n' '~')"
keys q
until_ok 40 mode_closed || fail "q did not close the chooser (after withdraw)"
FORM=
R=$(slack 'bind #general claude:abc')
[ "$R" = "bound #general" ] || fail "bind did not answer: $R"
# The bridge: one push per bound agent however many bindings match (the
# channel's and the thread's both match a reply), and not for the user's
# own words unless @slack-bridge-me is on. No mailbox plugin runs here, so
# every push attempt is one "bridge to" line in the log.
R=$(slack 'bind #general/1700000300.000100 claude:abc')
[ "$R" = "bound #general/1700000300.000100" ] || fail "bind thread did not answer: $R"
bridged() { $TMUX plugin-log slack | grep -c 'bridge to claude:abc'; }
B0=$(bridged)
slack 'rtm-inject {"type":"message","channel":"C1","user":"U2","text":"for the agent","ts":"2000000001.000100","thread_ts":"1700000300.000100"}' >/dev/null
until_ok 40 sh -c "[ \$($TMUX plugin-log slack | grep -c 'bridge to claude:abc') -gt $B0 ]" || fail "a reply in a bound thread was not bridged"
sleep 1
[ "$(bridged)" = "$((B0 + 1))" ] || fail "the reply was bridged $(( $(bridged) - B0 )) times, not once: $($TMUX plugin-log slack | grep 'bridge to' | tail -3 | tr '\n' '~')"
slack 'rtm-inject {"type":"message","channel":"C1","user":"U1","text":"my own words","ts":"2000000002.000100","thread_ts":"1700000300.000100"}' >/dev/null
sleep 1.5
[ "$(bridged)" = "$((B0 + 1))" ] || fail "the user's own reply was bridged"
$TMUX set-option -s @slack-bridge-me 1
slack 'rtm-inject {"type":"message","channel":"C1","user":"U1","text":"my own words again","ts":"2000000003.000100","thread_ts":"1700000300.000100"}' >/dev/null
until_ok 40 sh -c "[ \$($TMUX plugin-log slack | grep -c 'bridge to claude:abc') -eq $((B0 + 2)) ]" || fail "@slack-bridge-me did not bridge the user's own reply: $(bridged)"
$TMUX set-option -su @slack-bridge-me
R=$(slack 'unbind #general/1700000300.000100')
[ "$R" = "unbound #general/1700000300.000100" ] || fail "unbind thread did not answer: $R"
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
