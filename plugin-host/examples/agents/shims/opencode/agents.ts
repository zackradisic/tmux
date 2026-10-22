// ONLY for `opencode --standalone`. opencode 2.x runs its plugins in a
// background service shared by every pane (`opencode serve --service`),
// whose TMUX_PANE is whichever pane started it: installed there, this
// would stamp every session on that one pane. The roster no longer needs
// it - it reads opencode's database directly (see shims/README.md and
// the plugin's `resolve::opencode`). Under --standalone the plugin runs in
// the pane's own process, and this file still works: install it under
// ~/.config/opencode/plugins/agents.ts.
//
// This plugin does two jobs:
//
//   1. identify - once per session, it tells the tmux `agents` plugin
//      opencode's durable session id, so the roster binds the pane to a
//      stable id (and links a resumed session to its history).
//   2. status - it reports opencode's turn state, which is also where the
//      roster gets its activity time for opencode (there is no per-session
//      file to date).
//
// The pane travels in the command scope. TMUX / TMUX_PANE come from
// opencode's environment.
export const AgentsPlugin = async ({ $ }: any) => {
  let identified = false;

  const send = async (args: string[]) => {
    const tmux = process.env.TMUX;
    const pane = process.env.TMUX_PANE;
    if (!tmux || !pane) return;
    const sock = tmux.split(",")[0];
    try {
      await $`tmux -S ${sock} plugin-command -t ${pane} agents ${args}`.quiet();
    } catch {}
  };

  const report = (status: string) => send([status]);

  const identify = async (id?: string) => {
    if (identified || !id) return;
    identified = true;
    await send(["identify", String(id)]);
  };

  return {
    event: async ({ event }: any) => {
      const sid = event.properties?.sessionID ?? event.properties?.session?.id;
      switch (event.type) {
        case "session.status":
          await identify(sid);
          await report(
            event.properties?.status?.type === "busy" ? "working" : "waiting",
          );
          break;
        case "permission.asked":
        case "question.asked":
          await report("needs_input");
          break;
        case "session.idle":
          await report("waiting");
          break;
        case "session.deleted":
          await report("done");
          break;
      }
    },
  };
};
