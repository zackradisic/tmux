#!/bin/sh
# A push never replaces a plugin the remote loaded itself. B loads the
# services_probe plugin from its own configuration (role both) before the
# link; A links and pushes the same plugin. B keeps its copy, role and
# grants, and A's view talks to that copy once B grants A (a self-loaded
# copy is gated; only a pushed copy is auto-allowed).
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

# B's own copy: role both, with service-serve so it can answer and forward.
$TMUX2 load-plugin -c service-serve -c service-call -c write-options "$WASM" ||
    fail "load-plugin on B"
$TMUX load-plugin -c service-serve -c service-call -c write-options "$WASM" ||
    fail "load-plugin on A"

AOPT="$TMUX show-options -s -v"
BOPT="$TMUX2 show-options -s -v"
wait_for 6 "[ \"\$($BOPT @probe_role)\" = both ]" || fail "role on B"
wait_for 6 "[ \"\$($AOPT @probe_role)\" = both ]" || fail "role on A"

$TMUX remote-attach -t work fakehost || fail "remote-attach"
wait_for 10 "[ \"\$($AOPT @probe_up_fakehost)\" = 1 ]" || fail "link-up event"

# B keeps its OWN copy, so the push does not auto-allow A: B must grant A.
# (A pushed copy would be auto-allowed; a self-loaded one is gated.)
wait_for 6 "$TMUX2 plugin-peers list | grep -q '	services_probe	deny'" ||
    fail "no deny row on B: $($TMUX2 plugin-peers list)"
ANAME=$($TMUX2 plugin-peers list | awk -F'\t' '$2=="services_probe"&&$3=="deny"{print $1; exit}')
[ -n "$ANAME" ] || fail "no A name on B"
$TMUX2 plugin-peers allow "$ANAME" services_probe || fail "allow A on B"

# The echo fired once at link-up, before the grant, so it was denied.
# Force a reconnect: the grant persists, and the probe re-echoes on the
# fresh link-up, now allowed.
client=$($TMUX2 list-clients -F '#{client_name}' | head -1)
$TMUX2 detach-client -t "$client" || fail "detach-client"
wait_for 15 "[ \"\$($AOPT @probe_up_fakehost)\" = 2 ]" || fail "no reconnect"

# A's view gets its answer from B's own copy, which says "both".
wait_for 10 "[ \"\$($AOPT @probe_echo_fakehost)\" = both:ping ]" ||
    fail "remote echo: $($AOPT @probe_echo_fakehost)"
wait_for 10 "[ \"\$($AOPT @probe_tick_fakehost)\" -ge 1 ] 2>/dev/null" ||
    fail "remote ticks"

# B still runs its own copy: role both, not marked as pushed.
sleep 1
$TMUX2 show-plugins | grep -q 'services_probe: .*role both' ||
    fail "B's copy changed: $($TMUX2 show-plugins)"
$TMUX2 show-plugins | grep 'services_probe:' | grep -q 'pushed' &&
    fail "B's copy marked as pushed"
[ "$($BOPT @probe_role)" = both ] || fail "B re-initialised as $($BOPT @probe_role)"
[ -f "$XDG_B/tmux/plugin-cache/"*"/services_probe.wasm" ] &&
    fail "push wrote a cache file on B"

# A version match: both sides accept.
[ "$($AOPT @probe_accept_fakehost)" = 1 ] || fail "A rejected B: $($AOPT @probe_accept_fakehost)"
exit 0
