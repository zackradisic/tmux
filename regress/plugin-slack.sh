#!/bin/sh
# The slack plugin against a fake Slack (plugin-slack-fake.py, python3's
# standard library): no real workspace, no Keychain. The plugin is loaded
# with `-o api_base=http://127.0.0.1:PORT -o token=... -o cookie=...`, so
# every web API call lands on the fake and the identity comes from
# `auth.test` alone. Check:
#
#   boot: `status` names the user and team and counts the 3 channels;
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
#     unread view (no read channel in it); `q` closes;
#   `open #general`: the channel view with the topic, the history with
#     mrkdwn resolved (<@U1> -> @zack, *this* -> this) and the
#     "new messages" rule before the first unread message that is not
#     ours; `i`, typed text, Enter posts to the fake and the view shows
#     it; `q` closes;
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
TMUX="$TEST_TMUX -Lslack-test"
OUTER="$TEST_TMUX -Lslack-test-outer"
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
		echo "--- what the client sees:" >&2
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
# How many messages with text $2 were posted to channel $1.
nposted() {
	fetch "$BASE/_posted" | python3 -c 'import json, sys
print(sum(1 for p in json.load(sys.stdin) if p["channel"] == sys.argv[1] and p["text"] == sys.argv[2]))' "$1" "$2"
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
mode_closed() { [ -z "$(mode_pane)" ]; }
keys() { $TMUX send-keys -t "$FORM" "$@"; sleep 0.4; }
slack() { $TMUX plugin-command slack "$*"; }
status_ready() { slack status | grep -q 'zack on Morph'; }
posted() { [ "$(nposted "$1" "$2")" -ge 1 ]; }
marks_grew() { [ "$(ncalls conversations.mark channel C1)" -gt "$1" ]; }
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
( sleep 150; echo "FAIL: watchdog" >&2; $TMUX kill-server 2>/dev/null; kill $$ 2>/dev/null ) &
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
echo "$ST" | grep -q '3 channels' || fail "status does not count 3 channels: $ST"
echo "$ST" | grep -q 'test.slack.com' || fail "status does not name the workspace: $ST"
[ "$(ncalls auth.test)" -ge 1 ] || fail "auth.test was not called"
[ "$(ncalls users.conversations)" -ge 1 ] || fail "users.conversations was not called"
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
$OUTER -f/dev/null new-session -d -x 160 -y 45 "$TEST_TMUX -Lslack-test attach -t main" || fail "outer new-session"
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
keys q
until_ok 40 mode_closed || fail "q did not close the sidebar"
FORM=

# --- the channel view ---------------------------------------------------------
$TMUX plugin-command -t "$PANE" slack 'open #general' || fail "plugin-command open"
until_ok 40 mode_open || fail "open opened no plugin mode"
FORM=$(mode_pane)
until_ok 60 screen_has 'morning' || fail "the channel view never showed the history"
screen_has '#general' || fail "the channel view lacks its title"
screen_has 'the town square' || fail "the channel view lacks the topic"
screen_has '@zack look at this' || fail "mrkdwn was not resolved"
screen_has '── new messages ──' || fail "no new-messages rule"
NEW=$(screen | grep -n 'new messages' | head -1 | cut -d: -f1)
HEY=$(screen | grep -n 'look at this' | head -1 | cut -d: -f1)
FINE=$(screen | grep -n 'fine' | head -1 | cut -d: -f1)
[ "$NEW" -lt "$HEY" ] || fail "the rule ($NEW) is not before the hey message ($HEY)"
[ "$FINE" -lt "$NEW" ] || fail "our own message ($FINE) counts as new (rule at $NEW)"
# Compose: i, the text, Enter.
keys i
$TMUX send-keys -t "$FORM" -l 'hello there'; sleep 0.4
screen_has 'hello there' || fail "the typed text is not in the compose box"
keys Enter
until_ok 40 posted C1 'hello there' || fail "the post did not reach the fake: $(fetch "$BASE/_posted")"
until_ok 40 screen_has 'hello there' || fail "the posted message is not shown"
screen | grep -q 'posting' && fail "the posting status stayed"
keys q
until_ok 40 mode_closed || fail "q did not close the channel view"
FORM=

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
$TMUX show-plugins | grep -q slack || fail "show-plugins lost the plugin"

echo "PASS"
exit 0
