//! slack: the workspace in a tmux float - a sidebar with badges, a
//! channel view that reads and posts, and the counts in the status line.
//!
//!   bind S plugin-command slack pick            # the sidebar
//!   tmux2 plugin-command slack 'open #general'  # a channel
//!   tmux2 plugin-command slack 'send @tejas on my way'
//!
//! The plugin is the Slack desktop app's signed-in user (auth.rs): no
//! bot, no app, no token to configure. It polls `client.counts` for the
//! badges (what the desktop sidebar shows) and the open channel's
//! history, and caches channels, sections, users and messages in its own
//! SQLite so the sidebar opens at once. The host's `net` imports, when
//! present, replace the poll with Slack's RTM websocket (feed.rs).
//!
//! Status line: `#{@slack_unread}` channels with unread messages,
//! `#{@slack_mentions}` mentions, `#{@slack_dms}` unread DMs.
//!
//! For tests and for a workspace reached another way: `-o api_base=URL`
//! sends every call to `URL/api/<method>` instead of
//! `https://<workspace>.slack.com`, and `-o token=xoxc-… -o cookie=xoxd-…`
//! together are the session as given, no desktop app, Keychain or stored
//! identity consulted (only `auth.test` is asked who they are).
//!
//! `plugin-command slack help` lists every verb; `help <verb>` and
//! `<verb> --help` the details.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use listkit::keys::KeyTable;
use tmux_plugin_sdk::prelude::*;

mod api;
mod auth;
mod chat;
mod emoji;
mod emoji_table;
mod feed;
mod help;
mod http;
mod mrkdwn;
mod sidebar;
mod store;
mod timefmt;

use store::{Binding, Channel, Identity, Kind, Message, Section, User};

pub const NAME: &str = "slack";

/// `load-plugin -o workspace=<subdomain> -o poll_secs=10 -o pick_<action>=<key>`;
/// `-o api_base=<url> -o token=<xoxc> -o cookie=<xoxd>` for a workspace
/// reached another way (see the crate doc).
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct Config {
    workspace: String,
    poll_secs: String,
    open_poll_secs: String,
    keep_days: String,
    api_base: String,
    token: String,
    cookie: String,
    #[serde(flatten)]
    rest: HashMap<String, serde_json::Value>,
}

#[derive(Clone, Debug)]
pub struct Cfg {
    pub workspace: String,
    pub poll_ms: u64,
    pub open_poll_ms: u64,
    pub keep_days: i64,
    /// Where `/api/<method>` lives: `https://<workspace>.slack.com` unless
    /// configured.
    pub api_base: String,
    /// A session given outright (both or neither); empty means the
    /// desktop app's.
    pub token: String,
    pub cookie: String,
}

/// Ids to the names the views draw, rebuilt after every sync.
#[derive(Default)]
pub struct NameMap {
    pub users: HashMap<String, String>,
    pub channels: HashMap<String, String>,
}

impl NameMap {
    /// Run `f` with mrkdwn name resolvers over this map.
    pub fn with<R>(&self, f: impl FnOnce(&mrkdwn::Names) -> R) -> R {
        let u = |id: &str| self.users.get(id).cloned();
        let c = |id: &str| self.channels.get(id).cloned();
        f(&mrkdwn::Names { user: &u, channel: &c })
    }
}

pub struct State {
    pub cfg: Cfg,
    pub keys: KeyTable,
    pub ident: Option<Identity>,
    pub auth_error: Option<String>,
    pub channels: Vec<Channel>,
    pub sections: Vec<Section>,
    pub users: HashMap<String, User>,
    pub names: Rc<NameMap>,
    pub side: Option<sidebar::Sidebar>,
    pub chat: Option<chat::Chat>,
    pub synced_ms: u64,
    pub counts_ms: u64,
    pub syncing: bool,
    pub threads_unread: bool,
    pub thread_mentions: i64,
    pub last_error: Option<String>,
    /// The RTM socket, while it is open.
    pub feed: Option<WsId>,
    pub feed_connecting: bool,
    pub feed_backoff_ms: u64,
    pub feed_since_ms: u64,
    pub feed_error: Option<String>,
    /// Conversations whose new messages go to an agent.
    pub bindings: Vec<Binding>,
    /// The ts of messages this plugin posted (an agent's reply): the
    /// bridge must not hand them back to the agent.
    pub own_posts: HashSet<String>,
}

