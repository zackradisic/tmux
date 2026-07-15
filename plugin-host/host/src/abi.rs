//! Guest ABI binding: instantiation, the `tmux` import namespace, memory
//! protocol, and budgeted guest calls.
//!
//! Together with engine.rs this file confines every wasmtime type in the
//! crate. The ABI itself (export names, host_call encoding, memory rules) is
//! documented in plugin-host/ABI.md and mirrored by tmux-plugin-abi.

use std::collections::HashSet;
use std::time::Instant;

use tmux_plugin_abi::{
    exports, imports, ErrorCode, ABI_VERSION, HOST_CALL_ABI_FAILURE,
    HOST_CALL_ERR, HOST_CALL_OK,
};
use wasmtime::{
    Caller, Engine, Instance as WtInstance, Linker, Memory, Module, Store,
    StoreLimits, StoreLimitsBuilder, TypedFunc, UpdateDeadline,
};

use crate::engine::{HARD_TICKS, SOFT_TICKS};
use crate::{dispatch, hostlog};

/// Default per-instance linear memory cap.
const DEFAULT_MEMORY_LIMIT: usize = 64 * 1024 * 1024;

/// Per-store data available to host imports during guest calls (the
/// instance itself is checked out of the registry while the guest runs, so
/// anything dispatch needs must live here).
pub struct StoreData {
    pub plugin: String,
    pub generation: u64,
    pub scope: crate::registry::ScopeId,
    pub caps: crate::caps::EffectiveCaps,
    pub subscriptions: HashSet<String>,
    pub soft_warned: bool,
    limits: StoreLimits,
}

/// A live guest instance: store + bound exports.
pub struct Guest {
    pub store: Store<StoreData>,
    #[allow(dead_code)] // held for liveness/debugging
    instance: WtInstance,
    memory: Memory,
    alloc: TypedFunc<i32, i32>,
    #[allow(dead_code)] // the guest frees what it receives; host use is rare
    free: TypedFunc<(i32, i32), ()>,
    init: TypedFunc<(i32, i32), i32>,
    on_event: TypedFunc<(i32, i32), ()>,
    pub on_unload: Option<TypedFunc<(), ()>>,
    pub on_async_complete: Option<TypedFunc<(i64, i32, i32, i32), ()>>,
    state_version: Option<TypedFunc<(), i32>>,
    snapshot: Option<TypedFunc<(i32, i32), i32>>,
    migrate: Option<TypedFunc<(i32, i32, i32), i32>>,
    on_config_changed: Option<TypedFunc<(i32, i32), i32>>,
}

/// Outcome of a budgeted guest call, for stats and the failure policy.
pub struct CallOutcome<T> {
    pub result: Result<T, String>,
    pub soft_warned: bool,
    pub elapsed_ns: u64,
}

impl<T> CallOutcome<T> {
    pub fn trapped(&self) -> bool {
        self.result.is_err()
    }
}

/// Check a compiled module for the required exports before any
/// instantiation, so obvious ABI mismatches fail at load time.
pub fn validate_module(module: &Module) -> Result<(), String> {
    let required: [&str; 6] = [
        "memory",
        exports::ABI_VERSION,
        exports::ALLOC,
        exports::FREE,
        exports::INIT,
        exports::ON_EVENT,
    ];
    for name in required {
        if module.get_export(name).is_none() {
            return Err(format!("missing required export {name:?}"));
        }
    }
    Ok(())
}

