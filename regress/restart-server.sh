#!/bin/sh
# Restart the server in place and check that nothing moved: the server keeps
# its pid, every pane keeps its pid and pty, and the sessions, layouts,
# options, key bindings, buffers and scrollback all come back unchanged.
#
# Panes must run explicit commands (bare shells can die immediately in
# sandboxed CI environments).

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lrestart-test -f/dev/null"
TMP=$(mktemp -d)

fail() {
	echo "FAIL: $*" >&2
	$TMUX kill-server 2>/dev/null
	rm -rf "$TMP"
	exit 1
}

$TMUX kill-server 2>/dev/null
sleep 0.5

# A pane that prints known lines, wraps some of them, and then waits.
cat >"$TMP/gen.sh" <<'EOF'
#!/bin/sh
i=1
while [ $i -le 40 ]; do
	printf 'line %s \033[31mRED\033[0m then a long tail that runs past the right edge of the pane\n' $i
	i=$((i + 1))
done
printf 'PROMPT> '
exec sleep 600
EOF
chmod +x "$TMP/gen.sh"

# --- the scene -------------------------------------------------------------
$TMUX new-session -d -s alpha -x 80 -y 20 "$TMP/gen.sh" || fail "new-session"
$TMUX split-window -t alpha:0 "sh -c 'echo MARK-A1; exec sleep 601'"
$TMUX split-window -h -t alpha:0 "sh -c 'echo MARK-A2; exec sleep 602'"
$TMUX select-layout -t alpha:0 main-vertical
$TMUX new-window -t alpha -n linked "sh -c 'echo MARK-A3; exec sleep 603'"
$TMUX new-session -d -s beta "sh -c 'echo MARK-B0; exec sleep 604'"
$TMUX link-window -s alpha:1 -t beta:9

$TMUX set -g @handoff-marker 'a user option'
$TMUX set -g status-left 'LEFT#{session_name}'
$TMUX set -t alpha history-limit 12345
$TMUX setw -t alpha:0 window-status-format 'WSF'
$TMUX set-environment -t alpha HANDOFF_VAR uniquevalue
$TMUX bind -T root F5 display-message 'a binding'
$TMUX bind -N 'a note' -T root F6 display-message 'noted'
$TMUX unbind -T prefix c
$TMUX set-buffer -b handoffbuf 'buffer contents'
$TMUX select-pane -t alpha:0.1
$TMUX set -g remain-on-exit on
$TMUX split-window -t beta:0 'sh -c "exit 7"'
$TMUX new-pane -d -t alpha:0 -x 40 -y 8 -X 10 -Y 5 \
    "sh -c 'echo MARK-FLOAT; exec sleep 605'"

# Zoom last, so that the window is zoomed while it holds a floating pane:
# zooming moves every cell to saved_layout_cell, which is where the save has
# to look to tell a floating pane from a tiled one.
$TMUX resize-pane -Z -t alpha:0.1
sleep 1

# Terminal modes a program in the pane turned on must come back too.
printf '\033[?2004h\033[?1006h\033[?1002h\033[?25l' \
    >"$($TMUX display-message -p -t alpha:0.0 '#{pane_tty}')"
sleep 0.5

snapshot() {
	$TMUX list-sessions -F '#{session_id} #{session_name} #{session_windows}'
	$TMUX list-windows -a -F '#{session_name}:#{window_index} @#{window_id} \
#{window_name} zoom=#{window_zoomed_flag} active=#{window_active}'

	# Compare the layout of every window except alpha:0, which holds the
	# floating pane. A floating cell sits in the cell list beside the tiled
	# ones, and a restart moves it to the end of that list, so the string
	# differs even though every pane is the same size in the same place.
	# The pane list above covers alpha:0's geometry, and the float is
	# checked on its own below.
	$TMUX list-windows -a -F '#{session_name}:#{window_index} #{window_layout}' \
	    | grep -v '^alpha:0 '
	$TMUX list-panes -a -F '#{session_name}:#{window_index}.#{pane_index} \
#{pane_id} #{pane_pid} #{pane_tty} #{pane_width}x#{pane_height} \
+#{pane_left},#{pane_top} active=#{pane_active} dead=#{pane_dead} \
status=[#{pane_dead_status}] cmd=[#{pane_start_command}]'
	$TMUX display-message -p '#{@handoff-marker}|#{status-left}'
	$TMUX display-message -p -t alpha:0.0 \
	    'mouse=#{mouse_any_flag} cursor=#{cursor_flag}'
	$TMUX show -t alpha -v history-limit
	$TMUX show -w -t alpha:0 -v window-status-format
	$TMUX show-environment -t alpha HANDOFF_VAR
	$TMUX list-keys -T root F5
	$TMUX list-keys -N -T root F6
	$TMUX list-keys -T prefix c 2>&1
	$TMUX list-keys | wc -l
	$TMUX list-buffers -F '#{buffer_name} [#{buffer_sample}]'
	$TMUX capture-pane -p -e -S - -t alpha:0.0
}

snapshot >"$TMP/before" 2>&1
before_pid=$($TMUX display-message -p '#{pid}')

# --- restart ---------------------------------------------------------------
$TMUX restart-server || fail "restart-server"
sleep 3

$TMUX has-session -t alpha 2>/dev/null || fail "server did not come back"
after_pid=$($TMUX display-message -p '#{pid}')
[ "$before_pid" = "$after_pid" ] || \
    fail "server pid changed: $before_pid -> $after_pid"

snapshot >"$TMP/after" 2>&1
if ! cmp -s "$TMP/before" "$TMP/after"; then
	diff -u "$TMP/before" "$TMP/after" >&2
	fail "state changed across the restart"
fi

# The floating pane must still be floating, in the same place.
float=$($TMUX display-message -p -t alpha:0.3 \
    '#{pane_width}x#{pane_height}+#{pane_left},#{pane_top}')
[ "$float" = "38x6+11,6" ] || fail "floating pane is [$float]"

# The mode check above is only worth anything if the modes were really on.
modes=$($TMUX display-message -p -t alpha:0.0 \
    'mouse=#{mouse_any_flag} cursor=#{cursor_flag}')
[ "$modes" = "mouse=1 cursor=0" ] || fail "pane modes are [$modes]"

# waitpid must still work: the panes are still our children.
pid=$($TMUX display-message -p -t beta:0.0 '#{pane_pid}')
kill -TERM "$pid" 2>/dev/null || fail "cannot signal pane process"
sleep 1
signal=$($TMUX display-message -p -t beta:0.0 '#{pane_dead_signal}')
[ "$signal" = "15" ] || fail "pane_dead_signal is [$signal], want 15"

$TMUX kill-server 2>/dev/null
rm -rf "$TMP"
exit 0
