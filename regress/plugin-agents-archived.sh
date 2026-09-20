#!/bin/sh
# The archive as a view of its own. The history view (.) mixes archived
# agents into every finished one; the archive key (A) narrows the list to
# the archived rows alone, the search box filters inside that set, and
# content search (C-f) greps an archived agent's saved capture once its
# pane is gone - the capture is taken when the agent is archived. Check:
#
#   A shows only the archived agent (header "archive"); the live one is
#     out of the list;
#   the search box filters within the archive;
#   after the archived agent's pane is killed, the row stays, and a
#     content search finds a token that only its saved capture holds;
#   A again returns to the live roster.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-archived-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm
XDG_DATA_HOME=$(mktemp -d)
export XDG_DATA_HOME

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
keys() { $TMUX send-keys -t "$FORM" "$@"; sleep 0.5; }

open_picker() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	( sleep 0.3; echo 'plugin-command agents pick'; sleep 60 ) |
	    $TMUX -C attach >/dev/null 2>&1 &
	CTL=$!
	sleep 1.5
	FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
	    awk '/plugin-mode/ { print $1 }')
	[ -n "$FORM" ] || fail "picker did not open"
}

load() {
	$TMUX load-plugin -s server -o trust_env=1 -c capture-pane -c run-command -c mode \
	    -c db -c env-read -c pane-fds -c fs-read -c fs-list "$WASM" \
	    || fail "load-plugin"
	sleep 1.0
}

[ -f "$WASM" ] || fail "agents.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5

# Two Claude panes. `alpha` prints a token that lives only in its grid;
# it is the one that gets archived. `beta` stays live.
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 \
    "sh -c 'printf \"NEEDLE_ARCHIVE_777 findme\\n\"; AI_AGENT=claude exec sleep 600'" \
    || fail "new-session alpha"
$TMUX new-session -d -s beta -x 200 -y 50 \
    "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-session beta"
sleep 0.5

load
open_picker
screen | grep -q '2 live' || fail "both agents are not on the roster"

# Archive alpha: narrow to it by name, unfocus, press `a`, clear the query.
keys /
$TMUX send-keys -t "$FORM" alpha; sleep 0.5
screen | grep -q 'beta' && fail "the search did not narrow to alpha"
keys Escape
keys a
screen | grep -q '1 live' || fail "archiving alpha did not drop it"
keys /
keys C-u
keys Escape
screen | grep -q 'beta' || fail "beta is missing from the live roster"

# A: the archive alone. The header says so, alpha is back (tagged), beta
# is not in the list.
keys A
sleep 0.5
screen | grep -q ', archive' || fail "the header does not say archive"
screen | grep -q 'alpha' || fail "the archived agent is missing from the archive view"
screen | grep -q 'archived' || fail "the archived tag is missing"
screen | grep -q 'beta' && fail "a live agent is in the archive view"

# The search box filters within the archive: beta is not there to find,
# alpha is.
keys /
$TMUX send-keys -t "$FORM" beta; sleep 0.5
screen | grep -q '(no agents)' || fail "searching the archive for a live agent found something"
keys C-u
$TMUX send-keys -t "$FORM" alpha; sleep 0.5
screen | grep -q 'alpha' || fail "searching the archive for the archived agent missed"
keys C-u
keys Escape

# Kill alpha's pane. The row ends but stays in the archive; the capture
# taken at archive time now stands in for the pane.
$TMUX kill-session -t alpha || fail "kill-session alpha"
sleep 2.5
screen | grep -q ', archive' || fail "the archive view did not survive the pane going"
screen | grep -q 'alpha' || fail "the ended archived agent left the archive view"

# Content search reaches the saved capture: the token is nowhere in the
# row's metadata, so it misses with content search off and hits with it
# on, showing the captured line as the snippet.
keys /
$TMUX send-keys -t "$FORM" NEEDLE_ARCHIVE; sleep 0.5
screen | grep -q '(no agents)' || fail "a capture-only token matched with content search off"
$TMUX send-keys -t "$FORM" C-f; sleep 0.8
screen | grep -q '(no agents)' && fail "content search did not reach the saved capture"
screen | grep -q 'NEEDLE_ARCHIVE_777' || fail "the capture snippet is missing from the row"
$TMUX send-keys -t "$FORM" C-f; sleep 0.5
keys C-u
keys Escape

# A again: back to the live roster (history was off before).
keys A
sleep 0.5
screen | grep -q ', archive' && fail "A did not leave the archive view"
screen | grep -q '+history' && fail "leaving the archive view left history on"
screen | grep -q 'beta' || fail "the live roster did not come back"

cleanup
exit 0
