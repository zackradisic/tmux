//! SDK for writing tmux plugins in Rust (compiled to
//! wasm32-unknown-unknown).
//!
//! ```ignore
//! use tmux_plugin_sdk::prelude::*;
//!
//! struct Hello { seen: u64 }
//!
//! impl Plugin for Hello {
//!     const NAME: &'static str = "hello";
//!     type Config = serde_json::Value;
//!
//!     fn init(ctx: &Ctx, _config: Self::Config) -> Result<Self, String> {
//!         ctx.subscribe(&["session-created"])?;
//!         ctx.spawn(async {
//!             loop {
//!                 sleep_ms(30_000).await.ok();
//!                 let out = run_job("git status --porcelain", None).await;
//!                 if let Ok(o) = out {
//!                     if !o.output.is_empty() {
//!                         let _ = display_message("git: dirty");
//!                     }
//!                 }
//!             }
//!         });
//!         Ok(Self { seen: 0 })
//!     }
//!
//!     fn on_event(&mut self, _ctx: &Ctx, event: Event) {
//!         self.seen += 1;
//!         let _ = display_message(&format!("{} #{}", event.name(), self.seen));
//!     }
//! }
//!
//! tmux_plugin!(Hello);
//! ```

pub mod api;
pub mod event;
pub mod executor;
pub mod ids;
pub mod runtime;
pub mod strings;

pub use api::*;
pub use event::Event;
pub use ids::*;
pub use strings::{AsTmuxStr, TmuxString};
pub use tmux_plugin_abi as abi;
pub use tmux_plugin_abi::{
    ClientInfo, EventScope, HostError, PaneInfo, SelfInfo, SessionInfo,
    WindowInfo,
};

pub mod prelude {
    pub use crate::api::*;
    pub use crate::event::Event;
    pub use crate::ids::*;
    pub use crate::strings::{AsTmuxStr, TmuxString};
    pub use crate::tmux_plugin;
    pub use crate::{params, Ctx, Plugin};
    pub use tmux_plugin_abi::db::{DbValue, ExecResult, Row, Rows};
    pub use tmux_plugin_abi::{
        ClientInfo, EventScope, HostError, PaneInfo, SelfInfo, SessionInfo,
        WindowInfo,
    };
}

/// Build the bound-parameter slice for the `db_*` calls from any values
/// that convert into [`DbValue`](tmux_plugin_abi::db::DbValue): integers,
/// bools, f64, `&str`/`String`, byte vectors and `Option`s of those
/// (`None` binds NULL).
///
/// ```ignore
/// db_exec("INSERT INTO jobs (name, every_ms) VALUES (?1, ?2)",
///         params!["backup", 3_600_000i64]).await?;
/// ```
#[macro_export]
macro_rules! params {
    () => {
        &[] as &[$crate::abi::db::DbValue]
    };
    ($($v:expr),+ $(,)?) => {
        &[$($crate::abi::db::DbValue::from($v)),+] as &[$crate::abi::db::DbValue]
    };
}

/// Re-exports used by the `tmux_plugin!` macro expansion; not public API.
#[doc(hidden)]
pub mod __internal {
    pub use crate::event;
    pub use crate::executor;
    pub use crate::runtime;
    pub use serde_json;
    pub use tmux_plugin_abi::ABI_VERSION;
}

