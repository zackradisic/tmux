//! Scheduled jobs for tmux, durable in the plugin's SQLite database.
//!
//! Verbs. The whole verb line is ONE tmux argument, so quote it:
//!
//!   plugin-command cron 'add [-n NAME] [--catchup skip|once|each] [--cwd DIR]
//!                        <every N<unit> | 5 cron fields> -- <shell|tmux> <command...>'
//!   plugin-command cron 'rm <id|name>'
//!   plugin-command cron ls
//!   plugin-command cron 'run <id|name>'          # start it now
//!   plugin-command cron 'enable <id|name>' / 'disable <id|name>'
//!   plugin-command cron 'last <id|name>'         # newest finished run
//!   plugin-command cron status
//!   plugin-command cron pick                     # the picker (needs -c mode)
//!
//! Schedules: `every 15m` (units ms/s/m/h/d, 1 s floor; ticks stay on a
//! fixed grid from the job's creation) or five numeric cron fields
//! `minute hour day-of-month month day-of-week` with `*`, lists, ranges
//! and `/step`, evaluated in local time (`tz = "utc"` to change that).
//! Day-of-month and day-of-week both restricted means either matches.
//!
//! Actions: `shell <command>` runs through the shell (`run_job`), ok iff
//! it exits 0; `tmux <command>` runs a tmux command through the command
//! queue. NOTE: a tmux command that runs and fails still records `ok`,
//! because only parse errors reach the plugin. To capture a status, use
//! `shell tmux -S #{socket_path} ...` instead.
//!
//! Every occurrence is a row in `runs`: pending (a retry waiting),
//! running, ok, failed, interrupted. The database is the state: there is
//! no in-memory schedule and no snapshot/restore. After a crash,
//! `kill-server` or `restart-server`, `init` marks rows still `running`
//! as `interrupted` (the plugin lost track of them; the process may have
//! finished) and then applies each overdue job's catch-up policy once:
//! `skip` just advances; `once` (default) runs the latest missed
//! occurrence; `each` runs every missed occurrence (at most 50) in order.
//! While the server is up, a late job simply runs once. A failed run is
//! retried `retry_max` times (default 2) with doubling `retry_backoff`
//! (default 30s); pending retries survive restarts. `interrupted` is
//! terminal. Each run keeps its exit code, duration and the last
//! `max_output_bytes` (default 4096) of output; runs are kept for
//! `keep_days` (14) and at most `keep_runs` (50) per job.
//!
//! Picker keys: Enter runs the highlighted job now, `d` deletes it (y/n),
//! `e` toggles enabled, `l` shows the last run's output, `r` refreshes,
//! j/k or the arrows move, Esc closes. Configurable as pick_run,
//! pick_delete, pick_toggle, pick_detail, pick_close.
//!
//! Manifest:
//!
//!   [plugins.cron]
//!   path  = "cron.wasm"
//!   caps  = ["db", "run-process", "run-command", "mode"]
//!   config = { keep_days = 14, keep_runs = 50, retry_max = 2,
//!              retry_backoff = "30s", tz = "local", notify_failures = true }
//!
//! First real use: schedule resurrect's autosave from here:
//!   plugin-command cron 'add -n resurrect every 1h -- tmux plugin-command resurrect save'

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use serde::Deserialize;
use tmux_plugin_sdk::prelude::*;

pub mod command;
pub mod picker;
pub mod runner;
pub mod schedule;
pub mod scheduler;
pub mod store;

use scheduler::{Ctl, Shared};

#[derive(Deserialize, Default)]
#[serde(default)]
struct CronConfig {
    keep_days: Option<serde_json::Value>,
    keep_runs: Option<serde_json::Value>,
    max_output_bytes: Option<serde_json::Value>,
    retry_max: Option<serde_json::Value>,
    retry_backoff: Option<serde_json::Value>,
    max_sleep: Option<serde_json::Value>,
    tz: Option<serde_json::Value>,
    notify_failures: Option<serde_json::Value>,
    pick_run: Option<String>,
    pick_delete: Option<String>,
    pick_toggle: Option<String>,
    pick_detail: Option<String>,
    pick_close: Option<String>,
}

#[derive(Clone)]
pub struct PickKeys {
    pub run: String,
    pub delete: String,
    pub toggle: String,
    pub detail: String,
    pub close: String,
}

impl Default for PickKeys {
    fn default() -> Self {
        Self {
            run: "Enter".into(),
            delete: "d".into(),
            toggle: "e".into(),
            detail: "l".into(),
            close: "Escape".into(),
        }
    }
}

/// The normalised configuration.
pub struct Config {
    pub keep_days: i64,
    pub keep_runs: i64,
    pub max_output_bytes: usize,
    pub retry_max: i64,
    pub retry_backoff_ms: i64,
    pub max_sleep_ms: i64,
    pub utc: bool,
    pub notify_failures: bool,
    pub keys: PickKeys,
}

/// A manifest value: TOML lets the user write "5m" or plain 300.
fn cfg_str(v: &serde_json::Value) -> String {
    match v.as_str() {
        Some(s) => s.to_string(),
        None => v.to_string(),
    }
}

