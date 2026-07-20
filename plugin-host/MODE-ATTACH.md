# Design: attaching plugin modes to existing panes

Status: **proposal, not implemented**. v1 plugin modes (see ABI.md "UI
modes") always run on a freshly spawned empty floating pane. This
document works out what it takes to let a plugin enter its mode on a
pane that already exists — the user's shell, an editor, any real pane —
and the policy questions that come with it.

## Precedent: this is tmux's normal case

In tmux, entering a mode on an existing pane is the *default* behavior
of the machinery, not an extension. `copy-mode`, `clock-mode` and
`choose-tree` all call `window_pane_set_mode(wp, ...)` on a live pane:

- A `window_mode_entry` is pushed onto the pane's mode stack and
  `wp->screen` flips to the mode's own screen. What is *displayed* is
  the mode's buffer; the pane's real screen (`wp->base`) keeps
  receiving the process's output underneath (flagged
  `PANE_UNSEENCHANGES`).
- Keys and resizes go to the mode instead of the process.
- On exit the entry pops, `wp->screen` flips back to `wp->base`, and
  the accumulated output is visible again. The process never knows.

The v1 float is the exotic variant: we spawn a throwaway empty pane
purely as a surface to enter the mode on. Attach = exposing the default
path the plugin API currently bypasses.

## What already works unchanged

The mode implementation (`window-plugin-mode.c`) is target-agnostic:

- Rendering (`mode_write` -> `input_parse_screen`), key/resize/closed
  events, and the retained preview rect never ask whether the pane is a
  float.
- Pane death already delivers `mode-closed {reason: "killed"}` via
  `window_pane_free_modes`.
- The Rust host's ownership map (modes.rs), generation checks, and
  instance-teardown purge are keyed by mode id only.

## Changes required

### ABI surface

`mode_open` grows a second, mutually exclusive target form:

```
mode_open {pane}                     -> {mode}   # attach variant
mode_open {window?, width, height, x?, y?, title?} -> {mode}  # v1 float
```

Scope check for the attach form is `check_pane_target` (the same rule
as `capture_pane`: pane scope = own pane only, window scope = its
panes, session scope = linked panes, server/`cross-scope` = anything).

C vtable: a separate slot is cleaner than widening the float open,
since the signatures barely overlap:

```
int64_t (*mode_attach)(uint32_t pane);
```

SDK: `ModeOpts` gains a target, e.g.
`ModeTarget::Float { window?, width, height, x?, y?, title? }` vs
`ModeTarget::Pane(PaneId)`, or a separate `mode_attach(PaneId)` entry
point.

### Open path (plugin-mode.c)

No spawn, no layout cell. Look up the pane, refuse if unusable, enter:

1. `window_pane_find_by_id`, reject dead/destroyed panes.
2. Walk `wp->modes`: if any entry is `window_plugin_mode`, refuse with
   `E_LIMIT` (see "Contention" below).
3. Stash the pending mode id, `window_pane_set_mode(wp, NULL,
   &window_plugin_mode, NULL, NULL, NULL)`, register `mode_id ->
   pane_id` with `owns_pane = 0`.

Focus is NOT changed (the float variant focuses because keys are the
point of a fresh panel; attaching to a pane the user is looking at
should not yank their focus). A later `focus: true` option is cheap.

### Close path forks on ownership

The registry entry needs an `owns_pane` flag:

- `owns_pane = 1` (v1 float): close = deferred `server_kill_pane`,
  unchanged.
- `owns_pane = 0` (attached): close = pop our mode entry and leave the
  pane alive. tmux can only reset the *front* mode
  (`window_pane_reset_mode`), so if the user stacked copy-mode on top,
  the deferred close loops: reset front until our entry has popped.
  Evicting the stacked copy-mode session is acceptable and matches what
  `window_pane_reset_mode_all` does on respawn.

