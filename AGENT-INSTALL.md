# Installing tmux2 on a new machine (instructions for an agent)

You are setting up **tmux2**, a tmux fork with WebAssembly plugins, remote
session links and in-place server restarts, for a user who runs coding
agents (Claude Code and others) in tmux panes. Follow the steps in order.
Every step is idempotent. Do not touch the user's stock `tmux` or its
config: tmux2 installs beside it, on its own socket, as the `tmux2` command.

Repo: https://github.com/zackradisic/tmux (branch `wasm-plugins`).
Background reading, if something is unclear: `GETTING-STARTED.md` (tour and
cheat sheet), `REMOTE.md` (links), `CROSS-AGENT.md` (messaging),
`plugin-host/examples/agents/shims/README.md` (agent hooks).

## 0. Requirements

- Linux x86_64 / aarch64, or macOS arm64. `curl`, `tar`, a POSIX `sh`.
- `~/.local/bin` on `PATH` for interactive shells. (Step 2 covers ssh.)
- Claude Code for the agents features; Codex, pi and opencode work with the
  shims in `plugin-host/examples/agents/shims/`.
- `sqlite3` is optional; only useful for looking at a plugin's store.

## 1. Install the release

Releases are dated prereleases on the `wasm-plugins` branch. **Always pass
a tag.** The installer's default resolves GitHub's "latest" release, which
is an old non-prerelease and lags far behind. Take the newest tag from
https://github.com/zackradisic/tmux/releases (they sort by date; the name
is `wasm-plugins-<date>-<sha>`), then:

```sh
TAG=wasm-plugins-20260925-31fffb93   # <- replace with the newest tag
curl -fsSL "https://github.com/zackradisic/tmux/releases/download/$TAG/install-tmux2.sh" -o /tmp/install-tmux2.sh \
  || curl -fsSL "https://raw.githubusercontent.com/zackradisic/tmux/wasm-plugins/scripts/install-tmux2.sh" -o /tmp/install-tmux2.sh
sh /tmp/install-tmux2.sh "$TAG"
```

That lays down:

```
~/.local/share/tmux2/bin/tmux      the fork binary
~/.local/share/tmux2/plugins/      the wasm plugins and their .toml capability sidecars
~/.local/share/tmux2/VERSION       the tag
~/.local/bin/tmux2                 wrapper: the fork on socket -L tmux2, its bin first on PATH
```

Check: `~/.local/bin/tmux2 -V` prints `tmux next-3.8`, and
`ls ~/.local/share/tmux2/plugins` lists seven `.wasm` files.

Later updates, from inside a tmux2 session or not:
`tmux2 update -y canary` installs the newest prerelease and restarts the
server **in place**, keeping every pane's process. The binary and the
plugins always ship together; never mix a plugin from one tag with a
binary from another (a plugin can import host functions the older binary
lacks and then fails to load with "unknown import").

## 2. Make `tmux2` reachable from non-interactive ssh

`~/.local/bin` is not on the PATH of `ssh host cmd`, and remote links use
exactly that. If this machine will ever be linked to from another one:

```sh
sudo ln -sf "$HOME/.local/bin/tmux2" /usr/local/bin/tmux2
ssh localhost 'command -v tmux2'     # must print a path
```

## 3. The plugin manifest

Write `~/.tmux/plugins.toml`. Paths may use `~`. The capability lists below
are what the current plugins need; a capability a plugin requests but is
not granted is silently stripped, and features then fail without a
message, so copy them exactly.

