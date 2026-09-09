#!/bin/sh
# Unread waiting agents. A live agent that enters `waiting` is unread until
# the user gets to it. Acknowledging happens two ways: the picker cursor
# lands on the row (real navigation, not the auto-selection at open), or the
# user jumps to the pane. Unread rows sort to the top of the waiting band
# and count in the header. Asserted against the store db.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-unread-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm

HOME=$(mktemp -d); export HOME
XDG_DATA_HOME="$HOME/.local/share"; export XDG_DATA_HOME
DEPLOY=$(mktemp -d); cp "$WASM" "$DEPLOY/agents.wasm"
cat >"$DEPLOY/agents.toml" <<'TOML'
[caps]
requests = ["capture-pane","run-command","mode","db","env-read","pane-fds"]
[caps.env-read]
names = ["AI_AGENT","OPENCODE"]
TOML

# A small DB reader: prints the first scalar of an arbitrary query.
DUMP="$DEPLOY/dump.py"
cat >"$DUMP" <<'PY'
import sqlite3, sys, glob
dbs = glob.glob(sys.argv[1] + "/**/store.db", recursive=True)
if not dbs:
    print(""); sys.exit()
c = sqlite3.connect(dbs[0])
rows = list(c.execute(sys.argv[2]))
print("" if not rows or rows[0][0] is None else rows[0][0])
PY
UNREAD_SQL="SELECT COUNT(*) FROM agents WHERE status='waiting' \
AND waiting_ms IS NOT NULL AND (acked_ms IS NULL OR acked_ms < waiting_ms)"

trap 'kill $CTL 2>/dev/null; $TMUX kill-server 2>/dev/null; rm -rf "$HOME" "$DEPLOY"' EXIT
cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$HOME" "$DEPLOY"
}
fail() { echo "FAIL: $*" >&2; echo "--- screen:" >&2; screen >&2; cleanup; exit 1; }
q() { python3 "$DUMP" "$XDG_DATA_HOME" "$1"; }
screen() { $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }
open_picker() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	( sleep 0.3; echo 'plugin-command agents pick'; sleep 60 ) |
	    $TMUX -C attach >/dev/null 2>&1 &
	CTL=$!
	sleep 1.5
	FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
	    awk '/plugin-mode/ { print $1 }')
	[ -n "$FORM" ] || fail "picker did not open"
}
# Poll the store until the unread count reaches N.
poll_unread() {
	i=0
	while [ "$i" -lt 12 ]; do
		[ "$(q "$UNREAD_SQL")" = "$1" ] && return 0
		sleep 1; i=$((i + 1))
	done
	fail "unread count never reached $1 (got $(q "$UNREAD_SQL"))"
}

[ -f "$WASM" ] || fail "agents.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5
# Two agent panes in one window.
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 \
    "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-session"
$TMUX split-window -t alpha \
    "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "split-window"
sleep 0.5
P1=$($TMUX list-panes -t alpha -F '#{pane_id}' | sed -n 1p)
P2=$($TMUX list-panes -t alpha -F '#{pane_id}' | sed -n 2p)

$TMUX load-plugin -s server -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds "$DEPLOY/agents.wasm" \
    || fail "load-plugin"
sleep 1.5

# Drive each agent working -> waiting. The transition into waiting stamps
# waiting_ms; with no ack yet, both are unread.
for P in "$P1" "$P2"; do
	$TMUX plugin-command -t "$P" agents "working"
	sleep 0.3
	$TMUX plugin-command -t "$P" agents "waiting"
	sleep 0.3
done
poll_unread 2

# Opening the picker must NOT acknowledge (only real navigation does).
open_picker
screen | grep -q '2 unread' || fail "header does not show 2 unread"
screen | grep -q '◉' || fail "no unread badge on screen"
[ "$(q "$UNREAD_SQL")" = "2" ] || fail "opening the picker acked a row"

# Navigate down onto the second row: that acknowledges it, not the first.
$TMUX send-keys -t "$FORM" j; sleep 0.6
i=0
while [ "$i" -lt 10 ]; do
	[ "$(q "$UNREAD_SQL")" = "1" ] && break
	sleep 1; i=$((i + 1))
done
[ "$(q "$UNREAD_SQL")" = "1" ] ||
    fail "navigating onto a row did not ack exactly one (got $(q "$UNREAD_SQL"))"
screen | grep -q '1 unread' || fail "header did not drop to 1 unread"

# Jump (Enter) to the remaining unread agent's pane: that acks it too.
$TMUX send-keys -t "$FORM" k; sleep 0.4
$TMUX send-keys -t "$FORM" Enter; sleep 0.8
i=0
while [ "$i" -lt 10 ]; do
	[ "$(q "$UNREAD_SQL")" = "0" ] && break
	sleep 1; i=$((i + 1))
done
[ "$(q "$UNREAD_SQL")" = "0" ] ||
    fail "jump/select did not clear the last unread (got $(q "$UNREAD_SQL"))"

cleanup
exit 0
