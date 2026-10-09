//! The panel: every server's accounts with their rate-limit windows,
//! the login marked, keys to switch, add, refresh, hold out, remove.
//!
//! One block per server: the local one first, then each linked server
//! that is up, each fetched from its own provider (the local one
//! directly, the others over the bridge). A block that has no provider
//! says so and stays. The cursor walks the accounts across blocks.

use std::cell::RefCell;
use std::rc::Rc;

use tmux_plugin_sdk::abi::ErrorCode;
use tmux_plugin_sdk::prelude::*;

use crate::claude;
use crate::oauth::Window;
use crate::provider::{self, AddReq, ListReq, Reply, SlotReq, Snapshot};
use crate::timefmt;

pub const LOCAL: &str = "local";
const MIN_WIDTH: u32 = 70;
const MAX_WIDTH: u32 = 110;
const MIN_HEIGHT: u32 = 12;
/// While open, the blocks are refetched on this cadence (the providers
/// only hit the API when a slot is due).
const REFRESH_MS: u64 = 20_000;
const BAR: usize = 30;

#[derive(Debug, Clone, PartialEq)]
pub enum Confirm {
    Switch { server: String, slot: i64, label: String },
    Remove { server: String, slot: i64, label: String },
    Best { server: String },
}

#[derive(Debug, Default)]
pub struct Block {
    pub server: String,
    pub snap: Option<Snapshot>,
    pub error: String,
    pub fetching: bool,
    pub fetched_ms: u64,
}

pub struct Panel {
    pub mode: ModeId,
    pub width: u32,
    pub height: u32,
    pub blocks: Vec<Block>,
    /// (block, slot) the cursor is on.
    pub cur: Option<(usize, i64)>,
    pub scroll: usize,
    pub status: String,
    pub confirm: Option<Confirm>,
    pub timer: Option<TaskId>,
    pub client: Option<u64>,
    pub compact: bool,
    pub help: bool,
    pub busy: bool,
    pub tz_min: i32,
}

pub type Shared = Rc<RefCell<Option<Panel>>>;

// ---------------------------------------------------------------------------
// servers and fetching
// ---------------------------------------------------------------------------

fn server_list(only: Option<&str>) -> Vec<String> {
    let mut v = vec![LOCAL.to_string()];
    for s in service::linked_servers() {
        if s.up && !s.local {
            v.push(s.name);
        }
    }
    match only {
        Some(o) if v.iter().any(|s| s == o) => vec![o.to_string()],
        _ => v,
    }
}

async fn fetch_list(server: &str, req: &ListReq) -> Result<Snapshot, HostError> {
    if server == LOCAL {
        Ok(provider::snapshot(req).await)
    } else {
        service::call_json::<_, Snapshot>(&format!("@{server}"), "list", req).await
    }
}

async fn call(server: &str, method: &str, req: &SlotReq) -> Reply {
    if server == LOCAL {
        match method {
            "switch" => provider::switch(req.slot).await,
            "remove" => provider::remove(req.slot).await,
            "disable" => provider::set_disabled(req.slot, req.on).await,
            "profile" => provider::profile(req.slot).await,
            _ => Reply { ok: false, message: format!("no {method}"), slot: None, dir: None },
        }
    } else {
        match service::call_json::<_, Reply>(&format!("@{server}"), method, req).await {
            Ok(r) => r,
            Err(e) => Reply { ok: false, message: format!("{server}: {}", e.message), slot: None, dir: None },
        }
    }
}

async fn call0(server: &str, method: &str) -> Reply {
    if server == LOCAL {
        match method {
            "best" => provider::best().await,
            "add" => provider::add(&AddReq::default()).await,
            "import" => provider::import_cswap().await,
            _ => Reply { ok: false, message: format!("no {method}"), slot: None, dir: None },
        }
    } else {
        let r = if method == "add" {
            service::call_json::<_, Reply>(&format!("@{server}"), method, &AddReq::default()).await
        } else {
            service::call_json::<_, Reply>(&format!("@{server}"), method, &()).await
        };
        match r {
            Ok(r) => r,
            Err(e) => Reply { ok: false, message: format!("{server}: {}", e.message), slot: None, dir: None },
        }
    }
}

