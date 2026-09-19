#!/bin/sh
# The action menu. Space opens a tmux menu on the client that asked for
# it, listing every picker action for the selected row - the ones that do
# not apply to that row dimmed rather than missing. Its items do not act
# themselves: each hands its own key back to the picker through
# `menu-key`, so the menu can never disagree with the keymap. Check:
#
#   Space draws a menu, titled with the selected agent, over the picker;
#   an action that cannot apply to the row is dimmed - shown, but with
#     no key and unselectable (a menu you consult must answer "no", not
#     stay silent);
#   `menu-key` runs the picker key an item stands for (copy id, and the
#     band move);
#   an unknown key through that door is a no-op;
#   `menu-key` with no picker open is ignored, not a crash.
#
# A menu is a client overlay on a real tty: a control client cannot draw
# one, and no format reports one ("client_overlay" does not exist). So
# this test runs a SECOND tmux server whose pane attaches to the first -
# that inner attach is a genuine pty client, and capturing the outer
# pane is what the user's terminal would show, overlays and all.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-menu-test"
OUTER="$TEST_TMUX -Lagents-menu-outer"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm
XDG_DATA_HOME=$(mktemp -d)
export XDG_DATA_HOME

SID=3a91f7c4-2d16-4b88-a05e-6f2b9c374e10

trap 'cleanup' EXIT
cleanup() {
	$OUTER kill-server 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$XDG_DATA_HOME"
}
fail() {
	echo "FAIL: $*" >&2
	echo "--- what the client sees:" >&2
	seen >&2
	echo "--- picker pane:" >&2
	screen >&2
	cleanup
	exit 1
}
# The picker's own pane, without overlays.
screen() { $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }
# What the attached terminal renders, overlays included.
seen() { $OUTER capture-pane -p -t "$VIEW" | sed '/^ *$/d'; }
keys() { $TMUX send-keys -t "$FORM" "$@"; sleep 0.8; }
report() { $TMUX plugin-command -t "$AGENT" agents "$*"; sleep 0.6; }
menukey() { $TMUX plugin-command agents "menu-key $1"; sleep 0.8; }

[ -f "$WASM" ] || fail "agents.wasm not built"

$TMUX kill-server 2>/dev/null
$OUTER kill-server 2>/dev/null
sleep 0.5

$TMUX -f/dev/null new-session -d -s alpha -x 120 -y 40 \
    "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-session"
sleep 0.5
AGENT=$($TMUX list-panes -t alpha -F '#{pane_id}' | head -1)
[ -n "$AGENT" ] || fail "no pane id"

$TMUX load-plugin -s server -o trust_env=1 -c capture-pane -c run-command \
    -c mode -c db -c env-read -c pane-fds -c fs-read -c fs-list "$WASM" \
    || fail "load-plugin"
sleep 1.0

# The viewer: a pane on another server running a real attach, which gives
# the plugin's server a client with a tty to draw overlays on.
$OUTER -f/dev/null new-session -d -x 120 -y 40 \
    "$TEST_TMUX -Lagents-menu-test attach -t alpha" || fail "outer new-session"
sleep 1.5
VIEW=$($OUTER list-panes -F '#{pane_id}' | head -1)
[ -n "$VIEW" ] || fail "no viewer pane"
$TMUX list-clients -F '#{client_tty}' | grep -q . || fail "no tty client attached"

$TMUX plugin-command agents pick
sleep 2
FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
    awk '/plugin-mode/ { print $1 }')
[ -n "$FORM" ] || fail "picker did not open"
screen | grep -q '1 live' || fail "the agent is not on the roster"
screen | grep -q 'actions' || fail "the footer does not point at the menu"

# --- Space draws the menu -------------------------------------------------

keys Space
seen | grep -q 'jump to pane' || fail "Space drew no menu"
seen | grep -q 'kill pane' || fail "the menu is missing its later items"
seen | grep -q 'claude' || fail "the menu is not titled with the agent"

# The keys in the menu are the picker's own, so it doubles as the
# cheatsheet the footer no longer has room for.
seen | grep -q '(a)' || fail "the archive item does not show its key"

# An action that cannot apply is dimmed: tmux draws a `-` name without
# its key and refuses to select it. This row has no durable id yet, so
# "copy id" must be there but keyless.
seen | grep -q 'copy id' || fail "the copy item is missing entirely"
seen | grep 'copy id' | grep -q '(c)' &&
	fail "copy id offered a key for a row with no durable id"

# --- the menu-key contract ------------------------------------------------

# Every item works by handing its key back. Drive that door directly: it
# is the whole interface between the menu and the picker.
menukey c
screen | grep -q 'no id yet' ||
	fail "menu-key c did not reach the picker: $(screen)"

report "identify claude:$SID"
$TMUX delete-buffer 2>/dev/null
menukey c
BUF=$($TMUX show-buffer 2>/dev/null)
[ "$BUF" = "$SID" ] || fail "menu-key c copied '$BUF', wanted '$SID'"

# Now that the row has an id, the menu offers the key it refused before.
keys Escape
sleep 0.3
$TMUX plugin-command agents pick
sleep 1.5
FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
    awk '/plugin-mode/ { print $1 }')
keys Space
seen | grep 'copy id' | grep -q '(c)' ||
	fail "copy id is still dimmed with a durable id bound: $(seen)"

# The band item moves the row, exactly as w does from the keyboard.
report "needs_input a question"
screen | grep -q 'needs input' || fail "the report did not reach the band"
menukey w
screen | grep -q 'waiting' || fail "menu-key w did not move the row"
screen | grep -q 'needs input' && fail "the row is still in the band"

# An unknown key through the same door is a no-op, not a crash.
menukey Z
screen | grep -q '1 live' || fail "an unknown menu key disturbed the picker"

# --- no picker, no crash --------------------------------------------------

keys Escape
sleep 0.5
menukey c
$TMUX list-sessions >/dev/null 2>&1 ||
	fail "menu-key with no picker killed the server"

cleanup
exit 0
