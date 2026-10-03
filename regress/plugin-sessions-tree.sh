#!/bin/sh
# The sessions chooser on one server: `pick w` shows sessions with their
# windows, `h`/`l` fold and unfold, Enter switches the client, `r`
# renames, `x` twice kills, `@sessions-filter` drops rows, and an
# `@sessions-key-<K>` option runs a command against the highlighted row.
#
# Needs the wasm example built:
#   cargo build -p sessions --target wasm32-unknown-unknown --release
# TEST_WASM_DIR points at another build's output when the binary and the
# plugin come from different trees.

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lsessions-tree-test"
[ -z "$TEST_WASM_DIR" ] && TEST_WASM_DIR=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release
WASM=$TEST_WASM_DIR/sessions.wasm

HOME=$(mktemp -d); export HOME
XDG_DATA_HOME="$HOME/.local/share"; export XDG_DATA_HOME
DEPLOY=$(mktemp -d); cp "$WASM" "$DEPLOY/sessions.wasm"
cat >"$DEPLOY/sessions.toml" <<'TOML'
[caps]
requests = ["read-state","run-command","mode","service-serve","service-call","send-keys"]
[caps.services]
call = ["sessions@*","agents@*"]
TOML

trap 'kill $CTL 2>/dev/null; $TMUX kill-server 2>/dev/null; rm -rf "$HOME" "$DEPLOY"' EXIT
cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$HOME" "$DEPLOY"
}
fail() { echo "FAIL: $*" >&2; echo "--- screen:" >&2; screen >&2; cleanup; exit 1; }
screen() { $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }
# The text of the cursor row.
currow() { $TMUX capture-pane -M -p -t "$FORM" | grep '^▸' | head -1; }
keys() { $TMUX send-keys -t "$FORM" "$@"; sleep 0.4; }
find_form() {
	FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
	    awk '/plugin-mode/ { print $1 }')
}
open_picker() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	( sleep 0.3; echo "plugin-command sessions 'pick $1'"; sleep 60 ) |
	    $TMUX -C attach -t alpha >/dev/null 2>&1 &
	CTL=$!
	sleep 1.5
	find_form
	[ -n "$FORM" ] || fail "picker did not open"
	i=0
	while [ "$i" -lt 15 ]; do
		$TMUX capture-pane -M -p -t "$FORM" | grep -q '▸' && return 0
		sleep 0.4; i=$((i + 1))
	done
	fail "picker did not render a cursor"
}

[ -f "$WASM" ] || fail "sessions.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 "sh -c 'exec sleep 600'" || fail "new-session"
$TMUX new-window -t alpha -n editor "sh -c 'exec sleep 600'"
$TMUX split-window -t alpha:editor "sh -c 'exec sleep 600'"
$TMUX new-session -d -s beta -x 200 -y 50 "sh -c 'exec sleep 600'"
sleep 0.5

$TMUX load-plugin -s server -c read-state -c run-command -c mode \
    -c service-serve -c service-call -c send-keys "$DEPLOY/sessions.wasm" \
    || fail "load-plugin"
sleep 1

# `pick w`: both sessions, alpha's windows under it, the cursor on the
# client's current window.
open_picker w
screen | grep -q 'alpha' || fail "no alpha row"
screen | grep -q 'beta' || fail "no beta row"
screen | grep -q '1: editor' || fail "windows not shown in w mode"
screen | grep -q '2 sessions' || fail "header does not count 2 sessions"
currow | grep -q 'editor' || fail "cursor not on the current window: $(currow)"

