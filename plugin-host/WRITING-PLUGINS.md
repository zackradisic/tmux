# Writing a tmux plugin

This is the complete guide to authoring a plugin in Rust with the SDK. It is
self-contained: everything needed to go from zero to a loaded, running
plugin is on this page. (The raw wire format lives in [ABI.md](ABI.md); you
only need it if you are targeting the ABI from another language.)

## What a plugin is

A WebAssembly module (`wasm32-unknown-unknown`) running **inside the tmux
server**. It keeps normal in-memory state, receives events (panes/windows/
sessions appearing, disappearing, changing), and acts on tmux through host
APIs. Rules the host enforces — your code cannot break tmux, but it can get
itself killed:

- **CPU budget**: every callback (init, event handler, async wakeup) runs
  on the tmux event loop, so keep it short. ~2 ms logs a warning. A
  callback that reaches 2 s is a runaway: it traps, the instance is torn
  down, and a fresh one starts. Never busy-wait; use the async APIs.
- **No blocking**: there is no filesystem, network, or process access
  except through the async host APIs. `std::thread`, `std::fs`,
  `std::net` do not exist in the sandbox.
- **Panics are traps**: a panic logs the message (see `plugin-log`) and
  destroys the instance. Three consecutive failures disable the plugin.
- **Weak handles**: object ids (`PaneId` etc.) may refer to dead objects;
  calls on them return `E_NO_SUCH_OBJECT` errors, never crash.

## Quickstart

Prereqs: Rust with the wasm target (`rustup target add
wasm32-unknown-unknown`) and this tmux fork built with
`./configure --enable-plugins && make`.

Crate setup — a plugin is a `cdylib` depending on the SDK (path dependency;
the SDK is not on crates.io):

```toml
# Cargo.toml
[package]
name = "myplugin"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
tmux-plugin-sdk = { path = "/home/zack/tmux2/plugin-host/sdk" }
serde = { version = "1", features = ["derive"] }
serde_json = "1"

[profile.release]
opt-level = "s"
panic = "abort"
```

Minimal plugin:

```rust
// src/lib.rs
use tmux_plugin_sdk::prelude::*;

struct MyPlugin {
    renames: u64,
}

impl Plugin for MyPlugin {
    const NAME: &'static str = "myplugin";
    type Config = serde_json::Value; // or a serde struct, see Config below

    fn init(ctx: &Ctx, _config: Self::Config) -> Result<Self, String> {
        ctx.subscribe(&["window-renamed"]).map_err(|e| e.message.clone())?;
        log("myplugin ready");
        Ok(Self { renames: 0 })
    }

    fn on_event(&mut self, _ctx: &Ctx, event: Event) {
        self.renames += 1;
        let _ = display_message(&format!(
            "window renamed ({} so far)", self.renames));
    }
}

tmux_plugin!(MyPlugin);
```

Build, load, iterate:

```sh
cargo build --target wasm32-unknown-unknown --release

tmux load-plugin ./target/wasm32-unknown-unknown/release/myplugin.wasm
tmux show-plugins -v          # is it running? instances, stats, caps
tmux plugin-log myplugin      # your log() output, errors, panics

# after rebuilding:
tmux reload-plugin myplugin   # live swap, state preserved via snapshot()
```

`load-plugin` is idempotent: re-running it with unchanged code/config does
nothing; with changed code it live-reloads. To remove a plugin,
`unload-plugin myplugin`.

### Declarative loading: the manifest

For everything beyond quick experiments, declare your plugins in a TOML
manifest and let `sync-plugins` reconcile the world against it — loading
new entries, live-reloading changed ones, and **unloading anything you
removed** (which plain `load-plugin` lines in tmux.conf never do):

```toml
# ~/.tmux/plugins.toml
[plugins.notify-toast]
path  = "notify_toast.wasm"          # relative to this manifest
scope = "server"
caps  = ["run-command", "mode"]
config = { duration_ms = 0 }         # native types, not -o strings

[plugins.git-status]
path  = "git_status.wasm"
scope = "pane"
caps  = ["run-process", "write-options"]
enabled = true                       # false = keep declared, disabled
```

