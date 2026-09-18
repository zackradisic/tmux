#!/bin/sh
# Pull-driven plugin push over a link. A's hello lists its plugins with
# their hashes; B asks for the bytes it lacks with Want, and A pushes only
# then. A plugin B already has in its cache loads from there, with no
# push. A reconnect with the same plugin on both sides moves nothing.
#
# Needs the wasm examples built:
#   cargo build -p ticker -p services_probe \
#       --target wasm32-unknown-unknown --release

. ./remote-common.inc

REL=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release
TICKER="$REL/ticker.wasm"
PROBE="$REL/services_probe.wasm"
[ -f "$TICKER" ] || fail "no $TICKER"
[ -f "$PROBE" ] || fail "no $PROBE"

XDG_A="$TMP/a"
XDG_B="$TMP/b"
mkdir -p "$XDG_A" "$XDG_B"

XDG_DATA_HOME="$XDG_B" $TMUX2 new-session -d -s work -x 80 -y 24 || fail "new-session on B"
XDG_DATA_HOME="$XDG_A" $TMUX new-session -d -s local -x 80 -y 24 || fail "new-session on A"
$TMUX set -s remote-ssh-command \
    "$TEST_TMUX -LtestB$$ -f/dev/null -C attach -t #{remote_session}" ||
    fail "set remote-ssh-command"
$TMUX load-plugin "$TICKER" || fail "load-plugin ticker"
wait_for 5 "$TMUX show-plugins | grep -q '^ticker: .*running'" || fail "ticker on A"

# First link: B has nothing, asks, and A pushes once.
$TMUX remote-attach -t work fakehost || fail "remote-attach"
wait_for 10 "$TMUX2 show-plugins | grep -q '^ticker: .*role provider, running'" ||
    fail "ticker not on B: $($TMUX2 show-plugins)"
$TMUX2 plugin-log | grep -q 'ticker from .*: want [0-9]* bytes' ||
    fail "B did not ask: $($TMUX2 plugin-log | grep bridge)"
[ "$($TMUX plugin-log | grep -c 'pushed ticker to peer')" = 1 ] ||
    fail "A pushes: $($TMUX plugin-log | grep pushed)"
$TMUX2 show-plugins | grep -q '^ticker: .*pushed by .*, path .*/plugin-cache/cas/' ||
    fail "not from the cache: $($TMUX2 show-plugins)"

# Cache hit: B drops its copy; A's next hello (a new plugin loaded) lists
# ticker again and B loads it from the cache, while the new plugin is
# pushed.
$TMUX2 unload-plugin ticker || fail "unload on B"
wait_for 5 "! $TMUX2 show-plugins | grep -q '^ticker'" || fail "ticker still on B"
$TMUX load-plugin -c service-serve -c service-call -c write-options "$PROBE" ||
    fail "load-plugin probe"
wait_for 10 "$TMUX2 show-plugins | grep -q '^ticker: .*role provider, running'" ||
    fail "ticker not back on B: $($TMUX2 show-plugins)"
wait_for 10 "$TMUX2 show-plugins | grep -q '^services_probe: .*role provider, running'" ||
    fail "probe not on B: $($TMUX2 show-plugins)"
$TMUX2 plugin-log | grep -q 'ticker from .*: cache hit' ||
    fail "no cache hit: $($TMUX2 plugin-log | grep bridge)"
[ "$($TMUX plugin-log | grep -c 'pushed ticker to peer')" = 1 ] ||
    fail "ticker pushed again: $($TMUX plugin-log | grep pushed)"
[ "$($TMUX plugin-log | grep -c 'pushed services_probe to peer')" = 1 ] ||
    fail "probe pushes: $($TMUX plugin-log | grep pushed)"

# Reconnect: the same bytes on both sides, so nothing is asked or pushed.
hellos=$($TMUX plugin-log | grep -c 'hello from fakehost')
client=$($TMUX2 list-clients -F '#{client_name}' | head -1)
[ -n "$client" ] || fail "no control client on B"
$TMUX2 detach-client -t "$client" || fail "detach-client"
wait_for 6 "[ \"\$($TMUX display-message -p -t fakehost/work '#{remote_connected}')\" = 0 ]" ||
    fail "still connected"
wait_for 15 "[ \"\$($TMUX display-message -p -t fakehost/work '#{remote_connected}')\" = 1 ]" ||
    fail "did not reconnect"
wait_for 5 "[ \"\$($TMUX plugin-log | grep -c 'hello from fakehost')\" -gt $hellos ]" ||
    fail "no hello after reconnect"
sleep 1
[ "$($TMUX2 plugin-log | grep -c 'ticker from .*: want ')" = 1 ] ||
    fail "B asked for ticker again: $($TMUX2 plugin-log | grep want)"
[ "$($TMUX2 plugin-log | grep -c 'services_probe from .*: want ')" = 1 ] ||
    fail "B asked for the probe again: $($TMUX2 plugin-log | grep want)"
[ "$($TMUX plugin-log | grep -c 'pushed ')" = 2 ] ||
    fail "A pushed again: $($TMUX plugin-log | grep pushed)"
$TMUX2 show-plugins | grep -q '^ticker: .*role provider, running' || fail "ticker gone on B"
n=$(ls "$XDG_B/tmux/plugin-cache/cas/"*.wasm | wc -l)
[ "$n" -eq 2 ] || fail "expected 2 cached modules on B, got $n"

exit 0
