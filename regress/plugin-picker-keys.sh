#!/bin/sh
# The session_creator picker keys. Open the plain form from an attached
# control client, then drive the folder field with send-keys and read the
# mode screen back with capture-pane -M:
#
#   C-j/C-k move in the list while it shows, and between fields once it is
#   hidden. Esc hides the list; a second Esc closes the form. Tab completes
#   the field with the highlighted row (or the first one) and stays in the
#   field. Typing brings a hidden list back.

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lpicker-keys-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/session_creator.wasm
TMP=$(mktemp -d)

cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$TMP"
}
fail() {
	echo "FAIL: $*" >&2
	echo "--- screen:" >&2
	screen >&2
	cleanup
	exit 1
}
screen() {
	$TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'
}
keys() {
	$TMUX send-keys -t "$FORM" "$@"
	sleep 0.4
}
# The focused field's text, given its label.
field() {
	screen | sed -n "s/^  $1 *//p"
}
# Highlighted row, or nothing.
highlighted() {
	screen | sed -n 's/^  ▸ \([^ ]*\).*/\1/p'
}
listed() {
	screen | grep -q '^  ──'
}

[ -f "$WASM" ] || fail "session_creator.wasm not built"
mkdir -p "$TMP/pick/alpha" "$TMP/pick/beta" "$TMP/pick/gamma/inner"

$TMUX kill-server 2>/dev/null
sleep 0.5
$TMUX -f /dev/null new-session -d -x 100 -y 30 -c "$TMP/pick" || fail "new-session"
$TMUX load-plugin -c mode -c run-process -c run-command -c fs-list \
    -c fs-read-any "$WASM" || fail "load-plugin"

# The form opens on the pressing client's current window, so it needs a
# client that is attached; a control client with a session will do.
( sleep 0.3; echo 'plugin-command session_creator new'; sleep 60 ) |
    $TMUX -C attach >/dev/null 2>&1 &
CTL=$!
sleep 1.5
FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
    awk '/plugin-mode/ { print $1 }')
[ -n "$FORM" ] || fail "form did not open"

# The folder is prefilled, so focus starts on name. C-k with no list
# moves to the field above and its list appears.
keys C-k
listed || fail "C-k did not focus folder (no list)"
keys C-u
keys -l "$TMP/pick/"
[ "$(screen | grep -c '^    [a-z]')" = 3 ] || fail "expected three rows"
[ -z "$(highlighted)" ] || fail "a row is highlighted before C-j"

keys C-j
[ -n "$(highlighted)" ] || fail "C-j did not enter the list"
first=$(highlighted)
keys C-j
second=$(highlighted)
[ "$second" != "$first" ] || fail "second C-j did not move down"
keys C-k
[ "$(highlighted)" = "$first" ] || fail "C-k did not move up"

# Esc hides the list; the field keeps its text.
keys Escape
listed && fail "Esc did not hide the list"
case "$(field folder)" in
*pick/) ;;
*) fail "Esc changed the folder: $(field folder)" ;;
esac

# Typing brings it back, filtered. Tab completes and stays in the field.
keys -l g
listed || fail "typing did not bring the list back"
[ "$(screen | grep -c '^    gamma')" = 1 ] || fail "filter did not keep gamma"
keys Tab
case "$(field folder)" in
*pick/gamma) ;;
*) fail "Tab did not complete gamma: $(field folder)" ;;
esac
screen | grep -q 'C-j list' || fail "Tab left the folder field"

# Hidden list: C-j/C-k move between fields; a second Esc closes the form.
keys Escape
keys C-j
screen | grep -q 'C-j/C-k field' || fail "C-j after Esc did not move to name"
keys C-k
listed || fail "C-k back to folder did not show its list"
keys Escape
keys Escape
sleep 0.3
mode=$($TMUX list-panes -a -F '#{pane_mode}' | grep -c plugin-mode)
[ "$mode" = 0 ] || fail "second Esc did not close the form"

cleanup
exit 0