```
# tmux.conf
sync-plugins ~/.tmux/plugins.toml
bind r source-file ~/.tmux.conf      # edits converge on reload
```

Rules worth knowing:

- **Identity is the `[plugins.NAME]` key.** Rename the `.wasm` file and
  update `path`: same plugin, and a no-op if the content is unchanged.
  Rename the key: that declares a *different* plugin (old unloads, new
  loads fresh).
- **Only managed plugins are swept.** Interactive `load-plugin` runs are
  unmanaged and survive any sync; a manifest entry with the same name
  adopts them (and an explicit `load-plugin` takes a name back out of the
  pool until the next sync).
- **Bad manifests change nothing**: parse errors, unknown capabilities or
  missing files reject the whole sync atomically.
- `config` tables pass native JSON types to your `Config` struct (numbers
  arrive as numbers — no string parsing as with `-o`).
- `show-plugins` marks managed plugins; the sync prints a summary
  (`synced ...: 1 loaded, 1 updated, 2 unchanged, 1 unloaded`).

## The `Plugin` trait

```rust
impl Plugin for MyPlugin {
    const NAME: &'static str;          // required
    const STATE_VERSION: i32 = 1;      // bump when snapshot shape changes
    type Config: Deserialize + Default;

    // Required. Subscribe to events and spawn async tasks here.
    fn init(ctx: &Ctx, config: Self::Config) -> Result<Self, String>;

    // Subscribed events + lifecycle events (*-created / *-destroyed).
    fn on_event(&mut self, ctx: &Ctx, event: Event) {}

    // Carry state across a live code reload. Stateless (None, default):
    // reload just re-inits. Stateful: snapshot() serializes STATE ONLY
    // (never config), and after the NEW code's init() runs, restore()
    // combines that fresh instance (config-derived fields) with the
    // carried state; the result replaces `fresh`. Returning None refuses
    // the state and the OLD code keeps running.
    fn snapshot(&self) -> Option<serde_json::Value> { None }
    fn restore(fresh: Self, old_version: i32, state: serde_json::Value)
        -> Option<Self> { None }

    // Return true to absorb a config change; false (default) = restart me
    // with the new config.
    fn on_config_changed(&mut self, ctx: &Ctx, config: Self::Config) -> bool { false }

    // Last words before teardown. Tiny budget: log/cleanup only.
    fn on_unload(&mut self, ctx: &Ctx) {}
}
```

### Config

`load-plugin -o key=value -o other=x` reaches your `Config` with **all
values as strings** (a bare `-o flag` becomes `true`). So
model config as `Option<String>` fields and parse numbers yourself:

```rust
#[derive(serde::Deserialize, Default)]
struct Config {
    #[serde(default)]
    interval_ms: Option<String>,
}
let interval: u64 = config.interval_ms.as_deref()
    .and_then(|s| s.parse().ok()).unwrap_or(1000);
```

## Scopes

`load-plugin -s server|session|window|pane` (default `server`) controls
instantiation:

- `server`: one instance, sees every event.
- `session`/`window`/`pane`: one instance **per object**, created
  automatically when the object appears and destroyed when it dies; it only
  receives events touching its object, and may only *act on* its own
  object (window scope: its panes; session scope: its windows/panes).
  Find your own identity with `self_info()` (scope kind + id +
  generation) or the ids in incoming events.

Each instance is fully isolated: own wasm memory, own state, own budget.

## Events

Events cross the ABI as binary buffers (interned name id + scope ids +
a flat field block); the SDK wraps them:

