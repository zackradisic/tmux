#!/bin/sh
# Renaming an agent from the picker (r). The user name and the harness
# name compete by recency, with one carve-out: a name set while the agent
# is still nameless beats the harness's FIRST name. Asserted against the
# store db (name_ms vs user_name_ms), polling so the async resolver has
# landed each harness name before the next step.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-rename-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm

HOME=$(mktemp -d); export HOME
XDG_DATA_HOME="$HOME/.local/share"; export XDG_DATA_HOME
mkdir -p "$HOME/.claude/sessions"
DEPLOY=$(mktemp -d); cp "$WASM" "$DEPLOY/agents.wasm"
cat >"$DEPLOY/agents.toml" <<'TOML'
[caps]
requests = ["capture-pane","run-command","mode","db","env-read","pane-fds","fs-read","fs-list"]
[caps.env-read]
names = ["AI_AGENT","OPENCODE"]
[caps.fs-read]
paths = ["~/.claude/sessions"]
TOML

# A small DB reader: prints one column of the single agent row.
DUMP="$DEPLOY/dump.py"
cat >"$DUMP" <<'PY'
import sqlite3, sys, glob
dbs = glob.glob(sys.argv[1] + "/**/store.db", recursive=True)
if not dbs:
    print(""); sys.exit()
c = sqlite3.connect(dbs[0])
rows = list(c.execute("SELECT " + sys.argv[2] + " FROM agents LIMIT 1"))
print("" if not rows or rows[0][0] is None else rows[0][0])
PY

trap 'kill $CTL 2>/dev/null; $TMUX kill-server 2>/dev/null; rm -rf "$HOME" "$DEPLOY"' EXIT
cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$HOME" "$DEPLOY"
}
fail() { echo "FAIL: $*" >&2; cleanup; exit 1; }
col() { python3 "$DUMP" "$XDG_DATA_HOME" "$1"; }
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
write_session() {
	now=$(date +%s)000
	cat >"$HOME/.claude/sessions/$PID.json" <<JSON
{"pid":424242,"sessionId":"25a936a2-cf9b-407d-9e4e-89e04a9636e7","cwd":"$HOME","startedAt":$((now-3600000)),"tmux":"alpha:$WINID.%$NUM","name":"$1","status":"idle","updatedAt":$now}
JSON
}
# Reopen and wait until the resolver has stored the expected harness name.
poll_name() {
	i=0
	while [ "$i" -lt 12 ]; do
		[ "$(col name)" = "$1" ] && return 0
		sleep 1; i=$((i + 1))
	done
	fail "harness name '$1' never resolved (got '$(col name)')"
}

[ -f "$WASM" ] || fail "agents.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 \
    "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-session"
sleep 0.5
WINID=$($TMUX list-panes -t alpha -F '#{window_id}' | head -1)
PANE=$($TMUX list-panes -t alpha -F '#{pane_id}' | head -1); NUM=${PANE#%}
PID=$($TMUX list-panes -t alpha -F '#{pane_pid}' | head -1)

$TMUX load-plugin -s server -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds -c fs-read -c fs-list "$DEPLOY/agents.wasm" \
    || fail "load-plugin"
sleep 1.0

# Nameless agent: rename it via r; it shows and persists.
open_picker
$TMUX send-keys -t "$FORM" r; sleep 0.4
$TMUX send-keys -t "$FORM" USERPICK; sleep 0.4
$TMUX send-keys -t "$FORM" Enter; sleep 0.6
screen | grep -q 'USERPICK' || fail "rename did not take"
[ "$(col user_name)" = "USERPICK" ] || fail "user_name not stored"

# The harness produces its FIRST name. Carve-out: it is stamped older
# than the rename (name_ms = 0), so USERPICK keeps winning.
write_session "harness-first"
open_picker
poll_name "harness-first"
[ "$(col name_ms)" = "0" ] || fail "carve-out: first name not stamped old (name_ms=$(col name_ms))"
screen | grep -q 'USERPICK' || fail "carve-out: USERPICK should still win"
screen | grep -q 'harness-first' && fail "first harness name should be hidden"

# A later, genuinely new harness name wins (most up to date): name_ms
# advances past the rename time.
write_session "harness-second"
open_picker
poll_name "harness-second"
NMS=$(col name_ms); UMS=$(col user_name_ms)
[ "$NMS" -gt "$UMS" ] || fail "most-up-to-date: name_ms ($NMS) should exceed user_name_ms ($UMS)"
screen | grep -q 'harness-second' || fail "a later harness name did not win"

cleanup
exit 0