pub type Shared = Rc<RefCell<State>>;

impl State {
    pub fn channel(&self, id: &str) -> Option<&Channel> {
        self.channels.iter().find(|c| c.id == id)
    }

    pub fn user_label(&self, id: &str) -> String {
        self.names.users.get(id).cloned().unwrap_or_else(|| id.to_string())
    }

    /// "#general", "@tejas", "@a, b" (a group DM).
    pub fn channel_label(&self, c: &Channel) -> String {
        match c.kind {
            Kind::Im => format!("@{}", c.user_id.as_deref().map(|u| self.user_label(u)).unwrap_or_else(|| c.name.clone())),
            Kind::Mpim => {
                // "mpdm-zack--tejas--ana-1": the handles.
                let body = c.name.trim_start_matches("mpdm-");
                let body = body.rsplit_once('-').map(|(b, _)| b).unwrap_or(body);
                let me = self.ident.as_ref().map(|i| i.user_name.clone()).unwrap_or_default();
                let names: Vec<&str> = body.split("--").filter(|n| *n != me).collect();
                format!("@{}", names.join(", "))
            }
            _ => format!("#{}", c.name),
        }
    }

    pub fn rebuild_names(&mut self) {
        let mut users = HashMap::new();
        for (id, u) in &self.users {
            users.insert(id.clone(), u.label().to_string());
        }
        let mut channels = HashMap::new();
        for c in &self.channels {
            channels.insert(c.id.clone(), c.name.clone());
        }
        self.names = Rc::new(NameMap { users, channels });
    }

    /// Find a channel by `#name`, `@user`, an id, or a bare name.
    pub fn resolve_channel(&self, arg: &str) -> Option<Channel> {
        let arg = arg.trim();
        if let Some(c) = self.channels.iter().find(|c| c.id == arg) {
            return Some(c.clone());
        }
        if let Some(user) = arg.strip_prefix('@') {
            let lower = user.to_lowercase();
            let uid = self
                .users
                .values()
                .find(|u| {
                    u.name.to_lowercase() == lower
                        || u.display_name.to_lowercase() == lower
                        || u.real_name.to_lowercase() == lower
                })
                .map(|u| u.id.clone())
                .or_else(|| self.users.contains_key(user).then(|| user.to_string()))?;
            return self.channels.iter().find(|c| c.kind == Kind::Im && c.user_id.as_deref() == Some(&uid)).cloned();
        }
        let name = arg.trim_start_matches('#').to_lowercase();
        self.channels.iter().find(|c| !c.kind.is_dm() && c.name.to_lowercase() == name && !c.is_archived).cloned()
    }

    pub fn tz(&self) -> i64 {
        self.ident.as_ref().map(|i| i.tz_offset).unwrap_or(0)
    }
}

struct Slack {
    st: Shared,
}

impl Plugin for Slack {
    const NAME: &'static str = NAME;
    type Config = Config;

    fn init(ctx: &Ctx, config: Self::Config) -> Result<Self, String> {
        ctx.subscribe(&["plugin-command", "mode-key", "mode-nav", "mode-paste", "mode-resize", "mode-closed", "ws-message", "ws-closed"])
            .map_err(|e| e.message.clone())?;
        store::migrate_sync()?;
        let num = |s: &str, d: u64| s.trim().parse::<u64>().ok().filter(|v| *v > 0).unwrap_or(d);
        let workspace = if config.workspace.trim().is_empty() { "morphllm".to_string() } else { config.workspace.trim().to_string() };
        let api_base = match config.api_base.trim().trim_end_matches('/') {
            "" => format!("https://{workspace}.slack.com"),
            b => b.to_string(),
        };
        let cfg = Cfg {
            workspace,
            poll_ms: num(&config.poll_secs, 10) * 1000,
            open_poll_ms: num(&config.open_poll_secs, 3) * 1000,
            keep_days: num(&config.keep_days, 30) as i64,
            api_base,
            token: config.token.trim().to_string(),
            cookie: config.cookie.trim().to_string(),
        };
        http::set_api_base(&cfg.api_base);
        let mut keys = sidebar::keys();
        for (k, v) in &config.rest {
            if let Some(action) = k.strip_prefix("pick_") {
                let v = v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
                keys.bind(action, Some(&v));
            }
        }
        let st: Shared = Rc::new(RefCell::new(State {
            cfg,
            keys,
            ident: None,
            auth_error: None,
            channels: Vec::new(),
            sections: Vec::new(),
            users: HashMap::new(),
            names: Rc::new(NameMap::default()),
            side: None,
            chat: None,
            synced_ms: 0,
            counts_ms: 0,
            syncing: false,
            threads_unread: false,
            thread_mentions: 0,
            last_error: None,
            feed: None,
            feed_connecting: false,
            feed_backoff_ms: 0,
            feed_since_ms: 0,
            feed_error: None,
            bindings: Vec::new(),
            own_posts: HashSet::new(),
        }));
        ctx.spawn(boot(Rc::clone(&st)));
        Ok(Self { st })
    }

