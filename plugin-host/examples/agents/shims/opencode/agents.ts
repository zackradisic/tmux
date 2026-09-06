// Install under ~/.config/opencode/plugin/agents.ts (or name it in the
// `plugin` array of opencode.json).
//
// opencode keeps a global database, not a per-session file, so the plugin
// cannot map a pane to a session from outside. This plugin does two jobs:
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
