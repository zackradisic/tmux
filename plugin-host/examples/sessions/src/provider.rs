//! The provider half: answers `tree` with this server's snapshot,
//! sets a session's or window's `@project` on request (`project`), and
//! publishes `changed` when the tree changes. Runs on every server
//! (`Both` locally, `Provider` where the plugin was pushed over a link).

use serde::Deserialize;
use tmux_plugin_sdk::prelude::*;

use crate::tree::{self, Formats};

pub const TOPIC: &str = "changed";

pub fn register_services() {
    for m in ["tree", "project", "palette"] {
        if let Err(e) = service::register(m) {
            log(&format!("sessions: register {m}: {}", e.message));
        }
    }
}

pub async fn handle(req: ServiceRequest) {
    match req.method.as_str() {
        "tree" => {
            let t = tree::snapshot(&Formats::load());
            let _ = req.reply_json(&t);
        }
        // A view on another server sets `@project` on one of OUR
        // sessions or windows: `set-option` on a shadow stays local to
        // the shadow, so the view asks the owning server to do it.
        "project" => {
            let Ok(q) = req.json::<ProjectReq>() else {
                let _ = req.fail("project: bad request");
                return;
            };
            match set_project(&q.kind, q.id, &q.value).await {
                Ok(()) => {
                    let _ = req.reply(b"ok");
                    broadcast();
                }
                Err(e) => {
                    let _ = req.fail(&format!("project: {e}"));
                }
            }
        }
        // The command palette asks what this plugin offers.
        "palette" => {
            let _ = req.reply_json(&serde_json::json!([
                { "title": "Sessions", "hint": "the session chooser, windows folded", "text": "pick s" },
                { "title": "Windows", "hint": "the chooser with every window shown", "text": "pick w" },
            ]));
        }
        _ => {
            let _ = req.fail("unknown method");
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ProjectReq {
    /// `session` or `window`.
    pub kind: String,
    /// The object's id on this server.
    pub id: u32,
    /// Empty unsets the option.
    #[serde(default)]
    pub value: String,
}

/// Set (or, with an empty value, unset) `@project` on a session or a
/// window of this server, through `set-option`: the plugin is granted
/// `run-command`, not `write-options`.
pub async fn set_project(kind: &str, id: u32, value: &str) -> Result<(), String> {
    let target = match kind {
        "session" => format!("-t '${id}'"),
        "window" => format!("-w -t '@{id}'"),
        other => return Err(format!("no {other} to tag")),
    };
    let value = value.trim();
    if value.contains('\'') {
        return Err("a quote I will not paste".into());
    }
    let cmd = if value.is_empty() {
        format!("set-option -u {target} @project")
    } else {
        format!("set-option {target} @project '{value}'")
    };
    run_command(&cmd).await.map_err(|e| e.message)
}

/// Publish the tree to every view that follows this server.
pub fn broadcast() {
    let t = tree::snapshot(&Formats::load());
    let _ = service::emit_json(TOPIC, &t);
}
