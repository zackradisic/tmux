#!/bin/sh
# The wheel over the live preview goes to the pane the preview shows, as
# if the pointer were over that pane: with `mouse on` the pane's own
# bindings run, so a plain pane enters copy mode and scrolls, wheeling
# back down leaves copy mode, and an application that asked for mouse
# input gets the event itself. The preview blits the pane's active
# screen, so copy mode is visible in it. Mouse keys are fed in through
# `menu-key <key> <x> <y>`, since a control client has no mouse.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-scroll-test"
[ -z "$WASM" ] && WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm

HOME=$(mktemp -d); export HOME
XDG_DATA_HOME="$HOME/.local/share"; export XDG_DATA_HOME
DEPLOY=$(mktemp -d); cp "$WASM" "$DEPLOY/agents.wasm"
cat >"$DEPLOY/agents.toml" <<'TOML'
[caps]
requests = ["capture-pane","run-command","mode","db","env-read","pane-fds","send-keys"]
[caps.env-read]
names = ["AI_AGENT","OPENCODE"]
TOML

trap 'kill $CTL 2>/dev/null; $TMUX kill-server 2>/dev/null; rm -rf "$HOME" "$DEPLOY"' EXIT
cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$HOME" "$DEPLOY"
}
fail() { echo "FAIL: $*" >&2; echo "--- screen:" >&2; screen >&2; cleanup; exit 1; }
screen() { $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }
shot() { [ -n "$SHOW" ] && { echo "--- $*"; screen; }; }
keys() { $TMUX send-keys -t "$FORM" "$@"; sleep 0.4; }
# A mouse key at cell (x, y) of the picker, by the menu-key back door.
click() { $TMUX plugin-command agents "menu-key $1 $2 $3"; sleep 0.2; }
wheel() { i=0; while [ "$i" -lt "$2" ]; do click "$1" 150 10; i=$((i + 1)); done; sleep 0.7; }
in_mode() { $TMUX display-message -p -t "$1" '#{pane_in_mode}'; }

[ -f "$WASM" ] || fail "agents.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5

# Window 0 holds the picker, with a scrubbed environment so a harness
# variable inherited from whoever runs this test does not make it an
# agent too. The agent in window 1 prints 300 numbered lines, so its
# pane has history well beyond the screen, then waits. The one in
# window 2 asks for mouse input and echoes what it gets, visibly.
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 'env -i PATH=/bin:/usr/bin sleep 600' || fail "new-session"
$TMUX new-window -d -t alpha:1 \
    "sh -c 'AI_AGENT=claude; export AI_AGENT; seq 1 300 | sed s/^/line-/; exec sleep 600'" ||
    fail "new-window"
$TMUX new-window -d -t alpha:2 \
    "sh -c 'AI_AGENT=claude; export AI_AGENT; printf \"\\033[?1000h\"; exec cat -v'" ||
    fail "new-window 2"
$TMUX set -g mouse on
sleep 1
PLAIN=$($TMUX list-panes -t alpha:1 -F '#{pane_id}')
MOUSY=$($TMUX list-panes -t alpha:2 -F '#{pane_id}')

$TMUX load-plugin -s server -o trust_env=1 -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds -c send-keys "$DEPLOY/agents.wasm" \
    || fail "load-plugin"
sleep 1.5

( sleep 0.3; echo 'plugin-command agents pick'; sleep 60 ) |
    $TMUX -C attach -t alpha:0 >/dev/null 2>&1 &
CTL=$!
sleep 1.5
FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' | awk '/plugin-mode/ { print $1 }')
[ -n "$FORM" ] || fail "picker did not open"
i=0
while [ "$i" -lt 20 ]; do
	screen | grep -q 'agents (2 live)' && break
	sleep 0.4; i=$((i + 1))
done

# Put the cursor on the plain pane's row: the one whose preview shows
# numbered lines.
i=0
while [ "$i" -lt 3 ]; do
	screen | grep -q 'line-300' && break
	keys j; i=$((i + 1))
done
shot live
screen | grep -q 'line-300' || fail "no row previews the numbered pane"
[ "$(in_mode "$PLAIN")" = 0 ] || fail "the pane starts out in a mode"

# One notch: the root binding puts the pane in copy mode; more notches
# scroll it (5 lines each by the copy-mode table), so an earlier line
# arrives and the preview, blitting the pane's active screen, shows it.
# (Copy mode's [n/m] badge sits at the pane's far right, outside the
# narrower blit, so it is not looked for.)
wheel WheelUpPane 1
[ "$(in_mode "$PLAIN")" = 1 ] || fail "a wheel notch over the preview did not put the pane in copy mode"
wheel WheelUpPane 6
shot "scrolled"
screen | grep -q 'line-300' && fail "the preview still shows the bottom after scrolling the pane"
screen | grep -q 'line-2[0-4][0-9]' || fail "the preview does not show earlier lines of the pane"

# Wheel down past the bottom: copy-mode -e leaves the mode.
wheel WheelDownPane 12
shot live-again
[ "$(in_mode "$PLAIN")" = 0 ] || fail "wheeling down did not leave copy mode"
screen | grep -q 'line-300' || fail "the preview did not return to the pane's bottom"

# The application that takes the mouse gets the notch itself: cat -v
# echoes the encoded sequence.
keys j
sleep 0.4
screen | grep -q 'line-300' && keys j
wheel WheelUpPane 2
shot mousy
[ "$(in_mode "$MOUSY")" = 0 ] || fail "a pane that takes the mouse was put in copy mode"
$TMUX capture-pane -p -t "$MOUSY" | grep -q '\^\[\[M' || fail "the application did not receive the wheel event"

cleanup
echo "ok"