    fn on_event(&mut self, _ctx: &Ctx, event: Event) {
        let name = event.name();
        match name.as_str() {
            "plugin-command" => on_command(&self.st, &event),
            "ws-message" => {
                if let Some(m) = WsMessage::from_event(&event) {
                    feed::on_message(&self.st, &m);
                }
            }
            "ws-closed" => {
                if let Some(c) = WsClosed::from_event(&event) {
                    feed::on_closed(&self.st, &c);
                }
            }
            "mode-key" | "mode-nav" | "mode-paste" | "mode-resize" | "mode-closed" => {
                let mode = event.get_i64("mode");
                let (is_side, is_chat) = {
                    let s = self.st.borrow();
                    (
                        s.side.as_ref().is_some_and(|p| Some(p.engine.mode.0 as i64) == mode),
                        s.chat.as_ref().is_some_and(|c| Some(c.mode.0 as i64) == mode),
                    )
                };
                if is_side {
                    sidebar::on_mode_event(&self.st, name.as_str(), &event);
                } else if is_chat {
                    chat::on_mode_event(&self.st, name.as_str(), &event);
                }
            }
            _ => {}
        }
    }

    fn on_unload(&mut self, _ctx: &Ctx) {
        sidebar::close(&self.st);
        chat::close(&self.st);
        feed::close(&self.st);
    }
}

tmux_plugin!(Slack);

// ---------------------------------------------------------------------------
// boot, sync, poll
// ---------------------------------------------------------------------------

/// What the store knows, into memory (the sidebar before the first fetch).
async fn load_cache(st: &Shared) {
    let channels = store::channels().await;
    let sections = store::sections().await;
    let users = store::users().await;
    let bindings = store::bindings().await;
    let mut s = st.borrow_mut();
    s.channels = channels;
    s.sections = sections;
    s.users = users.into_iter().map(|u| (u.id.clone(), u)).collect();
    s.bindings = bindings;
    s.rebuild_names();
}

async fn boot(st: Shared) {
    load_cache(&st).await;
    emoji::load_custom().await;
    let cfg = st.borrow().cfg.clone();
    match auth::ensure(&cfg).await {
        Ok(id) => {
            log(&format!("slack: {} on {} ({})", id.user_name, id.team_name, id.domain));
            st.borrow_mut().ident = Some(id);
        }
        Err(e) => {
            log(&format!("slack: no session: {e}"));
            let _ = display_message(&format!("slack: {e}"));
            st.borrow_mut().auth_error = Some(e);
            sidebar::refresh(&st);
            return;
        }
    }
    if let Err(e) = full_sync(&st).await {
        log(&format!("slack: sync: {e}"));
        let _ = display_message(&format!("slack: {e}"));
        st.borrow_mut().last_error = Some(e);
    }
    let keep = st.borrow().cfg.keep_days;
    store::sweep(keep).await;
    spawn(feed::connect(Rc::clone(&st)));
    poll_loop(st).await;
}

/// Channels, sections, users and badges, all of it.
pub async fn full_sync(st: &Shared) -> Result<(), String> {
    let Some(id) = st.borrow().ident.clone() else { return Err("no session".into()) };
    if st.borrow().syncing {
        return Ok(());
    }
    st.borrow_mut().syncing = true;
    let r = full_sync_inner(st, &id).await;
    st.borrow_mut().syncing = false;
    r
}

