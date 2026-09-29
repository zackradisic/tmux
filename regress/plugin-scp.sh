#!/bin/sh
# The scp form: from / path / to / path. Check:
#
#   `copy` prefills from=local and the pane's cwd with a trailing `/`;
#   the path field completes from a listing of files AND directories,
#     filtered by the fragment, dot entries only when asked for;
#   a directory row ends in `/`; Tab takes it and Enter steps into it;
#   the host fields list `local` plus the configured hosts and cycle;
#   a remote path completes over ssh (a fake ssh runs the ls here);
#   C-t swaps the two sides in place;
#   Enter copies with scp and closes the form (run = job here);
#   an empty source path is refused, a missing file shows scp's error.
#
# Needs the wasm example built:
#   cargo build -p scp --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lscp-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/scp.wasm

# A short path: the form clips long values from the left, and the checks
# below match whole paths.
HOME=$(mktemp -d /tmp/scpt.XXXXXX); HOME=$(cd "$HOME" && pwd -P); export HOME
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
open_form() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	( sleep 0.3; echo "plugin-command scp copy"; sleep 60 ) |
	    $TMUX -C attach -t alpha >/dev/null 2>&1 &
	CTL=$!
	i=0
	while [ "$i" -lt 20 ]; do
		FORM=$(form_pane)
		[ -n "$FORM" ] && break
		sleep 0.3; i=$((i + 1))
	done
	[ -n "$FORM" ] || fail "form did not open"
	sleep 0.8
}
wait_closed() {
	i=0
	while [ "$i" -lt 30 ]; do
		[ -z "$(form_pane)" ] && return 0
		sleep 0.3; i=$((i + 1))
	done
	fail "the form did not close"
}

[ -f "$WASM" ] || fail "scp.wasm not built"

# A source tree with files, a subdirectory and a dot entry; an empty
# destination directory.
mkdir -p "$WORK/src/sub" "$WORK/out"
echo a > "$WORK/src/alpha.txt"
echo b > "$WORK/src/beta.txt"
echo h > "$WORK/src/.hidden"
echo i > "$WORK/src/sub/inner.txt"

# A fake ssh on the server's PATH: drops the options, logs the host and
# runs the remote command here. The remote listing then comes from this
# disk, and the test can see which host was asked.
mkdir -p "$HOME/bin"
cat > "$HOME/bin/ssh" <<'SH'
#!/bin/sh
while [ $# -gt 0 ]; do
	case "$1" in
	-o) shift 2 ;;
	--) shift; break ;;
	-*) shift ;;
	*) break ;;
	esac
done
host=$1; shift
echo "$host" >> "$HOME/ssh.log"
exec /bin/sh -c "$*"
SH
chmod +x "$HOME/bin/ssh"
PATH="$HOME/bin:$PATH"; export PATH

$TMUX kill-server 2>/dev/null
sleep 0.5
$TMUX -f/dev/null new-session -d -s alpha -x 160 -y 50 -c "$WORK/src" \
    || fail "new-session"
sleep 0.3
$TMUX load-plugin -s server -c mode -c run-process -c run-command -c fs-list \
    -c fs-read-any -o run=job -o hosts=fakehost "$WASM" || fail "load-plugin"
sleep 1

# --- prefill ---------------------------------------------------------------
open_form
screen | grep -q 'Copy Files' || fail "the form did not open"
screen | grep -q 'from *local' || fail "from was not prefilled as local"
screen | grep -q "path *$WORK/src/" || fail "path was not prefilled from the pane cwd"
screen | grep -q 'to *local' || fail "to was not prefilled as local (no linked host)"

