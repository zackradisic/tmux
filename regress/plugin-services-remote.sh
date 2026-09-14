#!/bin/sh
# Services across a remote link. A loads the services_probe plugin (role
# both); linking to B pushes it there as a provider. The view half on A
# calls echo on both servers, follows the tick topic from both, sees the
# link go down (a call fails fast) and come back (calls and ticks resume).
#
# Needs the wasm example built:
#   cargo build -p services_probe --target wasm32-unknown-unknown --release

. ./remote-common.inc

WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/services_probe.wasm
[ -f "$WASM" ] || fail "no $WASM"

# Each server gets its own data directory: B's plugin cache and store
# must not be A's.
XDG_A="$TMP/a"
XDG_B="$TMP/b"
mkdir -p "$XDG_A" "$XDG_B"

XDG_DATA_HOME="$XDG_B" $TMUX2 new-session -d -s work -x 80 -y 24 || fail "new-session on B"
XDG_DATA_HOME="$XDG_A" $TMUX new-session -d -s local -x 80 -y 24 || fail "new-session on A"
$TMUX set -s remote-ssh-command \
    "$TEST_TMUX -LtestB$$ -f/dev/null -C attach -t #{remote_session}" ||
    fail "set remote-ssh-command"
$TMUX load-plugin -c service-serve -c service-call -c write-options "$WASM" ||
    fail "load-plugin"

# Option reads as plain command strings: wait_for runs its test in a new
# shell, where a function would not exist.
AOPT="$TMUX show-options -s -v"
BOPT="$TMUX2 show-options -s -v"

# The local half: role both, echo through the host queue, local ticks.
wait_for 6 "[ \"\$($AOPT @probe_role)\" = both ]" || fail "role on A"
wait_for 6 "[ \"\$($AOPT @probe_echo_local)\" = both:ping ]" ||
    fail "local echo: $($AOPT @probe_echo_local)"
wait_for 6 "[ \"\$($AOPT @probe_tick_local)\" -ge 1 ] 2>/dev/null" ||
    fail "local ticks"

# Link: the plugin is pushed to B as a provider and answers from there.
$TMUX remote-attach -t work fakehost || fail "remote-attach"
wait_for 10 "$TMUX2 show-plugins | grep -q 'services_probe: .*role provider'" ||
    fail "not pushed: $($TMUX2 show-plugins)"
wait_for 10 "[ \"\$($BOPT @probe_role)\" = provider ]" || fail "role on B"
wait_for 10 "[ \"\$($AOPT @probe_up_fakehost)\" = 1 ]" || fail "link-up event"
wait_for 10 "[ \"\$($AOPT @probe_echo_fakehost)\" = provider:ping ]" ||
    fail "remote echo: $($AOPT @probe_echo_fakehost)"
wait_for 10 "[ \"\$($AOPT @probe_tick_fakehost)\" -ge 1 ] 2>/dev/null" ||
    fail "remote ticks"
t1=$($AOPT @probe_tick_fakehost)
wait_for 6 "[ \"\$($AOPT @probe_tick_fakehost)\" -gt $t1 ]" ||
    fail "remote ticks stopped at $t1"

# A provider has no UI, and only the view half runs on A.
$TMUX2 show-plugins -v | grep -q "caps: .*service-serve" || fail "pushed caps"

# Link down: the view learns at once and a call fails fast.
client=$($TMUX2 list-clients -F '#{client_name}' | head -1)
$TMUX2 detach-client -t "$client" || fail "detach-client"
wait_for 10 "[ \"\$($AOPT @probe_down_fakehost)\" = 1 ]" || fail "link-down event"
wait_for 10 "[ \"\$($AOPT @probe_err_fakehost)\" = E_UNREACHABLE ]" ||
    fail "fail-fast: $($AOPT @probe_err_fakehost)"

# Link up again: the second hello re-pushes (unchanged), echo answers,
# the tick subscription is sent again and ticks resume.
wait_for 15 "[ \"\$($AOPT @probe_up_fakehost)\" = 2 ]" || fail "second link-up"
$TMUX set -s @probe_echo_fakehost ""
wait_for 10 "[ \"\$($AOPT @probe_echo_fakehost)\" = provider:ping ]" ||
    fail "echo after reconnect"
t2=$($AOPT @probe_tick_fakehost)
wait_for 10 "[ \"\$($AOPT @probe_tick_fakehost)\" -gt $t2 ]" ||
    fail "ticks after reconnect"

# B keeps its own store and cache under its data directory.
[ -f "$XDG_B/tmux/plugin-cache/"*"/services_probe.wasm" ] || fail "no cached wasm on B"
exit 0
