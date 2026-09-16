#!/bin/sh
# What a render must not destroy. Enrich-at-render reads each harness's
# session file and folds the result onto the row; twice now it has written
# away something the store already knew better. Check:
#
#   a saved capture does not block the provisional -> durable id move
#     (captures referenced agents(id) with ON DELETE CASCADE only, so the
#     rename failed the foreign key, silently, and the row then took no
#     writes at all - see schema v7);
#   the capture follows its agent to the new id;
#   a shim's `needs_input` survives a session file that can only say idle;
#   but real activity after that report still moves the row on.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-migrate-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm

HOME=$(mktemp -d); export HOME
XDG_DATA_HOME="$HOME/.local/share"; export XDG_DATA_HOME
mkdir -p "$HOME/.claude/sessions"
DEPLOY=$(mktemp -d); cp "$WASM" "$DEPLOY/agents.wasm"
cat >"$DEPLOY/agents.toml" <<'TOML'
[caps]
requests = ["capture-pane","run-command","mode","db","env-read",
            "pane-fds","fs-read","fs-list"]
[caps.env-read]
names = ["AI_AGENT","OPENCODE"]
[caps.fs-read]
paths = ["~/.claude/sessions"]
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

SID=25a936a2-cf9b-407d-9e4e-89e04a9636e7

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
# The session file Claude writes for this pane. $1 = status, $2 = updatedAt.
write_session_file() {
	cat >"$HOME/.claude/sessions/$PID.json" <<EOF
{"pid":424242,"sessionId":"$SID",
 "cwd":"$HOME","startedAt":$STARTED,"tmux":"alpha:$WIN.%$NUM",
 "name":"hyperyaml-91","status":"$1","updatedAt":$2}
EOF
}

[ -f "$WASM" ] || fail "agents.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5

$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 \
    "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-session"
sleep 0.5
WIN=$($TMUX list-panes -t alpha -F '#{window_id}' | head -1)
PANE=$($TMUX list-panes -t alpha -F '#{pane_id}' | head -1)
PID=$($TMUX list-panes -t alpha -F '#{pane_pid}' | head -1)
NUM=${PANE#%}
[ -n "$NUM" ] && [ -n "$PID" ] || fail "no pane id/pid"

now=$(date +%s)000
STARTED=$((now - 3600000))

# No session file yet, so the row enters under its provisional, pane-bound
# id - the state every agent passes through.
$TMUX load-plugin -s server -o trust_env=1 -c capture-pane -c run-command \
    -c mode -c db -c env-read -c pane-fds -c fs-read -c fs-list \
    "$DEPLOY/agents.wasm" || fail "load-plugin"
sleep 1.0
PROV=$(q "SELECT id FROM agents WHERE ended_ms IS NULL")
case "$PROV" in
prov-claude-*) ;;
*) fail "expected a provisional row, got '$PROV'" ;;
esac

# A finished turn saves the pane's text - under that provisional id.
$TMUX plugin-command -t "$PANE" agents done
sleep 0.6
[ "$(q "SELECT COUNT(*) FROM captures WHERE id = '$PROV'")" = 1 ] ||
	fail "the done report saved no capture to migrate past"

# The next turn brings the same pane back on the same provisional id.
$TMUX plugin-command -t "$PANE" agents "working editing the file"
sleep 0.6
[ "$(q "SELECT id FROM agents WHERE ended_ms IS NULL")" = "$PROV" ] ||
	fail "the working report did not revive the row"

# Now Claude's session file names the pane: the next render must move the
# row to the durable id, capture and all.
write_session_file idle "$now"
open_picker
[ "$(q "SELECT id FROM agents WHERE ended_ms IS NULL")" = "claude:$SID" ] ||
	fail "the row did not migrate off its provisional id: $(q "SELECT id FROM agents WHERE ended_ms IS NULL")"
[ "$(q "SELECT id FROM captures")" = "claude:$SID" ] ||
	fail "the capture did not follow its agent: $(q "SELECT id FROM captures")"
screen | grep -q 'hyperyaml-91' || fail "resolved name missing after the move"

# A shim says the turn is waiting on the USER. The session file has no way
# to say that - the most it manages is idle - so a render must leave it be.
$TMUX plugin-command -t "$PANE" agents "needs_input answer the question"
sleep 0.6
[ "$(q "SELECT status FROM agents WHERE ended_ms IS NULL")" = needs_input ] ||
	fail "the needs_input report did not land"
open_picker
[ "$(q "SELECT status FROM agents WHERE ended_ms IS NULL")" = needs_input ] ||
	fail "enrich flattened needs_input into the file's idle"
screen | grep -q 'needs input' || fail "the row left the needs input band"

# Activity in the file dated after the report is a new turn, and that does
# move the row on: needs_input is not sticky.
write_session_file idle "$((now + 600000))"
open_picker
[ "$(q "SELECT status FROM agents WHERE ended_ms IS NULL")" = waiting ] ||
	fail "a fresher idle did not move the row out of needs_input"

cleanup
exit 0
