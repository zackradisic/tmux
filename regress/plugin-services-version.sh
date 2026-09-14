#!/bin/sh
# Service versions across a link. The view half on A rejects every
# provider copy (config reject_peer=1 drives the plugin's accepts_provider
# hook), so a call to B fails at once with E_VERSION and B's topic events
# are dropped on A, while B's copy stays loaded.
#
# Needs the wasm example built:
#   cargo build -p services_probe --target wasm32-unknown-unknown --release

. ./remote-common.inc

WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/services_probe.wasm
[ -f "$WASM" ] || fail "no $WASM"

XDG_A="$TMP/a"
XDG_B="$TMP/b"
mkdir -p "$XDG_A" "$XDG_B"

XDG_DATA_HOME="$XDG_B" $TMUX2 new-session -d -s work -x 80 -y 24 || fail "new-session on B"
XDG_DATA_HOME="$XDG_A" $TMUX new-session -d -s local -x 80 -y 24 || fail "new-session on A"
$TMUX set -s remote-ssh-command \
    "$TEST_TMUX -LtestB$$ -f/dev/null -C attach -t #{remote_session}" ||
    fail "set remote-ssh-command"
$TMUX load-plugin -c service-serve -c service-call -c write-options \
    -o reject_peer=1 "$WASM" || fail "load-plugin"

AOPT="$TMUX show-options -s -v"
wait_for 6 "[ \"\$($AOPT @probe_echo_local)\" = both:ping ]" ||
    fail "local echo: $($AOPT @probe_echo_local)"

$TMUX remote-attach -t work fakehost || fail "remote-attach"
wait_for 10 "$TMUX2 show-plugins | grep -q 'services_probe: .*role provider'" ||
    fail "not pushed: $($TMUX2 show-plugins)"
wait_for 10 "[ \"\$($AOPT @probe_up_fakehost)\" = 1 ]" || fail "link-up event"

# The hook ran and said no. The first call went out before the verdict
# (B had no copy yet), so it may have got an answer.
wait_for 10 "[ \"\$($AOPT @probe_accept_fakehost)\" = 0 ]" ||
    fail "hook verdict: $($AOPT @probe_accept_fakehost)"

# A reconnect makes a fresh call after the verdict: it fails at once with
# E_VERSION, and B's ticks no longer reach A's view.
client=$($TMUX2 list-clients -F '#{client_name}' | head -1)
$TMUX2 detach-client -t "$client" || fail "detach-client"
wait_for 10 "[ \"\$($AOPT @probe_down_fakehost)\" = 1 ]" || fail "link-down event"
wait_for 15 "[ \"\$($AOPT @probe_up_fakehost)\" = 2 ]" || fail "second link-up"
wait_for 10 "[ \"\$($AOPT @probe_err_fakehost)\" = E_VERSION ]" ||
    fail "call error: $($AOPT @probe_err_fakehost)"
t1=$($AOPT @probe_tick_fakehost 2>/dev/null)
sleep 1.5
[ "$($AOPT @probe_tick_fakehost 2>/dev/null)" = "$t1" ] ||
    fail "ticks arrived from a rejected server"

# Local calls and ticks still work.
[ "$($AOPT @probe_echo_local)" = both:ping ] || fail "local echo lost"
wait_for 6 "[ \"\$($AOPT @probe_tick_local)\" -ge 1 ] 2>/dev/null" || fail "local ticks"

# B keeps the pushed copy running; the rejection is A's alone.
$TMUX2 show-plugins | grep -q 'services_probe: .*running' ||
    fail "B's copy not running: $($TMUX2 show-plugins)"
exit 0
