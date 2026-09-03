#!/bin/sh
# Roundtrip test for the resurrect plugin: build a scene (two sessions,
# splits, a custom layout, a floating pane, distinct markers and cwds),
# save-and-kill, restore on a fresh server, and assert the world came
# back. Needs the wasm example built:
#   cargo build -p resurrect --target wasm32-unknown-unknown --release
#
# Panes must run explicit commands (bare shells can die immediately in
# sandboxed CI environments).

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lresurrect-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/resurrect.wasm
# Own data dir: never touch the user's real saves.
XDG_DATA_HOME=$(mktemp -d)
export XDG_DATA_HOME
DATA=$XDG_DATA_HOME/tmux/plugins/resurrect
trap 'rm -rf "$XDG_DATA_HOME"' EXIT

fail() {
	echo "FAIL: $*" >&2
	$TMUX kill-server 2>/dev/null
	exit 1
}

[ -f "$WASM" ] || fail "resurrect.wasm not built"

$TMUX kill-server 2>/dev/null
rm -rf "$DATA"
sleep 0.5

# --- the scene -------------------------------------------------------------
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 \
    "sh -c 'echo MARK-A0; exec sleep 600'" || fail "new-session alpha"
$TMUX split-window -t alpha:0 -c /tmp "sh -c 'echo MARK-A1; exec sleep 600'"
$TMUX split-window -t alpha:0 -h -c "$HOME" \
    "sh -c 'echo MARK-A2; exec sleep 600'"
$TMUX select-layout -t alpha:0 main-vertical
$TMUX new-window -t alpha:1 -c /tmp "sh -c 'echo MARK-A3; exec sleep 600'"
$TMUX rename-window -t alpha:1 buildwin
$TMUX new-pane -d -t alpha:0 -x 60 -y 12 -X 20 -Y 10 \
    "sh -c 'echo MARK-FLOAT; exec sleep 600'" || fail "new-pane float"
$TMUX new-session -d -s beta -x 100 -y 30 \
    "sh -c 'echo MARK-B0; exec sleep 600'"
$TMUX select-window -t alpha:1
sleep 0.3

layout_before=$($TMUX display -p -t alpha:0 '#{window_layout}')

# --- save + kill -----------------------------------------------------------
$TMUX load-plugin -c capture-pane -c run-command -c fs-read -c fs-write \
    "$WASM" || fail "load-plugin (save side)"
sleep 0.3
$TMUX plugin-command resurrect kill

for _ in 1 2 3 4 5 6 7 8 9 10; do
	$TMUX ls >/dev/null 2>&1 || break
	sleep 0.5
done
$TMUX ls >/dev/null 2>&1 && fail "server still alive after kill"
[ -f "$DATA/state.bin" ] || fail "state.bin was not written"
[ -f "$DATA/state.bin.tmp" ] && fail "temp file left behind after publish"
for mark in MARK-A0 MARK-A1 MARK-A2 MARK-A3 MARK-FLOAT MARK-B0; do
	grep -aq "$mark" "$DATA/state.bin" ||
	    fail "marker $mark missing from state.bin"
done
grep -aq '"saved_at_ms":[1-9]' "$DATA/state.bin" ||
    fail "state.bin has no timestamp"

# --- restore ---------------------------------------------------------------
$TMUX -f/dev/null new-session -d -s bootstrap "sleep 600" ||
    fail "bootstrap session"
$TMUX load-plugin -c capture-pane -c run-command -c fs-read -c fs-write \
    "$WASM" || fail "load-plugin (restore side)"
sleep 0.3
$TMUX plugin-command resurrect restore

ok=
for _ in 1 2 3 4 5 6 7 8 9 10; do
	sleep 0.5
	$TMUX has-session -t alpha 2>/dev/null &&
	    $TMUX has-session -t beta 2>/dev/null && ok=1 && break
done
[ -n "$ok" ] || fail "sessions did not come back"
sleep 1

# --- asserts ---------------------------------------------------------------
# Layout: structurally identical modulo pane ids and the checksum.
# The float cell appears both inline in the tree and in the <...>
# z-order section; strip pane ids everywhere and compare both parts.
strip_ids() {
	echo "$1" | sed -e 's/^[0-9a-f]*,//' \
	    -e 's/,[0-9][0-9]*\([]},>]\)/\1/g' -e 's/,[0-9][0-9]*$//'
}
layout_after=$($TMUX display -p -t alpha:0 '#{window_layout}')
before_s=$(strip_ids "$layout_before")
after_s=$(strip_ids "$layout_after")
[ "$before_s" = "$after_s" ] ||
    fail "layout mismatch: '$before_s' vs '$after_s'"
case "$layout_after" in
*"<"*">") ;;
*) fail "floating pane missing from restored layout" ;;
esac

# Contents, per pane.
for spec in "alpha:0.0 MARK-A0" "alpha:0.1 MARK-A1" "alpha:0.2 MARK-A2" \
    "alpha:0.3 MARK-FLOAT" "alpha:1.0 MARK-A3" "beta:0.0 MARK-B0"; do
	pane=${spec%% *}
	mark=${spec##* }
	$TMUX capture-pane -p -t "$pane" | grep -q "$mark" ||
	    fail "pane $pane lost its contents ($mark)"
done

# Working directories.
cwd=$($TMUX display -p -t alpha:0.1 '#{pane_current_path}')
[ "$cwd" = "/tmp" ] || fail "cwd not restored (got $cwd)"

# Window name + current window.
name=$($TMUX display -p -t alpha:1 '#{window_name}')
[ "$name" = "buildwin" ] || fail "window name not restored (got $name)"
cur=$($TMUX display -p -t alpha: '#{window_index}')
[ "$cur" = "1" ] || fail "current window not restored (got $cur)"

$TMUX kill-server 2>/dev/null
echo OK
exit 0