/// Fetch one block and repaint. `force` fetches every slot's usage.
async fn fetch_block(panel: Shared, server: String, refresh: bool, force: bool) {
    {
        let mut b = panel.borrow_mut();
        let Some(p) = b.as_mut() else { return };
        let Some(blk) = p.blocks.iter_mut().find(|b| b.server == server) else { return };
        if blk.fetching {
            return;
        }
        blk.fetching = true;
        render(p);
    }
    let res = fetch_list(&server, &ListReq { refresh, force }).await;
    let mut b = panel.borrow_mut();
    let Some(p) = b.as_mut() else { return };
    let Some(blk) = p.blocks.iter_mut().find(|b| b.server == server) else { return };
    blk.fetching = false;
    blk.fetched_ms = now_ms();
    match res {
        Ok(snap) => {
            blk.snap = Some(snap);
            blk.error.clear();
        }
        Err(e) => {
            blk.error = match e.code {
                ErrorCode::Unreachable => "link down".into(),
                ErrorCode::NoSuchObject => "no accounts plugin there (grant it run-process, fs-write and fs-read-any in plugin-remote-caps)".into(),
                ErrorCode::Version => format!("version mismatch: {}", e.message),
                ErrorCode::Timeout => "no answer in 30 s".into(),
                _ => e.message.clone(),
            };
        }
    }
    if p.cur.is_none() {
        p.cur = first_row(p);
    }
    render(p);
}

pub async fn fetch_all(panel: Shared, refresh: bool, force: bool) {
    let servers: Vec<String> = {
        let b = panel.borrow();
        let Some(p) = b.as_ref() else { return };
        p.blocks.iter().map(|b| b.server.clone()).collect()
    };
    let futs: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>> = servers
        .into_iter()
        .map(|s| Box::pin(fetch_block(Rc::clone(&panel), s, refresh, force)) as _)
        .collect();
    provider::join_all(futs).await;
}

async fn tick(panel: Shared) {
    loop {
        if sleep_ms(REFRESH_MS).await.is_err() {
            return;
        }
        if panel.borrow().is_none() {
            return;
        }
        fetch_all(Rc::clone(&panel), true, false).await;
    }
}

// ---------------------------------------------------------------------------
// open and close
// ---------------------------------------------------------------------------

pub fn any_client() -> Option<u64> {
    list_clients().ok()?.into_iter().next().map(|c| u64::from(c.id))
}

pub async fn open(panel: Shared, client: Option<u64>, here: Option<u32>, only: Option<String>, cursor_slot: Option<i64>) {
    let client_window = |cid: u64| {
        list_clients()
            .ok()?
            .into_iter()
            .find(|c| u64::from(c.id) == cid)?
            .session
            .and_then(|s| resolve_session(SessionId(s)).ok())
            .and_then(|v| v.current_window)
    };
    let window = client
        .and_then(client_window)
        .or_else(|| here.and_then(|p| resolve_pane(PaneId(p)).ok().map(|pi| pi.window)))
        .or_else(|| any_client().and_then(client_window))
        .or_else(|| list_windows().ok().and_then(|w| w.first().map(|x| x.id)));
    let Some(window) = window else {
        let _ = display_message("accounts: no window to open the panel in");
        return;
    };
    let (ww, wh) = resolve_window(WindowId(window)).map(|w| (w.width, w.height)).unwrap_or((120, 40));
    let width = (ww.saturating_sub(4)).clamp(MIN_WIDTH.min(ww), MAX_WIDTH);
    let height = (wh.saturating_sub(2)).max(MIN_HEIGHT.min(wh));
    let mode = match mode_open(&ModeOpts {
        window: Some(WindowId(window)),
        width,
        height,
        title: Some("accounts".into()),
        ..Default::default()
    }) {
        Ok(m) => m,
        Err(e) => {
            let _ = display_message(&format!("accounts: cannot open the panel: {}", e.message));
            return;
        }
    };
    let tz_min = claude::platform().await.tz_min;
    let blocks = server_list(only.as_deref())
        .into_iter()
        .map(|server| Block { server, ..Default::default() })
        .collect();
    let compact = crate::store::get_setting("compact").await.ok().flatten().as_deref() == Some("1");
    let mut p = Panel {
        mode,
        width,
        height,
        blocks,
        cur: None,
        scroll: 0,
        status: String::new(),
        confirm: None,
        timer: None,
        client,
        compact,
        help: false,
        busy: false,
        tz_min,
    };
    if let (Some(s), Some(_)) = (cursor_slot, only.as_ref()) {
        p.cur = Some((0, s));
    }
    render(&mut p);
    *panel.borrow_mut() = Some(p);
    let t = spawn(tick(Rc::clone(&panel)));
    if let Some(p) = panel.borrow_mut().as_mut() {
        p.timer = Some(t);
    }
    fetch_all(panel, true, false).await;
}

