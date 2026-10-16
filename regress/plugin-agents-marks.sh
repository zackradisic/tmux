#!/bin/sh
# Marks and the undo. `J` / `K` mark the row and move; every row action
# then takes the marked rows: archive, the undo of an archive (`U`, the
# rows the last archive took come back), fold (`z` folds the projects of
# the marked rows together), interrupt (`x` sends C-c to each marked
# pane) and kill (`X` twice, counted). Check:
#
#   J marks two rows; a archives both and says U undoes it;
#   U brings both back;
#   z with two marked rows in two projects folds both projects;
#   x with two marked rows interrupts both (the fake agents die);
#   X twice with a marked row kills the marked panes, asking by count.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-marks-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm

HOME=$(mktemp -d); export HOME
XDG_DATA_HOME="$HOME/.local/share"; export XDG_DATA_HOME
DEPLOY=$(mktemp -d); cp "$WASM" "$DEPLOY/agents.wasm"
cat >"$DEPLOY/agents.toml" <<'TOML'
[caps]
requests = ["capture-pane","run-command","mode","db","env-read",
            "pane-fds","fs-read","fs-list","send-keys"]
[caps.env-read]
names = ["AI_AGENT","OPENCODE"]
TOML

trap 'kill $CTL 2>/dev/null; $TMUX kill-server 2>/dev/null; rm -rf "$HOME" "$DEPLOY"' EXIT
cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$HOME" "$DEPLOY"
}
fail() { echo "FAIL: $*" >&2; echo "--- screen:" >&2; screen >&2; cleanup; exit 1; }
q() { db=$(find "$XDG_DATA_HOME" -name store.db | head -1); [ -n "$db" ] && sqlite3 "$db" "$1"; }
screen() { $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }
keys() { $TMUX send-keys -t "$FORM" "$@"; sleep 0.5; }
open_picker() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	( sleep 0.3; echo 'plugin-command agents pick'; sleep 60 ) |
	    $TMUX -C attach -t alpha >/dev/null 2>&1 &
	CTL=$!
	sleep 1.5
	FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
	    awk '/plugin-mode/ { print $1 }' | head -1)
	[ -n "$FORM" ] || fail "picker did not open"
}
live() { q "SELECT COUNT(*) FROM agents WHERE ended_ms IS NULL AND life = 'active'"; }
# The rows' sessions, top to bottom.
rows() { screen | grep -v '^ *[▪▸▹]' | grep -o 'claude · [a-z]*' | sed 's/claude · //' | tr '\n' ' '; }

[ -f "$WASM" ] || fail "agents.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5

# Four agents in four sessions, each its own project (the session name).
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 \
    "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-session"
for s in beta gamma delta; do
	$TMUX new-session -d -s $s -x 200 -y 50 \
	    "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-session $s"
done
sleep 0.5

$TMUX load-plugin -s server -o trust_env=1 -c capture-pane -c run-command \
    -c mode -c db -c env-read -c pane-fds -c fs-read -c fs-list -c send-keys \
    "$DEPLOY/agents.wasm" || fail "load-plugin"
sleep 1.0
[ "$(live)" = 4 ] || fail "expected 4 live rows, got $(live)"

# --- J marks two rows, a archives both, U undoes --------------------------

open_picker
screen | grep -q '4 live' || fail "not 4 live"
keys g g
keys J; keys J
# J marks the row and moves on; two presses mark two rows (the cursor
# then sits on the third, which is not a target while rows are marked).
keys a
sleep 0.8
screen | grep -q '2 archived (U undoes)' || fail "a did not archive the two: no status"
[ "$(live)" = 2 ] || fail "expected 2 active after the archive, got $(live)"
keys U
sleep 0.8
screen | grep -q '2 unarchived' || fail "U did not bring them back: no status"
[ "$(live)" = 4 ] || fail "expected 4 active after the undo, got $(live)"
keys U
screen | grep -q 'nothing archived yet' || fail "a second U did not say there is nothing to undo"

# --- z with marks folds the marked rows' projects --------------------------

keys g g
keys J; keys J
keys z
[ "$(screen | grep -c "^[▹▸] .*(1)")" = 3 ] || fail "z did not fold the marked projects with the cursor's: $(screen | grep -c "^[▹▸] .*(1)") folded"
keys Escape
keys Z
# Z folds the level when any group is open, else unfolds it: a second
# press is the unfold when the first found an open one.
folded() { screen | grep -c "^[▹▸] .*(1)"; }
[ "$(folded)" = 0 ] || keys Z
[ "$(folded)" = 0 ] || fail "Z did not unfold them: $(folded) folded"

# --- x with marks interrupts both -----------------------------------------

# The cursor is on a header after Z: Right goes down onto a row first.
keys Right
keys g g
keys J; keys J
keys x
screen | grep -q 'interrupt sent to 2 panes' || fail "x did not interrupt two panes"
sleep 1.5
# C-c kills the fake agent's sleep: the two panes are gone.
[ "$($TMUX list-panes -a | wc -l | tr -d ' ')" = 3 ] ||
	fail "expected the picker and two panes left, got $($TMUX list-panes -a | wc -l | tr -d ' ')"

# --- X twice with a mark kills the marked panes, counted -------------------

keys g g
keys J; keys J
keys X
screen | grep -q 'kill 2 panes? X again to confirm' || fail "X did not ask by count"
keys X
sleep 1.5
screen | grep -q 'killed 2 panes' || fail "the second X did not kill both"

cleanup
exit 0
