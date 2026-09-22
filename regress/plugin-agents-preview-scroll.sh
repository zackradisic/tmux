#!/bin/sh
# Scrolling the live preview. The preview of a local agent is a blit of
# its pane; the wheel over it scrolls into the pane's history, the row
# under it says how far back the view is, wheel down returns to live, and
# a key typed into the pane snaps it back to live. Mouse keys are fed in
# through `menu-key <key> <x> <y>`, since a control client has no mouse.
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
click() { $TMUX plugin-command agents "menu-key $1 $2 $3"; sleep 0.15; }
wheel() { i=0; while [ "$i" -lt "$2" ]; do click "$1" 150 10; i=$((i + 1)); done; sleep 0.5; }

[ -f "$WASM" ] || fail "agents.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5

# Window 0 holds the picker, with a scrubbed environment so a harness
# variable inherited from whoever runs this test does not make it an
# agent too; the agent in window 1 prints 300 numbered lines, so its pane
# has history well beyond the screen, then waits.
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 'env -i PATH=/bin:/usr/bin sleep 600' || fail "new-session"
$TMUX new-window -d -t alpha:1 \
    "sh -c 'AI_AGENT=claude; export AI_AGENT; seq 1 300 | sed s/^/line-/; exec sleep 600'" ||
    fail "new-window"
sleep 1

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
	screen | grep -q 'line-300' && break
	sleep 0.4; i=$((i + 1))
done
shot live
screen | grep -q 'line-300' || fail "the live preview does not show the pane's bottom"
screen | grep -q 'line-200' && fail "line-200 is on the live preview: the pane is not as tall as this test assumes"
screen | grep -q 'lines back' && fail "the live preview claims to be scrolled"

# Ten notches up: 30 lines back. The bottom line leaves, an earlier one arrives.
wheel WheelUpPane 10
shot "30 back"
screen | grep -q '30 lines back' || fail "the preview did not report 30 lines back"
screen | grep -q 'line-300' && fail "the bottom line is still on a scrolled preview"
screen | grep -q 'line-270' || fail "line-270 did not scroll into view"

# Ten notches down: live again, no note.
wheel WheelDownPane 10
shot live-again
screen | grep -q 'lines back' && fail "the preview still reports a scroll after wheeling back down"
screen | grep -q 'line-300' || fail "the preview did not return to the pane's bottom"

# Scroll up, then type into the pane: the preview snaps back to live.
wheel WheelUpPane 5
screen | grep -q '15 lines back' || fail "the preview did not report 15 lines back"
click MouseDown1Pane 150 10
sleep 0.4
screen | grep -q 'typing into' || fail "a click on the preview did not focus it"
keys x
sleep 0.6
shot typed
screen | grep -q 'lines back' && fail "typing into the pane did not return the preview to live"
screen | grep -q 'line-300' || fail "the preview is not live after typing"

# Past the top: the offset clamps to the history there is (~250 lines
# above a 49-row preview), and the count says where it settled.
wheel WheelUpPane 120
shot clamped
screen | grep -q 'line-1$' || fail "wheeling past the top did not reach the first line"
n=$(screen | sed -n 's/.*↑ \([0-9]*\) lines back.*/\1/p' | head -1)
[ -n "$n" ] && [ "$n" -lt 360 ] || fail "the offset did not clamp (reported '$n')"

cleanup
echo "ok"
