//! Several Claude Code logins per machine, switched from a panel.
//!
//! Claude Code holds ONE login per machine (per user): the OAuth
//! credential in the macOS Keychain or `~/.claude/.credentials.json`, and
//! the identity in `~/.claude.json`. This plugin keeps more of them, as
//! numbered slots in its store, shows each slot's rate-limit windows
//! (5-hour, 7-day, per-model) from the usage API, and makes any slot the
//! login. The provider half runs on every server (a `remote-attach` link
//! pushes it there), so the panel on the workstation lists each linked
//! server's slots and switches a login over there without anything
//! installed on it beyond tmux2 (and curl).
//!
//! Commands (bind or run through `plugin-command accounts ...`):
//!
//!   pick [server] [slot]   the panel: every server, or just one, with the
//!                          cursor on a slot
//!   list                   one status line: slot, email, the binding window
//!   status                 the login here
//!   add [slot] [alias]     store the login here as a slot (or refresh its slot)
//!   login                  a claude in a popup under an empty profile, to log
//!                          in to another account; the login is stored as a
//!                          slot when it lands (the machine's is untouched)
//!   switch <slot|email|alias>
//!   best                   switch to the slot with the most room
//!   refresh                fetch every slot's usage now
//!   remove <slot>          forget a slot (the login is untouched)
//!   disable|enable <slot>  hold a slot out of best pick, or put it back
//!   alias <slot> <name>
//!   profile <slot>         make the slot's CLAUDE_CONFIG_DIR profile and
//!                          put its path in the @accounts_profile option
//!   import                 bring cswap's accounts over (macOS)
//!
//! A switch changes the machine's login: a claude already running there
//! moves to the new account at its next token refresh (it re-reads the
//! credential then), which is also why every write holds claude's own
//! lock directories. `n` in the panel (or `profile`) is the other way:
//! a per-window login through `CLAUDE_CONFIG_DIR`, which the agents
//! plugin's new-agent form launches.
//!
//! Services (the panel calls these on `@server`): `list`, `refresh`,
//! `switch`, `best`, `add`, `remove`, `disable`, `alias`, `profile`,
//! `import`; the `changed` topic follows every mutation. Nothing is
//! remote-callable the other way: a server that linked in cannot switch
//! the login here.
//!
//! Manifest:
//!
//!   [plugins.accounts]
//!   path = "accounts.wasm"
//!   scope = "server"
//!   caps = ["db", "run-process", "run-command", "mode", "fs-read",
//!           "fs-read-any", "fs-write", "fs-list", "service-serve",
//!           "service-call", "display-message"]
//!
//! On a remote the pushed copy needs `run-process`, `fs-write` and
//! `fs-read-any` in that server's `plugin-remote-caps` (they are not in
//! the default grant), or its own manifest entry.

use std::cell::RefCell;
use std::rc::Rc;

use tmux_plugin_sdk::prelude::*;

mod claude;
mod oauth;
mod provider;
mod store;
mod timefmt;
mod view;

use view::Shared;

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct AccountsConfig {
    /// Test knobs; see `claude::Overrides`.
    home: Option<String>,
    credentials: Option<String>,
    keychain_service: Option<String>,
    /// What a login window runs (default `claude`); a test stands
    /// something harmless in.
    claude: Option<String>,
}

thread_local! {
    static LOGIN_COMMAND: RefCell<String> = RefCell::new("claude".into());
}

pub(crate) fn login_command() -> String {
    LOGIN_COMMAND.with(|c| c.borrow().clone())
}

struct Accounts {
    role: Role,
    panel: Shared,
}

impl Plugin for Accounts {
    const NAME: &'static str = "accounts";
    type Config = AccountsConfig;

