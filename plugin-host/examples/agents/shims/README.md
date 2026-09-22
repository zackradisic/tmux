# agents shims

The `agents` plugin observes which panes run an agent CLI and shows a
roster. Membership and liveness come from observation, so the roster
works with no setup. The plugin then enriches each row from the
harness's OWN session file, so name, real times, and turn state need no
help for two of the four harnesses.

| harness | identity + time + state | needs a shim? |
|---------|-------------------------|---------------|
| Claude  | `~/.claude/sessions/<pid>.json` | no |
| Codex   | a hook reports the id + rollout path | yes |
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

## Codex (required for a stable id + name)

Codex now ships as an npm package, so the pane's foreground command is
`node`, and codex keeps no session file the plugin can map from outside.
The roster still SEES the pane (it detects `node` + a `_` that points at a
codex script), but it cannot bind a durable id without help. A codex hook
supplies it: the hook payload carries `session_id` and `transcript_path`,
and the shim reports both. The roster then reads the session nickname from
the transcript.

1. Copy `codex/agents-codex.sh` to `~/.config/agents/agents-codex.sh` and
   make it executable.
2. Copy `codex/hooks.json` to `~/.codex/hooks.json`.
3. Point codex at it: add `hooks = "hooks.json"` to `~/.codex/config.toml`.
4. Trust the hooks: codex asks once, or start it with
   `--dangerously-bypass-hook-trust` to skip the prompt.

Codex has no turn-complete hook, so the roster cannot mark a codex agent
`waiting` from a hook; `working` and `needs_input` report, and `done`
comes from the pane dying.

## Claude Code (optional, richer turn state)

The roster reads Claude from its session file, so no shim is needed. Add
one only if you want the exact turn state before the file catches up:

1. Copy `agent-status.sh` to `~/.config/agents/agent-status.sh` and make
   it executable.
2. Merge `claude-code/settings.json` into `~/.claude/settings.json`.

## The wire

Every shim sends one line, keyed by the pane, never an id:

    tmux -S "${TMUX%%,*}" plugin-command -t "$TMUX_PANE" agents "<verb>"

The verbs are `identify <id> [session_file]` (from the codex hook and the
in-process extensions) and the status words below.

## The map from harness events to roster status

| roster       | Claude Code       | Codex             | pi              | opencode              |
|--------------|-------------------|-------------------|-----------------|-----------------------|
| working      | UserPromptSubmit  | UserPromptSubmit  | agent_start     | session.status busy   |
|              | PreToolUse        | PreToolUse        | turn_start      |                       |
| needs_input  | Stop; PreToolUse¹ | PermissionRequest | ui_prompt_start | permission/question   |
| waiting      | (by hand: `w`)    | Stop              | agent_settled   | session.status idle   |
| done         | (pane close)      | (pane close)      | session_shutdown| session.deleted       |

¹ Claude's `Stop` reports `needs_input`, not `waiting`: under
`--dangerously-skip-permissions` there are no permission prompts, so "it
stopped" is the signal that it wants you, and the picker's `w` is how a
row you have judged leaves the band. Mid-turn, the exact signal is a
tool: `PreToolUse` runs the shim as `pretool`, which reports
`needs_input` with the question for `AskUserQuestion` and for
`ExitPlanMode` ("plan ready for review"), and `working` for every other
tool. The `Notification` hook is not used at all: its `permission_prompt`
never fires under skip-permissions, and its `idle_prompt` comes a minute
after the Stop already said it.

`done` is best taken from pane liveness, not a hook: a killed or crashed
CLI never fires its exit hook, but the pane always dies.

## Why an agent wants you

A `needs_input` report may carry text after the status word, and the
roster shows it on the row - so the "needs input" band says what each
agent is blocked on without jumping to the pane. It is cleared by the
next report, because it is only true while the agent is blocked.

`agent-status.sh` fills it in on its own: the Claude `Notification` hook
puts its own words in the payload's `message` field ("Claude needs your
permission to use Bash"), and the shim passes them on. Only for
`needs_input` - `PreToolUse` runs on every tool call and has no message
to read. Pass your own text instead by putting it after the status:

    ~/.config/agents/agent-status.sh needs_input waiting on the deploy

The user can also move a row out of the band by hand (`w` in the
picker), so a report that turns out not to need them is one key to
clear, not something to wait out.
