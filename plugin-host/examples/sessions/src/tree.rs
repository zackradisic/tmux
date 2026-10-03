//! One server's sessions, windows and panes, as the provider reports
//! them: the ABI records plus the formats the rows are made of, expanded
//! on the server that owns the objects (a shadow session's path and
//! command are the remote's, and a format expands where the object is).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tmux_plugin_sdk::prelude::*;

pub const LOCAL: &str = "local";

/// The row formats, read from the server's options so a user can shape
/// the rows the way `choose-tree -F` lets them. `#()` is disabled in a
/// plugin's format expansion, so a format can never run a shell.
pub const OPT_SESSION: &str = "@sessions-format-session";
pub const OPT_WINDOW: &str = "@sessions-format-window";
pub const OPT_PANE: &str = "@sessions-format-pane";
/// A node whose expansion is empty or `0` is left out, as `choose-tree
/// -f` does.
pub const OPT_FILTER: &str = "@sessions-filter";

pub const DEFAULT_SESSION: &str = "#{session_name}#{?session_attached,*,} (#{session_windows})";
pub const DEFAULT_WINDOW: &str =
    "#{window_index}: #{window_name}#{?window_active,*,}#{?window_zoomed_flag,Z,} #{pane_current_command}";
/// The home directory in a pane's row is shortened to `~` after the
/// expansion (a tmux format cannot see `$HOME`).
pub const DEFAULT_PANE: &str = "#{pane_index}: #{pane_current_command} #{pane_current_path}";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Tree {
    pub server: String,
    /// The provider's clock when the snapshot was taken.
    pub now_ms: i64,
    /// `#{host_short}` there.
    pub host: String,
    pub sessions: Vec<Sess>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Sess {
    pub id: u32,
    pub name: String,
    pub attached: bool,
    pub current_window: Option<u32>,
    /// Set on a shadow session: the host it mirrors, and the id of the
    /// session there (`$3`).
    pub remote_host: String,
    pub remote_id: String,
    /// `connecting` / `connected` / `disconnected`, and why.
    pub remote_state: String,
    pub remote_error: String,
    /// Epoch seconds.
    pub last_attached: i64,
    /// The row format, expanded.
    pub text: String,
    pub hidden: bool,
    pub windows: Vec<Win>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Win {
    pub id: u32,
    pub index: u32,
    pub name: String,
    pub active: bool,
    pub zoomed: bool,
    pub activity: bool,
    pub layout: String,
    pub width: u32,
    pub height: u32,
    pub active_pane: Option<u32>,
    pub text: String,
    pub hidden: bool,
    pub panes: Vec<Pn>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Pn {
    pub id: u32,
    pub index: u32,
    pub active: bool,
    pub dead: bool,
    pub floating: bool,
    /// For a shadow pane: the id of the pane on the remote (`%12`).
    pub remote_id: String,
    pub cmd: String,
    pub path: String,
    pub title: String,
    pub text: String,
    pub hidden: bool,
}

pub struct Formats {
    pub session: String,
    pub window: String,
    pub pane: String,
    pub filter: String,
}

/// A `@sessions-*` option, wherever the user set it: `set -g` lands in
/// the global session options, which a session's scope walks up to, and
/// `set -s` in the server options, which the plugin's own writes use.
/// Empty or unset reads as `None`.
pub fn read_opt(name: &str) -> Option<String> {
    let session = list_sessions().ok().and_then(|v| v.first().map(|s| s.id));
    let from_session = session.and_then(|id| get_option_in(OptionTarget::Session(SessionId(id)), name).ok());
    from_session
        .or_else(|| get_option(name).ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

impl Formats {
    pub fn load() -> Self {
        let get = |name: &str, def: &str| read_opt(name).unwrap_or_else(|| def.to_string());
        Self {
            session: get(OPT_SESSION, DEFAULT_SESSION),
            window: get(OPT_WINDOW, DEFAULT_WINDOW),
            pane: get(OPT_PANE, DEFAULT_PANE),
            filter: read_opt(OPT_FILTER).unwrap_or_default(),
        }
    }
}

/// Split one expansion into its fields; a format that failed gives
/// empty fields rather than no row.
fn fields(target: OptionTarget, fmt: &str, n: usize) -> Vec<String> {
    let s = format_expand(target, fmt).unwrap_or_default();
    let mut v: Vec<String> = s.split('\u{1}').map(str::to_string).collect();
    v.resize(n, String::new());
    v
}

fn hidden_by(filter: &str, v: &str) -> bool {
    !filter.is_empty() && (v.is_empty() || v == "0")
}

/// This server's tree, now.
pub fn snapshot(f: &Formats) -> Tree {
    let windows: HashMap<u32, WindowInfo> = list_windows().unwrap_or_default().into_iter().map(|w| (w.id, w)).collect();
    let panes: HashMap<u32, PaneInfo> = list_panes().unwrap_or_default().into_iter().map(|p| (p.id, p)).collect();
    let host = format_expand(OptionTarget::Server, "#{host_short}").unwrap_or_default();
    let home = home_dir().ok().filter(|h| h.len() > 1);
    let tilde = |s: String| match &home {
        Some(h) => s.replace(h.as_str(), "~"),
        None => s,
    };
    let sfmt = format!(
        "#{{session_remote_host}}\u{1}#{{session_remote_id}}\u{1}#{{remote_state}}\u{1}#{{remote_error}}\u{1}#{{session_last_attached}}\u{1}{}\u{1}{}",
        f.session, f.filter
    );
    let wfmt = format!(
        "#{{window_zoomed_flag}}\u{1}#{{window_activity_flag}}\u{1}#{{window_layout}}\u{1}#{{window_active}}\u{1}{}\u{1}{}",
        f.window, f.filter
    );
    let pfmt = format!(
        "#{{pane_remote_id}}\u{1}#{{pane_current_command}}\u{1}#{{pane_current_path}}\u{1}#{{pane_index}}\u{1}{}\u{1}{}\u{1}#{{pane_mode}}",
        f.pane, f.filter
    );
    let mut sessions = Vec::new();
    for s in list_sessions().unwrap_or_default() {
        let sf = fields(OptionTarget::Session(SessionId(s.id)), &sfmt, 7);
        let mut sess = Sess {
            id: s.id,
            name: s.name.clone(),
            attached: s.attached,
            current_window: s.current_window,
            remote_host: sf[0].clone(),
            remote_id: sf[1].clone(),
            remote_state: sf[2].clone(),
            remote_error: sf[3].clone(),
            last_attached: sf[4].parse().unwrap_or(0),
            text: sf[5].clone(),
            hidden: hidden_by(&f.filter, &sf[6]),
            windows: Vec::new(),
        };
        for (index, wid) in &s.windows {
            let Some(w) = windows.get(wid) else { continue };
            let wf = fields(OptionTarget::Window(WindowId(*wid)), &wfmt, 6);
            let mut win = Win {
                id: *wid,
                index: *index,
                name: w.name.clone(),
                active: s.current_window == Some(*wid),
                zoomed: wf[0] == "1",
                activity: wf[1] == "1",
                layout: wf[2].clone(),
                width: w.width,
                height: w.height,
                active_pane: w.active_pane,
                text: wf[4].clone(),
                hidden: hidden_by(&f.filter, &wf[5]),
                panes: Vec::new(),
            };
            for pid in &w.panes {
                let Some(p) = panes.get(pid) else { continue };
                let pf = fields(OptionTarget::Pane(PaneId(*pid)), &pfmt, 7);
                // A plugin's float (this picker's own, a form) is not a
                // pane anyone chooses; and it is the window's active
                // pane while open, which the preview must not follow.
                if pf[6] == "plugin-mode" {
                    continue;
                }
                win.panes.push(Pn {
                    id: *pid,
                    index: pf[3].parse().unwrap_or(0),
                    active: w.active_pane == Some(*pid),
                    dead: p.dead,
                    floating: p.floating,
                    remote_id: pf[0].clone(),
                    cmd: pf[1].clone(),
                    path: pf[2].clone(),
                    title: p.title.clone(),
                    text: tilde(pf[4].clone()),
                    hidden: hidden_by(&f.filter, &pf[5]),
                });
            }
            // The active pane, if it was a float that is left out, falls
            // back to the first pane kept.
            if !win.panes.iter().any(|pn| Some(pn.id) == win.active_pane) {
                win.active_pane = win.panes.first().map(|pn| pn.id);
                for pn in &mut win.panes {
                    pn.active = Some(pn.id) == win.active_pane;
                }
            }
            sess.windows.push(win);
        }
        sessions.push(sess);
    }
    Tree { server: LOCAL.into(), now_ms: now_ms() as i64, host, sessions }
}

/// `$3` -> 3, `%12` -> 12, `@7` -> 7.
pub fn id_of(s: &str) -> Option<u32> {
    s.trim().trim_start_matches(['$', '%', '@']).parse().ok()
}