pub fn close(panel: &Shared) {
    let mut b = panel.borrow_mut();
    if let Some(p) = b.take() {
        if let Some(t) = p.timer {
            cancel(t);
        }
        let _ = mode_close(p.mode);
    }
}

// ---------------------------------------------------------------------------
// rows and navigation
// ---------------------------------------------------------------------------

fn rows(p: &Panel) -> Vec<(usize, i64)> {
    let mut v = Vec::new();
    for (i, b) in p.blocks.iter().enumerate() {
        if let Some(s) = &b.snap {
            for a in &s.accounts {
                v.push((i, a.slot));
            }
        }
    }
    v
}

fn first_row(p: &Panel) -> Option<(usize, i64)> {
    let r = rows(p);
    // The login of the first block, else the first row.
    for b in &p.blocks {
        if let Some(s) = &b.snap {
            if let Some(slot) = s.live_slot {
                let i = p.blocks.iter().position(|x| x.server == b.server).unwrap_or(0);
                return Some((i, slot));
            }
        }
    }
    r.first().copied()
}

fn step(p: &mut Panel, delta: i64) {
    let r = rows(p);
    if r.is_empty() {
        p.cur = None;
        return;
    }
    let i = p.cur.and_then(|c| r.iter().position(|x| *x == c)).unwrap_or(0) as i64;
    let n = (i + delta).clamp(0, r.len() as i64 - 1) as usize;
    p.cur = Some(r[n]);
}

fn cur_account<'a>(p: &'a Panel) -> Option<(&'a Block, &'a provider::AccountRow)> {
    let (bi, slot) = p.cur?;
    let b = p.blocks.get(bi)?;
    let a = b.snap.as_ref()?.accounts.iter().find(|a| a.slot == slot)?;
    Some((b, a))
}

// ---------------------------------------------------------------------------
// keys
// ---------------------------------------------------------------------------

