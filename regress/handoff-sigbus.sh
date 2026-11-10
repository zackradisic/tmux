#!/bin/sh
# After restart-server a SIGBUS must not jump to address 0. execve(2) puts
# a caught signal back to SIG_DFL but on macOS keeps its flags, so the new
# image found SIGBUS as SIG_DFL with SA_SIGINFO, left there by the plugin
# host's trap handler in the old one. Wasmtime saved that as the handler to
# chain to and, seeing SA_SIGINFO, called it: the server died with
# EXC_BAD_ACCESS at 0x0 under _sigtramp, and the real fault was lost (Nov
# 10 2026, pid 34757, crash in the pane input path).
#
# Load a plugin so the trap handlers are installed, restart, and send the
# server a SIGBUS. Wasmtime's handler finds no wasm on the main thread and
# chains on: to a clean SIG_DFL it puts the default back and returns, which
# for a sent signal means the server carries on; to the stale one it calls
# 0 and the server is gone.
#
# Needs the wasm example built:
#   cargo build -p ticker --target wasm32-unknown-unknown --release
PATH=/bin:/usr/bin
TERM=screen
unset TMUX
[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
[ -z "$TEST_SOCKET" ] && TEST_SOCKET=handoff-sigbus-test$$
TMUX="$TEST_TMUX -L$TEST_SOCKET -f/dev/null"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/ticker.wasm

# Keep the plugin host's state away from the real one.
TMP=$(mktemp -d)
XDG_DATA_HOME=$TMP/data
XDG_CONFIG_HOME=$TMP/config
XDG_STATE_HOME=$TMP/state
export XDG_DATA_HOME XDG_CONFIG_HOME XDG_STATE_HOME

fail() {
	echo "FAIL: $*" >&2
	$TMUX kill-server 2>/dev/null
	rm -rf "$TMP"
	exit 1
}
[ -f "$WASM" ] || fail "ticker.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5
$TMUX new-session -d -s main -x 80 -y 24 "sleep 600" || fail "new-session"
$TMUX load-plugin -c write-options "$WASM" || fail "load-plugin"
sleep 0.5
$TMUX show-plugins | grep -q '^ticker:' || fail "plugin not loaded"
pid=$($TMUX display-message -p '#{pid}')

$TMUX restart-server || fail "restart-server"
sleep 3
$TMUX has-session -t main 2>/dev/null || fail "server did not come back"
[ "$($TMUX display-message -p '#{pid}')" = "$pid" ] || fail "pid changed"
# The handoff brings the plugin back; load it if it did not, so the new
# image has installed its trap handlers either way.
$TMUX show-plugins | grep -q '^ticker:' ||
	$TMUX load-plugin -c write-options "$WASM" || fail "load-plugin again"
sleep 0.5

kill -BUS "$pid"
sleep 1
$TMUX has-session -t main 2>/dev/null ||
	fail "a SIGBUS after restart-server killed the server"

$TMUX kill-server 2>/dev/null
rm -rf "$TMP"
echo PASS
