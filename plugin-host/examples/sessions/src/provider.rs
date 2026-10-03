//! The provider half: answers `tree` with this server's snapshot and
//! publishes `changed` when the tree changes. Runs on every server
//! (`Both` locally, `Provider` where the plugin was pushed over a link).

use tmux_plugin_sdk::prelude::*;

use crate::tree::{self, Formats};

pub const TOPIC: &str = "changed";

pub fn register_services() {
    if let Err(e) = service::register("tree") {
        log(&format!("sessions: register tree: {}", e.message));
    }
}

pub async fn handle(req: ServiceRequest) {
    match req.method.as_str() {
        "tree" => {
            let t = tree::snapshot(&Formats::load());
            let _ = req.reply_json(&t);
        }
        _ => {
            let _ = req.fail("unknown method");
        }
    }
}

/// Publish the tree to every view that follows this server.
pub fn broadcast() {
    let t = tree::snapshot(&Formats::load());
    let _ = service::emit_json(TOPIC, &t);
}
