#!/bin/sh
# The picker's interrupt and kill keys.
#
#   x  sends C-c to the agent's pane and leaves the pane alone
#   X  kills the pane, but only after a second X confirms
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-kill-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm
XDG_DATA_HOME=$(mktemp -d); export XDG_DATA_HOME
MARK=$(mktemp -d)

trap 'kill $CTL 2>/dev/null; $TMUX kill-server 2>/dev/null; rm -rf "$XDG_DATA_HOME" "$MARK"' EXIT
cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$XDG_DATA_HOME" "$MARK"
}
fail() { echo "FAIL: $*" >&2; echo "--- screen:" >&2; $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d' >&2; cleanup; exit 1; }
screen() { $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }
keys() { $TMUX send-keys -t "$FORM" "$@"; sleep 0.6; }

[ -f "$WASM" ] || fail "agents.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5
# The agent pane traps INT and records it, so the test can tell an
# interrupt that arrived from one that did not - and keeps running, which
# is the whole point of the interrupt key not being the kill key.
# AI_AGENT must be in the process's environment as it execs (that is what
# the pane-env read sees), not exported by the shell afterwards.
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 \
    "env AI_AGENT=claude sh -c 'trap \"echo INTERRUPTED >>$MARK/hits\" INT; while :; do sleep 0.2; done'" ||
	fail "new-session"
sleep 0.5
PANE=$($TMUX list-panes -t alpha -F '#{pane_id}' | head -1)

$TMUX load-plugin -s server -o trust_env=1 -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds -c send-keys "$WASM" || fail "load-plugin"
sleep 1.5

open_picker() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	( sleep 0.3; echo "plugin-command -t $PANE agents pick"; sleep 60 ) |
	    $TMUX -C attach >/dev/null 2>&1 &
	CTL=$!
	sleep 1.5
	FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
	    awk '/plugin-mode/ { print $1 }')
	[ -n "$FORM" ] || fail "picker did not open"
	i=0
	while [ "$i" -lt 15 ]; do
		screen | grep -q '1 live' && return 0
		sleep 0.4; i=$((i + 1))
	done
	fail "picker did not render the agent"
}

open_picker

# x interrupts: the trap fires and the pane is still there.
[ -f "$MARK/hits" ] && fail "the pane was interrupted before the test pressed anything"
keys x
i=0
while [ "$i" -lt 10 ]; do
	[ -f "$MARK/hits" ] && break
	sleep 0.4; i=$((i + 1))
done
[ -f "$MARK/hits" ] || fail "x did not deliver an interrupt to the agent pane"
$TMUX list-panes -a -F '#{pane_id}' | grep -qx "$PANE" ||
    fail "x killed the pane; it must only interrupt"
screen | grep -q 'interrupt sent' || fail "no interrupt status: $(screen)"

# One X asks and does NOT kill.
keys X
screen | grep -qi 'again to confirm' || fail "X did not ask for confirmation: $(screen)"
$TMUX list-panes -a -F '#{pane_id}' | grep -qx "$PANE" ||
    fail "a single X killed the pane without confirming"

# Another key cancels the pending kill: X then j, then X must ask again
# rather than kill.
keys j
keys X
screen | grep -qi 'again to confirm' ||
    fail "a key between the two X presses did not cancel the pending kill: $(screen)"
$TMUX list-panes -a -F '#{pane_id}' | grep -qx "$PANE" || fail "pane died during the cancel check"

# X twice kills it.
keys X
i=0
while [ "$i" -lt 15 ]; do
	$TMUX list-panes -a -F '#{pane_id}' | grep -qx "$PANE" || break
	sleep 0.4; i=$((i + 1))
done
$TMUX list-panes -a -F '#{pane_id}' | grep -qx "$PANE" &&
    fail "X X did not kill the pane"

cleanup
exit 0