/// Instantiate a module, verify the ABI handshake and bind exports.
/// Does NOT call the guest's init; the caller does that under budget.
pub fn instantiate(
    engine: &Engine,
    module: &Module,
    plugin: &str,
    generation: u64,
    scope: crate::registry::ScopeId,
    caps: crate::caps::EffectiveCaps,
) -> Result<Guest, String> {
    let data = StoreData {
        plugin: plugin.to_string(),
        generation,
        scope,
        caps,
        subscriptions: HashSet::new(),
        soft_warned: false,
        limits: StoreLimitsBuilder::new()
            .memory_size(DEFAULT_MEMORY_LIMIT)
            .memories(1)
            .tables(4)
            .instances(1)
            .build(),
    };
    let mut store = Store::new(engine, data);
    store.limiter(|d| &mut d.limits);

    // First deadline hit: warn and extend to the hard budget. Second: trap.
    // Runs on the main thread inside the guest call; must only flip flags.
    store.epoch_deadline_callback(|mut ctx| {
        let data = ctx.data_mut();
        if !data.soft_warned {
            data.soft_warned = true;
            Ok(UpdateDeadline::Continue(HARD_TICKS - SOFT_TICKS))
        } else {
            Err(wasmtime::Error::msg("plugin exceeded CPU budget"))
        }
    });
    // Instantiation itself (start functions, etc.) runs under a budget too.
    store.set_epoch_deadline(HARD_TICKS);

    let mut linker: Linker<StoreData> = Linker::new(engine);
    register_imports(&mut linker).map_err(|e| format!("linker: {e}"))?;

    let instance = linker
        .instantiate(&mut store, module)
        .map_err(|e| format!("instantiate: {e:#}"))?;

    let memory = instance
        .get_memory(&mut store, "memory")
        .ok_or_else(|| "no exported memory".to_string())?;

    let abi_version: TypedFunc<(), i32> = instance
        .get_typed_func(&mut store, exports::ABI_VERSION)
        .map_err(|e| format!("{}: {e}", exports::ABI_VERSION))?;
    store.set_epoch_deadline(HARD_TICKS);
    let got = abi_version
        .call(&mut store, ())
        .map_err(|e| format!("abi_version call: {e:#}"))?;
    if got != ABI_VERSION {
        return Err(format!(
            "plugin built for ABI {got}, host supports {ABI_VERSION}"
        ));
    }

    let alloc = instance
        .get_typed_func(&mut store, exports::ALLOC)
        .map_err(|e| format!("{}: {e}", exports::ALLOC))?;
    let free = instance
        .get_typed_func(&mut store, exports::FREE)
        .map_err(|e| format!("{}: {e}", exports::FREE))?;
    let init = instance
        .get_typed_func(&mut store, exports::INIT)
        .map_err(|e| format!("{}: {e}", exports::INIT))?;
    let on_event = instance
        .get_typed_func(&mut store, exports::ON_EVENT)
        .map_err(|e| format!("{}: {e}", exports::ON_EVENT))?;
    let on_unload = instance
        .get_typed_func(&mut store, exports::ON_UNLOAD)
        .ok();
    let on_async_complete = instance
        .get_typed_func(&mut store, exports::ON_ASYNC_COMPLETE)
        .ok();
    let state_version = instance
        .get_typed_func(&mut store, exports::STATE_VERSION)
        .ok();
    let snapshot = instance
        .get_typed_func(&mut store, exports::SNAPSHOT)
        .ok();
    let migrate = instance
        .get_typed_func(&mut store, exports::MIGRATE)
        .ok();
    let on_config_changed = instance
        .get_typed_func(&mut store, exports::ON_CONFIG_CHANGED)
        .ok();

    Ok(Guest {
        store,
        instance,
        memory,
        alloc,
        free,
        init,
        on_event,
        on_unload,
        on_async_complete,
        state_version,
        snapshot,
        migrate,
        on_config_changed,
    })
}

impl Guest {
    /// Write bytes into guest memory via the guest allocator; returns
    /// (ptr, len). The guest owns and frees the buffer.
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<(i32, i32), String> {
        let len = i32::try_from(bytes.len()).map_err(|_| "payload too large")?;
        let ptr = self
            .alloc
            .call(&mut self.store, len)
            .map_err(|e| format!("pgh_alloc: {e:#}"))?;
        if ptr == 0 {
            return Err("guest allocator returned NULL".into());
        }
        let data = self.memory.data_mut(&mut self.store);
        let start = ptr as usize;
        let end = start
            .checked_add(bytes.len())
            .ok_or("guest pointer overflow")?;
        if end > data.len() {
            return Err("guest allocator returned out-of-bounds pointer".into());
        }
        data[start..end].copy_from_slice(bytes);
        Ok((ptr, len))
    }

