#!/bin/sh
# Filter tokens in the search box. `@server`, `#session` and `~dir`
# narrow the roster (prefix, prefix, substring of the ~ path); typing a
# sigil opens a dropdown of the values the roster holds, Tab walks it,
# Enter takes one; a backslash makes the sigil an ordinary word; the
# header counts "n of m live" and shows the tokens; `s` / `S` / `d`
# narrow to the highlighted row's session / server / folder and again
# widen back.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-filters-test"
[ -z "$WASM" ] && WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm

HOME=$(mktemp -d); export HOME
XDG_DATA_HOME="$HOME/.local/share"; export XDG_DATA_HOME
mkdir -p "$HOME/.claude/sessions" "$HOME/proj-x"
DEPLOY=$(mktemp -d); cp "$WASM" "$DEPLOY/agents.wasm"
cat >"$DEPLOY/agents.toml" <<'TOML'
[caps]
requests = ["capture-pane", "run-command", "mode", "db", "env-read",
            "pane-fds", "fs-read", "fs-list"]
[caps.env-read]
names = ["AI_AGENT", "OPENCODE"]
[caps.fs-read]
paths = ["~/.claude/sessions", "~/.claude/projects"]
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
rows() { screen | grep -c 'claude *[0-9]'; }
header() { screen | sed -n 1p; }

[ -f "$WASM" ] || fail "agents.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5

# Session alpha: window 0 hosts the picker (scrubbed env), windows 1 and
# 2 are agents. Session beta: one agent, whose Claude session file gives
# it a working directory under $HOME (the resolver reads cwd from it).
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 'env -i PATH=/bin:/usr/bin sleep 600' || fail "new-session"
$TMUX new-window -d -t alpha:1 "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-window"
$TMUX new-window -d -t alpha:2 "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-window"
$TMUX new-session -d -s beta -x 200 -y 50 "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-session beta"
sleep 0.5
BP=$($TMUX list-panes -t beta -F '#{pane_id}')
BPID=$($TMUX list-panes -t beta -F '#{pane_pid}')
BW=$($TMUX list-panes -t beta -F '#{window_id}')
now=$(date +%s)000
cat >"$HOME/.claude/sessions/$BPID.json" <<EOF
{"pid":$BPID,"sessionId":"1b2c3d4e-0000-4000-8000-000000000001","cwd":"$HOME/proj-x","startedAt":$now,
 "tmux":"beta:$BW.$BP","name":"beta-agent","status":"idle","updatedAt":$now}
EOF

$TMUX load-plugin -s server -o trust_env=1 -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds -c fs-read -c fs-list "$DEPLOY/agents.wasm" \
    || fail "load-plugin"
sleep 1.5

( sleep 0.3; echo 'plugin-command agents pick'; sleep 90 ) |
    $TMUX -C attach -t alpha:0 >/dev/null 2>&1 &
CTL=$!
sleep 1.5
FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' | awk '/plugin-mode/ { print $1 }')
[ -n "$FORM" ] || fail "picker did not open"
i=0
while [ "$i" -lt 15 ]; do
	screen | grep -q '3 live' && break
	sleep 0.4; i=$((i + 1))
done
shot open
[ "$(rows)" -eq 3 ] || fail "expected 3 agent rows, got $(rows)"

# Type "#al": the dropdown offers #alpha (2 rows); Enter takes it.
keys /
keys '#' a l
shot "typing #al"
screen | grep -q '#alpha *2' || fail "no dropdown entry for #alpha with its count"
keys Enter
shot "#alpha"
[ "$(rows)" -eq 2 ] || fail "#alpha did not narrow to 2 rows ($(rows))"
header | grep -q '2 of 3 live' || fail "header does not count 2 of 3 live"
header | grep -q '#alpha' || fail "header does not show the token"
screen | grep -q 'beta' && fail "a beta row survived #alpha"

# Add "@lo" + Tab-walk + Enter: @local, still 2 rows (both filters hold).
keys '@' l o
screen | grep -q '@local' || fail "no dropdown entry for @local"
keys Tab Enter
[ "$(rows)" -eq 2 ] || fail "@local #alpha did not keep 2 rows ($(rows))"
header | grep -q '@local #alpha' || fail "header does not list both tokens"