```rust
pub struct Event {
    pub id: u32,              // interned event name id
    pub seq: u64,
    pub scope: EventScope,    // Option<u32> ids: client/session/window/pane
}
impl Event {
    fn name(&self) -> String;              // "pane-focus-in" (cached lookup)
    fn is(&self, name: &str) -> bool;      // integer compare after one intern
    fn get_str(&self, key: &str) -> Option<&str>;   // session_name, text, ...
    fn get_i64(&self, key: &str) -> Option<i64>;    // window_index, ...
    fn get_bool(&self, key: &str) -> Option<bool>;
    fn iter(&self) -> impl Iterator<Item = (String, ValueRef)>;
}
```

Delivered without subscription (lifecycle): `session-created`,
`window-created`, `pane-created`, `session-destroyed`, `window-destroyed`,
`pane-destroyed`, `client-destroyed`, `session-closed`.

Available via `ctx.subscribe(&[...])` — every event on the tmux event bus
(the bridge registers a sink for each hookable event, so new upstream
events appear here automatically). The vocabulary as of next-3.8:
`window-linked`, `window-unlinked`, `window-renamed`, `window-resized`,
`window-layout-changed`, `window-pane-changed`, `window-closed`,
`window-zoomed`, `window-unzoomed`, `session-renamed`,
`session-window-changed`, `pane-focus-in`, `pane-focus-out`, `pane-exited`
(with `exit_status`/`exit_signal`/`exit_success` fields), `pane-died`,
`pane-mode-changed`, `pane-title-changed`, `pane-set-clipboard`,
`pane-moved`, `pane-resized`, `pane-activity`, `pane-bell`,
`marked-pane-changed`, `client-attached`, `client-detached`,
`client-closed`, `client-resized`, `client-active`,
`client-session-changed`, `client-focus-in`, `client-focus-out`,
`client-dark-theme`, `client-light-theme`, `paste-buffer-changed`,
`paste-buffer-deleted` (buffer name in the `paste_buffer` field),
`alert-activity`, `alert-bell`, `alert-silence`.

Extra payload fields tmux attaches to an event (e.g. `window_index`,
`old_pane`, `exit_status`) are forwarded as fields: `event.get_i64(...)` /
`event.get_str(...)`.

Shell-integration events (from escape sequences a program emits inside a
pane; scope carries the pane and its window):

- `pane-shell-prompt` — OSC `133;A` (prompt shown, i.e. a command just
  finished). Enable by making the shell emit the mark, e.g. in ~/.bashrc:
  `PROMPT_COMMAND='printf "\e]133;A\a"'"${PROMPT_COMMAND:+;$PROMPT_COMMAND}"`
- `pane-command-started` / `pane-command-finished` — OSC `133;B/C/D`, with
  `command_status`, `command_start_time` and `command_duration` fields.
- `pane-notification` — OSC `9;message` (iTerm2 style; `9;4;...` progress
  reports are excluded) or OSC `777;notify;title;body` (rxvt style). The
  message arrives as `event.get_str("text")` (`title: body` for 777). Handy
  for agents/build scripts: `printf '\e]9;done\a'` from any pane, however
  deeply nested (ssh, make, ...), reaches a subscribed plugin.