# --- file completion --------------------------------------------------------
# Focus opens on the source path. Typing shows the list, filtered.
type_text al
screen | grep -q "in $WORK/src" || fail "typing did not show the file list"
screen | grep -q 'alpha.txt' || fail "alpha.txt is missing from the list"
screen | grep -q 'beta.txt' && fail "the fragment al let beta.txt through"
keys Tab
screen | grep -q "path *$WORK/src/alpha.txt" || fail "Tab did not complete the file"
# Dot entries only when the fragment asks for one.
keys C-u
type_text "$WORK/src/"
screen | grep -q 'alpha.txt' || fail "the full listing does not show alpha.txt"
screen | grep -q '\.hidden' && fail "a dot entry showed without being asked for"
type_text .
screen | grep -q '\.hidden' || fail "a dot fragment did not show the dot entry"
# A directory row ends in /; taking it and typing on steps into it.
keys C-u
type_text "$WORK/src/su"
screen | grep -q 'sub/' || fail "the directory row does not end in /"
keys Tab
screen | grep -q "path *$WORK/src/sub/" || fail "Tab did not take the directory"
# Enter on a directory row steps into it: its listing opens.
keys Enter
sleep 0.8
screen | grep -q "in $WORK/src/sub" || fail "Enter did not list the directory just taken"
screen | grep -q 'inner.txt' || fail "inner.txt is missing from the sub listing"
keys Tab
screen | grep -q "path *$WORK/src/sub/inner.txt" || fail "Tab did not complete inside sub"
keys Escape
sleep 0.3

# --- the host dropdown -------------------------------------------------------
keys C-j
screen | grep -q 'hosts' && fail "moving to the host field popped the list"
keys Tab
sleep 0.5
screen | grep -q 'in hosts' || fail "Tab did not show the host list"
screen | grep -q 'fakehost' || fail "the configured host is missing from the list"
screen | grep -q 'to *local' || fail "the first Tab did not keep local"
keys Tab
screen | grep -q 'to *fakehost' || fail "the second Tab did not cycle to fakehost"
keys Escape
sleep 0.3

# --- remote completion over (fake) ssh ---------------------------------------
keys C-j
type_text "$WORK/src/"
sleep 1
screen | grep -q "in fakehost:$WORK/src" || fail "the remote listing did not show"
screen | grep -q 'beta.txt' || fail "beta.txt is missing from the remote listing"
screen | grep -q 'sub/' || fail "the remote listing does not mark directories"
grep -qx fakehost "$HOME/ssh.log" || fail "ssh was not asked for fakehost"
type_text b
keys Tab
screen | grep -q "path *$WORK/src/beta.txt" || fail "Tab did not complete the remote path"
keys Escape
sleep 0.3

# --- C-t swaps the sides -------------------------------------------------------
keys C-t
sleep 0.5
screen | grep -q 'from *fakehost' || fail "C-t did not move the host to from"
screen | grep -q "to *local" || fail "C-t did not move local to to"
keys C-t
sleep 0.5
screen | grep -q 'from *local' || fail "the second C-t did not swap back"

# --- Enter copies ---------------------------------------------------------------
# Destination back to local, its path to the empty out directory.
keys C-k
keys C-u
type_text local
keys C-j
keys C-u
type_text "$WORK/out/"
# out/ is empty, so no list shows and Enter means the form.
keys Enter
wait_closed
[ -f "$WORK/out/inner.txt" ] || fail "inner.txt was not copied to out/"
[ "$(cat "$WORK/out/inner.txt")" = "i" ] || fail "the copied file differs"

# --- errors stay in the form -------------------------------------------------------
open_form
keys C-u
keys Enter
sleep 0.5
screen | grep -q 'source path is required' || fail "an empty source path was not refused"
[ -n "$(form_pane)" ] || fail "the form closed on a validation error"
type_text "$WORK/src/nope.txt"
keys C-j
keys C-j
type_text "$WORK/out/"
keys Enter
sleep 1.5
screen | grep -qi 'no such file' || fail "scp's error did not show in the form"
[ -n "$(form_pane)" ] || fail "the form closed on an scp error"
keys Escape
wait_closed

cleanup
exit 0