async fn full_sync_inner(st: &Shared, id: &Identity) -> Result<(), String> {
    let chans = api::conversations(id).await?;
    store::upsert_channels(&chans).await.map_err(|e| e.message.clone())?;
    match api::sections(id).await {
        Ok(secs) => {
            let _ = store::replace_sections(&secs).await;
        }
        Err(e) => log(&format!("slack: sections: {e}")),
    }
    match api::users(id).await {
        Ok(users) => {
            let _ = store::upsert_users(&users).await;
            if let Some(me) = users.iter().find(|u| u.id == id.user_id) {
                let mut s = st.borrow_mut();
                if let Some(i) = s.ident.as_mut() {
                    if i.tz_offset != me.tz_offset {
                        i.tz_offset = me.tz_offset;
                        let copy = i.clone();
                        drop(s);
                        let _ = store::set_auth(&copy).await;
                    }
                }
            }
        }
        Err(e) => log(&format!("slack: users: {e}")),
    }
    let counts = api::counts(id).await?;
    store::apply_counts(&counts.channels).await.map_err(|e| e.message.clone())?;
    if let Err(e) = emoji::refresh_custom(id).await {
        log(&format!("slack: emoji.list: {e}"));
    }
    load_cache(st).await;
    {
        let mut s = st.borrow_mut();
        s.synced_ms = now_ms();
        s.counts_ms = now_ms();
        s.threads_unread = counts.threads_unread;
        s.thread_mentions = counts.thread_mentions;
        s.last_error = None;
        publish(&s);
    }
    sidebar::refresh(st);
    chat::refresh_names(st);
    Ok(())
}

/// A change in a channel's badge worth telling the user about.
struct Ping {
    channel: Channel,
}

/// The badges, and what changed since last time.
pub async fn refresh_counts(st: &Shared) -> Result<(), String> {
    let Some(id) = st.borrow().ident.clone() else { return Err("no session".into()) };
    let counts = api::counts(&id).await?;
    store::apply_counts(&counts.channels).await.map_err(|e| e.message.clone())?;
    let mut pings: Vec<Ping> = Vec::new();
    let mut read_elsewhere: Vec<(String, String)> = Vec::new();
    {
        let mut s = st.borrow_mut();
        let first = s.counts_ms == 0;
        for c in &counts.channels {
            let Some(ch) = s.channels.iter_mut().find(|x| x.id == c.id) else { continue };
            let was_mentions = ch.mentions;
            let was_unread = ch.has_unreads;
            let was_read = ch.last_read.clone();
            if let Some(v) = &c.last_read {
                ch.last_read = Some(v.clone());
            }
            if let Some(v) = &c.latest {
                ch.latest = Some(v.clone());
            }
            ch.has_unreads = c.has_unreads;
            ch.mentions = c.mentions;
            if let Some(m) = c.is_muted {
                ch.is_muted = m;
            }
            if let Some(o) = c.is_open {
                ch.is_open = o;
            }
            if !first && !ch.is_muted {
                let dm_new = ch.kind.is_dm() && c.has_unreads && !was_unread;
                if c.mentions > was_mentions || dm_new {
                    pings.push(Ping { channel: ch.clone() });
                }
            }
            if was_read != ch.last_read && was_unread && !c.has_unreads {
                if let Some(lr) = &ch.last_read {
                    read_elsewhere.push((ch.id.clone(), lr.clone()));
                }
            }
        }
        s.counts_ms = now_ms();
        s.threads_unread = counts.threads_unread;
        s.thread_mentions = counts.thread_mentions;
        publish(&s);
    }
    for (cid, ts) in read_elsewhere {
        store::mark_read(&cid, &ts).await;
    }
    sidebar::refresh(st);
    for p in pings {
        announce(st, &id, p).await;
    }
    Ok(())
}

/// Tell the user about a new mention or DM: its text on the status line
/// (the notification feed will take this over).
async fn announce(st: &Shared, id: &Identity, p: Ping) {
    let Ok((msgs, _)) = api::history(id, &p.channel.id, None, None, 1).await else { return };
    let Some(m) = msgs.into_iter().next() else { return };
    let (who, label, text) = {
        let s = st.borrow();
        let who = m.user.as_deref().map(|u| s.user_label(u)).or_else(|| m.username.clone()).unwrap_or_default();
        let label = s.channel_label(&p.channel);
        let text = s.names.with(|n| mrkdwn::plain(&m.text, n));
        (who, label, text)
    };
    let line = if p.channel.kind == Kind::Im {
        format!("Slack {who}: {text}")
    } else {
        format!("Slack {label} {who}: {text}")
    };
    let _ = display_message(&listkit::text::clip(&line, 160));
    let _ = store::upsert_messages(&[m]).await;
}

