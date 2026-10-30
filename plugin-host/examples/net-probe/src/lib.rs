//! SDK example plugin for the `net` capability: one HTTP GET and one
//! websocket round trip, driven from the command line.
//!
//!   plugin-command net-probe "get <url>"   fetch; answer the status and
//!                                          the first body line
//!   plugin-command net-probe "ws <url>"    open, send "hello", report the
//!                                          first frame back, close
//!
//! The command is answered on the caller's terminal with `reply` at once
//! (the request has been started); the result arrives later, so it goes
//! to the status line and into the `@net_probe` server option, where a
//! script (regress/plugin-net.sh) can wait for it: `<status> <first
//! line>`, `frame <text>` or `error: <code> <message>`. A close lands in
//! `@net_probe_closed` as `<code> <reason>`.
//!
//! Needs `-c net -c write-options`.
//! Build: cargo build -p net-probe --target wasm32-unknown-unknown --release

use tmux_plugin_sdk::prelude::*;

struct NetProbe;

const RESULT: &str = "@net_probe";
const CLOSED: &str = "@net_probe_closed";

fn report(text: String) {
    let _ = set_option(RESULT, &text);
    let _ = display_message(&format!("net-probe: {text}"));
}

fn failed(e: &HostError) -> String {
    format!("error: {} {}", e.code.name(), e.message)
}

impl Plugin for NetProbe {
    const NAME: &'static str = "net-probe";
    type Config = serde_json::Value;

    fn init(ctx: &Ctx, _config: serde_json::Value) -> Result<Self, String> {
        ctx.subscribe(&["plugin-command"]).map_err(|e| e.message.clone())?;
        Ok(Self)
    }

    fn on_event(&mut self, ctx: &Ctx, event: Event) {
        if let Some(m) = WsMessage::from_event(&event) {
            report(format!("frame {}", m.as_text()));
            let _ = ws_close(m.socket);
            return;
        }
        if let Some(c) = WsClosed::from_event(&event) {
            let _ = set_option(CLOSED, &format!("{} {}", c.code, c.reason));
            log(&format!("{} closed: {} {}", c.socket, c.code, c.reason));
            return;
        }
        if !event.is("plugin-command") {
            return;
        }
        let text = event.get_str("text").unwrap_or("").trim().to_string();
        let (verb, arg) = text.split_once(' ').unwrap_or((text.as_str(), ""));
        let url = arg.trim().to_string();
        match verb {
            "get" if !url.is_empty() => {
                let _ = reply(&format!("net-probe: GET {url}"));
                let _ = set_option(RESULT, "pending");
                ctx.spawn(async move {
                    match http_request(&HttpRequest::get(&url)).await {
                        Ok(r) => {
                            let first = r.text().lines().next().unwrap_or("").to_string();
                            report(format!("{} {first}", r.status));
                        }
                        Err(e) => report(failed(&e)),
                    }
                });
            }
            "ws" if !url.is_empty() => {
                let _ = reply(&format!("net-probe: ws {url}"));
                let _ = set_option(RESULT, "pending");
                let _ = set_option(CLOSED, "");
                ctx.spawn(async move {
                    match ws_open(&url, &[]).await {
                        Ok(ws) => {
                            if let Err(e) = ws_send_text(ws, "hello") {
                                report(failed(&e));
                            } else {
                                log(&format!("{ws} open, hello sent"));
                            }
                        }
                        Err(e) => report(failed(&e)),
                    }
                });
            }
            _ => {
                let _ = reply("usage: net-probe get <url> | ws <url>");
            }
        }
    }
}

tmux_plugin!(NetProbe);
