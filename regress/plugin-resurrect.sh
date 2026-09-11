#!/bin/sh
# Roundtrip test for the resurrect plugin: build a scene (two sessions,
# splits, a custom layout, a floating pane, distinct markers and cwds),
# save-and-kill, restore on a fresh server, and assert the world came
# back. The save lands in the plugin's SQLite store (store.db): one
# snapshot row, one compressed pane_blob row per pane. Needs the wasm
# example built:
#   cargo build -p resurrect --target wasm32-unknown-unknown --release
#
# Panes must run explicit commands (bare shells can die immediately in
# sandboxed CI environments).

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lresurrect-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/resurrect.wasm
CAPS="-c capture-pane -c run-command -c fs-read -c fs-write -c db"
# Own data dir: never touch the user's real saves.
XDG_DATA_HOME=$(mktemp -d)
export XDG_DATA_HOME
DATA=$XDG_DATA_HOME/tmux/plugins/resurrect
DB=$DATA/store.db
trap 'rm -rf "$XDG_DATA_HOME"' EXIT

fail() {
	echo "FAIL: $*" >&2
	$TMUX kill-server 2>/dev/null
	exit 1
}

# One scalar from the store (empty for NULL / no row).
q() {
	python3 -c "import sqlite3, sys
c = sqlite3.connect('file:$DB?mode=ro', uri=True)
r = c.execute(sys.argv[1]).fetchone()
print('' if r is None or r[0] is None else r[0])" "$1"
}

[ -f "$WASM" ] || fail "resurrect.wasm not built"
command -v python3 >/dev/null || fail "python3 is needed to read store.db"

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
$TMUX load-plugin $CAPS "$WASM" || fail "load-plugin (save side)"
sleep 0.5
$TMUX plugin-command resurrect kill

for _ in 1 2 3 4 5 6 7 8 9 10; do
	$TMUX ls >/dev/null 2>&1 || break
	sleep 0.5
done
$TMUX ls >/dev/null 2>&1 && fail "server still alive after kill"
[ -f "$DB" ] || fail "store.db was not written"
[ -e "$DATA/state.bin" ] && fail "a legacy state.bin was written"
[ "$(q 'SELECT count(*) FROM snapshot')" = 1 ] ||
    fail "expected one snapshot row, got $(q 'SELECT count(*) FROM snapshot')"
[ "$(q 'SELECT reason FROM snapshot')" = kill ] ||
    fail "snapshot reason is $(q 'SELECT reason FROM snapshot'), not kill"
[ "$(q 'SELECT saved_at_ms > 0 FROM snapshot')" = 1 ] ||
    fail "snapshot has no timestamp"
[ "$(q 'SELECT sessions FROM snapshot')" = 2 ] ||
    fail "snapshot counts $(q 'SELECT sessions FROM snapshot') sessions, not 2"
[ "$(q 'SELECT panes FROM snapshot')" = 6 ] ||
    fail "snapshot counts $(q 'SELECT panes FROM snapshot') panes, not 6"
[ "$(q 'SELECT count(*) FROM pane_blob')" = 6 ] ||
    fail "expected six pane_blob rows, got $(q 'SELECT count(*) FROM pane_blob')"
# Every stored blob is a zstd frame (magic 28 b5 2f fd).
[ "$(q "SELECT count(*) FROM pane_blob WHERE hex(substr(data, 1, 4)) != '28B52FFD'")" = 0 ] ||
    fail "a pane_blob row is not a zstd frame"
# With the zstd CLI at hand, inflate one frame and find its marker.
if command -v zstd >/dev/null; then
	python3 -c "import sqlite3, sys
c = sqlite3.connect('file:$DB?mode=ro', uri=True)
sys.stdout.buffer.write(c.execute('SELECT data FROM pane_blob ORDER BY pane_id LIMIT 1').fetchone()[0])" |
	    zstd -d -c | grep -aq 'MARK-' || fail "inflated frame has no marker"
fi

# --- restore ---------------------------------------------------------------
$TMUX -f/dev/null new-session -d -s bootstrap "sleep 600" ||
    fail "bootstrap session"
$TMUX load-plugin $CAPS "$WASM" || fail "load-plugin (restore side)"
sleep 0.5
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

# Status names the snapshot and the store.
$TMUX plugin-command resurrect status
sleep 0.5
$TMUX show-messages | grep -q 'resurrect: #1 holds 2 sessions' ||
    fail "status did not report snapshot #1"

$TMUX kill-server 2>/dev/null
echo OK
exit 0