```toml
# Toast notifications (OSC 9 / OSC 777 from any program in a pane).
[plugins.notify_toast]
path = "~/.local/share/tmux2/plugins/notify_toast.wasm"
scope = "server"
caps = ["run-command", "mode", "service-call"]
config = { duration_ms = "0", chooser_width = "90%", chooser_height = "60%" }

# Per-pane git branch and dirty flag for the status line.
[plugins.git_status]
path = "~/.local/share/tmux2/plugins/git_status.wasm"
scope = "pane"
caps = ["run-process", "write-options"]

# New-session / worktree form.
[plugins.session_creator]
path = "~/.local/share/tmux2/plugins/session_creator.wasm"
scope = "server"
caps = ["mode", "run-process", "run-command", "fs-list", "fs-read-any"]

# Save and restore sessions with scrollback; restart the server in place.
[plugins.resurrect]
path = "~/.local/share/tmux2/plugins/resurrect.wasm"
scope = "server"
caps = ["capture-pane", "run-command", "fs-read", "fs-write", "db", "mode"]
config = { autosave = "5m", keep = "1" }

# Scheduled jobs that survive restarts.
[plugins.cron]
path = "~/.local/share/tmux2/plugins/cron.wasm"
scope = "server"
caps = ["db", "run-process", "run-command", "mode"]
config = { keep_days = "14", keep_runs = "50", retry_max = "2", retry_backoff = "30s", tz = "local" }

# The agents picker: every coding-agent session, live or finished, with
# conversation search, preview, info card, revive and fork.
#   claude-notify  pushes a mailbox message into a Claude session (needed
#                  for cross-agent messages to wake the reader)
#   send-keys      typing into the preview, the interrupt, the wheel
#   run-process, fs-read-any  the new-agent form's directory completion
[plugins.agents]
path = "~/.local/share/tmux2/plugins/agents.wasm"
scope = "server"
caps = ["capture-pane", "run-command", "mode", "db", "env-read", "pane-fds",
        "fs-read", "fs-list", "service-serve", "service-call", "claude-notify",
        "send-keys", "run-process", "fs-read-any"]

[plugins.agents.config]
keep_days = "14"        # finished agents stay in the default list this long
history_days = "365"    # conversations stay searchable this long

# Launchers for the new-agent form's command field: the name is what
# you type, the line is what runs. One named like a harness (claude) is
# what a new agent started from a Claude row runs. Adjust to taste.
[plugins.agents.config.launch]
claude = "claude --dangerously-skip-permissions"
claude-opus = "claude --dangerously-skip-permissions --model opus"

# Cross-agent messages (the box store behind `agents message`).
[plugins.mailbox]
path = "~/.local/share/tmux2/plugins/mailbox.wasm"
scope = "server"
caps = ["db", "service-serve", "service-call", "write-options", "display-message"]
```

## 4. The tmux config

Append this to `~/.tmux.conf` (or `~/.config/tmux/tmux.conf`, whichever
the user has). The `%if` guard makes stock tmux skip the block, so one
config serves both. If the user has no config, this block alone is a
working one; `mouse on` matters because the picker is clickable and the
wheel over its preview scrolls the agent's pane.

```tmux
set -g mouse on

%if "#{m:next-*,#{version}}"
# Load, live-reload and unload plugins from the manifest.
sync-plugins ~/.tmux/plugins.toml

# The agents picker. a: the default view, cursor on the agent of the
# pane you pressed it in. A: the same, but finds that agent in the
# history or archive when it has finished or was archived.
bind a plugin-command agents pick
bind A plugin-command agents 'pick here'

# Agent ids on screen (a mailbox message names its sender by one):
# F enters copy mode with the nearest id highlighted, n/N step between
# them, Enter opens the picker on the one under the cursor (or on a
# selected id; any other selection still copies).
bind-key F copy-mode \; send-keys -X search-backward '(claude|codex|pi|opencode):[0-9a-f][0-9a-f-]{7,}'
bind -T copy-mode-vi Enter run-shell -b "~/.config/agents/tmux-agent-at-cursor '#{pane_id}' '#{search_match}' '#{selection_present}'"

bind N   plugin-command notify_toast chooser      # notification feed
bind S   plugin-command session_creator new       # new session from a folder
bind W   plugin-command session_creator worktree  # new session from a git worktree
bind -T choose-tree S plugin-command session_creator new
bind -T choose-tree W plugin-command session_creator worktree
bind C-r plugin-command resurrect pick            # save/restore picker
bind U   confirm-before -p 'update tmux2? (y/n)' 'update -y canary'

# Remote link state in the status line; empty on a local session.
set -g status-left "#{?session_remote_host,#{?remote_connected,#[fg=colour245]⇄ #{session_remote_host}#[default] ,#[bg=red#,fg=white#,bold] ⚠ #{session_remote_host} disconnected#{?remote_error,: #{=/48/…:remote_error},} #[default] },}[#{session_name}] "
set -g status-right "#{@git_branch}#{?@git_dirty,*,} | %H:%M"
%endif
```

