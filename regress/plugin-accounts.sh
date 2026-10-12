#!/bin/sh
# The accounts plugin: several Claude Code logins per machine. Against a
# fake home (the plugin's `home` config) with file credentials, so the
# machine's real login and Keychain are never touched:
#
#   - the login is captured into a slot, a second login into another;
#   - a switch puts the first slot's credential and identity back in
#     place under claude's own lock directories, keeps the machine-wide
#     keys (mcpOAuth) from the live credential, leaves the file 0600 and
#     the locks removed, and backs the outgoing login up into its slot;
#   - a stale lock directory is stolen, a live one refuses the switch;
#   - over a link, the panel's provider on the remote lists and
#     switches that server's own slots.
#
# Needs the wasm example built:
#   sh scripts/build-plugins.sh accounts

. ./remote-common.inc

# PLUGIN_OUT names the build directory when the tmux under test is not
# the tree's own (an installed binary).
OUT=${PLUGIN_OUT:-$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release}
WASM=$OUT/accounts.wasm
[ -f "$WASM" ] || fail "no $WASM"

XDG_A="$TMP/a"
XDG_B="$TMP/b"
HOME_A="$TMP/home-a"
HOME_B="$TMP/home-b"
mkdir -p "$XDG_A" "$XDG_B" "$HOME_A/.claude" "$HOME_B/.claude"

# A fake login: identity in .claude.json, credential in the file.
login()
{
	# login <home> <email> <org> <tag>
	far=$(( $(date +%s) * 1000 + 86400000 ))
	cat > "$1/.claude.json" <<JSON
{"hasCompletedOnboarding":true,"theme":"dark","projects":{"/x":{"allowedTools":[]}},
 "oauthAccount":{"accountUuid":"uuid-$4","emailAddress":"$2","organizationUuid":"org-$4","organizationName":"$3"}}
JSON
	cat > "$1/.claude/.credentials.json" <<JSON
{"claudeAiOauth":{"accessToken":"at-$4","refreshToken":"rt-$4","expiresAt":$far,"scopes":["user:inference"],"subscriptionType":"max"},"mcpOAuth":{"server":"mcp-of-$4"}}
JSON
	chmod 600 "$1/.claude/.credentials.json"
}

CAPS="-c db -c run-process -c run-command -c mode -c fs-read -c fs-read-any -c fs-write -c fs-list -c service-serve -c service-call -c display-message"

XDG_DATA_HOME="$XDG_B" $TMUX2 new-session -d -s work -x 80 -y 24 || fail "new-session on B"
XDG_DATA_HOME="$XDG_A" $TMUX new-session -d -s local -x 80 -y 24 || fail "new-session on A"
$TMUX set -s remote-ssh-command \
    "$TEST_TMUX -LtestB$$ -f/dev/null #{remote_command}" || fail "set remote-ssh-command"
# B loads its own copy (the pushed one would lack run-process), against
# its own fake home.
XDG_DATA_HOME="$XDG_B" $TMUX2 load-plugin -s server $CAPS -o home="$HOME_B" -o credentials=file "$WASM" ||
    fail "load-plugin on B"
XDG_DATA_HOME="$XDG_A" $TMUX load-plugin -s server $CAPS -o home="$HOME_A" -o credentials=file \
    -o claude="sleep 120" "$WASM" || fail "load-plugin on A"

LOG="$TMUX plugin-log -n 200 accounts"
AOPT="$TMUX show-options -s -v"

# --- capture two logins into two slots -----------------------------------

login "$HOME_A" one@example.com "Org One" 1
$TMUX plugin-command accounts add || fail "add one"
wait_for 6 "$LOG | grep -q 'add: slot 1: one@example.com'" || fail "add one: $($LOG | tail -3)"

login "$HOME_A" two@example.com "Org Two" 2
$TMUX plugin-command accounts "add 2 second" || fail "add two"
wait_for 6 "$LOG | grep -q 'add: slot 2: two@example.com'" || fail "add two: $($LOG | tail -3)"

$TMUX plugin-command accounts status || fail "status"
wait_for 6 "$LOG | grep -q 'status: slot 2: two@example.com'" || fail "status: $($LOG | tail -3)"

# --- switch back to slot 1 ------------------------------------------------