# h goes up a level, to the session; f folds the group under the cursor
# and f again unfolds it, the cursor staying on it.
keys h
currow | grep -q 'alpha' || fail "h did not go up to the session: $(currow)"
keys f
screen | grep -q '1: editor' && fail "f did not fold alpha"
keys f
screen | grep -q '1: editor' || fail "f did not unfold alpha"
# l goes down into the first window; j walks the windows; l again shows
# the window's panes as rows and lands on the first.
keys l
currow | grep -q '0:' || fail "l did not go down to the first window: $(currow)"
keys j
currow | grep -q '1: editor' || fail "j did not walk to the next window: $(currow)"
keys l
screen | grep -q '1: sleep' || fail "l on the window did not show its panes"
currow | grep -q '0: sleep' || fail "l did not land on the first pane: $(currow)"

# Z folds every group at the cursor's level: on a session, every
# session; the windows go. Z again brings them back.
keys h; keys h
currow | grep -q 'alpha' || fail "cursor not back on alpha: $(currow)"
keys Z
screen | grep -q '1: editor' && fail "Z did not fold the sessions"
screen | grep -q 'beta' || fail "Z lost the beta row"
keys Z
screen | grep -q '1: editor' || fail "Z did not unfold the sessions"

# The search box narrows: `#be` keeps beta only.
keys /
$TMUX send-keys -t "$FORM" '#be'; sleep 0.4
screen | grep -q 'beta' || fail "filter lost beta"
screen | grep -q 'alpha' && fail "filter kept alpha"
keys Escape
keys C-u 2>/dev/null
keys /; keys C-u; keys Escape

# Enter on beta switches the control client to it, and closes.
keys G
# In w mode the last row is beta's window; Enter there switches to beta too.
currow | grep -qE 'beta|0: ' || fail "G did not reach beta: $(currow)"
keys Enter
sleep 0.6
$TMUX list-clients -F '#{client_session}' | grep -q '^beta$' || fail "Enter did not switch the client to beta: $($TMUX list-clients -F '#{client_session}')"
find_form
[ -z "$FORM" ] || fail "picker still open after Enter"

# Rename the session under the cursor.
$TMUX switch-client -t alpha
open_picker s
screen | grep -q '1: editor' && fail "s mode showed windows"
keys G
currow | grep -q 'beta' || fail "cursor not on beta: $(currow)"
keys r
$TMUX send-keys -t "$FORM" BSpace BSpace BSpace BSpace gamma Enter; sleep 0.8
$TMUX list-sessions -F '#{session_name}' | grep -q '^gamma$' || fail "rename did not take: $($TMUX list-sessions -F '#{session_name}')"
screen | grep -q 'gamma' || fail "renamed row not shown"

# x asks, x again kills the pane under the cursor.
keys g; keys g; keys l; keys j; keys l; keys j
currow | grep -q 'sleep' || fail "cursor not on a pane row: $(currow)"
BEFORE=$($TMUX list-panes -t alpha:editor | wc -l | tr -d ' ')
keys x
screen | grep -q 'again to confirm' || fail "x did not ask"
keys x
sleep 0.8
AFTER=$($TMUX list-panes -t alpha:editor | wc -l | tr -d ' ')
[ "$AFTER" -lt "$BEFORE" ] || fail "x x did not kill the pane ($BEFORE -> $AFTER)"

# A user key: @sessions-key-T runs against the highlighted window.
keys Escape
$TMUX set -g @sessions-key-T "rename-window -t '#{window_id}' tagged"
open_picker w
keys g; keys g; keys l
currow | grep -q '0:' || fail "cursor not on a window row: $(currow)"
keys T
sleep 0.6
$TMUX list-windows -t alpha -F '#{window_name}' | grep -q '^tagged$' || fail "@sessions-key-T did not run: $($TMUX list-windows -t alpha -F '#{window_name}')"
keys Escape

# @sessions-filter drops rows whose expansion is empty or 0.
$TMUX set -g @sessions-filter '#{!=:#{session_name},gamma}'
open_picker s
screen | grep -q 'gamma' && fail "filter did not drop gamma"
screen | grep -q 'alpha' || fail "filter dropped alpha"
screen | grep -q '1 sessions' || fail "header does not count 1 session"
keys Escape

cleanup
exit 0