The copy-mode Enter binding needs its script. From a checkout of the repo
(or fetch the raw file from GitHub at the same path):

```sh
mkdir -p ~/.config/agents
cp plugin-host/examples/agents/shims/tmux-agent-at-cursor ~/.config/agents/
chmod +x ~/.config/agents/tmux-agent-at-cursor
```

## 5. Claude Code hooks and settings

The roster works with no hooks (it reads Claude's own session files), but
the exact turn state, the "needs input" band with its reason, and the
conversation search's turn-by-turn indexing come from a status shim run
by Claude Code's hooks.

```sh
cp plugin-host/examples/agents/shims/agent-status.sh ~/.config/agents/
chmod +x ~/.config/agents/agent-status.sh
```

Then merge `plugin-host/examples/agents/shims/claude-code/settings.json`
into `~/.claude/settings.json`: it adds `UserPromptSubmit`, `PreToolUse`,
`Stop` and `SessionEnd` hooks that call the shim. Merge, do not replace,
and keep any hooks the user already has. Also add, at the top level of
`~/.claude/settings.json`:

```json
"crossSessionInbound": "accept"
```

Without it a message pushed into a Claude session that runs with
permissions bypassed pops an approval dialog instead of waking the agent.

If the shim's hooks cannot find the fork (`tmux` on the hook's PATH is the
stock one), set `TMUXBIN=$HOME/.local/share/tmux2/bin/tmux` in the
environment Claude Code starts from; the shim honours it.

## 6. Start it and check

```sh
tmux2 new -s main            # or: tmux2 attach
```

Inside: `tmux2 show-plugins` lists seven plugins, each `running`. Press
`prefix a`: the agents picker floats over the window (empty until an agent
runs). Press `?` in it for the key reference, `Esc` to leave.

If a plugin is missing from `show-plugins`, run
`tmux2 sync-plugins ~/.tmux/plugins.toml` and read what it prints. A
plugin that loads but does nothing usually lacks a capability: compare
its `caps` with step 3.

## 7. Remote links (optional)

To mirror a session from another machine into this one, that machine
needs tmux2 too (same tag, steps 1 to 3 there, and step 2 is required).
Then, here:

```sh
tmux2 remote-attach -L host              # list its sessions
tmux2 remote-attach -t <session> host    # mirror one as "host/<session>"
```

`host` is an ssh alias or `user@host` that works non-interactively. The
first time the far side offers a plugin that may call back (the mailbox's
`deliver`), tmux2 shows a menu; allow it, or later:
`tmux2 plugin-peers allow host mailbox`. Agents on the linked machine then
appear in the picker under their own server heading, and messages by id
reach them.

## 8. Where things live

```
~/.local/share/tmux/plugins/<plugin>/store.db   each plugin's SQLite store
~/.local/share/tmux/plugin-host/peers.db        which peers may call in
~/.tmux/plugins.toml                            the manifest (step 3)
```

Uninstall: kill the tmux2 server, delete `~/.local/share/tmux2`,
`~/.local/bin/tmux2`, the two data directories above, and the config
block. The stock tmux was never touched.
