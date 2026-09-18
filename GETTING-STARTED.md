# tmux2 — getting started

A fork of tmux with a **WebAssembly plugin system**, **remote session links
over ssh**, and a **server restart that keeps your panes alive**. It installs
on its own socket next to your normal tmux, so nothing you already have
breaks.

Everything here is quick and dirty on purpose. The real references are
`plugin-host/WRITING-PLUGINS.md` (authoring), `REMOTE.md` (links),
`SERVER-RESTART.md` (restart), `UX_NOTES.md` (known rough edges) and the
`PLUGINS` / `REMOTE SESSIONS` sections of `tmux.1`.

---

## TL;DR — what this fork adds

**Big things**

- **Plugins, written in WebAssembly.** Real code running inside the tmux
  server, with access to tmux's own panes, windows, options and screen
  contents. They can't freeze or crash it: every callback is on a CPU budget,
  and all I/O is queued to a worker pool off the event loop.
- **Plugins get a database.** Each one has its own SQLite store, so state
  survives restarts. They also get their own UI: a floating panel they draw
  into, with live previews of other panes rendered straight from tmux's
  internal grid (so it's fast).
- **Plugins are scoped.** A plugin can live per server, per session, per
  window or per pane, and instances appear and disappear with the thing they
  belong to.
- **Remote session links.** `remote-attach` mirrors a session from another
  machine over ssh as a local session. Output renders locally, keys go to the
  remote, layout follows the remote. Plugins work across the link — one
  picker, every machine.
- **Restart the server without killing anything.** `restart-server` swaps the
  binary underneath you; every pane process stays alive, with its scrollback,
  layouts, options and bindings. `update` does it from a GitHub release in one
  command.
- **Installs side by side.** Own binary, own socket, own `tmux2` command. Your
  stock tmux and its config are untouched.

**The plugins that ship with it**

- **agents** (`prefix + A`) — every coding-agent session you have running,
  grouped by *needs input / waiting / working / done*, with a live pane
  preview. Jump to one, archive it, filter, or grep live pane contents. Works
  across linked machines, and you can message an agent by id. *Tested mostly
  with Claude Code.*
- **resurrect** (`prefix + C-r`) — autosaves your panes to disk **with
  scrollback** and restores them; also saves-and-restarts the server in place.
- **session_creator** (`prefix + S` / `prefix + W`) — a form for making a new
  session from a folder or a git worktree. *Still WIP.*
- **mailbox** — messages between agents, on this machine or a linked one.
  Stored as data, never typed into a pane, so it can't corrupt a half-written
  prompt.
- **notify-toast** (`prefix + N`) — anything in a pane that emits a desktop
  notification escape code pops up as a toast, with a browsable feed.
- **git-status** — per-pane branch and dirty flag for the status line, that
  follows whichever pane you're in.
- **cron** — scheduled jobs that survive server restarts, with a picker.

**Housekeeping**

- **A manifest for plugins.** Declare them in `plugins.toml`; edits load,
  change or unload them. A registry + lock file pins exact versions, and
  `update-plugins` upgrades them.
- **Capabilities.** A plugin only gets what it asks for *and* you grant —
  down to which env vars and directories it may read. A linked machine calling
  back into yours needs your explicit say-so.

For what a plugin can actually *do* — greping every pane at once, panels with
live pane mirrors, durable SQLite state — see
[How powerful is this, really?](#how-powerful-is-this-really), with code.

---

## 1. Install

```sh
sh scripts/install-tmux2.sh          # latest release
sh scripts/install-tmux2.sh v0.3.0   # a specific tag
```

It lays down, without touching your stock tmux:

```
~/.local/share/tmux2/bin/tmux     the fork
~/.local/share/tmux2/plugins/     the bundled wasm plugins + capability sidecars
~/.local/bin/tmux2                wrapper: the fork on its own socket (-L tmux2)
```

Use `tmux2` instead of `tmux`. Inside a tmux2 session, `tmux` is the fork too
(the wrapper puts it first on `PATH`), so your config and scripts work
unchanged.

Later, from inside tmux: `update` downloads the newest release and restarts
the server **in place** — your pane processes stay alive. Bind it:

```tmux
bind U confirm-before -p 'update tmux2? (y/n)' 'update -y'
```

### Or build from source

```sh
sh autogen.sh
./configure --enable-plugins && make
```

Plugins are Rust → wasm; `rustup target add wasm32-unknown-unknown`, then
`sh scripts/build-plugins.sh` builds them all and copies each capability
sidecar (`*.toml`) next to the `.wasm`. **Always build with that script, not
bare `cargo build`** — cargo does not copy the sidecar, and a missing or stale
sidecar silently strips capabilities.

---

## 2. Turn on the plugins

Plugins are declared in a manifest and reconciled by `sync-plugins`: edit the
file, re-source your config, and additions load, changes live-reload, removals
unload. (Plain `load-plugin` lines never unload anything — that's why the
manifest exists.)

`~/.tmux/plugins.toml`:

```toml
# Toast notifications (OSC 9 / OSC 777 from any program in a pane).
[plugins.notify_toast]
path = "~/.local/share/tmux2/plugins/notify_toast.wasm"
scope = "server"
caps = ["run-command", "mode"]
config = { duration_ms = "0", chooser_width = "90%", chooser_height = "60%" }

# Per-pane git branch/dirty for the status line.
[plugins.git_status]
path = "~/.local/share/tmux2/plugins/git_status.wasm"
scope = "pane"
caps = ["run-process", "write-options"]

# New-session / worktree form.
[plugins.session_creator]
path = "~/.local/share/tmux2/plugins/session_creator.wasm"
scope = "server"
caps = ["mode", "run-process", "run-command", "fs-list", "fs-read-any"]

# Save/restore sessions with scrollback.
[plugins.resurrect]
path = "~/.local/share/tmux2/plugins/resurrect.wasm"
scope = "server"
caps = ["capture-pane", "run-command", "fs-read", "fs-write", "db", "mode"]
config = { autosave = "5m", keep = "1" }

# Roster of coding-agent sessions running in panes.
[plugins.agents]
path = "~/.local/share/tmux2/plugins/agents.wasm"
scope = "server"
caps = ["capture-pane", "run-command", "mode", "db", "env-read", "pane-fds",
        "fs-read", "fs-list", "service-serve", "service-call", "claude-notify"]
config = { keep_days = "14" }

# Cross-agent messages (the delivery primitive behind `agents message`).
[plugins.mailbox]
path = "~/.local/share/tmux2/plugins/mailbox.wasm"
scope = "server"
caps = ["db", "service-serve", "service-call", "write-options", "display-message"]

# Scheduled jobs, durable in the plugin's own SQLite store.
[plugins.cron]
path = "~/.local/share/tmux2/plugins/cron.wasm"
scope = "server"
caps = ["db", "run-process", "run-command", "mode"]
config = { keep_days = "14", keep_runs = "50", tz = "local" }
```

`~/.tmux.conf` — the `%if` guard means stock tmux ignores the whole block, so
you can keep one config for both:

```tmux
%if "#{m:next-*,#{version}}"
sync-plugins ~/.tmux/plugins.toml

bind A   plugin-command agents pick          # agent roster
bind C-r plugin-command resurrect pick       # save/restore picker
bind S   plugin-command session_creator new  # new session (folder)
bind W   plugin-command session_creator worktree
bind N   plugin-command notify_toast chooser # notification tree

# S / W also from inside the prefix+w tree
bind -T choose-tree S plugin-command session_creator new
bind -T choose-tree W plugin-command session_creator worktree

# Remote link state in the status line: only a linked session has
# #{session_remote_host}, so this is empty on a local session.
set -g status-left "#{?session_remote_host,#{?remote_connected,#[fg=colour245]⇄ #{session_remote_host}#[default] ,#[bg=red#,fg=white#,bold] ⚠ #{session_remote_host} disconnected#{?remote_error,: #{=/48/…:remote_error},} #[default] },}[#{session_name}] "
set -g status-right "#{@git_branch}#{?@git_dirty,*,} | %H:%M"
%endif
```

Instead of pinning `path`, you can leave the source out entirely and let the
**release registry** resolve it (GitHub releases carry the plugins and an
`index.toml`), or point at a `url` + `blake3:` hash. Either way the resolved
versions and hashes are written to `plugins.lock` next to the manifest — commit
it, and two machines run the same bytes. `update-plugins ~/.tmux/plugins.toml`
checks the registry, prints `agents: 0.1.0 -> 0.2.0`, writes the new lock and
live-reloads. `-n` is a dry run.

---

## 3. The plugin system

A plugin is a `wasm32-unknown-unknown` module running **inside the tmux
server**:

- **On the event loop, single-threaded.** Every callback has a CPU budget
  (~2 ms logs a warning, 2 s traps and restarts the instance). Nothing a
  plugin does can freeze or crash tmux.
- **Async everything else.** No `std::fs`, `std::net`, `std::thread` in the
  sandbox — filesystem, processes and jobs are queued onto a worker pool and
  come back as wakeups.
- **Own state.** Ordinary in-memory state in wasm linear memory, preserved
  across live reloads via a snapshot. For persistence, each plugin gets its
  own **SQLite database** (`db` capability, compress-on-write blobs).
- **Real access to tmux objects.** Panes, windows, sessions, options,
  key/mode input, captures. Because it reads tmux's internal grid directly,
  a plugin can render **live previews of other panes** in its own UI at
  basically no cost, and search pane contents in C (plain/SIMD, regex, or
  fuzzy) instead of hauling text into wasm.
- **Events.** The whole tmux event bus (panes/windows/sessions created,
  renamed, destroyed…), plus OSC 133 shell-integration marks and OSC 9 /
  OSC 777 notifications emitted by programs inside panes.

**Scopes** (`-s`): `server`, `session`, `window`, `pane`. A server plugin lives
and dies with the server; a pane plugin is instantiated per pane and dies with
it. Scoped instances appear and disappear automatically with their object.

**Capabilities** (`-c`): anything beyond reading state, timers and messages
must be granted — `write-options`, `send-keys`, `capture-pane`, `run-process`,
`run-command`, `cross-scope`, `mode`, `db`, `fs-read`, `service-*`, … A plugin
ships a TOML **sidecar** next to its `.wasm` declaring what it wants; the
effective set is `requests ∩ grants`. The sidecar can narrow further — the
agents plugin can only read four env var names and four directories, so
everything else in a pane's environment (tokens, secrets) stays unreadable.

**Roles** (`-r`) — the local/remote split:

- A **provider** sees one server: reads its processes and files, keeps its
  store, answers calls.
- A **view** runs locally, merges what every provider reports, and owns the UI.
- `both` (the default) is one instance doing both.

The two halves talk over **services** (methods + topics, `service-serve` /
`service-call`). Write the provider once and it runs locally or on the far
end of a remote link, unchanged. When you link a server, the local side offers
it every provider-capable plugin by hash; the remote loads what it lacks from
its own cache, fetches it itself, or asks for a push.

**UI**: the `mode` capability lets a plugin open a floating pane it draws into
directly — that's what all the pickers are.

### How powerful is this, really?

The short version: a plugin can see and do most of what tmux itself can, from
inside the server, in a few dozen lines. A handful of host calls carry most of
the weight.

**Grep every pane's scrollback in one call.** The search runs in C over tmux's
live grid — the pane contents never cross the wasm boundary, only the needle
in and the matches out. Plain (SIMD), POSIX regex, or fuzzy-scored:

```rust
let panes: Vec<PaneId> = list_panes()?.iter().map(|p| PaneId(p.id)).collect();
for hit in panes_search(&panes, "panic!", SearchMode::Plain, false, 0)? {
    log(&format!("{}:{} {}", hit.pane, hit.line, hit.snippet));
}
```

That is "which of my 40 panes has the failing test in it", answered instantly.
The agents picker uses the same call for its `C-f` live grep.

**Draw a panel with a live mirror of another pane in it.** `mode_open` gives
you a floating pane you own and write ANSI into; `mode_preview` pins a
rectangle of it to another pane's grid, and tmux keeps that rectangle painted
as the real pane produces output. You are not screenshotting or polling — it's
the same grid, so a full-motion preview costs about nothing:

```rust
let mode = mode_open(&ModeOpts {
    width: 100, height: 40, title: Some("my picker".into()), ..Default::default()
})?;
mode_write(mode, b"\x1b[2J\x1b[HPick a pane:\r\n")?;
mode_preview(mode, Some(&PreviewRect { pane, x: 40, y: 3, w: 58, h: 34 }))?;
```

Keys typed in the panel arrive as `mode-key` events. That's every picker in
this repo — agents, resurrect, cron, notify-toast, session_creator — and it's
why they all feel instant.

**Read what a pane is actually doing.** `capture_pane` (visible *and* history),
`pane_env` (the foreground process's environment, narrowed to an allowlist),
`pane_fds` (which files it has open), `resolve_pane` (title, shell, cwd, dead,
floating, remote). The agents plugin identifies a Claude session by the session
file the process has open — it never has to ask the agent anything, which is
why its rows survive a restart and can't be faked by an env var.

**Do real work without blocking the server.** `run_job` (a shell command),
`run_command` (any tmux command), `fs_*`, timers — all `.await`ed on a worker
pool. A 10-line polling loop is the whole of git-status:

```rust
ctx.spawn(async move {
    loop {
        if sleep_ms(5000).await.is_err() { return }        // instance gone
        if let Ok(out) = run_job("git status --porcelain", None).await {
            let _ = set_option("@git_dirty",
                if out.output.is_empty() { "0" } else { "1" });
        }
    }
});
```

**Keep state forever.** `db_exec` / `db_query` / `db_batch` against your own
SQLite file, with `zstd_ref` compressing blobs on the way in. That's how
resurrect stores every pane's scrollback and how agents keeps a roster that
outlives the server.

**Reach the rest of tmux.** `format_expand` evaluates any `#{...}` format
against any target, `set_option` publishes `@`-options the status line picks
up, `send_text` / `send_key` type into a pane, `display_message` talks to the
user. And with `service-call`, all of the above can be asked of a *different
machine* over a remote link.

### Things you could build in an afternoon

- A **build/test watcher**: subscribe to the shell-integration prompt event,
  `panes_search` for `FAILED`, toast the pane that broke.
- A **pane switcher that actually shows the panes** — a grid of live previews
  instead of a list of names.
- A **scrollback search across every session**, with fuzzy ranking, storing a
  history of your searches in SQLite.
- A **"what was I doing" journal**: record cwd, branch and title per pane on
  every window switch; query it a week later.
- A **deploy/CI status light** in the status line, polling an API with
  `run_job` and publishing `@ci_state`.
- A **pane babysitter** that watches for a known prompt and `send_key`s the
  answer — or messages you through mailbox when it can't.
- A **per-project layout launcher** reading a repo's config with `fs_read` and
  building the layout with `run_command`.

The plugins that ship here are just the ones that got finished: a roster with
live previews and cross-machine merge (agents), full session snapshots with
scrollback (resurrect), a scheduler (cron), a notification feed (notify-toast),
a status-line probe (git-status), a message bus (mailbox). None of them is more
than a few thousand lines, and every one of them is a normal Rust crate you can
copy.

### Writing one

Full guide: `plugin-host/WRITING-PLUGINS.md`. Wire format: `plugin-host/ABI.md`.
Working examples in `plugin-host/examples/` — `hello-raw` (no SDK),
`ticker`, `fs-probe`, `services-probe` are the tiny ones to read first.

Dev loop:

```sh
sh scripts/build-plugins.sh myplugin
tmux2 load-plugin ./plugin-host/target/wasm32-unknown-unknown/release/myplugin.wasm
tmux2 show-plugins -v        # running? instances, stats, caps
tmux2 plugin-log myplugin    # your log() output, errors, panics
tmux2 reload-plugin myplugin # live swap, state preserved
```

Also: `unload-plugin`, `enable-plugin` / `disable-plugin`.

---

## 4. The plugins

### agents — `prefix + A`
Roster of coding-agent CLI sessions running in your panes (claude, codex, pi,
opencode), grouped by state: **needs input / waiting / working / done**, with a
live preview of the selected pane. `j`/`k` move, `Enter` jumps to the pane,
`a` archives, `h` folds finished rows, `/` filters, `C-f` greps live pane
contents. Identity comes from the harness's own session files, not from a
guess at the process, so rows survive a `restart-server` — and the roster is
durable in the plugin's SQLite store.

> Only really tested hard with **Claude Code**. The others are detected but
> less exercised.

It's a view + provider plugin, so over a remote link one picker shows agents
on **every linked server**. `plugin-command agents 'message <agent-id> <text>'`
messages an agent by id — on this machine or another.

### mailbox — cross-agent messages
The delivery primitive behind `agents message`. Each agent id gets a box in a
SQLite store; a message to an agent on a linked server rides the plugin bridge
and lands in that server's store. Nothing is ever typed into a pane, so it
can't land in a half-written prompt.

```sh
tmux2 plugin-command mailbox 'send <box>[@server] hello'
tmux2 plugin-command mailbox 'inbox <box>'      # writes @mailbox_<box> as JSON
tmux2 plugin-command mailbox list               # unread counts
```

With `claude-notify`, a message is pushed straight into the receiving Claude
session's inbox socket, so the agent wakes with it. See `CROSS-AGENT.md`.

### resurrect — `prefix + C-r`
Periodically snapshots your panes to disk (**including scrollback**) and
restores them. In the picker: `Enter` restores, `C-d` deletes, `C-k` saves and
kills the server, `C-r` saves and **restarts the server in place** — pane
processes stay live. Also `plugin-command resurrect save|restore [id]|status`.
Config: `autosave = "5m"` (`"0"` = off), `keep = "1"`.

Note the restart trick is a core fork feature, not the plugin's:
`restart-server` replaces the server's own image with `execve`, keeping its
pid, so the panes stay its children and `waitpid`, `remain-on-exit` and
`#{pane_dead_status}` all keep working. Sessions, windows, layouts, options,
bindings, buffers, scrollback, floating panes, dead panes and remote links all
come back; **pane modes (copy mode, pickers) don't**. Details in
`SERVER-RESTART.md`.

### session_creator — `prefix + S` / `prefix + W` *(WIP)*
A form for creating a session: plain (folder + name) or git worktree
(repo / name / dest / branch), `C-t` toggles. Prefills come from the current
pane, or from the highlighted row if you press `S`/`W` inside the `prefix + w`
tree. `C-j`/`C-k` move between fields, `C-u` clears one, `Tab` completes in
place.

### notify-toast — `prefix + N`
Any program that emits OSC 9 or OSC 777 (e.g. an agent's stop hook running
`printf '\e]9;done\a'`) shows up as a toast in a floating pane, top-right, in
whatever window you're looking at. One line per notifying pane, so a chatty
pane can't flood it. `prefix + N` opens a `choose-tree`-style chooser of the
feed with a live preview: `Enter` jumps to the source pane, `d` dismisses,
`h`/`l` fold.

### git-status
Pane-scoped. Publishes `@git_branch` / `@git_dirty` on its own pane, so
`#{@git_branch}` in the status line always reflects the pane you're in and
empties out outside a repo. Refreshes on the shell prompt mark (OSC 133;A) and
on pane focus; background panes do no git work, and a slow `git status` never
piles up or blocks the server.

### cron
Scheduled jobs, durable in SQLite. The verb line is one tmux argument, so quote
it:

```sh
tmux2 plugin-command cron 'add -n backup every 1h -- shell ~/bin/backup'
tmux2 plugin-command cron 'add 0 9 * * 1-5 -- tmux display-message standup'
tmux2 plugin-command cron ls | status | 'run <id>' | 'rm <id>' | 'enable <id>'
tmux2 plugin-command cron pick    # picker: Enter run, e toggle, d delete, l detail
```

Missed occurrences while the server was down are handled per job
(`--catchup skip|once|each`), failures retry with backoff.

---

## 5. Remote links

```sh
tmux2 remote-attach -t work dev-box    # mirror (creating if needed) dev-box:work
tmux2 remote-attach -L dev-box         # list sessions there
tmux2 remote-attach -k dev-box         # drop the link (remote session keeps running)
```

The remote session shows up locally as `dev-box/work`. Every remote window and
pane becomes a local window and pane: output renders locally, keys go to the
remote, scrollback fills on connect, and the remote owns the layout. It speaks
control mode over ssh, so **any tmux 3.3+ works on the far side** — the remote
does not need this fork (though it does if you want provider plugins there).

Structural commands (`split-window`, `new-window`, `kill-pane`, `resize-pane`,
`select-layout`, `rename-*`, `respawn-*`, …) run on the remote; everything else
— copy mode, `select-pane`, `send-keys`, options, hooks, floating panes — stays
local. A drop leaves the panes in place with one disconnected line and retries
with backoff; `#{remote_state}`, `#{remote_error}`, `#{remote_connected}` and
`#{session_remote_host}` are what the status-line snippet above uses.

**Plugins just work across the link.** The local server pushes provider-role
plugins to the remote, so `prefix + A` lists agents on every linked machine and
you can message them.

Direction matters for security: calls *from* you *into* a server you ssh'd into
are always allowed; calls *back* from a linked server into yours are gated
twice — the callee must declare the method in `[caps.services] serve_remote`,
**and** you must allow the (server, plugin) pair. A menu offers it the first
time one asks, or:

```sh
tmux2 plugin-peers list
tmux2 plugin-peers allow dev-box mailbox
```

Read `REMOTE.md` for the design and `UX_NOTES.md` (§R1–R8) before calling
anything a bug — a failing link mostly looks like nothing happening.

---

## 6. Cheat sheet

| | |
|---|---|
| `tmux2` | the fork, on socket `-L tmux2` |
| `update` / `update -n` | install the latest release, restart in place |
| `restart-server [-b binary]` | restart onto a new binary, keep panes alive |
| `remote-attach [-t sess] host` | mirror a remote session locally |
| `sync-plugins <manifest>` | reconcile loaded plugins against the manifest |
| `update-plugins <manifest>` | registry check + lock + live reload |
| `load-plugin` / `reload-plugin` / `unload-plugin` | one-off plugin control |
| `show-plugins -v` / `plugin-log <name>` | is it running, and why not |
| `plugin-command <plugin> '<verb …>'` | talk to a plugin |
| `plugin-peers allow <server> <plugin>` | let a linked server call back |
