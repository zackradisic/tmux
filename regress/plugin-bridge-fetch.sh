#!/bin/sh
# A peer fetches a plugin itself instead of taking a push. A loads ticker
# from a url manifest entry (a file:// url here); its hello names that
# url and hash, so B, with plugin-remote-fetch on, fetches the module and
# loads it with no push. With the option off, B asks for a push.
#
# Needs the wasm examples built (and curl):
#   cargo build -p ticker -p services_probe \
#       --target wasm32-unknown-unknown --release

. ./remote-common.inc

ROOT=$(dirname "$TEST_TMUX")
REL="$ROOT/plugin-host/target/wasm32-unknown-unknown/release"
TICKER="$REL/ticker.wasm"
PROBE="$REL/services_probe.wasm"
[ -f "$TICKER" ] || fail "no $TICKER"
[ -f "$PROBE" ] || fail "no $PROBE"
command -v curl >/dev/null || fail "curl not on PATH"
INDEX="$ROOT/plugin-host/target/release/plugin-index"
[ -x "$INDEX" ] || INDEX="$ROOT/plugin-host/target/debug/plugin-index"
if [ ! -x "$INDEX" ]; then
	(cd "$ROOT/plugin-host" && cargo build -q -p plugin-index) ||
	    fail "cannot build plugin-index"
	INDEX="$ROOT/plugin-host/target/debug/plugin-index"
fi

XDG_A="$TMP/a"
XDG_B="$TMP/b"
mkdir -p "$XDG_A" "$XDG_B" "$TMP/url"
cp "$TICKER" "$TMP/url/"
"$INDEX" --dir "$TMP/url" --tag url --version ticker=0.1.0 >/dev/null || fail "index"
HASH=$(sed -n 's/^blake3 = "\(.*\)"/\1/p' "$TMP/url/index.toml")
[ -n "$HASH" ] || fail "no hash"
cat > "$TMP/plugins.toml" <<EOM
[plugins.ticker]
url = "file://$TMP/url/ticker.wasm"
hash = "blake3:$HASH"
EOM

XDG_DATA_HOME="$XDG_B" $TMUX2 new-session -d -s work -x 80 -y 24 || fail "new-session on B"
XDG_DATA_HOME="$XDG_A" $TMUX new-session -d -s local -x 80 -y 24 || fail "new-session on A"
$TMUX set -s remote-ssh-command \
    "$TEST_TMUX -LtestB$$ -f/dev/null -C attach -t #{remote_session}" ||
    fail "set remote-ssh-command"
$TMUX sync-plugins "$TMP/plugins.toml" >/dev/null || fail "sync on A"
wait_for 10 "$TMUX show-plugins | grep -q '^ticker: .*running'" ||
    fail "ticker on A: $($TMUX show-plugins)"

# Link: B fetches from the url in A's hello; A never pushes.
$TMUX remote-attach -t work fakehost || fail "remote-attach"
wait_for 10 "$TMUX2 show-plugins | grep -q '^ticker: .*role provider, running'" ||
    fail "ticker not on B: $($TMUX2 show-plugins)"
$TMUX2 plugin-log | grep -q "ticker from .*: fetching file://$TMP/url/ticker.wasm" ||
    fail "B did not fetch: $($TMUX2 plugin-log | grep bridge)"
$TMUX2 plugin-log | grep -q 'ticker from .* (fetched): loaded' ||
    fail "not loaded from the fetch: $($TMUX2 plugin-log | grep bridge)"
$TMUX plugin-log | grep -q 'pushed ticker' && fail "A pushed: $($TMUX plugin-log | grep pushed)"
[ -f "$XDG_B/tmux/plugin-cache/cas/$HASH.wasm" ] || fail "no cached module on B"

# Option off and cache empty: B asks for a push.
$TMUX2 set -s plugin-remote-fetch off || fail "set option"
$TMUX2 unload-plugin ticker || fail "unload on B"
wait_for 5 "! $TMUX2 show-plugins | grep -q '^ticker'" || fail "ticker still on B"
rm -f "$XDG_B/tmux/plugin-cache/cas/$HASH.wasm"
$TMUX load-plugin -c service-serve -c service-call -c write-options "$PROBE" ||
    fail "load-plugin probe"
wait_for 10 "$TMUX2 show-plugins | grep -q '^ticker: .*role provider, running'" ||
    fail "ticker not back on B: $($TMUX2 show-plugins)"
$TMUX2 plugin-log | grep -q 'ticker from .*: want [0-9]* bytes' ||
    fail "B did not ask: $($TMUX2 plugin-log | grep bridge)"
[ "$($TMUX plugin-log | grep -c 'pushed ticker to peer')" = 1 ] ||
    fail "A pushes: $($TMUX plugin-log | grep pushed)"
[ -f "$XDG_B/tmux/plugin-cache/cas/$HASH.wasm" ] || fail "pushed module not cached on B"

exit 0
