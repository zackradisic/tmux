#!/bin/sh
# The new-agent form: `n` in the picker opens a form over it, prefilled
# from the highlighted row - its session, its pane's folder, its harness -
# and Enter starts another agent. Check:
#
#   n opens the form as [window] with session, folder, name and command
#     prefilled from the row; name follows the folder's basename;
#   the command field completes from the harness list;
#   Enter makes a window in that session, rooted in the folder, running
#     the command; both floats close; the roster shows the new agent;
#   C-t cycles window -> session -> worktree -> window, carrying command
#     and remembering the session across the session kind;
#   the session kind makes a new session rooted in the folder;
#   the worktree kind detects the repo, mirrors dest and branch from name,
#     adds the worktree and puts the window in the session rooted in it.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-new-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm

# A short, resolved path: the form clips long values, and tmux reports
# pane paths resolved (/tmp is a link on macOS).
HOME=$(mktemp -d /tmp/agn.XXXXXX); HOME=$(cd "$HOME" && pwd -P); export HOME
XDG_DATA_HOME="$HOME/.local/share"; export XDG_DATA_HOME
WORK="$HOME/work"
DEPLOY=$(mktemp -d); cp "$WASM" "$DEPLOY/agents.wasm"
cat >"$DEPLOY/agents.toml" <<'TOML'
[caps]
requests = ["capture-pane","run-command","mode","db","env-read","pane-fds","send-keys","run-process","fs-list","fs-read-any"]
[caps.env-read]
names = ["AI_AGENT","OPENCODE"]
TOML

# The fake agent: a pane whose process carries the marker and echoes.
AGENT="sh -c 'AI_AGENT=claude exec cat'"

trap 'kill $CTL 2>/dev/null; $TMUX kill-server 2>/dev/null; rm -rf "$HOME" "$DEPLOY"' EXIT
cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$HOME" "$DEPLOY"
}
fail() { echo "FAIL: $*" >&2; echo "--- picker:" >&2; screen >&2; echo "--- form:" >&2; fscreen >&2; cleanup; exit 1; }
screen() { [ -n "$FORM" ] && $TMUX capture-pane -M -p -t "$FORM" 2>/dev/null | sed '/^ *$/d'; }
fscreen() { [ -n "$NEWF" ] && $TMUX capture-pane -M -p -t "$NEWF" 2>/dev/null | sed '/^ *$/d'; }
keys() { $TMUX send-keys -t "$FORM" "$@"; sleep 0.4; }
fkeys() { $TMUX send-keys -t "$NEWF" "$@"; sleep 0.5; }
ftype() { $TMUX send-keys -t "$NEWF" -l -- "$1"; sleep 0.7; }
modes() { $TMUX list-panes -a -F '#{pane_id} #{pane_mode}' | awk '/plugin-mode/ { print $1 }'; }
open_picker() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	( sleep 0.3; echo 'plugin-command agents pick'; sleep 90 ) |
	    $TMUX -C attach -t alpha >/dev/null 2>&1 &
	CTL=$!
	sleep 1.5
	FORM=$(modes | head -1)
	[ -n "$FORM" ] || fail "picker did not open"
	i=0
	while [ "$i" -lt 15 ]; do
		$TMUX capture-pane -M -p -t "$FORM" | grep -q '▸' && return 0
		sleep 0.4; i=$((i + 1))
	done
	fail "picker did not render a cursor"
}
# n in the picker: the form is the plugin-mode pane that is not the picker.
open_form() {
	keys n
	i=0
	while [ "$i" -lt 20 ]; do
		NEWF=$(modes | grep -v "^$FORM\$" | head -1)
		[ -n "$NEWF" ] && break
		sleep 0.3; i=$((i + 1))
	done
	[ -n "$NEWF" ] || fail "n did not open the form"
	sleep 0.8
}
wait_closed() {
	i=0
	while [ "$i" -lt 25 ]; do
		[ -z "$(modes)" ] && return 0
		sleep 0.3; i=$((i + 1))
	done
	fail "the floats did not close"
}
# Point the command field at the fake agent (the default, claude, is not
# installed here and a window whose command exits at once is gone).
set_command() {
	fkeys C-u
	ftype "$AGENT"
}

[ -f "$WASM" ] || fail "agents.wasm not built"

mkdir -p "$WORK/proj-a"
( cd "$WORK/proj-a" && git init -q && git -c user.name=t -c user.email=t@t \
    commit -q --allow-empty -m init ) || fail "git init"

$TMUX kill-server 2>/dev/null
sleep 0.5
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 -c "$WORK/proj-a" "$AGENT" \
    || fail "new-session"
sleep 0.5
# One launcher, "fake", standing for the fake agent: the command field
# completes it and Enter expands it.
$TMUX load-plugin -s server -o trust_env=1 -o "launch=fake = $AGENT" \
    -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds -c send-keys -c run-process -c fs-list -c fs-read-any \
    "$DEPLOY/agents.wasm" || fail "load-plugin"
sleep 1.5

open_picker
screen | grep -q '1 live' || fail "expected 1 live agent"
screen | grep -q 'n new' || fail "the footer does not offer n"

# --- n: the form, prefilled from the row -----------------------------------
open_form
fscreen | grep -q '\[window\]' || fail "the form did not open as window"
fscreen | grep -q 'session *alpha' || fail "session was not prefilled from the row"
fscreen | grep -q "folder *$WORK/proj-a" || fail "folder was not prefilled from the pane cwd"
fscreen | grep -q 'name *proj-a' || fail "name did not follow the folder"
fscreen | grep -q 'command *claude' || fail "command was not prefilled from the harness"
fscreen | grep -q 'Enter start' || fail "the hint does not say start"

