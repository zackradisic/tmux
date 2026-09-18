# fake_bin <dest>: a shell copied under another name, so a pane's command is
# what the agents plugin has to detect ("codex", "node", ...). A symlink will
# not do: a multi-call binary dispatches on argv[0] and refuses it. The copy
# takes `-c <script>` like any shell.
#
# Two macOS-only traps, both of which make the copy useless in a different
# way:
#
# 1. A plain copy of a system binary is SIGKILLed - it keeps the original's
#    signature, which stops validating once the file is no longer the
#    platform binary it claims to be. The pane then dies the instant it
#    starts and takes its session (and an otherwise-empty server) with it.
#    An ad-hoc re-sign makes the copy runnable.
#
# 2. /bin/sh there IS bash, and bash overwrites its own process name, so
#    `#{pane_current_command}` reads "bash" however the file is named and
#    nothing is ever detected. (A copy of /bin/cat under the same name
#    reports "codex", so this is bash, not the copying.) zsh does not do
#    this, so prefer it as the donor where it exists.
fake_bin()
{
	_src=/bin/sh
	[ -x /bin/zsh ] && _src=/bin/zsh
	if ! cp "$_src" "$1"; then
		echo "FAIL: fake_bin: cannot copy $_src to $1" >&2
		exit 1
	fi
	if command -v codesign >/dev/null 2>&1; then
		if ! codesign -f -s - "$1" >/dev/null 2>&1; then
			echo "FAIL: fake_bin: cannot ad-hoc sign $1" >&2
			exit 1
		fi
	fi
}
