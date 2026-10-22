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
| opencode| its global database (`~/.local/share/opencode/opencode.db`) | no |

## opencode (no shim: the roster reads its database)

opencode 2.x keeps every session, message and tool call in one SQLite
database and runs its plugins inside a background service shared by
every pane (`opencode serve --service`), so an in-process plugin cannot
say which pane it speaks for: the service's `TMUX_PANE` is whichever
pane happened to start it, and a shim there would stamp every session
on that one pane. **Do not install `opencode/agents.ts` under the
service** - it is kept for `opencode --standalone`, where the plugin
runs in the pane's own process.

Instead the roster reads the database through the `sqlite3` CLI (the
`run-process` capability; macOS ships `/usr/bin/sqlite3`, Linux needs
the package). A pane running opencode is matched to the session created
in its directory after the row appeared, or failing that to the session
in that directory last worked on since (one resumed with `-c`/`-s`);
the row moves to `opencode:<session id>`, its status follows the last
message (`idle` closes a turn), its title and times are the session's,
and the conversation - prompts, replies, tool calls condensed to one
line each - is read into the store, so the picker shows it live (Tab)
and after the pane is gone (history, `.`), and the search finds it. The
`i` card's resume line is `opencode -s <id>`.

The database is read once per row every 5 s while the picker is open
(a render asks, a cached answer serves the renders between), and once
more when the pane closes.

A mailbox message to an opencode row (`agents message opencode:<id>
...`) is pushed the same way Claude's is, but through the service: one
`curl` job posts it to `/api/session/<id>/prompt` as a steer, with the
address and password from `~/.local/state/opencode/service.json` (see
`CROSS-AGENT.md`, "Push"). The message's prefix names the pane as
`-t %N`, since `$TMUX_PANE` in the service is not the agent's pane.

## pi (required for a stable id + activity time)

pi keeps no file the plugin can map from outside, so a tiny in-process
extension reports the session id once (`identify`) and pushes turn
state: copy `pi/agents.ts` to `~/.pi/agent/extensions/agents.ts`.

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
pi extension) and the status words below.

## The map from harness events to roster status

| roster       | Claude Code       | Codex             | pi              | opencode (database)   |
|--------------|-------------------|-------------------|-----------------|-----------------------|
| working      | UserPromptSubmit  | UserPromptSubmit  | agent_start     | last message not idle |
|              | PreToolUse        | PreToolUse        | turn_start      |                       |
| needs_input  | Stop; PreToolUse¹ | PermissionRequest | ui_prompt_start | (not known)           |
| waiting      | (by hand: `w`)    | Stop              | agent_settled   | last message idle     |
| done         | (pane close)      | (pane close)      | session_shutdown| (pane close)          |

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
