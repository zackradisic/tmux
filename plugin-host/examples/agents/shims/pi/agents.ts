// Install as ~/.pi/agent/extensions/agents.ts
//
// pi has no session file the plugin can map from outside, so this small
// extension does two jobs:
//
//   1. identify - once per session, it tells the tmux `agents` plugin
//      pi's durable session id and its transcript file, so the roster can
//      bind the pane to a stable id and date it from the file's mtime.
//   2. status - it reports pi's turn state (optional, but it gives the
//      roster richer states than pane liveness alone).
//
// The pane travels in the command scope (-t $TMUX_PANE), so no id is
// needed on the wire. TMUX / TMUX_PANE come from pi's environment.
import { execFile } from "node:child_process";

function send(args: string[]) {
  const tmux = process.env.TMUX;
  const pane = process.env.TMUX_PANE;
  if (!tmux || !pane) return;
  const sock = tmux.split(",")[0];
  execFile(
    "tmux",
    ["-S", sock, "plugin-command", "-t", pane, "agents", ...args],
    () => {},
  );
}

function report(status: string) {
  send([status]);
}

export default function (pi: any) {
  // Field names are best-effort; adjust to pi's session API if they move.
  let identified = false;
  const identify = () => {
    if (identified) return;
    const id = pi.session?.id ?? pi.sessionId;
    const file = pi.session?.file ?? pi.session?.path;
    if (!id) return;
    identified = true;
    send(["identify", String(id), ...(file ? [String(file)] : [])]);
  };

  pi.on("agent_start", () => {
    identify();
    report("working");
  });
  pi.on("turn_start", () => report("working"));
  pi.on("ui_prompt_start", () => report("needs_input"));
  pi.on("agent_settled", () => report("waiting"));
  pi.on("session_shutdown", () => report("done"));
}