fn cfg_int(v: &Option<serde_json::Value>, name: &str, default: i64, min: i64) -> Result<i64, String> {
    match v {
        None => Ok(default),
        Some(v) => {
            let n: i64 = cfg_str(v)
                .trim()
                .parse()
                .map_err(|_| format!("bad {name} {v}"))?;
            if n < min {
                return Err(format!("{name} must be at least {min}"));
            }
            Ok(n)
        }
    }
}

fn cfg_bool(v: &Option<serde_json::Value>, name: &str, default: bool) -> Result<bool, String> {
    match v {
        None => Ok(default),
        Some(v) => match cfg_str(v).trim() {
            "1" | "true" | "on" | "yes" => Ok(true),
            "0" | "false" | "off" | "no" => Ok(false),
            other => Err(format!("bad {name} {other:?}")),
        },
    }
}

fn cfg_period(v: &Option<serde_json::Value>, name: &str, default_ms: i64) -> Result<i64, String> {
    match v {
        None => Ok(default_ms),
        Some(v) => schedule::parse_every(&cfg_str(v))
            .map(|ms| ms as i64)
            .map_err(|e| format!("bad {name}: {e}")),
    }
}

impl Config {
    fn from(c: &CronConfig) -> Result<Config, String> {
        let d = PickKeys::default();
        let pick = |v: &Option<String>, def: String| {
            v.as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or(def)
        };
        let utc = match &c.tz {
            None => false,
            Some(v) => match cfg_str(v).trim() {
                "local" => false,
                "utc" | "UTC" => true,
                other => return Err(format!("bad tz {other:?} (local|utc)")),
            },
        };
        Ok(Config {
            keep_days: cfg_int(&c.keep_days, "keep_days", 14, 1)?,
            keep_runs: cfg_int(&c.keep_runs, "keep_runs", 50, 1)?,
            max_output_bytes: cfg_int(&c.max_output_bytes, "max_output_bytes", 4096, 0)? as usize,
            retry_max: cfg_int(&c.retry_max, "retry_max", 2, 0)?,
            retry_backoff_ms: cfg_period(&c.retry_backoff, "retry_backoff", 30_000)?,
            max_sleep_ms: cfg_period(&c.max_sleep, "max_sleep", 60_000)?,
            utc,
            notify_failures: cfg_bool(&c.notify_failures, "notify_failures", true)?,
            keys: PickKeys {
                run: pick(&c.pick_run, d.run),
                delete: pick(&c.pick_delete, d.delete),
                toggle: pick(&c.pick_toggle, d.toggle),
                detail: pick(&c.pick_detail, d.detail),
                close: pick(&c.pick_close, d.close),
            },
        })
    }
}

struct Cron {
    sh: Ctl,
}

impl Plugin for Cron {
    const NAME: &'static str = "cron";
    type Config = CronConfig;

    fn init(ctx: &Ctx, config: Self::Config) -> Result<Self, String> {
        let cfg = Config::from(&config)?;
        ctx.subscribe(&["plugin-command"]).map_err(|e| e.message.clone())?;
        store::migrate_sync()?;
        let interrupted = store::mark_interrupted_sync(now_ms() as i64)?;
        if interrupted > 0 {
            log(&format!("cron: {interrupted} run(s) interrupted by the last server stop"));
        }
        let sh: Ctl = Rc::new(Shared {
            cfg,
            running: RefCell::new(HashSet::new()),
            loop_task: Cell::new(None),
            sleeping: Cell::new(false),
            wake: Cell::new(false),
            tz_offset_min: Cell::new(0),
            picker: RefCell::new(None),
        });
        scheduler::start(&sh);
        Ok(Self { sh })
    }

    fn on_event(&mut self, _ctx: &Ctx, event: Event) {
        match event.name().as_str() {
            "plugin-command" => self.on_command(&event),
            "mode-key" => picker::on_key(&self.sh, &event),
            "mode-resize" => picker::on_resize(&self.sh, &event),
            "mode-closed" => picker::on_closed(&self.sh, &event),
            _ => {}
        }
    }
}

impl Cron {
    fn on_command(&mut self, event: &Event) {
        let line = event.get_str("text").unwrap_or("").to_string();
        let client = event.scope.client;
        let cmd = match command::parse(&line) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("cron: {e}");
                let sent = client
                    .map(|c| display_message_to(ClientId(c), msg.as_str()).is_ok())
                    .unwrap_or(false);
                if !sent {
                    let _ = display_message(msg.as_str());
                }
                return;
            }
        };
        if let command::Cmd::Pick = cmd {
            if self.sh.picker.borrow().is_some() {
                let _ = display_message("cron: picker already open");
                return;
            }
            tmux_plugin_sdk::executor::spawn(picker::open(Rc::clone(&self.sh), client));
            return;
        }
        tmux_plugin_sdk::executor::spawn(command::run(
            Rc::clone(&self.sh),
            cmd,
            client.map(ClientId),
        ));
    }
}

tmux_plugin!(Cron);