/// What a key asks for, decided inside the borrow, run outside it.
enum Act {
    None,
    Close,
    Fetch { force: bool },
    Call { server: String, method: &'static str, req: SlotReq },
    Call0 { server: String, method: &'static str },
    NewAgent { server: String, slot: i64 },
}

pub fn on_key(panel: &Shared, ctx: &Ctx, event: &Event) {
    let Some(key) = event.get_str("key") else { return };
    let act = {
        let mut b = panel.borrow_mut();
        let Some(p) = b.as_mut() else { return };
        if event.get_i64("mode") != Some(p.mode.0 as i64) {
            return;
        }
        let act = key_action(p, key);
        render(p);
        act
    };
    match act {
        Act::None => {}
        Act::Close => close(panel),
        Act::Fetch { force } => {
            let panel = Rc::clone(panel);
            ctx.spawn(async move {
                fetch_all(panel, true, force).await;
            });
        }
        Act::Call { server, method, req } => {
            let panel = Rc::clone(panel);
            ctx.spawn(async move {
                set_busy(&panel, true, &format!("{method} on {server}…"));
                let r = call(&server, method, &req).await;
                set_busy(&panel, false, &r.message);
                fetch_block(panel, server, true, false).await;
            });
        }
        Act::Call0 { server, method } => {
            let panel = Rc::clone(panel);
            ctx.spawn(async move {
                set_busy(&panel, true, &format!("{method} on {server}…"));
                let r = call0(&server, method).await;
                set_busy(&panel, false, &r.message);
                fetch_block(panel, server, true, false).await;
            });
        }
        Act::NewAgent { server, slot } => {
            let panel = Rc::clone(panel);
            ctx.spawn(async move {
                new_agent(panel, server, slot).await;
            });
        }
    }
}

fn set_busy(panel: &Shared, busy: bool, status: &str) {
    let mut b = panel.borrow_mut();
    if let Some(p) = b.as_mut() {
        p.busy = busy;
        p.status = status.to_string();
        render(p);
    }
}

fn key_action(p: &mut Panel, key: &str) -> Act {
    // A pending confirmation eats the next key.
    if let Some(c) = p.confirm.clone() {
        p.confirm = None;
        return match key {
            "y" | "Y" | "Enter" => match c {
                Confirm::Switch { server, slot, .. } => Act::Call { server, method: "switch", req: SlotReq { slot, ..Default::default() } },
                Confirm::Remove { server, slot, .. } => Act::Call { server, method: "remove", req: SlotReq { slot, ..Default::default() } },
                Confirm::Best { server } => Act::Call0 { server, method: "best" },
            },
            _ => {
                p.status = "cancelled".into();
                Act::None
            }
        };
    }
    if p.help {
        p.help = false;
        return Act::None;
    }
    if p.busy && !matches!(key, "Escape" | "q") {
        return Act::None;
    }
    match key {
        "Escape" | "q" => Act::Close,
        "j" | "Down" => {
            step(p, 1);
            Act::None
        }
        "k" | "Up" => {
            step(p, -1);
            Act::None
        }
        "g" | "Home" => {
            step(p, i64::MIN / 2);
            Act::None
        }
        "G" | "End" => {
            step(p, i64::MAX / 2);
            Act::None
        }
        "Tab" => {
            // Next block.
            let r = rows(p);
            if let Some((bi, _)) = p.cur {
                if let Some(n) = r.iter().find(|(b, _)| *b > bi) {
                    p.cur = Some(*n);
                } else if let Some(n) = r.first() {
                    p.cur = Some(*n);
                }
            }
            Act::None
        }
        "?" => {
            p.help = true;
            Act::None
        }
        "c" => {
            p.compact = !p.compact;
            let v = if p.compact { "1" } else { "0" };
            spawn(async move {
                let _ = crate::store::set_setting("compact", v).await;
            });
            Act::None
        }
        "r" => Act::Fetch { force: true },
        "Enter" => match cur_account(p) {
            Some((b, a)) if a.active => {
                p.status = format!("{} is already the login on {}", a.email, label_of(b));
                Act::None
            }
            Some((b, a)) => {
                p.confirm = Some(Confirm::Switch {
                    server: b.server.clone(),
                    slot: a.slot,
                    label: format!("{} {}", a.slot, a.email),
                });
                Act::None
            }
            None => Act::None,
        },
        "b" => match p.cur.and_then(|(bi, _)| p.blocks.get(bi)) {
            Some(b) => {
                p.confirm = Some(Confirm::Best { server: b.server.clone() });
                Act::None
            }
            None => Act::None,
        },
        "a" => match p.cur.and_then(|(bi, _)| p.blocks.get(bi)).or_else(|| p.blocks.first()) {
            Some(b) => Act::Call0 { server: b.server.clone(), method: "add" },
            None => Act::None,
        },
        "i" => match p.cur.and_then(|(bi, _)| p.blocks.get(bi)).or_else(|| p.blocks.first()) {
            Some(b) => Act::Call0 { server: b.server.clone(), method: "import" },
            None => Act::None,
        },
        "d" => match cur_account(p) {
            Some((b, a)) => Act::Call {
                server: b.server.clone(),
                method: "disable",
                req: SlotReq { slot: a.slot, on: !a.disabled, ..Default::default() },
            },
            None => Act::None,
        },
        "x" => match cur_account(p) {
            Some((b, a)) => {
                p.confirm = Some(Confirm::Remove {
                    server: b.server.clone(),
                    slot: a.slot,
                    label: format!("{} {}", a.slot, a.email),
                });
                Act::None
            }
            None => Act::None,
        },
        "n" => match cur_account(p) {
            Some((b, a)) => Act::NewAgent { server: b.server.clone(), slot: a.slot },
            None => Act::None,
        },
        k if k.len() == 1 && k.as_bytes()[0].is_ascii_digit() && k != "0" => {
            let n: i64 = k.parse().unwrap_or(0);
            let bi = p.cur.map(|(b, _)| b).unwrap_or(0);
            if p.blocks.get(bi).and_then(|b| b.snap.as_ref()).is_some_and(|s| s.accounts.iter().any(|a| a.slot == n)) {
                p.cur = Some((bi, n));
            }
            Act::None
        }
        _ => Act::None,
    }
}

pub fn on_nav(panel: &Shared, event: &Event) {
    let Some(dir) = event.get_str("dir") else { return };
    let mut b = panel.borrow_mut();
    let Some(p) = b.as_mut() else { return };
    if event.get_i64("mode") != Some(p.mode.0 as i64) {
        return;
    }
    match dir {
        "up" => step(p, -1),
        "down" => step(p, 1),
        _ => {}
    }
    render(p);
}

/// Start a claude as this slot in its own window: the slot's profile
/// directory from the provider, then the agents plugin's new-agent form
/// with the launcher prefixed, so it runs where the picker would run
/// it, with its completion and naming.
async fn new_agent(panel: Shared, server: String, slot: i64) {
    set_busy(&panel, true, "making the profile…");
    let r = call(&server, "profile", &SlotReq { slot, ..Default::default() }).await;
    let Some(dir) = r.dir.filter(|_| r.ok) else {
        set_busy(&panel, false, &r.message);
        return;
    };
    set_busy(&panel, false, &format!("profile {dir}"));
    let client = panel.borrow().as_ref().and_then(|p| p.client);
    close(&panel);
    // The form opens on the pressing client's screen: the command runs
    // from inside a plugin, which has no client of its own, so the
    // client goes by name in the text.
    let mut words = vec!["new".to_string()];
    if server != LOCAL {
        words.push(format!("@{server}"));
    }
    if let Some(name) = client.and_then(client_name) {
        words.push("--client".into());
        words.push(name);
    }
    words.push("--profile".into());
    words.push(dir);
    let cmd = format!("plugin-command agents {}", claude::sq(&words.join(" ")));
    if let Err(e) = run_command(&cmd).await {
        let _ = display_message(&format!("accounts: {}", e.message));
    }
}

fn client_name(id: u64) -> Option<String> {
    list_clients()
        .ok()?
        .into_iter()
        .find(|c| u64::from(c.id) == id)
        .map(|c| c.name)
        .filter(|n| !n.is_empty() && !n.contains(char::is_whitespace))
}

// ---------------------------------------------------------------------------
// render
// ---------------------------------------------------------------------------

fn label_of(b: &Block) -> String {
    match &b.snap {
        Some(s) if b.server == LOCAL => format!("local · {}", s.host),
        Some(s) if !s.host.is_empty() => format!("{} · {}", b.server, s.host),
        _ => b.server.clone(),
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

fn pct_color(pct: f64) -> &'static str {
    if pct >= 100.0 {
        "\x1b[31m"
    } else if pct >= 80.0 {
        "\x1b[33m"
    } else {
        "\x1b[32m"
    }
}

/// `━━━━━━╸─────────` over BAR cells.
fn bar(pct: f64) -> String {
    let full = (pct.clamp(0.0, 100.0) / 100.0 * BAR as f64 * 2.0).round() as usize;
    let (whole, half) = (full / 2, full % 2);
    let mut s = String::new();
    s.push_str(pct_color(pct));
    for _ in 0..whole {
        s.push('━');
    }
    if half == 1 && whole < BAR {
        s.push('╸');
    }
    s.push_str("\x1b[2m");
    for _ in (whole + half)..BAR {
        s.push('─');
    }
    s.push_str("\x1b[0m");
    s
}

fn window_line(name: &str, w: &Window, now: i64, tz: i32) -> String {
    let mut s = format!("{name:<5} {} {:>4.0}%", bar(w.pct), w.pct);
    if let Some(r) = w.resets_ms {
        s.push_str(&format!("  resets {} · {}", timefmt::countdown(r, now), timefmt::clock(r, now, tz)));
    }
    if w.pct >= 100.0 {
        s.push_str("  \x1b[31m(!)\x1b[0m");
    } else if let Some(exp) = w.expected_pct(now) {
        // Spending ahead of the window: more used than a steady spend
        // would have by now, by a margin (cswap's 15 points) so the
        // first hours after a reset do not read as a warning.
        if w.span_ms.is_some_and(|s| s > 6 * 3600 * 1000) && w.pct - exp >= 15.0 {
            s.push_str("  \x1b[33m(ahead of pace)\x1b[0m");
        }
    }
    s
}

/// The lines of one account, without the gutter.
fn account_lines(a: &provider::AccountRow, now: i64, tz: i32, compact: bool, w: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut head = format!("\x1b[1m{}\x1b[0m  {}", a.slot, a.email);
    if !a.org_name.is_empty() {
        head.push_str(&format!("  \x1b[2m[{}]\x1b[0m", clip(&a.org_name, 28)));
    }
    if !a.alias.is_empty() {
        head.push_str(&format!("  \x1b[36m{}\x1b[0m", a.alias));
    }
    if a.active {
        head.push_str("   \x1b[32m● active\x1b[0m");
    }
    if a.disabled {
        head.push_str("   \x1b[2m(held out)\x1b[0m");
    }
    if !a.has_refresh {
        head.push_str("   \x1b[33mno refresh token\x1b[0m");
    }
    if a.usage_fetched_ms > 0 {
        head.push_str(&format!("   \x1b[2m· {}\x1b[0m", timefmt::ago(a.usage_fetched_ms, now)));
    }
    out.push(head);
    match &a.usage {
        Some(u) if compact => {
            let parts: Vec<String> = u
                .windows()
                .iter()
                .map(|(n, w)| format!("{n} {}{:.0}%\x1b[0m", pct_color(w.pct), w.pct))
                .collect();
            out.push(format!("   \x1b[2m{}\x1b[0m", parts.join("  ")));
        }
        Some(u) => {
            for (n, win) in u.windows() {
                out.push(format!("   {}", window_line(&n, win, now, tz)));
            }
            if let Some(sp) = &u.spend {
                out.push(format!(
                    "   \x1b[2mextra {:.2} of {:.2} {} ({:.0}%)\x1b[0m",
                    sp.used, sp.limit, sp.currency, sp.pct
                ));
            }
        }
        None => {}
    }
    if !a.usage_error.is_empty() {
        out.push(format!("   \x1b[33m{}\x1b[0m", clip(&a.usage_error, w.saturating_sub(8))));
    }
    out
}

fn help_lines() -> Vec<&'static str> {
    vec![
        "  j/k  move       Tab  next server     1-9  jump to a slot",
        "  ⏎    switch the login on that server to the slot (asks)",
        "  b    best pick: the slot with the most room (asks)",
        "  a    add the server's current login as a slot (or update it)",
        "  n    start a claude as this slot in its own window (agents form)",
        "  r    refetch every slot's usage now",
        "  d    hold a slot out of best pick / put it back",
        "  x    remove a slot (asks; the login itself is untouched)",
        "  i    import cswap's accounts (macOS Keychain)",
        "  c    compact rows    ?  this    q/Esc  close",
        "",
        "  A switch changes the machine's login: claudes already running",
        "  there move to it at their next token refresh. n is per window.",
    ]
}

pub fn render(p: &mut Panel) {
    let w = p.width as usize;
    let h = p.height as usize;
    let now = now_ms() as i64;
    let mut lines: Vec<(String, bool)> = Vec::new(); // (text, selected)

    if p.help {
        for l in help_lines() {
            lines.push((l.to_string(), false));
        }
    } else {
        for (bi, b) in p.blocks.iter().enumerate() {
            let label = label_of(b);
            let mut head = format!("\x1b[1m{label}\x1b[0m");
            if let Some(s) = &b.snap {
                match &s.live {
                    Some(id) if s.live_slot.is_some() => {
                        head.push_str(&format!("  \x1b[2mlogin:\x1b[0m {}", id.email));
                    }
                    Some(id) => head.push_str(&format!("  \x1b[2mlogin:\x1b[0m {} \x1b[33m(not a slot)\x1b[0m", id.email)),
                    None => head.push_str("  \x1b[2mno login\x1b[0m"),
                }
            }
            if b.fetching {
                head.push_str("  \x1b[2m…\x1b[0m");
            }
            lines.push((format!("\x1b[2m─\x1b[0m {head}"), false));
            if !b.error.is_empty() {
                lines.push((format!("   \x1b[33m{}\x1b[0m", clip(&b.error, w - 4)), false));
            }
            if let Some(s) = &b.snap {
                if !s.warning.is_empty() {
                    lines.push((format!("   \x1b[33m{}\x1b[0m", s.warning), false));
                }
                if s.accounts.is_empty() && !s.warning.contains("plugin-remote-caps") {
                    lines.push((
                        "   \x1b[2mno slots yet: a adds the login here, i imports cswap's\x1b[0m".into(),
                        false,
                    ));
                }
                for a in &s.accounts {
                    let sel = p.cur == Some((bi, a.slot));
                    let gutter = if a.disabled {
                        "\x1b[2m▌\x1b[0m"
                    } else if a.usage.as_ref().and_then(|u| u.headroom()).is_some_and(|hd| hd <= 0.0) {
                        "\x1b[31m▌\x1b[0m"
                    } else if a.usage.as_ref().and_then(|u| u.headroom()).is_some_and(|hd| hd <= 20.0) {
                        "\x1b[33m▌\x1b[0m"
                    } else if a.active {
                        "\x1b[32m▌\x1b[0m"
                    } else {
                        "\x1b[34m▌\x1b[0m"
                    };
                    for (i, l) in account_lines(a, now, p.tz_min, p.compact, w).into_iter().enumerate() {
                        lines.push((format!(" {gutter} {l}"), sel && i == 0));
                    }
                }
            } else if b.error.is_empty() {
                lines.push(("   \x1b[2mfetching…\x1b[0m".into(), false));
            }
            lines.push((String::new(), false));
        }
    }

    // Header and footer frame the scrolling body.
    let body_h = h.saturating_sub(3).max(1);
    // Keep the cursor's line in view.
    if let Some(i) = lines.iter().position(|(_, s)| *s) {
        if i < p.scroll {
            p.scroll = i.saturating_sub(1);
        } else if i + 4 >= p.scroll + body_h {
            p.scroll = (i + 5).saturating_sub(body_h);
        }
    }
    if p.scroll + body_h > lines.len() {
        p.scroll = lines.len().saturating_sub(body_h);
    }

    let mut out = String::from("\x1b[2J\x1b[H");
    let n_acc: usize = p.blocks.iter().filter_map(|b| b.snap.as_ref()).map(|s| s.accounts.len()).sum();
    let n_srv = p.blocks.len();
    let servers = if n_srv > 1 { format!(", {n_srv} servers") } else { String::new() };
    out.push_str(&format!(
        "\x1b[1;1H\x1b[1m accounts\x1b[0m \x1b[2m({n_acc} slots{servers})\x1b[0m"
    ));
    let hint = "? help";
    out.push_str(&format!("\x1b[1;{}H\x1b[2m{hint}\x1b[0m", w.saturating_sub(hint.len())));

    for (row, (text, sel)) in lines.iter().skip(p.scroll).take(body_h).enumerate() {
        let y = row + 3;
        // Every line is cut to the width: a wrapped tail would push the
        // rows below it down by one.
        let text = clip_ansi(text, w);
        if *sel {
            out.push_str(&format!("\x1b[{y};1H\x1b[7m{:<pad$}\x1b[0m", "", pad = w));
            out.push_str(&format!("\x1b[{y};1H\x1b[7m{}", strip_sgr_reset_safe(&text)));
            out.push_str("\x1b[0m");
        } else {
            out.push_str(&format!("\x1b[{y};1H{text}"));
        }
    }

    // The last line: a confirmation, the status, or the key hints.
    let foot = if let Some(c) = &p.confirm {
        match c {
            Confirm::Switch { server, label, .. } => {
                let srv = p.blocks.iter().find(|b| &b.server == server).map(label_of).unwrap_or(server.clone());
                format!("\x1b[33mswitch {srv} to {label}? \x1b[0my/n")
            }
            Confirm::Remove { label, .. } => format!("\x1b[33mremove slot {label}? \x1b[0my/n"),
            Confirm::Best { server } => format!("\x1b[33mswitch {server} to its best pick? \x1b[0my/n"),
        }
    } else if p.busy {
        format!("\x1b[2m{}\x1b[0m", p.status)
    } else if !p.status.is_empty() {
        p.status.clone()
    } else {
        "\x1b[2m⏎ switch  b best  a add login  n new agent  r refresh  d hold  x remove  ? more  esc\x1b[0m".into()
    };
    out.push_str(&format!("\x1b[{h};1H{}", clip_ansi(&foot, w)));
    let _ = mode_write(p.mode, out.as_bytes());
}

/// Inside a reverse-video row, an SGR reset would drop the highlight;
/// turn each `\x1b[0m` into "reset then reverse again".
fn strip_sgr_reset_safe(s: &str) -> String {
    s.replace("\x1b[0m", "\x1b[0m\x1b[7m")
}

/// Clip a line with escapes to `max` visible cells.
fn clip_ansi(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut cells = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            out.push(c);
            for d in chars.by_ref() {
                out.push(d);
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        if cells >= max {
            break;
        }
        out.push(c);
        cells += 1;
    }
    out.push_str("\x1b[0m");
    out
}

/// Which servers currently have blocks (for a `changed` event).
pub fn has_block(panel: &Shared, server: &str) -> bool {
    panel.borrow().as_ref().is_some_and(|p| p.blocks.iter().any(|b| b.server == server))
}