async fn poll_loop(st: Shared) {
    let mut n: u64 = 0;
    loop {
        let ms = st.borrow().cfg.poll_ms;
        if sleep_ms(ms).await.is_err() {
            return;
        }
        if st.borrow().ident.is_none() {
            return;
        }
        n += 1;
        // A full sync every ~30 minutes catches new channels and names.
        let full_every = (30 * 60 * 1000 / ms).max(1);
        let r = if n % full_every == 0 { full_sync(&st).await } else { refresh_counts(&st).await };
        if let Err(e) = r {
            log(&format!("slack: poll: {e}"));
            // A rejected session: derive again next time round.
            if e.contains("invalid_auth") || e.contains("not_authed") || e.contains("token_revoked") {
                let cfg = st.borrow().cfg.clone();
                store::clear_auth(&cfg.workspace).await;
                match auth::ensure(&cfg).await {
                    Ok(id) => st.borrow_mut().ident = Some(id),
                    Err(e2) => {
                        let _ = display_message(&format!("slack: {e2}"));
                    }
                }
            }
        }
    }
}

/// The counts the status line reads.
pub fn publish(s: &State) {
    let unread = s.channels.iter().filter(|c| c.has_unreads && !c.is_muted && !c.is_archived).count();
    let mentions: i64 = s.channels.iter().filter(|c| !c.is_muted).map(|c| c.mentions).sum::<i64>() + s.thread_mentions;
    let dms = s.channels.iter().filter(|c| c.kind.is_dm() && c.has_unreads).count();
    let _ = set_option("@slack_unread", &unread.to_string());
    let _ = set_option("@slack_mentions", &mentions.to_string());
    let _ = set_option("@slack_dms", &dms.to_string());
}

