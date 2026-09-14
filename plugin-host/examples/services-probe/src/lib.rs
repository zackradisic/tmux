//! SDK example plugin: the smallest provider/view pair, for the services
//! regress test and as a template.
//!
//! The provider half (role `provider` or `both`) registers `echo`, which
//! answers `<role>:<payload>`, and publishes a `tick` topic every 300 ms.
//! The view half (role `view` or `both`) calls `echo` on the local
//! provider at init and on every server that comes up, follows `tick`
//! everywhere, and writes what it sees into server user options a test
//! can read:
//!
//!   @probe_role              this instance's role
//!   @probe_echo_<server>     the echo reply from that server
//!   @probe_err_<server>      the error code of a call to a down server
//!   @probe_tick_<server>     the last tick sequence from that server
//!   @probe_up_<server>       how often the server's link came up
//!   @probe_down_<server>     how often it went down
//!   @probe_accept_<server>   the view's verdict on that server's copy
//!
//! Config `reject_peer = "1"` makes the view half reject every provider
//! copy, so a test can drive the E_VERSION path with one build.
//!
//! Build: cargo build -p services_probe --target wasm32-unknown-unknown --release

use tmux_plugin_sdk::prelude::*;

const NAME: &str = "services_probe";

struct Probe {
    role: Role,
    ups: u64,
    downs: u64,
    reject_peer: bool,
}

#[derive(serde::Deserialize, Default)]
struct Config {
    #[serde(default)]
    reject_peer: String,
}

fn store(name: &str, value: &str) {
    if let Err(e) = set_option(name, value) {
        log(&format!("set {name}: {}", e.message));
    }
}

/// Call echo on one server and record the reply or the error code.
fn probe_echo(server: String) {
    spawn(async move {
        let target = if server == "local" {
            NAME.to_string()
        } else {
            format!("{NAME}@{server}")
        };
        match service::call(&target, "echo", b"ping").await {
            Ok(reply) => store(
                &format!("@probe_echo_{server}"),
                &String::from_utf8_lossy(&reply),
            ),
            Err(e) => store(&format!("@probe_err_{server}"), e.code.name()),
        }
    });
}

impl Plugin for Probe {
    const NAME: &'static str = NAME;
    type Config = Config;

    fn init(ctx: &Ctx, config: Self::Config) -> Result<Self, String> {
        let role = ctx.role();
        store("@probe_role", &role.to_string());

        if role.provides() {
            service::register("echo").map_err(|e| e.message.clone())?;
            ctx.spawn(async {
                let mut n: u64 = 0;
                loop {
                    if sleep_ms(300).await.is_err() {
                        return;
                    }
                    n += 1;
                    let _ = service::emit_json("tick", &n);
                }
            });
        }

        if role.views() {
            ctx.subscribe(&["link-up", "link-down"])
                .map_err(|e| e.message.clone())?;
            service::subscribe(NAME, "tick").map_err(|e| e.message.clone())?;
            probe_echo("local".to_string());
            // Servers already linked when the plugin loaded.
            for s in service::servers().unwrap_or_default() {
                if !s.local && s.up {
                    let _ = service::subscribe(&format!("{NAME}@{}", s.name), "tick");
                    probe_echo(s.name.clone());
                }
            }
        }
        Ok(Self { role, ups: 0, downs: 0, reject_peer: config.reject_peer == "1" })
    }

    fn accepts_provider(&self, _ctx: &Ctx, server: &str, theirs: Version) -> bool {
        let ok = !self.reject_peer && Self::service_version().compatible(theirs);
        store(&format!("@probe_accept_{server}"), if ok { "1" } else { "0" });
        ok
    }

    fn on_event(&mut self, _ctx: &Ctx, event: Event) {
        let Some(server) = event.get_str("server").map(str::to_string) else {
            return;
        };
        if event.is("link-up") {
            self.ups += 1;
            store(&format!("@probe_up_{server}"), &self.ups.to_string());
            let _ = service::subscribe(&format!("{NAME}@{server}"), "tick");
            probe_echo(server);
        } else if event.is("link-down") {
            self.downs += 1;
            store(&format!("@probe_down_{server}"), &self.downs.to_string());
            // A call to a down server must fail at once.
            probe_echo(server);
        }
    }

    fn on_service_request(&mut self, _ctx: &Ctx, req: ServiceRequest) {
        if req.method == "echo" {
            let reply = format!("{}:{}", self.role, String::from_utf8_lossy(&req.payload));
            let _ = req.reply(reply.as_bytes());
        } else {
            let _ = req.fail("unknown method");
        }
    }

    fn on_service_event(&mut self, _ctx: &Ctx, event: ServiceEvent) {
        if event.topic == "tick" {
            store(&format!("@probe_tick_{}", event.server), &event.seq.to_string());
        }
    }
}

tmux_plugin!(Probe);