# The live (two) credential rotated since it was captured: the switch
# must back the rotation up into slot 2 before overwriting.
sed -i.bak 's/"refreshToken":"rt-2"/"refreshToken":"rt-2-rotated"/' "$HOME_A/.claude/.credentials.json"
$TMUX plugin-command accounts "switch 1" || fail "switch 1"
wait_for 10 "$LOG | grep -q 'switch: switched to 1 one@example.com'" || fail "switch: $($LOG | tail -3)"

grep -q '"accessToken":"at-1"' "$HOME_A/.claude/.credentials.json" || fail "credential not switched"
grep -q '"mcp-of-2"' "$HOME_A/.claude/.credentials.json" || fail "mcpOAuth not kept from the live credential"
grep -q '"emailAddress": *"one@example.com"' "$HOME_A/.claude.json" || fail "identity not switched"
grep -q '"allowedTools"' "$HOME_A/.claude.json" || fail "the rest of .claude.json was lost"
case $(ls -l "$HOME_A/.claude/.credentials.json" | cut -c1-10) in
	-rw-------) ;;
	*) fail "credentials file mode: $(ls -l "$HOME_A/.claude/.credentials.json")" ;;
esac
[ -d "$HOME_A/.claude/.oauth_refresh.lock" ] && fail "refresh lock left behind"
[ -d "$HOME_A/.claude.lock" ] && fail "legacy lock left behind"
[ -d "$HOME_A/.claude.json.lock" ] && fail "config lock left behind"
sqlite3 "$XDG_A/tmux/plugins/accounts/store.db" "select credentials from accounts where slot = 2" |
    grep -q 'rt-2-rotated' || fail "the outgoing login's rotation was not backed up"

$TMUX plugin-command accounts status || fail "status after"
wait_for 6 "$LOG | grep -q 'status: slot 1: one@example.com'" || fail "status after: $($LOG | tail -3)"

# --- locks -----------------------------------------------------------------

# A stale lock (older than 60 s) is somebody's leftover: stolen.
mkdir "$HOME_A/.claude/.oauth_refresh.lock"
touch -t 202001010000 "$HOME_A/.claude/.oauth_refresh.lock"
$TMUX plugin-command accounts "switch 2" || fail "switch 2"
wait_for 10 "$LOG | grep -q 'switch: switched to 2 two@example.com'" || fail "stale lock: $($LOG | tail -3)"
[ -d "$HOME_A/.claude/.oauth_refresh.lock" ] && fail "stale lock left behind"

# A live lock (fresh) belongs to a running claude: the switch refuses
# instead of racing it, and the login stays.
mkdir "$HOME_A/.claude/.oauth_refresh.lock"
$TMUX plugin-command accounts "switch 1" || fail "switch 1 under lock"
wait_for 20 "$LOG | grep -q 'switch: .*lock is held'" || fail "live lock: $($LOG | tail -3)"
grep -q '"accessToken":"at-2"' "$HOME_A/.claude/.credentials.json" || fail "switched under a live lock"
rmdir "$HOME_A/.claude/.oauth_refresh.lock"

# --- a login window ----------------------------------------------------------

# `login` opens a claude (here: sleep) under a profile with no credential
# and watches it; a login landing there becomes a slot, and the machine's
# own login is untouched.
$TMUX plugin-command accounts login || fail "login"
wait_for 6 "$LOG | grep -q 'login profile '" || fail "no login profile: $($LOG | tail -3)"
PROF=$($LOG | grep 'login profile ' | tail -1 | sed 's/.*login profile //')
[ -d "$PROF" ] || fail "profile $PROF not made"
wait_for 6 "$TMUX list-windows -F '#{window_name}' | grep -q '^login$'" || fail "no login window"
grep -q '"hasCompletedOnboarding": true' "$PROF/.claude.json" || fail "profile config not seeded"
[ -f "$PROF/.credentials.json" ] && fail "a login profile must start without a credential"
far=$(( $(date +%s) * 1000 + 86400000 ))
cat > "$PROF/.credentials.json" <<JSON
{"claudeAiOauth":{"accessToken":"at-3","refreshToken":"rt-3","expiresAt":$far,"scopes":["user:inference"]}}
JSON
sed -i.bak 's/"hasCompletedOnboarding": true/"hasCompletedOnboarding": true, "oauthAccount": {"accountUuid":"uuid-3","emailAddress":"three@example.com","organizationUuid":"org-3","organizationName":"Org Three"}/' "$PROF/.claude.json"
wait_for 15 "$LOG | grep -q 'login: added slot 3: three@example.com'" || fail "login not captured: $($LOG | tail -3)"
grep -q '"accessToken":"at-2"' "$HOME_A/.claude/.credentials.json" || fail "the machine login changed on a login window"
sqlite3 "$XDG_A/tmux/plugins/accounts/store.db" "select email from accounts where slot = 3" |
    grep -q 'three@example.com' || fail "slot 3 not stored"