/// The sidebar and the chat view, after a change to what they show.
pub fn redraw_all(st: &Shared) {
    sidebar::refresh(st);
    chat::redraw(st);
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

fn on_command(st: &Shared, event: &Event) {
    let text = event.get_str("text").unwrap_or("").trim().to_string();
    let client = event.scope.client.map(u64::from);
    let pane = event.scope.pane;
    let mut words = text.splitn(2, char::is_whitespace);
    let verb = words.next().unwrap_or("").to_string();
    let rest = words.next().unwrap_or("").trim().to_string();
    let is_help = |w: &str| w == "--help" || w == "-h";
    let second = rest.split_whitespace().next().unwrap_or("");
    if verb.is_empty() || verb == "help" || is_help(&verb) {
        let _ = reply(help::topic(Some(second).filter(|s| !s.is_empty())));
        return;
    }
    if is_help(second) {
        let _ = reply(help::topic(Some(&verb)));
        return;
    }
    match verb.as_str() {
        "pick" | "sidebar" => {
            chat::close(st);
            sidebar::open(st, client, pane);
        }
        "open" => {
            let mut it = rest.split_whitespace();
            let Some(target) = it.next() else {
                let _ = reply("open: which channel? `open #name`, `open @user`");
                return;
            };
            let thread = it.next().map(str::to_string);
            let Some(ch) = st.borrow().resolve_channel(target) else {
                let _ = reply(&format!("open: no channel {target} (try `slack refresh`)"));
                return;
            };
            sidebar::close(st);
            chat::open(st, ch, thread, client, pane, false);
        }
        "send" | "reply" => {
            let mut it = rest.splitn(if verb == "reply" { 3 } else { 2 }, char::is_whitespace);
            let Some(target) = it.next().filter(|s| !s.is_empty()) else {
                let _ = reply(&format!("{verb}: `{verb} #channel{} <text>`", if verb == "reply" { " <thread_ts>" } else { "" }));
                return;
            };
            let thread = if verb == "reply" { it.next().map(str::to_string) } else { None };
            let Some(msg) = it.next().map(str::trim).filter(|s| !s.is_empty()) else {
                let _ = reply(&format!("{verb}: nothing to send"));
                return;
            };
            let Some(ch) = st.borrow().resolve_channel(target) else {
                let _ = reply(&format!("{verb}: no channel {target}"));
                return;
            };
            let label = st.borrow().channel_label(&ch);
            let _ = reply(&format!("posting to {label}…"));
            let st2 = Rc::clone(st);
            let msg = msg.to_string();
            spawn(async move {
                match post_message(&st2, &ch.id, &msg, thread.as_deref()).await {
                    Ok(ts) => {
                        let _ = display_message(&format!("slack: posted to {label} ({ts})"));
                    }
                    Err(e) => {
                        let _ = display_message(&format!("slack: {label}: {e}"));
                    }
                }
            });
        }
        "mark" => {
            let Some(ch) = st.borrow().resolve_channel(&rest) else {
                let _ = reply(&format!("mark: no channel {rest}"));
                return;
            };
            let st2 = Rc::clone(st);
            spawn(async move {
                mark_read(&st2, &ch.id).await;
            });
        }
        "unread" => {
            let s = st.borrow();
            let mut lines: Vec<String> = s
                .channels
                .iter()
                .filter(|c| c.has_unreads && !c.is_muted && !c.is_archived)
                .map(|c| {
                    let m = if c.mentions > 0 { format!(" ({} mention{})", c.mentions, if c.mentions == 1 { "" } else { "s" }) } else { String::new() };
                    format!("{}{m}", s.channel_label(c))
                })
                .collect();
            if s.threads_unread {
                lines.push("threads".into());
            }
            let _ = reply(&if lines.is_empty() { "nothing unread".to_string() } else { lines.join("\n") });
        }
        "status" => {
            let s = st.borrow();
            let text = match (&s.ident, &s.auth_error) {
                (Some(id), _) => {
                    let unread = s.channels.iter().filter(|c| c.has_unreads && !c.is_muted).count();
                    let mentions: i64 = s.channels.iter().map(|c| c.mentions).sum();
                    let age = |ms: u64| if ms == 0 { "never".to_string() } else { listkit::text::fmt_age((now_ms().saturating_sub(ms)) / 1000) + " ago" };
                    let feed = match (&s.feed, &s.feed_error) {
                        (Some(_), _) => format!("live feed up {}", age(s.feed_since_ms).trim_end_matches(" ago")),
                        (None, Some(e)) => format!("live feed down ({e}), polling"),
                        (None, None) => "live feed connecting".to_string(),
                    };
                    format!(
                        "{} on {} ({}.slack.com), tz {:+}h\n{} channels, {} users cached; {} unread, {} mentions{}\nbadges {}, full sync {}, {feed}{}",
                        id.user_name,
                        id.team_name,
                        id.domain,
                        id.tz_offset / 3600,
                        s.channels.len(),
                        s.users.len(),
                        unread,
                        mentions,
                        if s.threads_unread { ", threads unread" } else { "" },
                        age(s.counts_ms),
                        age(s.synced_ms),
                        s.last_error.as_ref().map(|e| format!("\nlast error: {e}")).unwrap_or_default()
                    )
                }
                (None, Some(e)) => format!("no session: {e}\nSign into {}.slack.com in the Slack desktop app, then `slack auth`.", s.cfg.workspace),
                (None, None) => "connecting…".into(),
            };
            let _ = reply(&text);
        }
        "refresh" => {
            let _ = reply("refreshing…");
            let st2 = Rc::clone(st);
            spawn(async move {
                if let Err(e) = full_sync(&st2).await {
                    let _ = display_message(&format!("slack: {e}"));
                }
            });
        }
        "auth" => {
            let _ = reply("deriving the session from the desktop app…");
            let st2 = Rc::clone(st);
            spawn(async move {
                let cfg = st2.borrow().cfg.clone();
                store::clear_auth(&cfg.workspace).await;
                match auth::ensure(&cfg).await {
                    Ok(id) => {
                        let who = format!("{} on {}", id.user_name, id.team_name);
                        {
                            let mut s = st2.borrow_mut();
                            s.ident = Some(id);
                            s.auth_error = None;
                        }
                        let _ = display_message(&format!("slack: {who}"));
                        let _ = full_sync(&st2).await;
                    }
                    Err(e) => {
                        st2.borrow_mut().auth_error = Some(e.clone());
                        let _ = display_message(&format!("slack: {e}"));
                    }
                }
            });
        }
        "bind" | "unbind" => {
            let mut it = rest.split_whitespace();
            let Some(target) = it.next() else {
                let _ = reply(&format!("{verb}: `{verb} #channel[/thread_ts]{}`", if verb == "bind" { " <agent-id>" } else { "" }));
                return;
            };
            let (chan, thread) = target.split_once('/').map(|(c, t)| (c, t.to_string())).unwrap_or((target, String::new()));
            let Some(ch) = st.borrow().resolve_channel(chan) else {
                let _ = reply(&format!("{verb}: no channel {chan}"));
                return;
            };
            let agent = it.next().map(str::to_string);
            if verb == "bind" && agent.is_none() {
                let _ = reply("bind: which agent? `bind #channel claude:<id>`");
                return;
            }
            let label = st.borrow().channel_label(&ch);
            let _ = reply(&format!("{} {label}{}", if verb == "bind" { "bound" } else { "unbound" }, if thread.is_empty() { String::new() } else { format!("/{thread}") }));
            let st2 = Rc::clone(st);
            spawn(async move {
                if let Some(a) = agent {
                    let since = ch.latest.clone().unwrap_or_default();
                    store::bind(&ch.id, &thread, &a, &since).await;
                } else {
                    store::unbind(&ch.id, &thread).await;
                }
                let b = store::bindings().await;
                st2.borrow_mut().bindings = b;
            });
        }
        "notify" => {
            // `notify <title> | <body>`: a line in the notification feed, the
            // way a Slack notification arrives (for hooks and tests).
            let (title, body) = rest.split_once('|').map(|(t, b)| (t.trim(), b.trim())).unwrap_or(("slack", rest.trim()));
            if body.is_empty() && title.is_empty() {
                let _ = reply("notify: `notify <title> | <body>`");
                return;
            }
            let (title, body) = (title.to_string(), body.to_string());
            spawn(async move {
                notify(&title, &body, None, None).await;
            });
        }
        "close" => {
            sidebar::close(st);
            chat::close(st);
        }
        // "menu-key <key>": an item of a view's Space menu (display-menu)
        // handing its key back, so the menu is a mirror of the keymap and
        // never a second code path. The channel view has the focus when
        // it is open; else the sidebar.
        "menu-key" => {
            let key = rest.split_whitespace().next().unwrap_or("").to_string();
            if key.is_empty() {
                return;
            }
            let chat_open = st.borrow().chat.is_some();
            if chat_open {
                chat::replay_key(st, &key);
            } else {
                sidebar::replay_key(st, &key);
            }
        }
        _ => {
            let _ = reply(&format!("slack: unknown verb {verb:?} - `slack help` lists them"));
        }
    }
}

/// A line for the notification feed (the notify-toast plugin's `notify`
/// service); the status line when that plugin is not there.
pub async fn notify(title: &str, body: &str, key: Option<&str>, action: Option<&str>) {
    #[derive(serde::Serialize)]
    struct Req<'a> {
        source: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        key: Option<&'a str>,
        title: &'a str,
        body: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        action: Option<&'a str>,
    }
    let req = Req { source: "slack", key, title, body, action };
    if let Err(e) = service::call_json::<_, serde_json::Value>("notify_toast", "notify", &req).await {
        log(&format!("slack: notify: {} (toast plugin not loaded?)", e.message));
        let line = format!("Slack {title}: {body}");
        let _ = display_message(&listkit::text::clip(&listkit::text::one_line(&line), 180));
    }
}