# name has the focus (everything else is prefilled); a suffix, then the
# command field, whose list is the harness list.
ftype "-two"
fscreen | grep -q 'name *proj-a-two' || fail "typing did not reach name"
fkeys C-j
sleep 0.8
# Moving to the field does not pop its list; Tab does, filtered on the
# field's value: "claude" matches one of four.
fscreen | grep -q 'launchers' && fail "moving to the command field popped the list"
fkeys Tab
sleep 0.8
fscreen | grep -q 'launchers' || fail "Tab did not show the launcher list"
fscreen | grep -q '1 of 5' || fail "the launcher list was not filtered by the value"
fscreen | grep -q 'Enter take' || fail "the hint does not say Enter takes the row"
# Enter on the highlighted row takes it and closes the list; the form
# stays up.
fkeys Enter
fscreen | grep -q 'launchers' && fail "Enter did not close the list"
[ -n "$(modes | grep -v "^$FORM\$")" ] || fail "Enter on a highlighted row submitted the form"
fscreen | grep -q 'command *claude' || fail "the taken row is not in the field"
# C-j from a shown list still moves to the next field, and back.
fkeys Tab
fkeys C-j
fscreen | grep -q 'launchers' && fail "C-j went into the list instead of the next field"
fkeys C-k
fkeys C-u
fscreen | grep -q 'codex' || fail "codex is missing from the launcher list"
fscreen | grep -q "fake *sh -c" || fail "the launcher is not listed with its line"
ftype "$AGENT"

# --- Enter: a window in the session, running the command -------------------
fkeys Enter
wait_closed
$TMUX list-windows -t alpha -F '#{window_name}' | grep -qx 'proj-a-two' ||
    fail "no window proj-a-two in alpha: $($TMUX list-windows -t alpha -F '#{window_name}')"
[ "$($TMUX display-message -p -t alpha:proj-a-two '#{pane_current_path}')" = "$WORK/proj-a" ] ||
    fail "the new window is not rooted in the folder"
[ "$($TMUX display-message -p -t alpha:proj-a-two '#{pane_current_command}')" = "cat" ] ||
    fail "the new window is not running the command"
open_picker
# The new pane is classified asynchronously (env, fds, session files):
# give the roster a moment to show it.
i=0
while [ "$i" -lt 20 ]; do
	screen | grep -q '2 live' && break
	sleep 0.4; i=$((i + 1))
done
screen | grep -q '2 live' || fail "the roster did not pick up the new agent"

# --- C-t cycles the kinds, carrying command and remembering the session ----
open_form
fkeys C-j; set_command
fkeys C-t
fscreen | grep -q '\[session\]' || fail "C-t did not swap to session"
fscreen | grep -q 'session *alpha' && fail "the session kind still shows a session field"
fscreen | grep -q "command *sh -c" || fail "command did not travel to the session kind"
fkeys C-t
sleep 1
fscreen | grep -q '\[worktree\]' || fail "C-t did not swap to worktree"
fscreen | grep -q "repo *$WORK/proj-a ✓" || fail "the repo root was not detected"
fscreen | grep -q 'session *alpha' || fail "the session was not remembered across the session kind"
fscreen | grep -q "command *sh -c" || fail "command did not travel to the worktree kind"
fkeys C-t
fscreen | grep -q '\[window\]' || fail "C-t did not come back to window"

# --- the session kind: a new session rooted in the folder -----------------
# This one runs the launcher by name: the status says what it expands to.
fkeys C-t
fscreen | grep -q '\[session\]' || fail "C-t did not reach session again"
fkeys C-u
ftype "fake"
fscreen | grep -q "runs: sh -c" || fail "the launcher expansion is not shown: $(fscreen)"
# The focus followed the command field through the swaps; name is the
# field above it.
fkeys C-k
fkeys C-u
ftype "beta"
fscreen | grep -q 'name *beta' || fail "name did not take beta: $(fscreen)"
fkeys Enter
wait_closed
$TMUX list-sessions -F '#{session_name}' | grep -qx 'beta' || fail "session beta was not created"
[ "$($TMUX display-message -p -t beta: '#{pane_current_path}')" = "$WORK/proj-a" ] ||
    fail "session beta is not rooted in the folder"
[ "$($TMUX display-message -p -t beta: '#{pane_current_command}')" = "cat" ] ||
    fail "the launcher did not expand to the fake agent"

# --- the worktree kind: add the tree, window in the session ---------------
sleep 1
open_picker
open_form
fkeys C-j; set_command
fkeys C-t; fkeys C-t
sleep 1
fscreen | grep -q '\[worktree\]' || fail "did not reach the worktree kind"
fkeys C-k; fkeys C-k; fkeys C-k; fkeys C-k
fkeys C-u
ftype "feat"
fscreen | grep -q 'name *feat' || fail "name did not take feat: $(fscreen)"
fscreen | grep -q "dest *$WORK/proj-a-worktrees/feat" || fail "dest did not follow name"
fscreen | grep -q 'branch *feat' || fail "branch did not follow name"
fkeys Enter
wait_closed
git -C "$WORK/proj-a" worktree list | grep -q "proj-a-worktrees/feat" || fail "the worktree was not added"
$TMUX list-windows -t alpha -F '#{window_name}' | grep -qx 'feat' || fail "no window feat in alpha"
[ "$($TMUX display-message -p -t alpha:feat '#{pane_current_path}')" = "$WORK/proj-a-worktrees/feat" ] ||
    fail "the feat window is not rooted in the worktree"

cleanup
exit 0
