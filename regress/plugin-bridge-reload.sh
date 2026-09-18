#!/bin/sh
# A reload reaches the links that are already up.
#
# The bridge is told about a local plugin when an INSTANCE STARTS. A
# reload starts none - it swaps them in place - so before this the peer
# kept running whatever it was pushed at link time, and a peer that had
# dropped its copy never got another until the link was bounced.
#
# Two cases, both over a live link, with no reconnect anywhere:
#   1. B runs the plugin; A reloads changed bytes; B ends up on them.
#   2. B has unloaded its copy; A reloads; B has it again.
#
# Needs the wasm example built:
#   cargo build -p ticker --target wasm32-unknown-unknown --release

. ./remote-common.inc

REL=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release
TICKER="$REL/ticker.wasm"
[ -f "$TICKER" ] || fail "no $TICKER"

# New bytes for the SAME plugin: an empty wasm custom section (id 0, a
# 4-byte name, no payload) appended to the module. The hash changes, the
# module still loads, and the code is identical - so the reload's state
# migration accepts it. Two DIFFERENT plugins would not work here: v2's
# migrate rejects v1's snapshot, the reload keeps v1, and there would be
# nothing to announce.
bump() { printf '\000\005\004test' >> "$1"; }

XDG_A="$TMP/a"
XDG_B="$TMP/b"
mkdir -p "$XDG_A" "$XDG_B"

# A's plugin is a copy, so the test can change the bytes under it. The two
# modules differ, which is the point: the hash B ends up on says which one
# it is running.
MOD="$TMP/live.wasm"
cp "$TICKER" "$MOD"

hash_on_b() { XDG_DATA_HOME="$XDG_B" $TMUX2 show-plugins -v | awk '/^live:/ { f = 1 } f && /hash:/ { print $2; exit }'; }
hash_on_a() { XDG_DATA_HOME="$XDG_A" $TMUX show-plugins -v | awk '/^live:/ { f = 1 } f && /hash:/ { print $2; exit }'; }

XDG_DATA_HOME="$XDG_B" $TMUX2 new-session -d -s work -x 80 -y 24 || fail "new-session on B"
XDG_DATA_HOME="$XDG_A" $TMUX new-session -d -s local -x 80 -y 24 || fail "new-session on A"
$TMUX set -s remote-ssh-command \
    "$TEST_TMUX -LtestB$$ -f/dev/null -C attach -t #{remote_session}" ||
    fail "set remote-ssh-command"
$TMUX load-plugin -n live -c run-command "$MOD" || fail "load-plugin live"
wait_for 5 "$TMUX show-plugins | grep -q '^live: .*running'" || fail "live not on A"

$TMUX remote-attach -t work fakehost || fail "remote-attach"
wait_for 10 "$TMUX2 show-plugins | grep -q '^live: .*role provider, running'" ||
    fail "live never reached B: $($TMUX2 show-plugins)"
FIRST=$(hash_on_b)
[ -n "$FIRST" ] || fail "no hash for live on B: $($TMUX2 show-plugins -v)"
[ "$FIRST" = "$(hash_on_a)" ] || fail "B started on other bytes than A ($FIRST vs $(hash_on_a))"

# 1. Change the bytes under A and reload. The link is up throughout.
bump "$MOD"
$TMUX reload-plugin live || fail "reload-plugin"
wait_for 5 "[ \"\$($TMUX show-plugins -v | awk '/^live:/ { f = 1 } f && /hash:/ { print \$2; exit }')\" != $FIRST ]" ||
    fail "A did not pick up the new bytes"
SECOND=$(hash_on_a)
wait_for 15 "[ \"\$($TMUX2 show-plugins -v | awk '/^live:/ { f = 1 } f && /hash:/ { print \$2; exit }')\" = $SECOND ]" ||
    fail "the reload never reached B (B $(hash_on_b), A $SECOND)"
[ "$($TMUX list-sessions -F '#{remote_connected}' 2>/dev/null | head -1)" != 0 ] ||
    fail "the link went down; the test proves nothing about a live one"

# 2. B drops its copy. A reload must put it back, with no reconnect.
XDG_DATA_HOME="$XDG_B" $TMUX2 unload-plugin live || fail "unload-plugin on B"
wait_for 5 "! $TMUX2 show-plugins | grep -q '^live:'" || fail "live still on B after unload"
bump "$MOD"
$TMUX reload-plugin live || fail "reload-plugin (second)"
wait_for 15 "$TMUX2 show-plugins | grep -q '^live: .*running'" ||
    fail "a reload did not restore the copy B had dropped: $($TMUX2 show-plugins)"
[ "$(hash_on_b)" = "$(hash_on_a)" ] ||
    fail "B came back on stale bytes (B $(hash_on_b), A $(hash_on_a))"

exit 0