    /// Run one budgeted call into the guest.
    fn budgeted<T>(
        &mut self,
        ticks: u64,
        f: impl FnOnce(&mut Self) -> wasmtime::Result<T>,
    ) -> CallOutcome<T> {
        self.store.data_mut().soft_warned = false;
        self.store.set_epoch_deadline(ticks);
        let started = Instant::now();
        let result = f(self).map_err(|e| format!("{e:#}"));
        let elapsed_ns = started.elapsed().as_nanos() as u64;
        let soft_warned = self.store.data().soft_warned;
        if soft_warned {
            let plugin = self.store.data().plugin.clone();
            hostlog::warn(
                &plugin,
                &format!("callback exceeded soft CPU budget ({:.2}ms)",
                    elapsed_ns as f64 / 1e6),
            );
        }
        CallOutcome { result, soft_warned, elapsed_ns }
    }

    /// Call the guest's init export with its config JSON.
    pub fn call_init(&mut self, config_json: &str) -> CallOutcome<()> {
        self.budgeted(HARD_TICKS, |g| {
            let (ptr, len) = g
                .write_bytes(config_json.as_bytes())
                .map_err(wasmtime::Error::msg)?;
            let rc = g.init.call(&mut g.store, (ptr, len))?;
            if rc != 0 {
                return Err(wasmtime::Error::msg(format!(
                    "init returned {rc}"
                )));
            }
            Ok(())
        })
    }

    /// Deliver one event to the guest.
    pub fn call_on_event(&mut self, event_json: &str) -> CallOutcome<()> {
        self.budgeted(HARD_TICKS, |g| {
            let (ptr, len) = g
                .write_bytes(event_json.as_bytes())
                .map_err(wasmtime::Error::msg)?;
            g.on_event.call(&mut g.store, (ptr, len))?;
            Ok(())
        })
    }

    /// Deliver an async completion to the guest.
    pub fn call_on_async_complete(
        &mut self,
        token: u64,
        json: &str,
        is_error: bool,
    ) -> CallOutcome<()> {
        if self.on_async_complete.is_none() {
            return CallOutcome {
                result: Ok(()),
                soft_warned: false,
                elapsed_ns: 0,
            };
        }
        self.budgeted(HARD_TICKS, |g| {
            let (ptr, len) = g
                .write_bytes(json.as_bytes())
                .map_err(wasmtime::Error::msg)?;
            let f = g.on_async_complete.as_ref().unwrap();
            f.call(
                &mut g.store,
                (token as i64, ptr, len, i32::from(is_error)),
            )?;
            Ok(())
        })
    }

    /// Snapshot the guest's state for a code reload. Returns
    /// (state_version, bytes) or None when the guest keeps no state (or
    /// lacks the exports) - in which case reload proceeds with fresh init.
    pub fn call_snapshot(&mut self) -> Option<(i32, Vec<u8>)> {
        self.snapshot.as_ref()?;
        let outcome = self.budgeted(HARD_TICKS, |g| {
            let version = match &g.state_version {
                Some(f) => f.call(&mut g.store, ()).unwrap_or(1),
                None => 1,
            };
            // Two 4-byte out-slots for {ptr, len}.
            let slot = g.alloc.call(&mut g.store, 8)?;
            if slot == 0 {
                return Err(wasmtime::Error::msg("alloc failed"));
            }
            let f = g.snapshot.as_ref().unwrap();
            let rc = f.call(&mut g.store, (slot, slot + 4))?;
            if rc != 0 {
                g.free.call(&mut g.store, (slot, 8))?;
                return Ok(None); // stateless
            }
            let data = g.memory.data(&g.store);
            let read_u32 = |at: usize| -> Option<u32> {
                data.get(at..at + 4)
                    .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
            };
            let ptr = read_u32(slot as usize).unwrap_or(0);
            let len = read_u32(slot as usize + 4).unwrap_or(0);
            let bytes = data
                .get(ptr as usize..(ptr as usize + len as usize))
                .map(<[u8]>::to_vec)
                .unwrap_or_default();
            g.free.call(&mut g.store, (ptr as i32, len as i32))?;
            g.free.call(&mut g.store, (slot, 8))?;
            Ok(Some((version, bytes)))
        });
        match outcome.result {
            Ok(v) => v,
            Err(_) => None, // snapshot trap => treat as stateless
        }
    }

