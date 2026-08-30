#!/bin/sh
# A plugin that traps comes back. Load the ticker told to panic on
# window-linked, then open windows to trap it. After each of the first two
# traps a fresh instance must appear. The third trap inside the failure
# window must disable the plugin, and reload-plugin must then start an
# instance again from a state with none live - the case that used to
# return 0 and change nothing.
#
# Then the same wasm pane-scoped: it must get one instance per existing
# pane at load, and reload-plugin must restore the count after a pane goes.
# Both go through enumerate_ids, which parsed list_objects as JSON long
# after the ABI moved to records, so no object-scoped plugin ever got an
# instance at load.

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lrespawn-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/ticker.wasm

fail() {
	echo "FAIL: $*" >&2
	$TMUX kill-server 2>/dev/null
	exit 1
}
instances() {
	$TMUX show-plugins | sed -n "s/^${1:-ticker}: .*, \([0-9]*\) instance.*/\1/p"
}
state() {
	$TMUX show-plugins | sed -n "s/^${1:-ticker}: scope [a-z]*, \([a-z]*\).*/\1/p"
}
panes() {
	$TMUX list-panes -a | wc -l | tr -d ' '
}

[ -f "$WASM" ] || fail "ticker.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5
$TMUX -f/dev/null new-session -d -x 80 -y 24 "sleep 600" || fail "new-session"
$TMUX load-plugin -c write-options -c run-process -c run-command \
    -o panic_on=window-linked "$WASM" || fail "load-plugin"
sleep 0.5
[ "$(instances)" = 1 ] || fail "after load: expected 1 instance, got '$(instances)'"

# Traps 1 and 2: torn down, then back with fresh state.
for n in 1 2; do
	$TMUX new-window -d "sleep 600"
	sleep 0.7
	[ "$(state)" = running ] || fail "trap $n: state '$(state)'"
	[ "$(instances)" = 1 ] || fail "trap $n: expected a fresh instance, got '$(instances)'"
	$TMUX plugin-log ticker | grep -q "restarting with fresh state" ||
	    fail "trap $n: restart not logged"
done

# Trap 3 inside the window: disabled, and nothing comes back on its own.
$TMUX new-window -d "sleep 600"
sleep 0.7
[ "$(state)" = disabled ] || fail "trap 3: state '$(state)', expected disabled"
[ "$(instances)" = 0 ] || fail "trap 3: expected 0 instances, got '$(instances)'"

# reload-plugin with no live instance starts one.
$TMUX reload-plugin ticker || fail "reload-plugin"
sleep 0.7
[ "$(state)" = running ] || fail "after reload: state '$(state)'"
[ "$(instances)" = 1 ] || fail "after reload: expected 1 instance, got '$(instances)'"

# Pane-scoped: one instance per pane at load.
$TMUX load-plugin -n tickpane -s pane -c write-options -c run-process \
    -c run-command "$WASM" || fail "load-plugin -s pane"
sleep 0.7
[ "$(panes)" -ge 2 ] || fail "test needs at least 2 panes, have $(panes)"
[ "$(instances tickpane)" = "$(panes)" ] ||
    fail "pane scope: $(panes) panes but $(instances tickpane) instances at load"

# Drop a pane, then reload: the count follows the panes.
$TMUX kill-window -t :1 || fail "kill-window"
sleep 0.5
$TMUX reload-plugin tickpane || fail "reload-plugin tickpane"
sleep 0.7
[ "$(instances tickpane)" = "$(panes)" ] ||
    fail "pane scope after reload: $(panes) panes but $(instances tickpane) instances"

$TMUX kill-server
echo OK
