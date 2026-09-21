#!/bin/sh
# The session creator form: plain and worktree kinds. Check:
#
#   `new` prefills folder from the pane's cwd and name from its basename;
#   the folder field completes from a scan of the directory being typed
#     in, filtered by the fragment, and Tab takes the highlighted row;
#   C-t swaps to worktree in place, detecting the repo root (✓) and
#     mirroring dest and branch from name; C-t again comes back;
#   Enter creates a session rooted in the folder and closes the form;
#   a name that exists is refused with an error, the form stays;
#   a missing folder asks for a second Enter, then is created;
#   `worktree` runs git worktree add and roots the session in it.
#
# Needs the wasm example built:
#   cargo build -p session_creator --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lsession-creator-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/session_creator.wasm

# A short path: the form clips long values from the left, and the checks
# below match whole paths.
HOME=$(mktemp -d /tmp/sct.XXXXXX); HOME=$(cd "$HOME" && pwd -P); export HOME
XDG_DATA_HOME="$HOME/.local/share"; export XDG_DATA_HOME
WORK="$HOME/work"

trap 'kill $CTL 2>/dev/null; $TMUX kill-server 2>/dev/null; rm -rf "$HOME"' EXIT
cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$HOME"
}
fail() { echo "FAIL: $*" >&2; echo "--- screen:" >&2; screen >&2; cleanup; exit 1; }
screen() { $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }
keys() { $TMUX send-keys -t "$FORM" "$@"; sleep 0.5; }
type_text() { $TMUX send-keys -t "$FORM" -l -- "$1"; sleep 0.7; }
form_pane() {
	$TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
	    awk '/plugin-mode/ { print $1 }' | head -1
}
# Open the form with a verb, from a control client on alpha (its current
# pane is the target, so the cwd prefill comes from there).
open_form() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	( sleep 0.3; echo "plugin-command session_creator $1"; sleep 60 ) |
	    $TMUX -C attach -t alpha >/dev/null 2>&1 &
	CTL=$!
	i=0
	while [ "$i" -lt 20 ]; do
		FORM=$(form_pane)
		[ -n "$FORM" ] && break
		sleep 0.3; i=$((i + 1))
	done
	[ -n "$FORM" ] || fail "form did not open for '$1'"
	sleep 0.8
}
# Wait for the form to be gone (a successful Enter closes it).
wait_closed() {
	i=0
	while [ "$i" -lt 20 ]; do
		[ -z "$(form_pane)" ] && return 0
		sleep 0.3; i=$((i + 1))
	done
	fail "the form did not close"
}

[ -f "$WASM" ] || fail "session_creator.wasm not built"

# A repo with a commit (worktree add needs a HEAD) and two plain dirs.
mkdir -p "$WORK/proj-a" "$WORK/proj-b" "$WORK/proj-c"
( cd "$WORK/proj-a" && git init -q && git -c user.name=t -c user.email=t@t \
    commit -q --allow-empty -m init ) || fail "git init"

$TMUX kill-server 2>/dev/null
sleep 0.5
$TMUX -f/dev/null new-session -d -s alpha -x 160 -y 50 -c "$WORK/proj-a" \
    || fail "new-session"
sleep 0.3
$TMUX load-plugin -s server -c mode -c run-process -c run-command -c fs-list \
    -c fs-read-any "$WASM" || fail "load-plugin"
sleep 1

# --- plain: prefill --------------------------------------------------------
open_form new
screen | grep -q '\[plain\]' || fail "the form did not open as plain"
screen | grep -q "folder *$WORK/proj-a" || fail "folder was not prefilled from the pane cwd"
screen | grep -q 'name *proj-a' || fail "name did not mirror the folder basename"

