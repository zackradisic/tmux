#!/bin/sh
# The codex hook wire: a codex hook reports the durable id and the rollout
# path via `identify codex:<sid> <transcript>`, plus a status. The roster
# migrates the provisional row to `codex:<sid>` and reads the session
# nickname (agent_nickname) from the transcript's session_meta. This drives
# the wire directly (plugin-command), the way shims/codex/agents-codex.sh
# does, without running real codex.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen
SID=01a08523-5384-7823-b840-a3fe6d8f6091
NICK=NICK_CODEX_9

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-codex-hook-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm

HOME=$(mktemp -d); export HOME
XDG_DATA_HOME="$HOME/.local/share"; export XDG_DATA_HOME
FAKE=$(mktemp -d); cp /bin/sh "$FAKE/node"
# A fake codex rollout transcript with a session_meta first line.
TDIR="$HOME/.codex/sessions/2026/09/09"; mkdir -p "$TDIR"
TRANSCRIPT="$TDIR/rollout-2026-09-09T07-48-00-$SID.jsonl"
printf '{"timestamp":"2026-09-09T07:48:08Z","type":"session_meta","payload":{"session_id":"%s","cwd":"%s","agent_nickname":"%s","cli_version":"0.142.5"}}\n' \
    "$SID" "$HOME" "$NICK" > "$TRANSCRIPT"

DEPLOY=$(mktemp -d); cp "$WASM" "$DEPLOY/agents.wasm"
cat >"$DEPLOY/agents.toml" <<TOML
[caps]
requests = ["capture-pane","run-command","mode","db","env-read","pane-fds","fs-read","fs-list"]
[caps.env-read]
names = ["AI_AGENT","OPENCODE","_"]
[caps.fs-read]
paths = ["$HOME/.codex/sessions"]
TOML

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

trap 'kill $CTL 2>/dev/null; $TMUX kill-server 2>/dev/null; rm -rf "$HOME" "$FAKE" "$DEPLOY"' EXIT
cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$HOME" "$FAKE" "$DEPLOY"
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
	i=0
	while [ "$i" -lt 15 ]; do
		$TMUX capture-pane -M -p -t "$FORM" | grep -q '▸' && return 0
		sleep 0.4; i=$((i + 1))
	done
	fail "picker did not render"
}

[ -f "$WASM" ] || fail "agents.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 \
    "env _=/opt/agents/codex $FAKE/node -c 'sleep 600'" || fail "new-session"
sleep 0.5
PANE=$($TMUX list-panes -t alpha -F '#{pane_id}' | head -1)

$TMUX load-plugin -s server -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds -c fs-read -c fs-list "$DEPLOY/agents.wasm" \
    || fail "load-plugin"
sleep 1.5

# Detected as codex, still provisional.
[ "$(q "SELECT id FROM agents WHERE pane=${PANE#%}")" = "prov-codex-${PANE#%}" ] ||
    fail "pane not detected as provisional codex (got '$(q "SELECT id FROM agents WHERE pane=${PANE#%}")')"

# The hook wire: identify (durable id + transcript) then a status.
$TMUX plugin-command -t "$PANE" agents "identify codex:$SID $TRANSCRIPT"
sleep 0.5
$TMUX plugin-command -t "$PANE" agents "working"
sleep 0.5

# The row migrated to the durable id.
i=0
while [ "$i" -lt 10 ]; do
	[ "$(q "SELECT id FROM agents WHERE pane=${PANE#%}")" = "codex:$SID" ] && break
	sleep 1; i=$((i + 1))
done
[ "$(q "SELECT id FROM agents WHERE pane=${PANE#%}")" = "codex:$SID" ] ||
    fail "row did not migrate to codex:$SID (got '$(q "SELECT id FROM agents WHERE pane=${PANE#%}")')"

# Opening the picker enriches: the nickname from the transcript shows and
# is stored (not the cwd fallback).
open_picker
i=0
while [ "$i" -lt 10 ]; do
	[ "$(q "SELECT name FROM agents WHERE pane=${PANE#%}")" = "$NICK" ] && break
	sleep 1; i=$((i + 1))
done
[ "$(q "SELECT name FROM agents WHERE pane=${PANE#%}")" = "$NICK" ] ||
    fail "nickname not resolved from transcript (got '$(q "SELECT name FROM agents WHERE pane=${PANE#%}")')"
screen | grep -q "$NICK" || fail "nickname not shown on screen"

cleanup
exit 0
