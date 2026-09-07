#!/bin/sh
# Content search in the agents roster. Pressing `/` filters on metadata
# (name, kind, status, ...). Pressing the content-search hotkey (C-f)
# ALSO greps the live pane contents - the grid is searched inside tmux
# through `panes_search`, so the pane text never crosses the plugin ABI.
# Check:
#
#   a query that hits only the pane CONTENTS misses while content search
#     is off (the row drops out - "(no agents)");
#   C-f turns content search on (the header shows "find"), and the row
#     returns because its grid holds the needle;
#   the matching line shows as the row's snippet.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-search-test"
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
keys() { $TMUX send-keys -t "$FORM" "$@"; sleep 0.4; }

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
	$TMUX load-plugin -s server -c capture-pane -c run-command -c mode \
	    -c db -c env-read -c pane-fds -c fs-read -c fs-list "$WASM" \
	    || fail "load-plugin"
	sleep 1.0
}

[ -f "$WASM" ] || fail "agents.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5

# A Claude pane that prints a distinctive token, then idles. The token
# lives in the pane's grid, NOT in the row's name or kind, so only a
# content search can find it.
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 \
    "sh -c 'printf \"NEEDLE_XYZZY_123 findme\\n\"; AI_AGENT=claude exec sleep 600'" \
    || fail "new-session"
sleep 0.5

load
open_picker
screen | grep -q '1 live' || fail "the agent is not on the roster"
screen | grep -q 'claude' || fail "the agent row is missing"

# Filter on the token with content search OFF: metadata does not hold it,
# so the row drops out.
keys /
$TMUX send-keys -t "$FORM" xyzzy; sleep 0.4
screen | grep -q '(no agents)' ||
    fail "a content-only token matched with content search off"

# Turn content search ON with C-f: the row returns (plain mode, the pane
# grid holds the token), the header shows the mode, and the match line
# shows as the snippet.
$TMUX send-keys -t "$FORM" C-f; sleep 0.6
screen | grep -q 'plain' || fail "content search did not report plain mode"
screen | grep -q '(no agents)' &&
    fail "content search did not match the pane grid"
screen | grep -q 'NEEDLE_XYZZY_123' ||
    fail "the match snippet is missing from the row"

# Regex mode: a query with metacharacters is auto-detected as regex.
$TMUX send-keys -t "$FORM" C-u; sleep 0.3
$TMUX send-keys -t "$FORM" 'N.*findme'; sleep 0.5
screen | grep -q 'regex' || fail "regex query was not auto-detected"
screen | grep -q '(no agents)' && fail "regex did not match the pane"

# Fuzzy fallback: a plain query that is not a substring but IS an in-order
# subsequence falls back to fuzzy.
$TMUX send-keys -t "$FORM" C-u; sleep 0.3
$TMUX send-keys -t "$FORM" fdm; sleep 0.5
screen | grep -q 'fuzzy' || fail "fuzzy fallback did not engage"
screen | grep -q '(no agents)' && fail "fuzzy did not match the pane"

# Back to a plain miss, then C-f turns content search off: row drops out.
$TMUX send-keys -t "$FORM" C-u; sleep 0.3
$TMUX send-keys -t "$FORM" xyzzy; sleep 0.4
$TMUX send-keys -t "$FORM" C-f; sleep 0.6
screen | grep -q '(no agents)' || fail "C-f did not turn content search off"

cleanup
exit 0
