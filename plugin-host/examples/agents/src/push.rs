//! Push: a message left in an agent's mailbox is delivered into that
//! agent's session at once, so the agent wakes with it instead of
//! finding it on its next `inbox`.
//!
//! The mailbox plugin emits its `mailbox` topic on every stored message.
//! The provider half of this plugin follows that topic on its own server
//! and, when the box is a live agent here, hands the text to its harness:
//!
//! - Claude: the agent's session file (the row's `source_path`) names
//!   `messagingSocketPath`; `claude_notify` writes one queued user turn
//!   to that socket. Claude reads it between tool calls, or starts a
//!   turn with it when idle.
//! - opencode: the row's session is posted to the service that runs it
//!   (`POST /api/session/<id>/prompt`, delivery `steer`) through a
//!   `curl` job. The service's address and password are in its
//!   registration file, `~/.local/state/opencode/service.json`; the
//!   job's shell reads the password from that file itself, so it is
//!   never on a command line. opencode takes a steer at the next step
//!   boundary of a running turn, or starts a turn with it when idle.
//!
//! Nothing is typed into the pane either way. A box that is neither (a
//! plain box, a codex or pi agent, a Claude with no socket, an opencode
//! row still provisional) is left alone: the message stays unread for
//! the pull path. A pushed message is marked read.

use tmux_plugin_sdk::prelude::*;

use crate::resolve;
use crate::store::{self, Agent};

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

/// Deliver one stored message into the session that owns its box, if
/// there is one here. The roster is enriched first so a box addressed
/// by durable id resolves to its session file or service even when no
/// picker has opened to materialize it; a box that is no live agent of
/// a harness we can speak to is left for `inbox`.
pub async fn deliver(d: Delivered) {
    let mut rows = store::live_agents().await.unwrap_or_default();
    crate::provider::enrich_live(&mut rows).await;
    let Some(a) = rows.into_iter().find(|a| a.id == d.box_) else { return };
    if !a.live() {
        return;
    }
    let text = prefix(&a.kind, a.pane, &d.sender, &d.body);
    let pushed = match a.kind.as_str() {
        "claude" => claude(&a, &text).await,
        "opencode" => opencode(&a, &d.sender, &text).await,
        _ => return,
    };
    match pushed {
        Ok(()) => {
            let _ = service::call_json::<_, serde_json::Value>(
                MAILBOX,
                "mark_read",
                &serde_json::json!({ "id": d.id }),
            )
            .await;
        }
        Err(e) => log(&format!("agents: push to {}: {e}", d.box_)),
    }
}

/// The line before the body: the sender (already the full address,
/// server suffix included when it crossed a link), the exact reply
/// command and the guide, so a reader that never heard of the mailbox
/// can still answer. Claude runs its shell in its own pane, where
/// `$TMUX_PANE` names it; opencode runs tools in a shared service whose
/// environment names whichever pane started it, so its row's pane is
/// written out.
fn prefix(kind: &str, pane: Option<i64>, sender: &str, body: &str) -> String {
    let pane = match (kind, pane) {
        ("opencode", Some(p)) => format!("%{p}"),
        _ => "\"$TMUX_PANE\"".to_string(),
    };
    format!(
        "Message from {s} via the tmux2 mailbox (reply: tmux2 plugin-command -t {pane} \
         agents \"message {s} <text>\"; guide: tmux2 skill -t {pane} show mailbox):\n{body}",
        s = sender
    )
}

/// Claude: one line on the inbox socket its session file names.
async fn claude(a: &Agent, text: &str) -> Result<(), String> {
    let path = a.source_path.as_deref().ok_or("no session file")?;
    let (bytes, _) = fs_read(path, 0, 16 * 1024).await.map_err(|e| e.message)?;
    let sock = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| v.get("messagingSocketPath")?.as_str().map(str::to_string))
        .ok_or("no inbox socket in the session file")?;
    claude_notify(&sock, text).map_err(|e| e.message)
}

/// opencode: one prompt posted to the session through its service. The
/// service answers with the enqueued inbox item (2xx) or an error; a
/// service that is not registered or not listening is reported, and the
/// message stays unread.
async fn opencode(a: &Agent, sender: &str, text: &str) -> Result<(), String> {
    let sid = a
        .id
        .strip_prefix("opencode:")
        .and_then(resolve::opencode_sid)
        .ok_or("the row has no session id yet")?;
    let body = serde_json::json!({
        "text": text,
        "delivery": "steer",
        "metadata": { "source": "tmux2-mailbox", "sender": sender },
    });
    // The service's address and password come from its registration;
    // the password never reaches a command line (see `opencode_api`).
    resolve::opencode_api("POST", &format!("/api/session/{sid}/prompt"), &body).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_names_the_pane_an_agent_can_use() {
        let c = prefix("claude", Some(7), "claude:abc@host", "hi");
        assert!(c.starts_with("Message from claude:abc@host via the tmux2 mailbox (reply: tmux2 plugin-command -t \"$TMUX_PANE\" agents \"message claude:abc@host <text>\"; guide: tmux2 skill -t \"$TMUX_PANE\" show mailbox):\nhi"), "{c}");
        // opencode's tools run in its service, where $TMUX_PANE is not
        // the agent's pane: the row's pane is written out.
        let o = prefix("opencode", Some(7), "claude:abc", "hi");
        assert!(o.contains("plugin-command -t %7 agents \"message claude:abc <text>\"; guide: tmux2 skill -t %7 show mailbox):\nhi"), "{o}");
        let p = prefix("opencode", None, "x", "hi");
        assert!(p.contains("-t \"$TMUX_PANE\" agents"), "{p}");
    }
}
