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

- **CPU budget**: every callback (init, event handler, async wakeup) must
  finish in a few milliseconds. ~2 ms logs a warning; ~8 ms aborts the
  callback. Never busy-wait; use the async APIs.
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
nothing; with changed code it live-reloads. Put the line in `~/.tmux.conf`
to load at server start. To remove a plugin, `unload-plugin myplugin`
(removing the config line alone does not unload a running server's copy).

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
    // reload just re-inits. Stateful: snapshot() serializes, and after the
    // NEW code's init() runs, restore() replaces the fresh value. restore
    // returning None refuses the state and the OLD code keeps running.
    fn snapshot(&self) -> Option<serde_json::Value> { None }
    fn restore(old_version: i32, state: serde_json::Value) -> Option<Self> { None }

    // Return true to absorb a config change; false (default) = restart me
    // with the new config.
    fn on_config_changed(&mut self, ctx: &Ctx, config: Self::Config) -> bool { false }

    // Last words before teardown. Tiny budget: log/cleanup only.
    fn on_unload(&mut self, ctx: &Ctx) {}
}
```

### Config

`load-plugin -o key=value -o other=x` becomes `{"key":"value","other":"x"}`
— **all values are JSON strings** (a bare `-o flag` becomes `true`). So
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
  Find your own identity with `host_call("self", json!({}))` or the ids in
  incoming events.

Each instance is fully isolated: own wasm memory, own state, own budget.

## Events

```rust
pub struct Event {
    pub event: String,        // e.g. "pane-focus-in"
    pub seq: u64,
    pub scope: EventScope,    // Option<u32> ids: client/session/window/pane
    pub data: serde_json::Value, // names: session_name, window_name, ...
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
(with `exit_status`/`exit_signal`/`exit_success` in data), `pane-died`,
`pane-mode-changed`, `pane-title-changed`, `pane-set-clipboard`,
`pane-moved`, `pane-resized`, `pane-activity`, `pane-bell`,
`marked-pane-changed`, `client-attached`, `client-detached`,
`client-closed`, `client-resized`, `client-active`,
`client-session-changed`, `client-focus-in`, `client-focus-out`,
`client-dark-theme`, `client-light-theme`, `paste-buffer-changed`,
`paste-buffer-deleted` (buffer name as `data.paste_buffer`),
`alert-activity`, `alert-bell`, `alert-silence`.

Extra payload fields tmux attaches to an event (e.g. `window_index`,
`old_pane`, `exit_status`) are forwarded verbatim in `event.data`.

Shell-integration events (from escape sequences a program emits inside a
pane; scope carries the pane and its window):

- `pane-shell-prompt` — OSC `133;A` (prompt shown, i.e. a command just
  finished). Enable by making the shell emit the mark, e.g. in ~/.bashrc:
  `PROMPT_COMMAND='printf "\e]133;A\a"'"${PROMPT_COMMAND:+;$PROMPT_COMMAND}"`
- `pane-command-started` / `pane-command-finished` — OSC `133;B/C/D`, with
  `command_status`, `command_start_time` and `command_duration` in data.
- `pane-notification` — OSC `9;message` (iTerm2 style; `9;4;...` progress
  reports are excluded) or OSC `777;notify;title;body` (rxvt style). The
  message arrives as `event.data["text"]` (`title: body` for 777). Handy
  for agents/build scripts: `printf '\e]9;done\a'` from any pane, however
  deeply nested (ssh, make, ...), reaches a subscribed plugin.

## API reference (`tmux_plugin_sdk::prelude::*`)

Sync (return immediately):

```rust
subscribe(&["event", ...]) / unsubscribe(&[...])        -> Result<(), HostError>
list_sessions() / list_windows() / list_panes() / list_clients()
                                                        -> Result<Value, HostError>
send_text(pane: PaneId, text: &str)                     // literal keystrokes
send_key(pane: PaneId, key: &str)                       // "Enter", "C-c", "M-x"
capture_pane(pane, start: Option<i32>, end: Option<i32>) -> Result<String, _>
    // rows relative to visible top; negative = history; caps: 2000 lines/256 KiB
resolve_pane(PaneId) -> Result<Value, _>    // {id, window, width, height,
                                            //  active, floating, dead, cwd?, shell?}
resolve_window(WindowId) -> Result<Value, _> // {id, name, width, height,
                                            //  sessions, panes, active_pane?}
resolve_session(SessionId) -> Result<Value, _> // {id, name, attached,
                                            //  current_window?, windows}
get_option(name: &str) -> Result<String, _>             // any option
set_option(name: &str, value: &str)                     // @-options only
display_message(msg: &str)                              // status line + log
log(msg: &str)                                          // plugin-log only
host_call(method, params) -> Result<Value, HostError>   // raw escape hatch
```

Async (`.await` inside spawned tasks):

```rust
sleep_ms(ms: u64).await
run_job("shell command", cwd: Option<&str>).await
    -> Result<JobOutput { status, signalled, output }, HostError>
run_command("any tmux command string").await            // via command queue
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
