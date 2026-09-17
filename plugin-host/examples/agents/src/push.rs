//! Push: a message left in a Claude agent's mailbox is delivered into that
//! Claude session at once, so the agent wakes with it instead of finding
//! it on its next `inbox`.
//!
//! The mailbox plugin emits its `mailbox` topic on every stored message.
//! The provider half of this plugin follows that topic on its own server:
//! when the box is a live Claude agent here, it reads the agent's session
//! file (the row's `source_path`) for `messagingSocketPath` and hands the
//! text to `claude_notify`, which writes one queued user turn to that
//! socket. Claude reads it between tool calls, or starts a turn with it
//! when idle. Nothing is typed into the pane.
//!
//! A box that is not a Claude agent here (a plain box, a codex or pi
//! agent, a Claude with no socket) is left alone: the message stays
//! unread for the pull path. A pushed message is marked read.

use tmux_plugin_sdk::prelude::*;

use crate::store;

/// The mailbox topic payload: one stored message and its box.
#[derive(serde::Deserialize)]
pub struct Delivered {
    #[serde(rename = "box")]
    pub box_: String,
    pub id: i64,
    pub sender: String,
    pub body: String,
}

/// The mailbox plugin's name and topic, as it registers them.
pub const MAILBOX: &str = "mailbox";
pub const TOPIC: &str = "mailbox";

/// Follow the local mailbox topic. Best-effort: without the mailbox
/// plugin, or without service-call, messages simply stay pull-only.
pub fn follow() {
    if let Err(e) = service::subscribe(MAILBOX, TOPIC) {
        log(&format!("agents: follow mailbox: {}", e.message));
    }
}

/// Deliver one stored message into the Claude session that owns its box,
/// if there is one here. The roster is enriched first so a box addressed
/// by durable id resolves to its session file (and its socket) even when
/// no picker has opened to materialize it; a box that is no live Claude
/// agent here is left for `inbox`.
pub async fn deliver(d: Delivered) {
    let mut rows = store::live_agents().await.unwrap_or_default();
    crate::provider::enrich_live(&mut rows).await;
    let Some(a) = rows.into_iter().find(|a| a.id == d.box_) else { return };
    if !a.live() || a.kind != "claude" {
        return;
    }
    let Some(path) = a.source_path.as_deref() else { return };
    let Ok((bytes, _)) = fs_read(path, 0, 16 * 1024).await else {
        return;
    };
    let sock = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| v.get("messagingSocketPath")?.as_str().map(str::to_string));
    let Some(sock) = sock else { return };
    let text = format!("Message from {} via the tmux2 mailbox:\n{}", d.sender, d.body);
    match claude_notify(&sock, &text) {
        Ok(()) => {
            let _ = service::call_json::<_, serde_json::Value>(
                MAILBOX,
                "mark_read",
                &serde_json::json!({ "id": d.id }),
            )
            .await;
        }
        Err(e) => log(&format!("agents: push to {}: {}", d.box_, e.message)),
    }
}
