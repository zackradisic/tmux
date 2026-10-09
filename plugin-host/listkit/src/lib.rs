//! listkit: the list-with-a-preview machinery a plugin picker is made of,
//! lifted out of the agents picker so other plugins build on it rather
//! than copy it (the cron picker and formkit's completion list each
//! carried a third copy before this crate existed).
//!
//! The layers, each usable without the others:
//!
//! - [`text`]: clipping, flattening, key labels, ages, the text that is
//!   safe inside a tmux command string.
//! - [`styled`]: a line as styled cells, light Markdown rendered into
//!   them, word wrap, match highlighting, and the SGR emission.
//! - [`query`]: the search box parsed into free words and sigil tokens
//!   (`@server`, `#session`, `~dir`), the prefix/substring rank, and the
//!   pure halves of the token dropdown.
//! - [`lines`]: the display lines of a list (headers, spacers, items) as
//!   the scroll coordinate space, and the scroll rule that keeps the
//!   selection on screen while pulling headers in.
//! - [`remotes`]: what the providers on linked servers last reported,
//!   the bounded concurrent fetch, and the spinner that turns only while
//!   a fetch is slow.
//! - [`keys`]: a rebindable key table with help text, so `?` and the
//!   action menu are generated rather than written twice.
//! - [`node`] and [`engine`]: the list itself. A consumer hands the
//!   engine a tree of nodes (what to draw, what to search, what to
//!   preview) on every refresh; the engine owns the cursor, the marks,
//!   the search box, expand and collapse, the preview column and the
//!   keys that drive them, and hands back an [`engine::Outcome`] for
//!   everything else. It never calls the host: `render` returns the
//!   bytes and the preview rect for the consumer to send.
//!
//! This is a compile-time library: a plugin that uses it runs the calls
//! itself, with its own capabilities.

pub mod engine;
pub mod keys;
pub mod lines;
pub mod node;
pub mod query;
pub mod remotes;
mod render;
pub mod styled;
pub mod text;

pub use engine::{Engine, Outcome};
pub use node::{Node, NodeKind, Preview, SigilSpec};
pub use render::cells;

use tmux_plugin_sdk::prelude::*;

/// The window a server-scoped picker floats over: the pressing client's
/// current one; else the window of the pane the command targeted (a
/// script run from a copy-mode binding has no attached client, only the
/// pane); else the first attached client's, so a request that arrives
/// over a link lands where the user looks; the first window only as a
/// last resort.
pub fn window_for(client: Option<u64>, pane: Option<u32>) -> Option<u32> {
    let client_window = |cid: u64| {
        list_clients()
            .ok()?
            .into_iter()
            .find(|c| u64::from(c.id) == cid)?
            .session
            .and_then(|s| resolve_session(SessionId(s)).ok())
            .and_then(|v| v.current_window)
    };
    client
        .and_then(client_window)
        .or_else(|| pane.and_then(|p| resolve_pane(PaneId(p)).ok().map(|pi| pi.window)))
        .or_else(|| any_client().and_then(client_window))
        .or_else(|| list_windows().ok().and_then(|w| w.first().map(|x| x.id)))
}

/// The client to open a picker for when no key press names one: the
/// first attached client, which on a workstation is the user's.
pub fn any_client() -> Option<u64> {
    list_clients().ok()?.into_iter().next().map(|c| u64::from(c.id))
}
