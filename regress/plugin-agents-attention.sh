#!/bin/sh
# The attention band: what puts a row in it, what it says there, and how a
# row leaves it by hand. A `needs_input` report may carry the harness's own
# words for why it wants you (the Claude `Notification` message), and the
# roster shows them on the row - but only while the agent is blocked, so
# the next status report must take them away again. The band itself is a
# guess the user can override in both directions. Also the copy key, which
# puts the agent's durable id (no `kind:` prefix) on the clipboard. Check:
#
#   a `needs_input` report with text lands the row in "needs input" and
#     shows the text on it;
#   a later `working` report clears the text (it is not a task);
#   `w` moves the row out of the band into "waiting", text and all;
#   `w` on a waiting row raises it back into the band;
#   `c` copies the durable id, with the `claude:` prefix stripped;
#   `c` on a row with no durable id yet refuses instead of copying the
#     provisional one.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-attention-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm
XDG_DATA_HOME=$(mktemp -d)
export XDG_DATA_HOME

SID=7f3c1d20-8b44-4e51-9a0e-2c6d5e8f1b93
MSG="Claude needs your permission to use Bash"

trap 'kill $CTL 2>/dev/null; $TMUX kill-server 2>/dev/null; rm -rf "$XDG_DATA_HOME"' EXIT

cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$XDG_DATA_HOME"
}
fail() {
	echo "FAIL: $*" >&2
	echo "--- screen:" >&2
	screen >&2
	cleanup
	exit 1
}
screen() { $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }
keys() { $TMUX send-keys -t "$FORM" "$@"; sleep 0.6; }
report() { $TMUX plugin-command -t "$PANE" agents "$*"; sleep 0.6; }

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

[ -f "$WASM" ] || fail "agents.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5

$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 \
    "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-session"
sleep 0.5
PANE=$($TMUX list-panes -t alpha -F '#{pane_id}' | head -1)
[ -n "$PANE" ] || fail "no pane id"

$TMUX load-plugin -s server -o trust_env=1 -c capture-pane -c run-command \
    -c mode -c db -c env-read -c pane-fds -c fs-read -c fs-list "$WASM" \
    || fail "load-plugin"
sleep 1.0

open_picker
screen | grep -q '1 live' || fail "the agent is not on the roster"

# --- the note: why the agent wants you, shown on the row -------------------

report "needs_input $MSG"
screen | grep -q 'needs input' || fail "the report did not reach the band"
screen | grep -q 'permission to use Bash' ||
    fail "the notification message is not on the row: $(screen)"

# It is a note, not a task: the next turn takes it away. Without this the
# reason an agent was blocked five turns ago sits on the row forever.
report "working"
screen | grep -q 'permission to use Bash' &&
	fail "a working report kept the stale note"
screen | grep -q 'working' || fail "the working report did not land"

# --- the band is a guess the user can override -----------------------------

report "needs_input $MSG"
screen | grep -q 'needs input' || fail "back into the band"
keys w
screen | grep -q 'waiting' || fail "w did not move the row to waiting"
screen | grep -q 'needs input' &&
	fail "the row is still in the attention band after w"
screen | grep -q 'permission to use Bash' &&
	fail "the note survived a hand-set status"

# A refresh must not undo it: the hand-set status stamps last_status_ms,
# which is what keeps enrich-at-render off it.
sleep 2.5
screen | grep -q 'needs input' && fail "a refresh put the row back in the band"

# And the other way: the user knows it needs them, no hook said so.
keys w
screen | grep -q 'needs input' || fail "w did not raise the row into the band"

# --- copy the durable id ---------------------------------------------------

# No resolver has run (no session file), so the row is still provisional.
$TMUX delete-buffer 2>/dev/null
keys c
screen | grep -q 'no id yet' ||
	fail "c copied something for a row with no durable id: $(screen)"

# Bind a durable id the way a shim does, then copy it.
report "identify claude:$SID"
keys c
BUF=$($TMUX show-buffer 2>/dev/null)
[ "$BUF" = "$SID" ] ||
	fail "c copied '$BUF', wanted the bare session id '$SID'"
screen | grep -q "copied $SID" || fail "the copy was not confirmed on screen"

cleanup
exit 0
