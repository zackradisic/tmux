//! sessions: the session / window / pane chooser, in the shape of the
//! agents picker - list on the left, live preview on the right, search
//! as you type - and aware of remote links.
//!
//!   bind s plugin-command sessions 'pick s'    # sessions, folded
//!   bind w plugin-command sessions 'pick w'    # windows shown
//!
//! Rows are the `@sessions-format-session` / `-window` / `-pane` formats
//! (choose-tree's `-F`), `@sessions-filter` drops rows (its `-f`), and
//! `@sessions-key-<K>` runs a tmux command for an unbound key, expanded
//! against the highlighted row's object (as a `choose-tree` binding
//! does). The row formats expand on the server that owns the object: a
//! shadow session shows what the remote says, and a session the remote
//! has that is not linked here shows dimmed, with Enter linking it.
//!
//! Role split: the provider half answers `tree` and publishes `changed`
//! on every server; the view half (the local server) merges them, and
//! asks the agents plugin on each server which panes hold an agent, to
//! badge the rows.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use listkit::keys::KeyTable;
use tmux_plugin_sdk::prelude::*;

mod layout;
mod provider;
mod tree;
mod view;

use view::{Memo, Picker, Remotes, NAME};

/// `pick_<action> = "<key>"` rebinds a key of the picker.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct Config {
    #[serde(flatten)]
    keys: HashMap<String, serde_json::Value>,
}

struct Sessions {
    role: Role,
    keys: KeyTable,
    picker: Rc<RefCell<Option<Picker>>>,
    remotes: Rc<RefCell<Remotes>>,
    busy: Rc<Cell<bool>>,
    memo: Rc<RefCell<Memo>>,
    /// A `changed` broadcast is scheduled (events are debounced).
    pending: Rc<Cell<bool>>,
}

/// The events that change the tree.
const TREE_EVENTS: [&str; 15] = [
    "session-created",
    "session-closed",
    "session-renamed",
    "session-window-changed",
    "window-linked",
    "window-unlinked",
    "window-renamed",
    "window-layout-changed",
    "window-pane-changed",
    "pane-exited",
    "pane-died",
    "pane-title-changed",
    "pane-command-changed",
    "pane-destroyed",
    "client-session-changed",
];

impl Plugin for Sessions {
    const NAME: &'static str = NAME;
    type Config = Config;

    fn init(ctx: &Ctx, config: Self::Config) -> Result<Self, String> {
        let role = ctx.role();
        let mut events = vec!["plugin-command"];
        events.extend(TREE_EVENTS);
        if role.views() {
            events.extend(["mode-key", "mode-nav", "mode-resize", "mode-closed", "link-up", "link-down", "peer-changed"]);
        }
        ctx.subscribe(&events).map_err(|e| e.message.clone())?;
        let mut keys = view::keys();
        for (k, v) in &config.keys {
            if let Some(action) = k.strip_prefix("pick_") {
                let v = v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
                keys.bind(action, Some(&v));
            }
        }
        if role.provides() {
            provider::register_services();
        }
        if role.views() {
            for s in service::servers().unwrap_or_default() {
                if !s.local && s.up && s.linked {
                    view::follow(&s.name);
                }
            }
        }
        Ok(Self {
            role,
            keys,
            picker: Rc::new(RefCell::new(None)),
            remotes: Rc::new(RefCell::new(Remotes::default())),
            busy: Rc::new(Cell::new(false)),
            memo: Rc::new(RefCell::new(HashMap::new())),
            pending: Rc::new(Cell::new(false)),
        })
    }