/// A tmux plugin. One value of the implementing type exists per plugin
/// instance (per pane/window/session/server, depending on the declared
/// scope); it is created in `init` and dropped at unload.
pub trait Plugin: Sized + 'static {
    const NAME: &'static str;

    /// Bump when the shape returned by [`Plugin::snapshot`] changes;
    /// [`Plugin::restore`] receives the old version on code reload.
    const STATE_VERSION: i32 = 1;

    /// Configuration shape (from `load-plugin -o key=value ...`). Use
    /// `serde_json::Value` if you don't care. (Config crosses the ABI as
    /// a binary field block; the SDK decodes it into JSON for serde.)
    type Config: serde::de::DeserializeOwned + Default;

    fn init(ctx: &Ctx, config: Self::Config) -> Result<Self, String>;

    /// Called for subscribed events and implicit lifecycle events.
    fn on_event(&mut self, _ctx: &Ctx, _event: Event) {}

    /// State to carry across a code reload. `None` (the default) means the
    /// plugin is stateless: reloads simply re-init. The bytes are opaque
    /// to the host; serde_json is a fine choice.
    fn snapshot(&self) -> Option<serde_json::Value> {
        None
    }

    /// Rebuild from a previous version's snapshot after a code reload.
    /// Runs after `init`: `fresh` is the instance init just built from the
    /// CURRENT config — take config-derived fields from it and only the
    /// carried state from `state` (a snapshot must never override config).
    /// The returned value replaces `fresh`; `None` refuses the state,
    /// keeping the OLD code running.
    fn restore(
        _fresh: Self,
        _old_version: i32,
        _state: serde_json::Value,
    ) -> Option<Self> {
        None
    }

    /// Absorb a config change without a restart; return false (the
    /// default) to be restarted with the new config instead.
    fn on_config_changed(&mut self, _ctx: &Ctx, _config: Self::Config) -> bool {
        false
    }

    /// Called just before the instance is destroyed (tiny CPU budget - do
    /// not do real work here).
    fn on_unload(&mut self, _ctx: &Ctx) {}
}

/// Handle to the plugin runtime, passed to trait callbacks. All methods are
/// also available as free functions in [`api`]; the type exists so
/// callback signatures have an anchor for future context (and to keep
/// plugin code explicit about talking to tmux).
#[derive(Clone, Copy)]
pub struct Ctx(());

impl Ctx {
    #[doc(hidden)]
    pub fn new() -> Self {
        Ctx(())
    }

    pub fn subscribe(&self, events: &[&str]) -> Result<(), HostError> {
        api::subscribe(events)
    }

    /// Spawn a detached task, returning a handle. Pass the handle to
    /// [`Ctx::cancel`] to stop the task before it finishes.
    pub fn spawn(
        &self,
        fut: impl std::future::Future<Output = ()> + 'static,
    ) -> executor::TaskId {
        executor::spawn(fut)
    }

    /// Stop a task started by [`Ctx::spawn`]. A pending sleep is
    /// cancelled host-side; any other host operation in flight has its
    /// completion discarded when it arrives.
    pub fn cancel(&self, id: executor::TaskId) {
        executor::cancel(id);
    }

    pub fn log(&self, msg: &str) {
        runtime::log(1, msg);
    }

    pub fn display_message(&self, msg: &str) -> Result<(), HostError> {
        api::display_message(msg)
    }
}

impl Default for Ctx {
    fn default() -> Self {
        Self::new()
    }
}

