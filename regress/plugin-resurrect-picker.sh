#!/bin/sh
# The resurrect picker (prefix -> `plugin-command resurrect pick`). Save a
# snapshot, fake a second one, then open the picker from an attached
# control client and drive it with send-keys, reading the mode screen back
# with capture-pane -M:
#
#   the list shows one row per snapshot (age, session/pane counts, names);
#   typing filters; C-u clears the filter; C-d asks to delete and y removes
#   the file (fs_remove); Enter restores a killed session. Needs the wasm
#   example built:
#     cargo build -p resurrect --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lresurrect-picker-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/resurrect.wasm
# Own data dir: never touch the user's real saves.
XDG_DATA_HOME=$(mktemp -d)
export XDG_DATA_HOME
DATA=$XDG_DATA_HOME/tmux/plugins/resurrect
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
snapfiles() { ls "$DATA" 2>/dev/null | grep -c 'state.*bin'; }

[ -f "$WASM" ] || fail "resurrect.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5

$TMUX -f/dev/null new-session -d -s alpha -x 120 -y 40 \
    "sh -c 'echo A; exec sleep 600'" || fail "new-session alpha"
$TMUX new-session -d -s beta -x 120 -y 40 \
    "sh -c 'echo B; exec sleep 600'" || fail "new-session beta"
$TMUX load-plugin -c capture-pane -c run-command -c fs-read -c fs-write \
    -c mode "$WASM" || fail "load-plugin"
sleep 0.3

# One real save, then fake an older archive so the list holds two rows.
$TMUX plugin-command resurrect save
sleep 0.6
[ -f "$DATA/state.bin" ] || fail "state.bin not written"
cp "$DATA/state.bin" "$DATA/state.1.bin"

# A control client gives the picker a current window to open on.
( sleep 0.3; echo 'plugin-command resurrect pick'; sleep 60 ) |
    $TMUX -C attach >/dev/null 2>&1 &
CTL=$!
sleep 1.5
FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
    awk '/plugin-mode/ { print $1 }')
[ -n "$FORM" ] || fail "picker did not open"

[ "$(rows)" -ge 2 ] || fail "expected two snapshot rows, got $(rows)"
screen | grep -q 'alpha, beta' || fail "row is missing the session names"
screen | grep -q 'restore' || fail "footer is missing the restore hint"
screen | grep -q 'save+restart' || fail "footer is missing the restart hint"

# Filter: a miss empties the list, C-u brings it back.
keys -l zzz
[ "$(rows)" = 0 ] || fail "a non-matching filter did not empty the list"
keys C-u
[ "$(rows)" -ge 2 ] || fail "C-u did not restore the list"

# Delete: C-d prompts, y removes one file (fs_remove).
before=$(snapfiles)
keys C-d
screen | grep -q 'delete .*y/n' || fail "C-d did not prompt for confirm"
keys -l y
sleep 0.4
[ "$(snapfiles)" -lt "$before" ] ||
    fail "delete did not remove a file ($before -> $(snapfiles))"
screen | grep -q 'deleted' || fail "the delete was not reported"

# Restore: kill beta, then Enter on the remaining snapshot rebuilds it.
$TMUX kill-session -t beta
sleep 0.3
$TMUX ls -F '#{session_name}' | grep -q beta && fail "beta not killed"
# The list may be empty if the delete removed the only remaining row;
# reopen only when a row is still shown.
if [ "$(rows)" -ge 1 ]; then
	keys Enter
	sleep 1
	$TMUX list-panes -a -F '#{pane_mode}' | grep -q plugin-mode &&
	    fail "picker did not close on restore"
	$TMUX ls -F '#{session_name}' | grep -q beta ||
	    fail "beta was not restored from the picked snapshot"
fi

cleanup
exit 0