/// Post, cache, mark read. Returns the ts.
pub async fn post_message(st: &Shared, channel: &str, text: &str, thread_ts: Option<&str>) -> Result<String, String> {
    let Some(id) = st.borrow().ident.clone() else { return Err("no session".into()) };
    let m = api::post(&id, channel, text, thread_ts).await?;
    let ts = m.ts.clone();
    {
        let mut s = st.borrow_mut();
        if s.own_posts.len() > 500 {
            s.own_posts.clear();
        }
        s.own_posts.insert(ts.clone());
    }
    let _ = store::upsert_messages(&[m.clone()]).await;
    store::note_message(channel, &ts, true, false).await;
    {
        let mut s = st.borrow_mut();
        if let Some(c) = s.channels.iter_mut().find(|c| c.id == channel) {
            c.latest = Some(ts.clone());
            c.last_read = Some(ts.clone());
            c.has_unreads = false;
            c.mentions = 0;
        }
    }
    chat::on_posted(st, m);
    sidebar::refresh(st);
    Ok(ts)
}

/// Mark a channel read up to its latest message, on Slack and here.
pub async fn mark_read(st: &Shared, channel: &str) {
    let (id, ts) = {
        let s = st.borrow();
        let Some(id) = s.ident.clone() else { return };
        let ts = s.channel(channel).and_then(|c| c.latest.clone());
        (id, ts)
    };
    let ts = match ts {
        Some(t) => t,
        None => match store::latest_ts(channel).await {
            Some(t) => t,
            None => return,
        },
    };
    if let Err(e) = api::mark(&id, channel, &ts).await {
        log(&format!("slack: mark {channel}: {e}"));
        return;
    }
    store::mark_read(channel, &ts).await;
    {
        let mut s = st.borrow_mut();
        if let Some(c) = s.channels.iter_mut().find(|c| c.id == channel) {
            c.last_read = Some(ts.clone());
            c.has_unreads = false;
            c.mentions = 0;
        }
        publish(&s);
    }
    sidebar::refresh(st);
}