# --- completion on the folder field ---------------------------------------
# Focus opens on name (folder is prefilled); Up moves to folder. Moving
# there does NOT pop the list - a field move never lands in a dropdown.
keys Up
sleep 0.8
screen | grep -q "in $WORK" && fail "moving to the folder field popped the list"
# Tab shows it, filtered by the fragment "proj-a": only proj-a matches.
keys Tab
sleep 0.8
screen | grep -q "in $WORK" || fail "Tab did not show the folder list"
screen | grep -q 'proj-b' && fail "the fragment proj-a let proj-b through"
# Backspace to "proj-": all three show.
keys BSpace
sleep 0.8
screen | grep -q 'proj-b' || fail "proj-b is missing from the list"
screen | grep -q 'proj-c' || fail "proj-c is missing from the list"
# "b" narrows to proj-b; Tab takes it and name follows.
type_text b
screen | grep -q 'proj-c' && fail "the fragment proj-b let proj-c through"
keys Tab
screen | grep -q "folder *$WORK/proj-b" || fail "Tab did not complete the folder"
screen | grep -q 'name *proj-b' || fail "name did not follow the completed folder"

# --- C-t: worktree in place, repo detected --------------------------------
# Back to proj-a first (a repo), so the swap has a root to detect.
keys C-u
type_text "$WORK/proj-a"
keys C-t
sleep 1
screen | grep -q '\[worktree\]' || fail "C-t did not swap to worktree"
screen | grep -q "repo *$WORK/proj-a ✓" || fail "the repo root was not detected"
screen | grep -q "dest *$WORK/proj-a-worktrees/proj-a" || fail "dest did not mirror repo and name"
screen | grep -q 'branch *proj-a' || fail "branch did not mirror name"
keys C-t
screen | grep -q '\[plain\]' || fail "C-t did not swap back to plain"
screen | grep -q "folder *$WORK/proj-a" || fail "folder did not come back from repo"

# --- Enter creates the session --------------------------------------------
keys Enter
wait_closed
$TMUX list-sessions -F '#{session_name}' | grep -qx 'proj-a' || fail "session proj-a was not created"
[ "$($TMUX display-message -p -t proj-a: '#{pane_current_path}')" = "$WORK/proj-a" ] ||
    fail "session proj-a is not rooted in the folder"

# --- a duplicate name is refused; a missing folder is created on the second Enter
open_form new
# Clearing name leaves it empty with the default as a dim placeholder;
# Enter uses that default, which is the session that already exists.
keys C-u
$TMUX capture-pane -M -p -e -t "$FORM" | grep -qE "\[[0-9;]*2mproj-a" ||
    fail "an emptied name did not show its default as a dim placeholder"
keys Enter
sleep 0.5
screen | grep -q "already exists" || fail "a duplicate name was not refused"
[ -n "$(form_pane)" ] || fail "the form closed on an error"
keys Up
keys C-u
type_text "$WORK/newdir"
screen | grep -q 'name *newdir' || fail "name did not follow the typed folder"
keys Enter
sleep 0.8
screen | grep -q 'does not exist' || fail "a missing folder was not asked about"
keys Enter
wait_closed
[ -d "$WORK/newdir" ] || fail "the folder was not created"
$TMUX list-sessions -F '#{session_name}' | grep -qx 'newdir' || fail "session newdir was not created"

# --- worktree: git worktree add, session rooted in it --------------------
open_form worktree
screen | grep -q '\[worktree\]' || fail "the form did not open as worktree"
screen | grep -q "repo *$WORK/proj-a ✓" || fail "worktree did not detect the repo"
type_text feat
screen | grep -q "dest *$WORK/proj-a-worktrees/feat" || fail "dest did not follow name"
screen | grep -q 'branch *feat' || fail "branch did not follow name"
keys Enter
wait_closed
git -C "$WORK/proj-a" worktree list | grep -q "proj-a-worktrees/feat" || fail "git worktree was not added"
git -C "$WORK/proj-a-worktrees/feat" symbolic-ref --short HEAD | grep -qx feat || fail "the worktree is not on branch feat"
$TMUX list-sessions -F '#{session_name}' | grep -qx 'feat' || fail "session feat was not created"
[ "$($TMUX display-message -p -t feat: '#{pane_current_path}')" = "$WORK/proj-a-worktrees/feat" ] ||
    fail "session feat is not rooted in the worktree"

cleanup
exit 0
