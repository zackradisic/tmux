//! SDK example plugin: async tick loop + shell jobs + tmux commands.
//!
//! Every `interval_ms` (config, default 1000) it bumps the `@ticker` user
//! option and, every fifth tick, runs a shell job and a tmux command to
//! prove the async paths.
//!
//! Build: cargo build -p ticker --target wasm32-unknown-unknown --release

use serde::Deserialize;
use tmux_plugin_sdk::prelude::*;

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    interval_ms: Option<String>, // -o values arrive as strings
    /// Event name that makes the plugin panic (failure-policy testing).
    #[serde(default)]
    panic_on: Option<String>,
}

struct Ticker {
    events: u64,
    panic_on: Option<String>,
}

impl Plugin for Ticker {
    const NAME: &'static str = "ticker";
    type Config = Config;

    fn init(ctx: &Ctx, config: Config) -> Result<Self, String> {
        let interval: u64 = config
            .interval_ms
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1000);

        ctx.subscribe(&["session-created", "window-linked"])
            .map_err(|e| e.message.clone())?;
        if let Some(ev) = &config.panic_on {
            ctx.subscribe(&[ev.as_str()]).map_err(|e| e.message.clone())?;
        }
        log(&format!("ticker starting, interval {interval}ms"));

        ctx.spawn(async move {
            let mut tick: u64 = 0;
            loop {
                if sleep_ms(interval).await.is_err() {
                    return;
                }
                tick += 1;
                let _ = set_option("@ticker", &tick.to_string());

                if tick % 5 == 0 {
                    match run_job("echo -n job-tick-$$", None).await {
                        Ok(out) => log(&format!(
                            "job ok (status {}): {}",
                            out.status, out.output
                        )),
                        Err(e) => log(&format!("job failed: {}", e.message)),
                    }
                    let _ = run_command(
                        "set-option -g @ticker-cmd yes",
                    )
                    .await;
                }
            }
        });

        Ok(Self { events: 0, panic_on: config.panic_on })
    }

    fn on_event(&mut self, _ctx: &Ctx, event: Event) {
        self.events += 1;
        if self.panic_on.as_deref() == Some(event.event.as_str()) {
            panic!("ticker asked to panic on {}", event.event);
        }
        log(&format!("event {} (#{})", event.event, self.events));
    }

    fn snapshot(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({ "events": self.events }))
    }

    fn restore(_old_version: i32, state: serde_json::Value) -> Option<Self> {
        let events = state.get("events")?.as_u64()?;
        log(&format!("restored with {events} events after reload"));
        Some(Self { events, panic_on: None })
    }

    fn on_unload(&mut self, _ctx: &Ctx) {
        log(&format!("ticker unloading after {} events", self.events));
    }
}

tmux_plugin!(Ticker);