    /// Hand old-generation state to a fresh guest. Ok(()) on success;
    /// Err means the new code refused the state (keep the old instance).
    pub fn call_migrate(
        &mut self,
        old_version: i32,
        state: &[u8],
    ) -> Result<(), String> {
        let Some(_) = self.migrate else {
            return Err("plugin has no migrate export".into());
        };
        let outcome = self.budgeted(HARD_TICKS, |g| {
            let (ptr, len) =
                g.write_bytes(state).map_err(wasmtime::Error::msg)?;
            let f = g.migrate.as_ref().unwrap();
            let rc = f.call(&mut g.store, (old_version, ptr, len))?;
            if rc != 0 {
                return Err(wasmtime::Error::msg(format!(
                    "migrate returned {rc}"
                )));
            }
            Ok(())
        });
        outcome.result
    }

    /// Offer a changed config. Some(true) = absorbed; Some(false) = plugin
    /// asks for a restart; None = no export (restart).
    pub fn call_on_config_changed(&mut self, config_json: &str) -> Option<bool> {
        self.on_config_changed.as_ref()?;
        let outcome = self.budgeted(HARD_TICKS, |g| {
            let (ptr, len) = g
                .write_bytes(config_json.as_bytes())
                .map_err(wasmtime::Error::msg)?;
            let f = g.on_config_changed.as_ref().unwrap();
            Ok(f.call(&mut g.store, (ptr, len))?)
        });
        match outcome.result {
            Ok(rc) => Some(rc == 1),
            Err(_) => Some(false),
        }
    }

    /// Best-effort unload notification with a tiny budget.
    pub fn call_on_unload(&mut self) -> CallOutcome<()> {
        if self.on_unload.is_none() {
            return CallOutcome { result: Ok(()), soft_warned: false, elapsed_ns: 0 };
        }
        self.budgeted(SOFT_TICKS, |g| {
            // Disjoint field borrows: the func handle and the store.
            let f = g.on_unload.as_ref().unwrap();
            f.call(&mut g.store, ())?;
            Ok(())
        })
    }
}

