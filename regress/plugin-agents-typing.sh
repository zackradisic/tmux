#!/bin/sh
# Typing into the preview. `l` (or Right, or a click on the preview) hands
# the keyboard to the preview: every key then goes to the highlighted
# agent's pane, the list's own keys included, until `C-]` (or a click on
# the list) takes it back. The mouse works on the list too: a click
# selects, a double click jumps, the wheel scrolls. Mouse keys are fed in
# through `menu-key <key> <x> <y>`, since a control client has no mouse.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-typing-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm

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
# The screen line the cursor marker sits on.
curline() { $TMUX capture-pane -M -p -t "$FORM" | grep -n '▸' | head -1 | cut -d: -f1; }
keys() { $TMUX send-keys -t "$FORM" "$@"; sleep 0.4; }
# A mouse key at cell (x, y) of the picker, by the menu-key back door.
click() { $TMUX plugin-command agents "menu-key $1 $2 $3"; sleep 0.5; }
# What the agent panes show: the typed keys echo in exactly one of them.
agents() { $TMUX capture-pane -p -t "$A0"; $TMUX capture-pane -p -t "$A1"; }
open_picker() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	( sleep 0.3; echo 'plugin-command agents pick'; sleep 60 ) |
	    $TMUX -C attach >/dev/null 2>&1 &
	CTL=$!
	sleep 1.5
	FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
	    awk '/plugin-mode/ { print $1 }')
	[ -n "$FORM" ] || fail "picker did not open"
	i=0
	while [ "$i" -lt 15 ]; do
		$TMUX capture-pane -M -p -t "$FORM" | grep -q '▸' && return 0
		sleep 0.4; i=$((i + 1))
	done
	fail "picker did not render a cursor"
}

[ -f "$WASM" ] || fail "agents.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5
# Two agents that echo what they are sent: the tty echoes each key, and
# cat repeats the line on Enter.
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 \
    "sh -c 'AI_AGENT=claude exec cat'" || fail "new-session"
$TMUX split-window -h -t alpha "sh -c 'AI_AGENT=claude exec cat'"
sleep 0.5
A0=$($TMUX list-panes -t alpha -F '#{pane_id}' | sed -n 1p)
A1=$($TMUX list-panes -t alpha -F '#{pane_id}' | sed -n 2p)

$TMUX load-plugin -s server -o trust_env=1 -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds -c send-keys "$DEPLOY/agents.wasm" \
    || fail "load-plugin"
sleep 1.5

open_picker
screen | grep -q '2 live' || fail "expected 2 live agents"
screen | grep -q 'l type' || fail "the footer does not offer typing"
screen | grep -q '\. history' || fail "the footer does not name the history key"
# Start from the top row: the picker opens on the "you are here" row,
# which is whichever of the two agents sorted second.
keys g; keys g
TOP=$(curline)

# --- l hands the keyboard to the preview ----------------------------------
keys l
screen | grep -q 'typing into' || fail "l did not focus the preview"
screen | grep -q 'back to list' || fail "the typing footer does not say how to get back"

# Typed keys reach the pane, the list's own keys included: `j` is text
# now, and the cursor stays put.
$TMUX send-keys -t "$FORM" T Y P E D j 1 2; sleep 0.6
agents | grep -q 'TYPEDj12' || fail "typed keys did not reach the agent's pane: $(agents)"
[ "$(curline)" = "$TOP" ] || fail "j moved the cursor while typing"
# Enter submits: cat echoes the line back.
keys Enter
sleep 0.3
[ "$(agents | grep -c 'TYPEDj12')" -ge 2 ] || fail "Enter did not reach the pane: $(agents)"
# Esc is the agent's too (Claude uses it), not the picker's.
keys Escape
screen | grep -q 'typing into' || fail "Esc left the preview (it belongs to the agent)"