/// Register a [`Plugin`] implementation as this wasm module's plugin,
/// generating the ABI exports.
#[macro_export]
macro_rules! tmux_plugin {
    ($ty:ty) => {
        mod __tmux_plugin_glue {
            use super::*;
            use $crate::__internal::{event, executor, runtime, serde_json};

            std::thread_local! {
                static PLUGIN: std::cell::RefCell<Option<$ty>> =
                    const { std::cell::RefCell::new(None) };
            }

            #[no_mangle]
            pub extern "C" fn pgh_abi_version() -> i32 {
                $crate::__internal::ABI_VERSION
            }

            #[no_mangle]
            pub extern "C" fn pgh_state_version() -> i32 {
                <$ty as $crate::Plugin>::STATE_VERSION
            }

            #[no_mangle]
            pub extern "C" fn pgh_snapshot(
                out_ptr_ptr: i32,
                out_len_ptr: i32,
            ) -> i32 {
                let state = PLUGIN.with(|p| {
                    p.borrow().as_ref().and_then($crate::Plugin::snapshot)
                });
                let Some(state) = state else { return 1 };
                let Ok(bytes) = serde_json::to_vec(&state) else { return 1 };
                let (ptr, len) = runtime::give_buf(&bytes);
                unsafe {
                    runtime::write_u32_slot(out_ptr_ptr, ptr as u32);
                    runtime::write_u32_slot(out_len_ptr, len as u32);
                }
                0
            }

            #[no_mangle]
            pub extern "C" fn pgh_migrate(
                old_version: i32,
                ptr: i32,
                len: i32,
            ) -> i32 {
                let bytes = runtime::take_buf(ptr, len);
                let Ok(state) = serde_json::from_slice(&bytes) else {
                    return 1;
                };
                // init already ran with the current config; hand that
                // fresh instance to restore so config always wins over
                // snapshot state.
                let Some(fresh) = PLUGIN.with(|p| p.borrow_mut().take())
                else {
                    return 1;
                };
                match <$ty as $crate::Plugin>::restore(fresh, old_version, state) {
                    Some(plugin) => {
                        PLUGIN.with(|p| *p.borrow_mut() = Some(plugin));
                        executor::run_until_stalled();
                        0
                    }
                    None => 1,
                }
            }

            #[no_mangle]
            pub extern "C" fn pgh_on_config_changed(ptr: i32, len: i32) -> i32 {
                let bytes = runtime::take_buf(ptr, len);
                let value = event::config_value(&bytes);
                let config: <$ty as $crate::Plugin>::Config =
                    match serde_json::from_value(value) {
                        Ok(c) => c,
                        Err(_) => return 0, // restart me
                    };
                let ctx = $crate::Ctx::new();
                let absorbed = PLUGIN.with(|p| {
                    p.borrow_mut().as_mut().map(|plugin| {
                        $crate::Plugin::on_config_changed(plugin, &ctx, config)
                    })
                });
                let absorbed = absorbed.unwrap_or(false);
                executor::run_until_stalled();
                i32::from(absorbed)
            }

            #[no_mangle]
            pub extern "C" fn pgh_alloc(size: i32) -> i32 {
                runtime::alloc(size)
            }

            #[no_mangle]
            pub extern "C" fn pgh_free(ptr: i32, size: i32) {
                runtime::free(ptr, size)
            }

            #[no_mangle]
            pub extern "C" fn pgh_init(ptr: i32, len: i32) -> i32 {
                runtime::install_panic_hook();
                let bytes = runtime::take_buf(ptr, len);
                let value = event::config_value(&bytes);
                let config: <$ty as $crate::Plugin>::Config =
                    serde_json::from_value(value).unwrap_or_default();
                let ctx = $crate::Ctx::new();
                match <$ty as $crate::Plugin>::init(&ctx, config) {
                    Ok(plugin) => {
                        PLUGIN.with(|p| *p.borrow_mut() = Some(plugin));
                        executor::run_until_stalled();
                        0
                    }
                    Err(e) => {
                        runtime::log(3, &format!("init failed: {e}"));
                        1
                    }
                }
            }

            #[no_mangle]
            pub extern "C" fn pgh_on_event(ptr: i32, len: i32) {
                let bytes = runtime::take_buf(ptr, len);
                let Some(event) = $crate::Event::parse(bytes) else {
                    runtime::log(3, "bad event buffer from host");
                    return;
                };
                let ctx = $crate::Ctx::new();
                PLUGIN.with(|p| {
                    if let Some(plugin) = p.borrow_mut().as_mut() {
                        $crate::Plugin::on_event(plugin, &ctx, event);
                    }
                });
                executor::run_until_stalled();
            }

            #[no_mangle]
            pub extern "C" fn pgh_on_async_complete(
                token: i64,
                err: i32,
                v0: i64,
                v1: i64,
                ptr: i32,
                len: i32,
            ) {
                let data = runtime::take_buf(ptr, len);
                executor::complete(token as u64, err, v0, v1, &data);
                executor::run_until_stalled();
            }

            #[no_mangle]
            pub extern "C" fn pgh_on_unload() {
                let ctx = $crate::Ctx::new();
                PLUGIN.with(|p| {
                    if let Some(mut plugin) = p.borrow_mut().take() {
                        $crate::Plugin::on_unload(&mut plugin, &ctx);
                    }
                });
            }
        }
    };
}