    fn on_event(&mut self, ctx: &Ctx, event: Event) {
        let name = event.name();
        match name.as_str() {
            "plugin-command" => self.on_command(ctx, &event),
            "mode-key" => view::on_mode_key(&self.picker, &self.remotes, &self.memo, &self.busy, &event),
            "mode-nav" => view::on_mode_nav(&self.picker, &self.remotes, &self.memo, &self.busy, &event),
            "mode-paste" => view::on_mode_paste(&self.picker, &self.remotes, &self.memo, &self.busy, &event),
            "mode-resize" => view::on_mode_resize(&self.picker, &event),
            "mode-closed" => view::on_mode_closed(&self.picker, &self.memo, &event),
            "link-up" => {
                let Some(server) = event.get_str("server").map(str::to_string) else { return };
                let linked = service::servers().unwrap_or_default().into_iter().any(|s| s.name == server && s.linked);
                if !linked {
                    return;
                }
                view::follow(&server);
                self.remotes.borrow_mut().mark_up(&server);
                let picker = Rc::clone(&self.picker);
                let remotes = Rc::clone(&self.remotes);
                ctx.spawn(async move {
                    refresh_tree(&picker, &remotes).await;
                    view::fetch_remotes(picker, remotes, 0).await;
                });
            }
            "link-down" => {
                let Some(server) = event.get_str("server").map(str::to_string) else { return };
                self.remotes.borrow_mut().mark_down(&server);
                view::refresh_if_open(&self.picker, &self.remotes);
            }
            "peer-changed" => view::refresh_if_open(&self.picker, &self.remotes),
            _ if TREE_EVENTS.contains(&name.as_str()) => self.schedule_changed(ctx),
            _ => {}
        }
    }

    fn on_service_request(&mut self, ctx: &Ctx, req: ServiceRequest) {
        if !self.role.provides() {
            let _ = req.fail("this instance is a view");
            return;
        }
        ctx.spawn(provider::handle(req));
    }

    fn on_service_event(&mut self, _ctx: &Ctx, event: ServiceEvent) {
        if event.topic != provider::TOPIC || event.server == tree::LOCAL {
            return;
        }
        let Ok(mut t) = event.json::<tree::Tree>() else { return };
        t.server = event.server.clone();
        let now = t.now_ms;
        self.remotes.borrow_mut().apply(&event.server, vec![t], now);
        view::refresh_if_open(&self.picker, &self.remotes);
    }
}

/// Re-read the local tree into an open picker.
async fn refresh_tree(picker: &Rc<RefCell<Option<Picker>>>, remotes: &Rc<RefCell<Remotes>>) {
    view::refresh_local(picker, remotes).await;
}

impl Sessions {
    /// The tree changed: publish it to the views that follow this
    /// server and redraw an open picker, once the burst settles.
    fn schedule_changed(&self, ctx: &Ctx) {
        if self.pending.replace(true) {
            return;
        }
        let pending = Rc::clone(&self.pending);
        let picker = Rc::clone(&self.picker);
        let remotes = Rc::clone(&self.remotes);
        let provides = self.role.provides();
        let views = self.role.views();
        ctx.spawn(async move {
            let _ = sleep_ms(50).await;
            pending.set(false);
            if provides {
                provider::broadcast();
            }
            if views {
                refresh_tree(&picker, &remotes).await;
            }
        });
    }

    fn on_command(&self, ctx: &Ctx, event: &Event) {
        let text = event.get_str("text").unwrap_or("").trim().to_string();
        let mut words = text.split_whitespace();
        let verb = words.next().unwrap_or("");
        match verb {
            "pick" => {
                if !self.role.views() {
                    let _ = display_message("sessions: a provider has no picker");
                    return;
                }
                // `pick s` folded, `pick w` with windows, `pick pane`
                // folded but opened on the targeted pane's row (what the
                // agents picker's flip runs, with -t the agent's pane).
                let (entry, seek_pane) = match words.next() {
                    Some("w") | Some("windows") => ('w', false),
                    Some("pane") => ('s', true),
                    _ => ('s', false),
                };
                view::close(&self.picker);
                let client = event.scope.client.map(u64::from);
                let here = event.scope.pane;
                ctx.spawn(view::pick_open(
                    Rc::clone(&self.picker),
                    Rc::clone(&self.remotes),
                    Rc::clone(&self.memo),
                    self.keys.clone(),
                    client,
                    here,
                    entry,
                    seek_pane,
                ));
            }
            "menu-key" => {
                let Some(key) = words.next() else { return };
                let mouse = match (words.next().and_then(|v| v.parse::<u32>().ok()), words.next().and_then(|v| v.parse::<u32>().ok())) {
                    (Some(x), Some(y)) => Some((x, y)),
                    _ => None,
                };
                view::on_menu_key(&self.picker, &self.remotes, &self.memo, &self.busy, key, mouse);
            }
            "close" => view::close(&self.picker),
            _ => {
                let _ = display_message("sessions: pick [s|w|pane] | close");
            }
        }
    }
}

tmux_plugin!(Sessions);
