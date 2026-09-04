//! The picker: a float listing every job with its schedule, next run and
//! last result. Enter runs the highlighted job now, `e` toggles it, `d`
//! deletes it after a y/n confirm, `l` shows the last run's output, Esc
//! closes. A ticker redraws once a second while it is open so the
//! "next" column counts down.

use std::rc::Rc;

use tmux_plugin_sdk::executor;
use tmux_plugin_sdk::prelude::*;

use crate::command::{last_text, next_text};
use crate::scheduler::{self, Ctl};
use crate::store::{self, JobListing, RunRow};
use crate::{runner, schedule, PickKeys};

const WIDTH: u32 = 96;
/// Rows on screen at once; a longer list scrolls.
const LIST_MAX: usize = 12;
/// Title, rule, gap, status, footer.
const CHROME: u32 = 5;
/// Lines of the detail area (header + output tail).
const DETAIL_LINES: usize = 10;

pub struct Picker {
    pub mode: ModeId,
    width: u32,
    height: u32,
    sized: (u32, u32),
    rows: Vec<JobListing>,
    sel: usize,
    top: usize,
    detail: Option<RunRow>,
    show_detail: bool,
    /// Job id waiting for a y/n delete confirm.
    confirm: Option<i64>,
    status: Option<String>,
    keys: PickKeys,
    /// Generation of the ticker task, so a closed picker's ticker exits.
    gen: u64,
}

/// What a key asked for, decided under the borrow and run after it.
enum After {
    None,
    Close(ModeId),
    Run(i64),
    Toggle(i64, bool),
    Delete(i64),
    Detail(i64),
    Refresh,
}

impl Picker {
    fn visible(&self) -> usize {
        self.rows.len().min(LIST_MAX).max(1)
    }

    fn wanted_size(&self) -> (u32, u32) {
        let detail = if self.show_detail { DETAIL_LINES as u32 } else { 0 };
        (self.width, CHROME + self.visible() as u32 + detail)
    }

    fn scroll_to_selection(&mut self) {
        let h = self.rows.len().min(LIST_MAX);
        if h == 0 {
            self.top = 0;
        } else if self.sel < self.top {
            self.top = self.sel;
        } else if self.sel >= self.top + h {
            self.top = self.sel + 1 - h;
        }
    }

    fn selected_id(&self) -> Option<i64> {
        self.rows.get(self.sel).map(|r| r.job.id)
    }