# --- C-] takes it back ----------------------------------------------------
keys C-]
screen | grep -q 'typing into' && fail "C-] did not unfocus the preview"
screen | grep -q 'j/k move' || fail "the list footer did not come back"
keys j
[ "$(curline)" != "$TOP" ] || fail "j did not move the cursor after C-]"
keys k
[ "$(curline)" = "$TOP" ] || fail "k did not move the cursor back"

# --- Right is l ------------------------------------------------------------
keys Right
screen | grep -q 'typing into' || fail "Right did not focus the preview"
keys C-]

# --- a directional select-pane on the float is the picker's --------------
# tmux hands it to the plugin as mode-nav instead of stepping off the
# float, so `prefix h`-style bindings work while every plain key is the
# agent's: left leaves the preview, right enters it, up/down move the
# highlight without dropping it.
keys g; keys g
$TMUX select-pane -R -t "$FORM"; sleep 0.4
screen | grep -q 'typing into' || fail "select-pane -R did not focus the preview"
$TMUX select-pane -D -t "$FORM"; sleep 0.4
[ "$(curline)" != "$TOP" ] || fail "select-pane -D did not move the highlight"
screen | grep -q 'typing into' || fail "select-pane -D dropped the preview focus"
$TMUX select-pane -U -t "$FORM"; sleep 0.4
[ "$(curline)" = "$TOP" ] || fail "select-pane -U did not move the highlight back"
$TMUX select-pane -L -t "$FORM"; sleep 0.4
screen | grep -q 'typing into' && fail "select-pane -L did not take the keyboard back"
$TMUX list-panes -a -F '#{pane_mode}' | grep -q 'plugin-mode' ||
    fail "select-pane on the float closed or left it"
[ "$($TMUX display-message -p -t alpha '#{pane_id}')" = "$FORM" ] ||
    fail "select-pane moved off the float"

# --- the mouse -------------------------------------------------------------
# The picker is 180 wide here (9/10 of 200, capped), so the list takes
# 108 columns and the preview starts past it. A click on the preview
# starts typing; a click on a row of the list selects it and stops.
click MouseDown1Pane 150 5
screen | grep -q 'typing into' || fail "a click on the preview did not focus it"
$TMUX send-keys -t "$FORM" C L I C K; sleep 0.6
agents | grep -q 'CLICK' || fail "keys after a click did not reach the pane"

# The rows: screen lines (1-based) that name an agent. Click the second.
ROW2=$($TMUX capture-pane -M -p -t "$FORM" | grep -n 'claude' | sed -n 2p | cut -d: -f1)
[ -n "$ROW2" ] || fail "no second agent row on screen"
click MouseDown1Pane 10 $((ROW2 - 1))
screen | grep -q 'typing into' && fail "a click on the list did not take the keyboard back"
[ "$(curline)" = "$ROW2" ] || fail "the click did not select the row it landed on ($(curline), want $ROW2)"

# The wheel moves the cursor.
click WheelUpPane 10 5
[ "$(curline)" = "$TOP" ] || fail "the wheel did not move the cursor up ($(curline), want $TOP)"
click WheelDownPane 10 5
[ "$(curline)" = "$ROW2" ] || fail "the wheel did not move the cursor down"

# A click on the search line focuses the box.
click MouseDown1Pane 10 1
screen | grep -q 'Esc unfocus' || fail "a click on the search line did not focus it"
keys Escape

# `.` is the history key now that `h` is a direction.
keys .
sleep 0.4
screen | grep -q '+history' || fail ". did not toggle history"
keys .
sleep 0.4
screen | grep -q '+history' && fail ". did not toggle history back off"
keys h
screen | grep -q '+history' && fail "h still toggles history"

# A double click jumps: the picker closes on the clicked row's pane.
click DoubleClick1Pane 10 $((ROW2 - 1))
sleep 0.5
$TMUX list-panes -a -F '#{pane_mode}' | grep -q 'plugin-mode' &&
    fail "a double click did not close the picker"

cleanup
exit 0