# --- over a link ------------------------------------------------------------

login "$HOME_B" remote@example.com "Org Remote" 9
XDG_DATA_HOME="$XDG_B" $TMUX2 plugin-command accounts add || fail "add on B"
BLOG="XDG_DATA_HOME=$XDG_B $TMUX2 plugin-log -n 200 accounts"
wait_for 6 "$BLOG | grep -q 'add: slot 1: remote@example.com'" || fail "add on B: $($BLOG | tail -3)"

$TMUX remote-attach -t work fakehost || fail "remote-attach"
wait_for 10 "[ \"\$($TMUX display-message -p -t fakehost/work '#{remote_connected}')\" = 1 ]" ||
    fail "link not up"

# The panel, opened from a control client (a mode takes keys only from
# an attached client): B's block lists B's own slot, and Escape closes.
( sleep 0.3; echo 'plugin-command accounts "pick fakehost"'; sleep 60 ) |
    $TMUX -C attach >/dev/null 2>&1 &
CTL=$!
wait_for 10 "$TMUX list-panes -a -F '#{pane_id} #{pane_mode}' | grep -q plugin-mode" || fail "no panel pane"
PANE=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' | awk '/plugin-mode/ { print $1 }')
wait_for 10 "$TMUX capture-pane -M -p -t $PANE | grep -q 'remote@example.com'" ||
    fail "remote slot not listed: $($TMUX capture-pane -M -p -t $PANE)"
$TMUX capture-pane -M -p -t "$PANE" | grep -q 'fakehost' || fail "block not named after the server"
# A second login on B, so there is something to switch to. The cursor
# opens on B's login (slot 2); k moves to slot 1, Enter asks, y switches
# the login on B over the link.
login "$HOME_B" other@example.com "Org Other" 8
XDG_DATA_HOME="$XDG_B" $TMUX2 plugin-command accounts add || fail "add other on B"
wait_for 6 "$BLOG | grep -q 'add: slot 2: other@example.com'" || fail "add other: $($BLOG | tail -3)"
$TMUX send-keys -t "$PANE" r
wait_for 10 "$TMUX capture-pane -M -p -t $PANE | grep -q 'other@example.com'" ||
    fail "second remote slot not listed: $($TMUX capture-pane -M -p -t $PANE)"
$TMUX send-keys -t "$PANE" G; sleep 0.3
$TMUX send-keys -t "$PANE" Enter; sleep 0.4
$TMUX capture-pane -M -p -t "$PANE" | grep -q 'already the login' ||
    fail "Enter on the login should say so: $($TMUX capture-pane -M -p -t $PANE | tail -2)"
$TMUX send-keys -t "$PANE" k; sleep 0.3
$TMUX send-keys -t "$PANE" Enter; sleep 0.4
$TMUX capture-pane -M -p -t "$PANE" | grep -q 'switch fakehost.* to 1 remote@example.com? y/n' ||
    fail "no confirmation: $($TMUX capture-pane -M -p -t $PANE | tail -2)"
$TMUX send-keys -t "$PANE" y
wait_for 15 "$TMUX capture-pane -M -p -t $PANE | grep -q 'switched to 1 remote@example.com'" ||
    fail "remote switch: $($TMUX capture-pane -M -p -t $PANE | tail -2)"
grep -q '"accessToken":"at-9"' "$HOME_B/.claude/.credentials.json" || fail "B's credential not switched"
grep -q '"emailAddress": *"remote@example.com"' "$HOME_B/.claude.json" || fail "B's identity not switched"
$TMUX send-keys -t "$PANE" Escape
wait_for 6 "! $TMUX list-panes -a -F '#{pane_mode}' | grep -q plugin-mode" || fail "panel did not close"
kill $CTL 2>/dev/null

echo PASS