- `plugin-command` — sent by the `plugin-command <plugin> <command>` tmux
  command, so users can wire key bindings to your plugin (e.g. `bind N
  plugin-command notify_toast chooser`). Targeted: only the named plugin
  receives it. Subscribe to it, match `event.get_str("text")`, and use the
  scope (the `-t` target's pane/window/session) to know where to act.

  Chooser keys: keys unhandled by choose-tree (`prefix w` / `prefix s`),
  choose-client or choose-buffer are looked up in the `choose-tree`,
  `choose-client` or `choose-buffer` key table (choose-tree first
  consults a table for the selected row's type: `choose-tree-session`,
  `choose-tree-window` or `choose-tree-pane`). Formats in the bound
  command expand against the *highlighted* item before parsing, and the
  command runs with that item as target — so `bind -T choose-tree W
  plugin-command session_creator worktree` delivers a `plugin-command`
  event whose session/window/pane describe whatever row the user's
  cursor was on (a session row resolves to its active window/pane). The
  chooser closes like Enter unless the binding has `-k` (keep open). The
  `session_creator` example builds on this: a two-kind new-session form
  (plain folder or git worktree) prefilled from the target pane's cwd.
  The mechanism is plugin-agnostic — plain tmux commands work too, e.g.
  `bind -T choose-tree-session -k R command-prompt -I '#{session_name}'
  "rename-session -t '#{session_id}' '%%'"` renames the highlighted
  session in place, chooser still open (bind `rename-window` to R in
  `choose-tree-window` for window rows). Note the pressing client's
  *current* window is not in the event — the target may live in another
  session; resolve the client id via `list_clients` if you need to open
  UI where the user is looking.

## API reference (`tmux_plugin_sdk::prelude::*`)

Sync (return immediately):

```rust
subscribe(&["event", ...]) / unsubscribe(&[...])        -> Result<(), HostError>
list_sessions()  -> Result<Vec<SessionInfo>, HostError>
list_windows()   -> Result<Vec<WindowInfo>, HostError>
list_panes()     -> Result<Vec<PaneInfo>, HostError>
list_clients()   -> Result<Vec<ClientInfo>, HostError>
send_text(pane: PaneId, text: &str)                     // literal keystrokes
send_key(pane: PaneId, key: &str)                       // "Enter", "C-c", "M-x"
capture_pane(pane, start: Option<i32>, end: Option<i32>) -> Result<String, _>
capture_pane_into(pane, start, end, escapes: bool, &mut Vec<u8>) // reusable buf
    // rows relative to visible top; negative = history; ≤2000 lines/call
resolve_pane(PaneId) -> Result<PaneInfo, _>      // id, window, size, active,
                                                 // floating, dead, title/shell/
                                                 // cwd ("" = absent)
resolve_window(WindowId) -> Result<WindowInfo, _>   // + sessions, panes (in
                                                    // window order), active_pane
resolve_session(SessionId) -> Result<SessionInfo, _> // + windows [(idx, id)]
self_info() -> Result<SelfInfo, _>               // scope kind/id + generation
get_option(name: &str) -> Result<String, _>             // any option
set_option(name: &str, value: &str)                     // @-options only
format_expand(target, "#{window_layout} ...") -> Result<String, _>
    // any #{...} format against a scope; #() jobs disabled
display_message(msg: &str)                              // status line + log
log(msg: &str)                                          // plugin-log only
intern(name) -> u32 / intern_name(id) -> Option<String> // event/key name ids
fs_root() -> Result<String, _>       // the plugin's private data dir
now_ms() -> u64                      // Unix time, milliseconds
home_dir() -> String                                    // for expanding a leading ~
fs_write_sync(path, data, append) / fs_read_sync(path, offset, &mut buf)
    // small files; paths relative to fs_root; caps fs-write / fs-read

// UI modes (capability: mode) — see the "UI modes" section
mode_open(&ModeOpts { window?, width, height, x?, y?, title? })
                                                        -> Result<ModeId, _>
mode_write(ModeId, data: &[u8])                         // ANSI bytes, ≤256 KiB
mode_preview(ModeId, Option<&PreviewRect>)              // live pane mirror
mode_move(ModeId, window: Option<WindowId>)             // relocate the float,
                                                        // id/screen intact
mode_resize(ModeId, width: u32, height: u32)             // grow/shrink the float;
                                                        // a mode-resize confirms
mode_close(ModeId)
```

Async (`.await` inside spawned tasks):

```rust
sleep_ms(ms: u64).await
run_job("shell command", cwd: Option<&str>).await
    -> Result<JobOutput { status, signalled, output }, HostError>
run_command("any tmux command string").await            // via command queue
fs_write(path, data: Vec<u8>, append).await -> bytes    // fs executor,
fs_read(path, offset, capacity).await -> (Vec<u8>, eof) // zero-copy, no cap
fs_list(path) -> Listing                                // dir entries + d_type;
                                                        // names borrow the buffer
fs_rename(from, to, RenameFlag).await                   // atomic in the sandbox;
    // Replace | NoReplace | Exchange. Crash-safe publish: write x.tmp,
    // then fs_rename("x.tmp", "x", RenameFlag::Replace) - the worker
    // syncs data before the name moves, so a reader never sees a mix
```

Async tasks are spawned with `ctx.spawn(async move { ... })` in `init` (or
anywhere). Tasks are detached and independent of `&mut self`; communicate
back through options, messages, or by keeping shared state in the task.
A polling loop looks like:

```rust
ctx.spawn(async move {
    loop {
        if sleep_ms(5000).await.is_err() { return } // instance torn down
        if let Ok(out) = run_job("git status --porcelain", None).await {
            let _ = set_option("@git_dirty",
                if out.output.is_empty() { "0" } else { "1" });
        }
    }
});
```

`ctx.spawn` returns a `TaskId`. Pass it to `ctx.cancel` to stop the task
before it finishes:

```rust
let id = ctx.spawn(async { sleep_ms(200).await.ok(); do_the_thing(); });
ctx.cancel(id);          // the task never reaches do_the_thing()
```

Cancelling drops the task's future, which drops whatever host operation it
was awaiting. A pending `sleep_ms` is cancelled host-side, so its timer
never fires and never re-enters the guest. Any other operation already in
flight (a job, a command, an fs call) still runs to completion on the host
- only its result is discarded, along with any buffer the host worker was
using. Cancelling a finished task does nothing, and a task may cancel
itself.

Prefer *not* spawning to spawning-then-cancelling. If the question is "do
not start a second one of these", one boolean plus a loop is simpler than
a handle:

```rust
if !self.running {                       // one worker at a time
    self.running = true;
    ctx.spawn(worker());                 // worker loops until there is
}                                        // nothing left, then clears it
```

Typed ids: `PaneId(u32)`, `WindowId(u32)`, `SessionId(u32)`, `ClientId(u32)`
(Display as `%5`, `@3`, `$1`, `#2`).

## Capabilities

Beyond the always-granted `read-state`, `display-message`, `timers`
(timers cover `sleep_ms`), everything must be granted at load time:

```
load-plugin -c send-keys -c run-process ... myplugin.wasm
```

| capability | unlocks |
|---|---|
| `write-options` | `set_option` (@-options) |
| `send-keys` | `send_text` / `send_key` |
| `capture-pane` | `capture_pane` |
| `run-process` | `run_job` |
| `run-command` | `run_command` |
| `cross-scope` | acting on objects outside the instance's scope |
| `mode` | UI modes (`mode_open` and friends) |

Denied calls return `HostError { code: E_CAP_DENIED }` — handle errors, do
not unwrap host results.

Optionally ship `myplugin.toml` next to the `.wasm` declaring what you
need (users see it; effective caps = your requests ∩ their grants):

```toml
[caps]
requests = ["run-process", "write-options"]
[caps.run-process]
argv0 = ["git"]
```

## UI modes: interactive panels

In tmux, a *window mode* is a takeover of a pane: while a mode is
entered, the mode — not the process in the pane — owns the screen the
pane displays and receives its keys and resizes. `copy-mode`,
`clock-mode` and the `choose-tree` browser are all window modes. The
`mode` capability exposes this machinery to plugins; in v1 a plugin mode
always runs on a **freshly spawned empty floating pane** (entering a mode
on an existing pane is deliberately not offered yet — the design for
that is in [MODE-ATTACH.md](MODE-ATTACH.md)), so in practice it behaves
like a floating panel your plugin draws directly.

`mode_open` spawns the empty float, enters the mode on it and focuses
it. You render by sending ANSI bytes — the server parses them with the
full terminal escape parser, so anything from `printf`-style positioning
to a ratatui buffer works. Keys pressed while the panel is focused come
back to you as events; one optional **preview rect** shows a live mirror
of another pane inside your panel (refreshed automatically, ~2x/second).

```rust
// Open: centered 60x12 float in this window (pane/window scope can omit
// `window`; server scope must name one).
let mode = mode_open(&ModeOpts {
    width: 60, height: 12,
    title: Some("picker".into()),
    ..Default::default()
})?;

// Draw: full redraws are idiomatic. \x1b[2J clear, \x1b[row;colH move,
// \x1b[7m reverse video, \x1b[0m reset. Rows/columns are 1-based.
mode_write(mode, b"\x1b[2J\x1b[1;2HPick a pane:\x1b[3;2H\x1b[7m 1. shell \x1b[0m")?;

// Live preview of pane %5 on the right half (cells are 0-based here).
mode_preview(mode, Some(&PreviewRect { pane: PaneId(5), x: 30, y: 0, w: 29, h: 10 }))?;
```

Events arrive through the normal `on_event`, targeted at your instance
only (no subscription needed); match them by the mode id in
the `mode` field (`event.get_i64("mode")`):

- `mode-key`: `get_str("key")` is a tmux key name ("q", "Enter", "Escape",
  "Down", "MouseDown1Pane", ...); mouse keys add `mouse_x`/`mouse_y`/
  `mouse_b` fields
  with pane-relative cell coordinates.
- `mode-resize`: `get_i64("width")`/`get_i64("height")` — redraw.
- `mode-closed`: terminal, with `get_str("reason")` `"closed"` (your
  `mode_close`) or `"killed"` (user killed the pane, reload, window
  died). Drop your state for the mode; the id is dead.

Notes:

- The panel is a real pane: users can kill it, resize it, or stack
  copy-mode on top (your writes fail with `E_NO_SUCH_OBJECT` while they
  browse; the pane stays yours when copy-mode exits).
- `mode_close` tears the pane down at the next event-loop pass and then
  delivers `mode-closed` — treat that event, not the call, as "gone".
- A panel can follow the user across windows with `mode_move`: the same
  pane is relinked into the new window, so your mode id, rendered screen
  and event stream survive the move (at most a `mode-resize` arrives).
  Call it from a `session-window-changed` handler; see the notify-toast
  chooser. One caveat: when the float leaves a window, tmux's focus
  fallback fires `window-pane-changed` there — don't misread your own
  fallout as user input.
- Your modes are force-closed when your instance is reloaded/unloaded or
  its scope object dies.

The notify-toast example's chooser (`examples/notify-toast/`) is a
complete mode UI: a session > window > pane tree drawn like
`choose-tree`, with a selection bar, scrolling, folding (`h`/`l`),
hotkeys, mouse selection, and a live preview of the selected row's pane.

There is no host call for format expansion, but `set -F` expands a value
server-side, so a scratch `@`-option is a round trip that gets one back:

```
run_command("set -p -F -t %7 @scratch '#{pane_current_command}'").await;
let cmd = get_option_in(OptionTarget::Pane(PaneId(7)), "@scratch")?;
```

Keep the option pane-scoped (or window-scoped): concurrent expansions
then cannot read each other's answer, and the option dies with its
object. notify-toast uses this for `#{pane_current_command}` and
`#{pane_title}`.

## Debugging checklist

- `tmux plugin-log [-n N] myplugin` — your `log()` lines, panics with
  location, trap backtraces, budget warnings.
- `tmux show-plugins -v` — instances, generations, callback counts, soft
  budget overruns, traps, granted caps.
- Plugin disabled? Three consecutive failures. Fix the bug, then
  `reload-plugin myplugin` (resets the failure count).
- `tmux -Ltest -f /dev/null -v new-session -d` gives a throwaway server
  with a `tmux-server-*.log` in the CWD containing everything.

## Reference example

`plugin-host/examples/ticker/` exercises every feature: config, events,
async loop, jobs, tmux commands, snapshot/restore. Build it with
`cargo build -p ticker --target wasm32-unknown-unknown --release` from
`plugin-host/`.