/// Register the `tmux` import namespace on a linker.
fn register_imports(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    linker.func_wrap(
        imports::MODULE,
        imports::HOST_CALL,
        |mut caller: Caller<'_, StoreData>,
         req_ptr: i32,
         req_len: i32,
         out_ptr: i32,
         out_len_ptr: i32|
         -> i32 {
            let request = match read_guest_bytes(&mut caller, req_ptr, req_len) {
                Ok(b) => b,
                Err(_) => return HOST_CALL_ABI_FAILURE,
            };

            let (status, response) = match dispatch::dispatch(caller.data(), &request) {
                Ok(value) => (
                    HOST_CALL_OK,
                    serde_json::json!({ "ok": value }).to_string(),
                ),
                Err(err) => (
                    HOST_CALL_ERR,
                    serde_json::json!({ "err": err }).to_string(),
                ),
            };

            // Apply subscription changes (dispatch cannot borrow the store
            // data mutably while it also reads it; returns deltas instead).
            dispatch::apply_pending_subscriptions(caller.data_mut());

            match write_response(&mut caller, out_ptr, out_len_ptr, response.as_bytes()) {
                Ok(()) => status,
                Err(_) => HOST_CALL_ABI_FAILURE,
            }
        },
    )?;

    linker.func_wrap(
        imports::MODULE,
        imports::HOST_REQUEST,
        |mut caller: Caller<'_, StoreData>, req_ptr: i32, req_len: i32| -> i64 {
            let Ok(request) = read_guest_bytes(&mut caller, req_ptr, req_len)
            else {
                return -i64::from(ErrorCode::BadRequest.as_num());
            };
            match dispatch::dispatch_async(caller.data(), &request) {
                Ok(token) => token as i64,
                Err(e) => {
                    let plugin = caller.data().plugin.clone();
                    hostlog::debug(
                        &plugin,
                        &format!("host_request rejected: {}", e.message),
                    );
                    -i64::from(e.code.as_num())
                }
            }
        },
    )?;

    linker.func_wrap(
        imports::MODULE,
        imports::HOST_LOG,
        |mut caller: Caller<'_, StoreData>, level: i32, ptr: i32, len: i32| {
            let Ok(bytes) = read_guest_bytes(&mut caller, ptr, len) else {
                return;
            };
            let msg = String::from_utf8_lossy(&bytes).into_owned();
            let plugin = caller.data().plugin.clone();
            match level {
                0 => hostlog::debug(&plugin, &msg),
                1 => hostlog::info(&plugin, &msg),
                2 => hostlog::warn(&plugin, &msg),
                _ => hostlog::error(&plugin, &msg),
            }
        },
    )?;

    Ok(())
}

fn caller_memory(caller: &mut Caller<'_, StoreData>) -> Result<Memory, ()> {
    caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or(())
}

fn read_guest_bytes(
    caller: &mut Caller<'_, StoreData>,
    ptr: i32,
    len: i32,
) -> Result<Vec<u8>, ()> {
    if ptr < 0 || len < 0 {
        return Err(());
    }
    let memory = caller_memory(caller)?;
    let data = memory.data(&caller);
    let start = ptr as usize;
    let end = start.checked_add(len as usize).ok_or(())?;
    if end > data.len() {
        return Err(());
    }
    Ok(data[start..end].to_vec())
}

/// Allocate a response buffer in the guest (via its allocator), copy the
/// payload in, and store {ptr,len} into the two out-slots. The guest frees
/// the buffer after decoding.
fn write_response(
    caller: &mut Caller<'_, StoreData>,
    out_ptr: i32,
    out_len_ptr: i32,
    payload: &[u8],
) -> Result<(), ()> {
    let alloc = caller
        .get_export(exports::ALLOC)
        .and_then(|e| e.into_func())
        .ok_or(())?
        .typed::<i32, i32>(&*caller)
        .map_err(|_| ())?;
    let len = i32::try_from(payload.len()).map_err(|_| ())?;
    // Re-entrant guest call (allocator only); memory may grow/move, so the
    // Memory handle is re-fetched afterwards and never cached.
    let ptr = alloc.call(&mut *caller, len).map_err(|_| ())?;
    if ptr == 0 {
        return Err(());
    }

    let memory = caller_memory(caller)?;
    let data = memory.data_mut(caller);

    let start = ptr as usize;
    let end = start.checked_add(payload.len()).ok_or(())?;
    if end > data.len() {
        return Err(());
    }
    data[start..end].copy_from_slice(payload);

    for (slot, value) in [(out_ptr, ptr), (out_len_ptr, len)] {
        let s = slot as usize;
        let e = s.checked_add(4).ok_or(())?;
        if slot < 0 || e > data.len() {
            return Err(());
        }
        data[s..e].copy_from_slice(&value.to_le_bytes());
    }
    Ok(())
}
