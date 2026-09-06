# agents shims

The `agents` plugin observes which panes run an agent CLI and shows a
roster. Membership and liveness come from observation, so the roster
works with no setup. The plugin then enriches each row from the
harness's OWN session file, so name, real times, and turn state need no
help for two of the four harnesses.

| harness | identity + time + state | needs a shim? |
|---------|-------------------------|---------------|
| Claude  | `~/.claude/sessions/<pid>.json` | no |
| Codex   | the rollout `*.jsonl` the TUI holds open | no |
| pi      | none - an in-process extension reports it | yes |
| opencode| global db, no per-session file | yes |

## pi and opencode (required for a stable id + activity time)

These two keep no file the plugin can map from outside, so a tiny
in-process extension reports the session id once (`identify`) and pushes
turn state. Install the one you use:

* pi: copy `pi/agents.ts` to `~/.pi/agent/extensions/agents.ts`.
* opencode: copy `opencode/agents.ts` to
  `~/.config/opencode/plugin/agents.ts` (or name it in the `plugin`
  array of `opencode.json`).

## Claude Code and Codex (optional, richer turn state)

The roster already reads these from the session file / open rollout, so
no shim is needed. Add one only if you want the exact turn state before
the file catches up:

1. Copy `agent-status.sh` to `~/.config/agents/agent-status.sh` and make
   it executable.
2. Claude Code: merge `claude-code/settings.json` into
   `~/.claude/settings.json`.
3. Codex: copy `codex/hooks.json` to `~/.codex/hooks.json` (or merge its
   keys into `[hooks]` in `~/.codex/config.toml`).

## The wire

Every shim sends one line, keyed by the pane, never an id:

    tmux -S "${TMUX%%,*}" plugin-command -t "$TMUX_PANE" agents "<verb>"

The verbs are `identify <session_id> [session_file]` (once, from the
in-process extensions) and the status words below.

## The map from harness events to roster status

| roster       | Claude Code       | Codex             | pi              | opencode              |
|--------------|-------------------|-------------------|-----------------|-----------------------|
| working      | UserPromptSubmit  | UserPromptSubmit  | agent_start     | session.status busy   |
|              | PreToolUse        | PreToolUse        | turn_start      |                       |
| needs_input  | Notification      | PermissionRequest | ui_prompt_start | permission/question   |
| waiting      | Stop              | Stop              | agent_settled   | session.status idle   |
| done         | (pane close)      | (pane close)      | session_shutdown| session.deleted       |

`done` is best taken from pane liveness, not a hook: a killed or crashed
CLI never fires its exit hook, but the pane always dies.