    /// Replace the rows, keeping the highlighted job highlighted.
    fn set_rows(&mut self, rows: Vec<JobListing>) {
        let keep = self.selected_id();
        self.rows = rows;
        self.sel = keep
            .and_then(|id| self.rows.iter().position(|r| r.job.id == id))
            .unwrap_or(0)
            .min(self.rows.len().saturating_sub(1));
        self.scroll_to_selection();
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

fn keyname(k: &str) -> &str {
    match k {
        "Escape" => "Esc",
        other => other,
    }
}

fn render(p: &mut Picker) {
    let want = p.wanted_size();
    if want != p.sized && mode_resize(p.mode, want.0, want.1).is_ok() {
        p.sized = want;
    }
    let w = p.width as usize;
    let now = now_ms() as i64;
    let mut out = String::from("\x1b[2J\x1b[H");
    let enabled = p.rows.iter().filter(|r| r.job.enabled).count();
    out.push_str(&format!(
        "\x1b[1m cron jobs ({})\x1b[0m \x1b[2m{enabled} enabled\x1b[0m\r\n",
        p.rows.len()
    ));
    let rule = "─".repeat(w.saturating_sub(2));
    out.push_str(&format!("  \x1b[2m{rule}\x1b[0m\r\n"));

    if p.rows.is_empty() {
        out.push_str("  \x1b[2m(no jobs; plugin-command cron 'add every 15m -- shell ...')\x1b[0m\r\n");
    } else {
        let vh = p.rows.len().min(LIST_MAX);
        for vi in p.top..(p.top + vh).min(p.rows.len()) {
            let r = &p.rows[vi];
            let cur = vi == p.sel;
            let marker = if cur { "▸" } else { " " };
            let glyph = if r.running {
                "▶"
            } else if r.job.enabled {
                "●"
            } else {
                "○"
            };
            let name = match &r.job.name {
                Some(n) => format!("#{} {n}", r.job.id),
                None => format!("#{}", r.job.id),
            };
            let cmd = format!("{} {}", r.job.kind.as_str(), r.job.command);
            let line = format!(
                "{marker} {glyph} {:<14} {:<16} {:<28} {:<14} {}",
                clip(&name, 14),
                clip(&r.job.schedule, 16),
                clip(&cmd, 28),
                next_text(r, now),
                last_text(r, now),
            );
            let line = clip(&line, w.saturating_sub(1));
            if cur {
                out.push_str(&format!(
                    "\x1b[7m{line:<pad$}\x1b[0m\r\n",
                    pad = w.saturating_sub(1)
                ));
            } else if r.job.enabled {
                out.push_str(&format!("{line}\r\n"));
            } else {
                out.push_str(&format!("\x1b[2m{line}\x1b[0m\r\n"));
            }
        }
    }

    if p.show_detail {
        out.push_str(&format!("  \x1b[2m{rule}\x1b[0m\r\n"));
        match &p.detail {
            None => out.push_str("  \x1b[2mlast run: none yet\x1b[0m\r\n"),
            Some(r) => {
                let when = r
                    .finished_ms
                    .map(|t| schedule::describe_ago(t, now))
                    .unwrap_or_default();
                let detail = match (&r.error, r.exit_code) {
                    (Some(e), _) => e.clone(),
                    (None, Some(c)) if r.signalled => format!("signal {c}"),
                    (None, Some(c)) => format!("exit {c}"),
                    (None, None) => String::new(),
                };
                out.push_str(&format!(
                    "  last run: {} {} in {}, {} ({})\r\n",
                    r.state,
                    detail,
                    schedule::fmt_duration(r.duration_ms.unwrap_or(0).max(0) as u64),
                    when,
                    r.reason
                ));
                let lines: Vec<&str> = r
                    .output
                    .as_deref()
                    .unwrap_or("")
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .collect();
                let skip = lines.len().saturating_sub(DETAIL_LINES - 2);
                for l in &lines[skip..] {
                    out.push_str(&format!("  \x1b[2m│\x1b[0m {}\r\n", clip(l, w - 6)));
                }
            }
        }
    }

    out.push_str("\r\n");
    if let Some(id) = p.confirm {
        let label = p
            .rows
            .iter()
            .find(|r| r.job.id == id)
            .map(|r| r.job.label())
            .unwrap_or_else(|| format!("#{id}"));
        out.push_str(&format!(
            "  \x1b[33mdelete {label} and its runs? y/n\x1b[0m\r\n"
        ));
    } else if let Some(s) = &p.status {
        out.push_str(&format!("  \x1b[36m{}\x1b[0m\r\n", clip(s, w - 4)));
    } else {
        out.push_str("\r\n");
    }
    let k = &p.keys;
    let footer = format!(
        "{} run · {} del · {} toggle · {} detail · r refresh · {} close",
        keyname(&k.run),
        keyname(&k.delete),
        keyname(&k.toggle),
        keyname(&k.detail),
        keyname(&k.close),
    );
    out.push_str(&format!("  \x1b[2m{}\x1b[0m", clip(&footer, w - 4)));
    let _ = mode_write(p.mode, out.as_bytes());
}

/// Open the picker in the pressing client's current window.
pub async fn open(sh: Ctl, client: Option<u32>) {
    let window = client
        .and_then(|cid| list_clients().ok()?.into_iter().find(|c| c.id == cid))
        .and_then(|c| c.session)
        .and_then(|s| resolve_session(SessionId(s)).ok())
        .and_then(|v| v.current_window);
    let Some(window) = window else {
        let _ = display_message("cron: no client to open the picker");
        return;
    };
    let rows = store::list_jobs().await.unwrap_or_default();
    let height = CHROME + rows.len().min(LIST_MAX).max(1) as u32;
    let mode = match mode_open(&ModeOpts {
        window: Some(WindowId(window)),
        width: WIDTH,
        height,
        title: Some("cron".into()),
        ..Default::default()
    }) {
        Ok(m) => m,
        Err(e) => {
            let _ = display_message(&format!("cron: cannot open picker: {}", e.message));
            return;
        }
    };
    let gen = sh.picker.borrow().as_ref().map_or(1, |p| p.gen + 1);
    let mut p = Picker {
        mode,
        width: WIDTH,
        height,
        sized: (WIDTH, height),
        rows,
        sel: 0,
        top: 0,
        detail: None,
        show_detail: false,
        confirm: None,
        status: None,
        keys: sh.cfg.keys.clone(),
        gen,
    };
    render(&mut p);
    *sh.picker.borrow_mut() = Some(p);
    executor::spawn(ticker(sh, gen));
}

/// Redraw once a second while the picker is open.
async fn ticker(sh: Ctl, gen: u64) {
    loop {
        if sleep_ms(1000).await.is_err() {
            return;
        }
        let open = sh.picker.borrow().as_ref().is_some_and(|p| p.gen == gen);
        if !open {
            return;
        }
        reload_inner(&sh).await;
    }
}

/// Re-query and redraw, if the picker is open.
pub fn refresh(sh: &Ctl) {
    if sh.picker.borrow().is_some() {
        executor::spawn(reload(Rc::clone(sh)));
    }
}

async fn reload_inner(sh: &Ctl) {
    if sh.picker.borrow().is_none() {
        return;
    }
    let rows = store::list_jobs().await.unwrap_or_default();
    let want_detail = {
        let b = sh.picker.borrow();
        b.as_ref().filter(|p| p.show_detail).and_then(|p| p.selected_id())
    };
    let detail = match want_detail {
        Some(id) => store::last_run(id).await.ok().flatten(),
        None => None,
    };
    let mut b = sh.picker.borrow_mut();
    if let Some(p) = b.as_mut() {
        p.set_rows(rows);
        if p.show_detail {
            p.detail = detail;
        }
        render(p);
    }
}

async fn reload(sh: Ctl) {
    reload_inner(&sh).await;
}

/// The mode-resize event: the host settled the float's size.
pub fn on_resize(sh: &Ctl, event: &Event) {
    let mut b = sh.picker.borrow_mut();
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
    p.sized = (p.width, p.height);
    render(p);
}

pub fn on_closed(sh: &Ctl, event: &Event) {
    let mut b = sh.picker.borrow_mut();
    if b.as_ref().is_some_and(|p| event.get_i64("mode") == Some(p.mode.0 as i64)) {
        *b = None;
    }
}

/// A key inside the float.
pub fn on_key(sh: &Ctl, event: &Event) {
    let mode_id = event.get_i64("mode");
    let key = event.get_str("key").unwrap_or("").to_string();
    let mut after = After::None;
    {
        let mut b = sh.picker.borrow_mut();
        let Some(p) = b.as_mut() else { return };
        if mode_id != Some(p.mode.0 as i64) {
            return;
        }
        p.status = None;
        if let Some(id) = p.confirm {
            match key.as_str() {
                "y" | "Y" => {
                    p.confirm = None;
                    after = After::Delete(id);
                }
                "n" | "N" | "Escape" => {
                    p.confirm = None;
                    render(p);
                }
                _ => {}
            }
        } else if key == p.keys.close {
            after = After::Close(p.mode);
        } else if key == p.keys.run {
            if let Some(id) = p.selected_id() {
                after = After::Run(id);
            }
        } else if key == p.keys.delete {
            if p.selected_id().is_some() {
                p.confirm = p.selected_id();
                render(p);
            }
        } else if key == p.keys.toggle {
            if let Some(r) = p.rows.get(p.sel) {
                after = After::Toggle(r.job.id, !r.job.enabled);
            }
        } else if key == p.keys.detail {
            p.show_detail = !p.show_detail;
            match p.selected_id() {
                Some(id) if p.show_detail => after = After::Detail(id),
                _ => render(p),
            }
        } else {
            match key.as_str() {
                "Down" | "j" | "C-n" | "C-j" => {
                    if !p.rows.is_empty() {
                        p.sel = (p.sel + 1).min(p.rows.len() - 1);
                        p.scroll_to_selection();
                        if p.show_detail {
                            after = After::Detail(p.selected_id().unwrap_or(0));
                        } else {
                            render(p);
                        }
                    }
                }
                "Up" | "k" | "C-p" | "C-k" => {
                    p.sel = p.sel.saturating_sub(1);
                    p.scroll_to_selection();
                    if p.show_detail {
                        after = After::Detail(p.selected_id().unwrap_or(0));
                    } else {
                        render(p);
                    }
                }
                "r" => after = After::Refresh,
                _ => {}
            }
        }
    }
    let sh = Rc::clone(sh);
    match after {
        After::None => {}
        After::Close(mode) => {
            let _ = mode_close(mode);
        }
        After::Refresh => refresh(&sh),
        After::Detail(id) => {
            executor::spawn(async move {
                let run = store::last_run(id).await.ok().flatten();
                let mut b = sh.picker.borrow_mut();
                if let Some(p) = b.as_mut() {
                    p.detail = run;
                    render(p);
                }
            });
        }
        After::Run(id) => {
            executor::spawn(async move {
                let now = now_ms() as i64;
                if sh.running.borrow().contains(&id) {
                    set_status(&sh, format!("#{id} is already running"));
                    return;
                }
                let job = match store::find_job_by_id(id).await {
                    Ok(Some(j)) => j,
                    _ => return,
                };
                match store::insert_run(id, "manual", now, now).await {
                    Ok(run_id) => {
                        sh.running.borrow_mut().insert(id);
                        set_status(&sh, format!("running {} now", job.label()));
                        executor::spawn(runner::execute(Rc::clone(&sh), job, run_id, 1));
                        reload_inner(&sh).await;
                    }
                    Err(e) => set_status(&sh, format!("run failed: {}", e.message)),
                }
            });
        }
        After::Toggle(id, enabled) => {
            executor::spawn(async move {
                let now = now_ms() as i64;
                let off = sh.tz_offset_min.get();
                let job = match store::find_job_by_id(id).await {
                    Ok(Some(j)) => j,
                    _ => return,
                };
                let next = if enabled {
                    schedule::parse(&job.schedule)
                        .ok()
                        .and_then(|s| schedule::next_after(&s, now, now, off))
                } else {
                    None
                };
                match store::set_enabled(id, enabled, next, now).await {
                    Ok(()) => {
                        scheduler::kick(&sh);
                        set_status(
                            &sh,
                            format!(
                                "{} {}",
                                job.label(),
                                if enabled { "enabled" } else { "disabled" }
                            ),
                        );
                    }
                    Err(e) => set_status(&sh, format!("toggle failed: {}", e.message)),
                }
                reload_inner(&sh).await;
            });
        }
        After::Delete(id) => {
            executor::spawn(async move {
                match store::delete_job(id).await {
                    Ok(dropped) => {
                        sh.running.borrow_mut().remove(&id);
                        scheduler::kick(&sh);
                        set_status(&sh, format!("deleted #{id} ({dropped} runs dropped)"));
                    }
                    Err(e) => set_status(&sh, format!("delete failed: {}", e.message)),
                }
                reload_inner(&sh).await;
            });
        }
    }
}

fn set_status(sh: &Ctl, msg: String) {
    let mut b = sh.picker.borrow_mut();
    if let Some(p) = b.as_mut() {
        p.status = Some(msg);
        render(p);
    }
}
