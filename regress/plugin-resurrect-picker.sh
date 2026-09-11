#!/bin/sh
# The resurrect picker (prefix -> `plugin-command resurrect pick`). Save
# two snapshots, then open the picker from an attached control client
# and drive it with send-keys, reading the mode screen back with
# capture-pane -M:
#
#   the list shows one row per snapshot (id, age, reason, counts, names);
#   typing filters; C-u clears the filter; p pins the highlighted row;
#   C-d asks to delete and y removes the row; Enter restores a killed
#   session; retention (keep_snapshots) drops the oldest unpinned row.
# Needs the wasm example built:
#     cargo build -p resurrect --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lresurrect-picker-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/resurrect.wasm
CAPS="-c capture-pane -c run-command -c fs-read -c fs-write -c db -c mode"
# Own data dir: never touch the user's real saves.
XDG_DATA_HOME=$(mktemp -d)
export XDG_DATA_HOME
DATA=$XDG_DATA_HOME/tmux/plugins/resurrect
DB=$DATA/store.db
trap 'kill $CTL 2>/dev/null; $TMUX kill-server 2>/dev/null; rm -rf "$XDG_DATA_HOME"' EXIT

cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$XDG_DATA_HOME"
}
fail() {
	echo "FAIL: $*" >&2
	echo "--- screen:" >&2
	screen >&2
	cleanup
	exit 1
}
screen() { $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }
keys() { $TMUX send-keys -t "$FORM" "$@"; sleep 0.4; }
rows() { screen | grep -c 'sess /'; }
q() {
	python3 -c "import sqlite3, sys
c = sqlite3.connect('file:$DB?mode=ro', uri=True)
r = c.execute(sys.argv[1]).fetchone()
print('' if r is None or r[0] is None else r[0])" "$1"
}
snaps() { q 'SELECT count(*) FROM snapshot'; }

[ -f "$WASM" ] || fail "resurrect.wasm not built"
command -v python3 >/dev/null || fail "python3 is needed to read store.db"

$TMUX kill-server 2>/dev/null
sleep 0.5

$TMUX -f/dev/null new-session -d -s alpha -x 120 -y 40 \
    "sh -c 'echo A; exec sleep 600'" || fail "new-session alpha"
$TMUX new-session -d -s beta -x 120 -y 40 \
    "sh -c 'echo B; exec sleep 600'" || fail "new-session beta"
# keep_snapshots=2: the third save must drop the oldest unpinned row.
$TMUX load-plugin $CAPS -o keep_snapshots=2 "$WASM" || fail "load-plugin"
sleep 0.5

# Two manual saves: manual saves always insert, so the list holds two rows.
$TMUX plugin-command resurrect save
sleep 0.8
$TMUX plugin-command resurrect save
sleep 0.8
[ "$(snaps)" = 2 ] || fail "expected two snapshot rows, got $(snaps)"

# A control client gives the picker a current window to open on.
( sleep 0.3; echo 'plugin-command resurrect pick'; sleep 60 ) |
    $TMUX -C attach >/dev/null 2>&1 &
CTL=$!
sleep 1.5
FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
    awk '/plugin-mode/ { print $1 }')
[ -n "$FORM" ] || fail "picker did not open"

[ "$(rows)" = 2 ] || fail "expected two snapshot rows, got $(rows)"
screen | grep -q 'alpha, beta' || fail "row is missing the session names"
screen | grep -q '#2 ' || fail "row is missing the snapshot id"
screen | grep -q 'manual' || fail "row is missing the reason"
screen | grep -q '2 snapshots' || fail "header is missing the count"
screen | grep -q 'restore' || fail "footer is missing the restore hint"
screen | grep -q 'pin' || fail "footer is missing the pin hint"
screen | grep -q 'save+restart' || fail "footer is missing the restart hint"

# Filter: a miss empties the list, C-u brings it back.
keys -l zzz
[ "$(rows)" = 0 ] || fail "a non-matching filter did not empty the list"
keys C-u
[ "$(rows)" = 2 ] || fail "C-u did not restore the list"

# Pin: the highlighted row (the newest, #2) gets a star and the flag.
keys -l p
screen | grep -q 'pinned #2' || fail "p did not report the pin"
screen | grep -q '★' || fail "pinned row has no star"
[ "$(q 'SELECT pinned FROM snapshot WHERE id = 2')" = 1 ] ||
    fail "pin flag not stored"

# Delete: move to the older row, C-d prompts, y removes it.
keys Down
keys C-d
screen | grep -q 'delete snapshot #1? y/n' || fail "C-d did not prompt for confirm"
keys -l y
sleep 0.4
[ "$(snaps)" = 1 ] || fail "delete did not remove the row ($(snaps) left)"
screen | grep -q 'deleted #1' || fail "the delete was not reported"
[ "$(rows)" = 1 ] || fail "the list did not drop the deleted row"

# Restore: kill beta, then Enter on the remaining snapshot rebuilds it.
$TMUX kill-session -t beta
sleep 0.3
$TMUX ls -F '#{session_name}' | grep -q beta && fail "beta not killed"
keys Enter
sleep 1
$TMUX list-panes -a -F '#{pane_mode}' | grep -q plugin-mode &&
    fail "picker did not close on restore"
$TMUX ls -F '#{session_name}' | grep -q beta ||
    fail "beta was not restored from the picked snapshot"
kill $CTL 2>/dev/null
CTL=

# Retention: three more saves (#3, #4, #5) with keep_snapshots=2 drop the
# oldest unpinned row (#3); the pinned #2 survives.
for _ in 1 2 3; do
	$TMUX plugin-command resurrect save
	sleep 0.8
done
[ "$(snaps)" = 3 ] || fail "expected pinned #2 plus two newest rows, got $(snaps)"
[ "$(q 'SELECT count(*) FROM snapshot WHERE id = 2')" = 1 ] ||
    fail "the pinned row was dropped by retention"
[ "$(q 'SELECT min(id) FROM snapshot WHERE pinned = 0')" = 4 ] ||
    fail "retention kept the wrong rows: $(q 'SELECT group_concat(id) FROM snapshot')"
[ "$(q 'SELECT count(*) FROM pane_blob WHERE snapshot_id = 3')" = 0 ] ||
    fail "pane_blob rows did not cascade with snapshot #3"

cleanup
echo OK
exit 0