    fn init(ctx: &Ctx, config: Self::Config) -> Result<Self, String> {
        let role = ctx.role();
        if let Some(c) = config.claude.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            LOGIN_COMMAND.with(|l| *l.borrow_mut() = c.to_string());
        }
        claude::set_overrides(claude::Overrides {
            home: config.home.clone().filter(|h| !h.is_empty()),
            file_only: config.credentials.as_deref() == Some("file"),
            keychain_service: config.keychain_service.clone().filter(|s| !s.is_empty()),
        });
        if role.provides() {
            db_exec_sync(store::SCHEMA, &[]).map_err(|e| e.message)?;
            for m in provider::METHODS {
                service::register(m).map_err(|e| e.message.clone())?;
            }
            // The store holds credentials and the host writes files
            // 0644: the data directory itself is the guard.
            if let Ok(root) = fs_root() {
                ctx.spawn(async move {
                    let _ = run_job(&format!("chmod 700 {}", claude::sq(&root)), None).await;
                });
            }
        }
        ctx.subscribe(&["plugin-command", "mode-key", "mode-nav", "mode-resize", "mode-closed", "link-up", "link-down"])
            .map_err(|e| e.message.clone())?;
        Ok(Accounts { role, panel: Rc::new(RefCell::new(None)) })
    }

    fn on_event(&mut self, ctx: &Ctx, event: Event) {
        match event.name().as_str() {
            "plugin-command" => self.on_command(ctx, &event),
            "mode-key" => view::on_key(&self.panel, ctx, &event),
            "mode-nav" => view::on_nav(&self.panel, &event),
            "mode-resize" => {
                let mut b = self.panel.borrow_mut();
                let Some(p) = b.as_mut() else { return };
                if event.get_i64("mode") != Some(p.mode.0 as i64) {
                    return;
                }
                if let Some(w) = event.get_i64("width") {
                    p.width = w as u32;
                }
                if let Some(h) = event.get_i64("height") {
                    p.height = h as u32;
                }
                view::render(p);
            }
            "mode-closed" => {
                let mut b = self.panel.borrow_mut();
                if b.as_ref().is_some_and(|p| event.get_i64("mode") == Some(p.mode.0 as i64)) {
                    if let Some(t) = b.as_ref().and_then(|p| p.timer) {
                        cancel(t);
                    }
                    *b = None;
                }
            }
            "link-up" | "link-down" => {
                // A block for that server may need (re)fetching.
                if let Some(server) = event.get_str("server").map(str::to_string) {
                    if view::has_block(&self.panel, &server) {
                        let panel = Rc::clone(&self.panel);
                        ctx.spawn(async move {
                            view::fetch_all(panel, false, false).await;
                        });
                    }
                }
            }
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

    fn on_service_event(&mut self, ctx: &Ctx, event: ServiceEvent) {
        if event.topic != provider::TOPIC {
            return;
        }
        if view::has_block(&self.panel, &event.server) {
            let panel = Rc::clone(&self.panel);
            ctx.spawn(async move {
                view::fetch_all(panel, false, false).await;
            });
        }
    }
}

impl Accounts {
    fn on_command(&self, ctx: &Ctx, event: &Event) {
        let text = event.get_str("text").unwrap_or("").trim().to_string();
        let mut words = text.split_whitespace();
        let verb = words.next().unwrap_or("").to_string();
        let args: Vec<String> = words.map(str::to_string).collect();
        let client = event.scope.client.map(u64::from);
        let here = event.scope.pane;
        match verb.as_str() {
            "login" => {
                if !self.role.views() {
                    let _ = display_message("accounts: a provider has no screen to log in on");
                    return;
                }
                let server = args.first().cloned().unwrap_or_else(|| view::LOCAL.to_string());
                let client = client.or_else(view::any_client);
                ctx.spawn(view::start_login(server, client));
            }
            "pick" => {
                if !self.role.views() {
                    let _ = display_message("accounts: a provider has no panel");
                    return;
                }
                view::close(&self.panel);
                let panel = Rc::clone(&self.panel);
                let only = args.first().cloned().filter(|s| s != "all");
                let slot = args.get(1).and_then(|s| s.parse::<i64>().ok());
                ctx.spawn(view::open(panel, client, here, only, slot));
            }
            "" | "help" => {
                let _ = display_message(
                    "accounts: pick | login | list | status | add | switch <slot> | best | refresh | remove | disable | enable | alias | profile | import",
                );
            }
            _ => {
                if !self.role.provides() {
                    let _ = display_message("accounts: this instance is a view");
                    return;
                }
                ctx.spawn(command(verb, args, client));
            }
        }
    }
}

fn say(client: Option<u64>, msg: &str) {
    let text = format!("accounts: {msg}");
    let r = match client {
        Some(c) => display_message_to(ClientId(c as u32), &text),
        None => display_message(&text),
    };
    if r.is_err() {
        let _ = display_message(&text);
    }
}

async fn slot_of(ident: Option<&String>) -> Result<i64, String> {
    let Some(id) = ident else { return Err("which slot?".into()) };
    match store::resolve(id).await {
        Ok(Some(a)) => Ok(a.slot),
        Ok(None) => Err(format!("no slot {id}")),
        Err(e) => Err(e.message.clone()),
    }
}

async fn command(verb: String, args: Vec<String>, client: Option<u64>) {
    let reply = match verb.as_str() {
        "list" => {
            let s = provider::snapshot(&provider::ListReq { refresh: true, force: false }).await;
            let parts: Vec<String> = s
                .accounts
                .iter()
                .map(|a| {
                    let worst = a
                        .usage
                        .as_ref()
                        .and_then(|u| u.windows().into_iter().map(|(n, w)| (w.pct, n)).fold(None, |m: Option<(f64, String)>, x| match m {
                            Some(m) if m.0 >= x.0 => Some(m),
                            _ => Some(x),
                        }))
                        .map(|(p, n)| format!(" {n} {p:.0}%"))
                        .unwrap_or_default();
                    format!("{}{} {}{worst}", if a.active { "*" } else { "" }, a.slot, a.email)
                })
                .collect();
            if parts.is_empty() { "no slots".to_string() } else { parts.join(" · ") }
        }
        "status" => {
            let s = provider::snapshot(&provider::ListReq::default()).await;
            match (s.live, s.live_slot) {
                (Some(id), Some(slot)) => format!("slot {slot}: {} [{}]", id.email, id.org_name),
                (Some(id), None) => format!("{} [{}] (not a slot; add)", id.email, id.org_name),
                (None, _) => "no login here".into(),
            }
        }
        "add" => {
            let slot = args.first().and_then(|s| s.parse::<i64>().ok());
            let alias = if slot.is_some() { args.get(1) } else { args.first() }.cloned().unwrap_or_default();
            provider::add(&provider::AddReq { slot, alias }).await.message
        }
        "switch" => match slot_of(args.first()).await {
            Ok(slot) => provider::switch(slot).await.message,
            Err(e) => e,
        },
        "best" => provider::best().await.message,
        "refresh" => {
            let s = provider::snapshot(&provider::ListReq { refresh: true, force: true }).await;
            format!("refreshed {} slots", s.accounts.len())
        }
        "remove" => match slot_of(args.first()).await {
            Ok(slot) => provider::remove(slot).await.message,
            Err(e) => e,
        },
        "disable" | "enable" => match slot_of(args.first()).await {
            Ok(slot) => provider::set_disabled(slot, verb == "disable").await.message,
            Err(e) => e,
        },
        "alias" => match (slot_of(args.first()).await, args.get(1)) {
            (Ok(slot), Some(name)) => provider::set_alias(slot, name).await.message,
            (Err(e), _) => e,
            (_, None) => "alias <slot> <name>".into(),
        },
        "profile" => match slot_of(args.first()).await {
            Ok(slot) => {
                let r = provider::profile(slot).await;
                if let Some(dir) = &r.dir {
                    let _ = run_command(&format!("set -s @accounts_profile {}", claude::sq(dir))).await;
                }
                r.message
            }
            Err(e) => e,
        },
        "import" => provider::import_cswap().await.message,
        other => format!("no verb {other:?}"),
    };
    log(&format!("{verb}: {reply}"));
    say(client, &reply);
}

tmux_plugin!(Accounts);
