#!/bin/sh
# The skill command: the agent guides compiled into the binary. list names
# them, show prints one with its live block expanded for the target pane,
# and install writes a static stub per skill into a harness directory,
# idempotently.

PATH=/bin:/usr/bin
TERM=screen
unset TMUX

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -LtestA$$ -f/dev/null"
$TMUX kill-server 2>/dev/null

TMP=$(mktemp -d)
cleanup()
{
	$TMUX kill-server 2>/dev/null
	rm -rf "$TMP"
}
trap cleanup EXIT

fail()
{
	echo "FAIL: $*" >&2
	exit 1
}

$TMUX new-session -d -s main -x 80 -y 24 || fail "new-session"

# list: the mailbox guide is there, with its description.
$TMUX skill list > "$TMP/list" || fail "skill list"
grep -q '^mailbox	Send a message to another agent' "$TMP/list" ||
    fail "list: $(cat "$TMP/list")"

# show: the markdown comes out whole, headers untouched by the format
# engine, and the live region is expanded: the markers are gone and a
# pane option set on the target shows up as the agent id.
$TMUX set-option -p -t main:0.0 @agent_id claude:skill-test || fail "set @agent_id"
$TMUX skill -t main:0.0 show mailbox > "$TMP/show" || fail "skill show"
grep -q '^# tmux2 cross-agent mailbox' "$TMP/show" || fail "show: no title"
grep -q '^## Send a message' "$TMP/show" || fail "show: headers mangled"
grep -q 'live -->' "$TMP/show" && fail "show: live markers printed"
grep -q 'Your agent id: `claude:skill-test`' "$TMP/show" ||
    fail "show: live block not expanded: $(grep 'agent id' "$TMP/show")"
grep -q 'Linked servers: none' "$TMP/show" || fail "show: links: $(grep 'Linked' "$TMP/show")"
# The prose outside the region keeps its format examples verbatim.
grep -q "#{session_remote_host}" "$TMP/show" || fail "show: code block expanded"

# A pane with no id says so instead of printing nothing.
$TMUX split-window -d -t main:0 || fail "split"
$TMUX skill -t main:0.1 show mailbox | grep -q 'Your agent id: unknown' ||
    fail "show: no fallback for a missing id"

# An unknown skill is an error, not an empty print.
$TMUX skill show nosuch 2>/dev/null && fail "show nosuch succeeded"

# install: one stub per skill under the directory, frontmatter and the
# show command inside; a second run changes nothing.
$TMUX skill -d "$TMP/skills" install > "$TMP/inst1" || fail "skill install"
STUB="$TMP/skills/tmux2-mailbox/SKILL.md"
[ -f "$STUB" ] || fail "no stub: $(cat "$TMP/inst1")"
grep -q ': written$' "$TMP/inst1" || fail "install did not report a write: $(cat "$TMP/inst1")"
grep -q '^name: tmux2-mailbox$' "$STUB" || fail "stub name"
grep -q '^description: Send a message' "$STUB" || fail "stub description"
grep -q 'tmux2 skill -t "\$TMUX_PANE" show mailbox' "$STUB" || fail "stub show command"
$TMUX skill -d "$TMP/skills" install > "$TMP/inst2" || fail "skill install again"
grep -q ': up to date$' "$TMP/inst2" || fail "second install not idempotent: $(cat "$TMP/inst2")"

# Only the named skill, and a bad name fails before touching the directory.
$TMUX skill -d "$TMP/one" install mailbox > /dev/null || fail "install one"
[ -f "$TMP/one/tmux2-mailbox/SKILL.md" ] || fail "install one: no stub"
$TMUX skill -d "$TMP/bad" install nosuch 2>/dev/null && fail "install nosuch succeeded"
[ -d "$TMP/bad" ] && fail "install nosuch created the directory"

exit 0