Nothing in the attached-close path destroys tmux objects, so unlike the
float kill it would even be safe to run inline during a drain; keeping
the single deferred-timer path for both variants is still preferred (one
teardown invariant instead of two).

`mode-closed` reasons: `"closed"` (plugin), `"killed"` (pane died),
plus a new `"cancelled"` (user escape hatch, below).

### Contention: one plugin mode per pane, ever

There is a single shared `const struct window_mode window_plugin_mode`,
and `window_pane_set_mode` *reuses* an existing stack entry with the
same mode pointer (moves it to the head) instead of creating a second
one. Consequences:

- Two plugin modes can never coexist on one pane — not even from two
  different plugins. First come, first served.
- The open path must detect an existing `window_plugin_mode` entry
  anywhere in the stack and refuse (`E_LIMIT`), or set_mode's reuse
  would silently hand plugin B the screen of plugin A's mode.

This also applies to the float variant in theory, but floats are
freshly spawned so the case cannot arise there.

### The user escape hatch (precondition, not optional)

On a float a hostage situation is self-limiting: the user can kill the
pane. On their *shell pane*, a buggy or malicious plugin holding a mode
open hijacks the keyboard with no recourse — our mode has no `command`
handler, so even `send-keys -X cancel` does nothing.

Before shipping attach, implement the `window_mode.command` vtable slot
on `window_plugin_mode` handling `cancel`:

- schedule the ownership-appropriate close (kill for floats, pop for
  attached),
- deliver `mode-closed {reason: "cancelled"}` to the owner.

That gives users `send-keys -X cancel -t %5` and lets them bind a key
to it. It hardens the v1 float case too (worth doing independently of
attach).

### Capability model

`mode` today means "may draw in panes it created". Attach changes the
meaning to "may commandeer panes it is scoped to" — the same trust step
as `send-keys`, arguably bigger (it swallows *all* keys, not just
injects some). Proposal: a separate `mode-attach` grant (implies
`mode`), so a user can allow floating panels without allowing pane
takeover. `1 << 14` in caps.rs; `required_cap` keys on the param form.

## Lifecycle matrix (attached variant)

| event | behavior |
|---|---|
| plugin `mode_close` | deferred pop of our entry; pane untouched; `mode-closed "closed"` |
| user `send-keys -X cancel` | same, `mode-closed "cancelled"` |
| pane killed / window dies | entry freed with the pane; `mode-closed "killed"` |
| process in pane exits | pane-exited/remain-on-exit as usual; if the pane closes, `"killed"` |
| `respawn-pane` | `window_pane_reset_mode_all` pops us; `mode-closed "killed"`; pane respawns |
| user stacks copy-mode on top | `mode_write`/`mode_preview` fail transiently (front-only rule); close pops through |
| plugin reload/unload/scope death | host purge force-closes via the vtable, as v1 |
| pane resized | `mode-resize`, as v1 |

## Open questions

- **Reuse vs. refuse for the same owner**: should a plugin calling
  attach twice on the same pane get its existing mode id back instead
  of `E_LIMIT`? (Leaning refuse: ids are cheap, implicit reuse hides
  bugs.)
- **Who may evict whom**: should `cancel` (user) also work while a
  stacked copy-mode hides the plugin mode? (Yes — cancel targets the
  plugin entry wherever it sits in the stack.)
- **Focus option**: `focus: bool` on attach, default false — confirm.
- **Format visibility**: expose the owning plugin in a format
  (e.g. `#{pane_mode}` already shows `plugin-mode`; add
  `#{pane_mode_plugin}`?) so status lines can show who holds a pane.

## Sizing

Roughly: ~80 lines C (attach open, `owns_pane` close branch, `command`
handler), ~40 lines Rust (dispatch arm, cap, vtable slot + cbindgen
regen), ~20 lines SDK, plus a host mock-vtable test for
attach/close-restores-pane and docs (ABI.md table + this file folded
in). All concentrated in files the mode feature just introduced.