# Clear; "~proj" completes to the beta agent's ~/proj-x and narrows to it.
keys C-u
keys '~' p r o j
shot "typing ~proj"
screen | grep -q '~/proj-x *1' || fail "no dropdown entry for ~/proj-x"
keys Enter
screen | grep -q 'search ~/proj-x' || fail "the accepted directory token is not ~/proj-x"
[ "$(rows)" -eq 1 ] || fail "~/proj-x did not narrow to 1 row ($(rows))"
screen | grep -q 'beta' || fail "the ~/proj-x row is not the beta agent"

# A bare sigil escaped with a backslash is a word, no dropdown: "\#alpha"
# matches nothing (no name holds that text).
keys C-u
keys '\' '#' a l p h a
shot "escaped"
screen | grep -q '#alpha *[0-9]' && fail "an escaped sigil opened the dropdown"
[ "$(rows)" -eq 0 ] || fail "an escaped token was still applied as a filter ($(rows))"
header | grep -q ' of ' && fail "an escaped token counts as a filter"

# Esc dismisses the dropdown for the token, a second Esc unfocuses.
keys C-u
keys '#' b
screen | grep -q '#beta *1' || fail "no dropdown entry for #beta"
keys Escape
screen | grep -q '#beta *1' && fail "Esc did not dismiss the dropdown"
screen | grep -q 'Esc unfocus' || fail "the first Esc unfocused the box"
keys Escape
screen | grep -q 'Esc unfocus' && fail "the second Esc did not unfocus"
keys C-u 2>/dev/null

# s on a row narrows to its session; s again widens.
keys / C-u Escape
keys g g
keys s
shot "s"
header | grep -q ' of 3 live' || fail "s did not narrow"
keys s
header | grep -q ' of ' && fail "s again did not widen"

# A paste lands in the search box: paste-buffer on the float (the same
# path a bracketed paste from the terminal takes) with a token narrows.
# (Esc from the list would close the picker: clear the box from inside.)
keys / C-u Escape
$TMUX set-buffer '#alpha'
$TMUX paste-buffer -t "$FORM"
sleep 0.6
shot pasted
screen | grep -q 'search #alpha' || fail "a paste did not land in the search box"
[ "$(rows)" -eq 2 ] || fail "the pasted token did not narrow ($(rows))"
keys C-u Escape

# `pick ids` from a pane whose screen shows an agent id (a mailbox
# message, say) opens the picker on that agent.
keys q
sleep 0.5
kill $CTL 2>/dev/null; CTL=
BID=claude:1b2c3d4e-0000-4000-8000-000000000001
$TMUX new-window -d -t alpha:3 "sh -c 'echo Message from $BID via the tmux2 mailbox; exec env -i PATH=/bin:/usr/bin sleep 600'" || fail "new-window 3"
sleep 1
P3=$($TMUX list-panes -t alpha:3 -F '#{pane_id}')
( sleep 0.3; echo "plugin-command -t $P3 agents 'pick ids'"; sleep 90 ) |
    $TMUX -C attach -t alpha:0 >/dev/null 2>&1 &
CTL=$!
sleep 2
FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' | awk '/plugin-mode/ { print $1 }')
[ -n "$FORM" ] || fail "pick ids did not open the picker"
sleep 0.5
shot "pick ids"
screen | grep '▸' | grep -q 'beta' || fail "pick ids did not land on the agent named on screen"

# `open <id>` with no one linked in opens locally, like pick id: the
# form the copy-mode Enter script uses on every host.
keys q
sleep 0.5
kill $CTL 2>/dev/null; CTL=
( sleep 0.3; echo "plugin-command -t $P3 agents 'open $BID'"; sleep 90 ) |
    $TMUX -C attach -t alpha:0 >/dev/null 2>&1 &
CTL=$!
sleep 2
FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' | awk '/plugin-mode/ { print $1 }')
[ -n "$FORM" ] || fail "open <id> did not open the picker"
sleep 0.5
screen | grep '▸' | grep -q 'beta' || fail "open <id> did not land on the agent"

# ? shows the quick reference in the preview column; Esc puts it away.
keys '?'
shot help
screen | grep -q 'quick reference' || fail "? did not show the quick reference"
screen | grep -q '#session' || fail "the reference does not mention the session token"
keys Escape
screen | grep -q 'quick reference' && fail "Esc did not put the reference away"

cleanup
echo ok