/// Messages that arrived in `channel` (from a fetch or the feed): cache
/// them and keep the in-memory channel row current.
pub async fn absorb(st: &Shared, channel: &str, msgs: &[Message]) {
    if msgs.is_empty() {
        return;
    }
    let _ = store::upsert_messages(msgs).await;
    let newest = msgs.iter().map(|m| m.ts.as_str()).max().unwrap_or("").to_string();
    let mut s = st.borrow_mut();
    if let Some(c) = s.channels.iter_mut().find(|c| c.id == channel) {
        if c.latest.as_deref().is_none_or(|l| l < newest.as_str()) {
            c.latest = Some(newest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        State {
            cfg: Cfg { workspace: "w".into(), poll_ms: 1, open_poll_ms: 1, keep_days: 1, api_base: "https://w.slack.com".into(), token: String::new(), cookie: String::new() },
            keys: sidebar::keys(),
            ident: Some(Identity { user_name: "zack".into(), ..Default::default() }),
            auth_error: None,
            channels: vec![
                Channel { id: "C1".into(), name: "general".into(), kind: Kind::Public, user_id: None, topic: String::new(), purpose: String::new(), is_member: true, is_muted: false, is_starred: false, is_archived: false, is_open: true, last_read: None, latest: None, has_unreads: false, mentions: 0, updated_ms: 0 },
                Channel { id: "D1".into(), name: "U2".into(), kind: Kind::Im, user_id: Some("U2".into()), topic: String::new(), purpose: String::new(), is_member: true, is_muted: false, is_starred: false, is_archived: false, is_open: true, last_read: None, latest: None, has_unreads: false, mentions: 0, updated_ms: 0 },
                Channel { id: "G1".into(), name: "mpdm-zack--tejas--ana-1".into(), kind: Kind::Mpim, user_id: None, topic: String::new(), purpose: String::new(), is_member: true, is_muted: false, is_starred: false, is_archived: false, is_open: true, last_read: None, latest: None, has_unreads: false, mentions: 0, updated_ms: 0 },
            ],
            sections: vec![],
            users: [("U2".to_string(), User { id: "U2".into(), name: "tejas".into(), real_name: "Tejas K".into(), display_name: "tejas".into(), ..Default::default() })].into_iter().collect(),
            names: Rc::new(NameMap::default()),
            side: None,
            chat: None,
            synced_ms: 0,
            counts_ms: 0,
            syncing: false,
            threads_unread: false,
            thread_mentions: 0,
            last_error: None,
            feed: None,
            feed_connecting: false,
            feed_backoff_ms: 0,
            feed_since_ms: 0,
            feed_error: None,
            bindings: Vec::new(),
            own_posts: HashSet::new(),
        }
    }

    #[test]
    fn labels_and_resolution() {
        let mut s = state();
        s.rebuild_names();
        assert_eq!(s.channel_label(&s.channels[0]), "#general");
        assert_eq!(s.channel_label(&s.channels[1]), "@tejas");
        assert_eq!(s.channel_label(&s.channels[2]), "@tejas, ana");
        assert_eq!(s.resolve_channel("#general").map(|c| c.id), Some("C1".into()));
        assert_eq!(s.resolve_channel("General").map(|c| c.id), Some("C1".into()));
        assert_eq!(s.resolve_channel("@Tejas").map(|c| c.id), Some("D1".into()));
        assert_eq!(s.resolve_channel("@Tejas K").map(|c| c.id), Some("D1".into()));
        assert_eq!(s.resolve_channel("D1").map(|c| c.id), Some("D1".into()));
        assert!(s.resolve_channel("#nope").is_none());
    }
}
