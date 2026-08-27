//! Guest ABI binding: instantiation, the `tmux` import namespace, memory
//! protocol, and budgeted guest calls.
//!
//! Together with engine.rs this file confines every wasmtime type in the
//! crate. The ABI itself (import signatures, buffer taxonomy, memory rules)
//! is documented in plugin-host/ABI.md and defined in tmux-plugin-abi.
//!
//! Memory discipline: guest memory is touched through `GuestMem` only.
//! Raw pointers into linear memory are valid only until the next guest
//! re-entry (`pgh_alloc` via `give_owned`); every method consumes its
//! borrowed inputs (or copies them) before any re-entry. The engine pins
//! memory so it can never move (engine.rs), but the discipline holds
//! regardless.

use std::collections::HashSet;
use std::time::Instant;

use tmux_plugin_abi::{exports, imports, ErrorCode, ABI_VERSION};
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
    /// Subscribed event ids (interned).
    pub subscriptions: HashSet<u32>,
    pub soft_warned: bool,
    limits: StoreLimits,
}

#[cfg(test)]
impl StoreData {
    /// Bare StoreData for dispatch tests (no live store behind it).
    pub(crate) fn for_tests(
        scope: crate::registry::ScopeId,
        caps: crate::caps::EffectiveCaps,
    ) -> Self {
        Self {
            plugin: "test".into(),
            generation: 1,
            scope,
            caps,
            subscriptions: HashSet::new(),
            soft_warned: false,
            limits: StoreLimitsBuilder::new().build(),
        }
    }
}

/// A live guest instance: store + bound exports.
pub struct Guest {
    pub store: Store<StoreData>,
    #[allow(dead_code)] // held for liveness/debugging
    instance: WtInstance,
    memory: Memory,
    alloc: TypedFunc<i32, i32>,
    free: TypedFunc<(i32, i32), ()>,
    init: TypedFunc<(i32, i32), i32>,
    on_event: TypedFunc<(i32, i32), ()>,
    pub on_unload: Option<TypedFunc<(), ()>>,
    pub on_async_complete:
        Option<TypedFunc<(i64, i32, i64, i64, i32, i32), ()>>,
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

/// Encode a plugin config Value as the field-block config payload. Scalar
/// top-level entries map directly; nested arrays/objects travel as the
/// JSON escape-hatch tag (interpreted by the guest SDK only).
pub fn encode_config(config: &serde_json::Value) -> Vec<u8> {
    use tmux_plugin_abi::{FieldWriter, KeyRef};

    let mut w = FieldWriter::new();
    match config {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                let key = KeyRef::Name(key);
                match value {
                    serde_json::Value::Null => w.null(key),
                    serde_json::Value::Bool(b) => w.bool(key, *b),
                    serde_json::Value::Number(n) => {
                        if let Some(i) = n.as_i64() {
                            w.i64(key, i);
                        } else {
                            w.f64(key, n.as_f64().unwrap_or(0.0));
                        }
                    }
                    serde_json::Value::String(s) => w.str(key, s),
                    nested => w.json(key, &nested.to_string()),
                }
            }
        }
        serde_json::Value::Null => {}
        other => w.json(KeyRef::Name("config"), &other.to_string()),
    }
    w.finish()
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

    /// Call the guest's init export with its config field block.
    pub fn call_init(&mut self, config: &[u8]) -> CallOutcome<()> {
        self.budgeted(HARD_TICKS, |g| {
            let (ptr, len) =
                g.write_bytes(config).map_err(wasmtime::Error::msg)?;
            let rc = g.init.call(&mut g.store, (ptr, len))?;
            if rc != 0 {
                return Err(wasmtime::Error::msg(format!(
                    "init returned {rc}"
                )));
            }
            Ok(())
        })
    }

    /// Deliver one binary event buffer to the guest.
    pub fn call_on_event(&mut self, event: &[u8]) -> CallOutcome<()> {
        self.budgeted(HARD_TICKS, |g| {
            let (ptr, len) =
                g.write_bytes(event).map_err(wasmtime::Error::msg)?;
            g.on_event.call(&mut g.store, (ptr, len))?;
            Ok(())
        })
    }

    /// Deliver an async completion to the guest. `err` is 0 or an
    /// ErrorCode number; `data` becomes a guest-owned buffer (error
    /// message bytes on error, per-method payload on success).
    pub fn call_on_async_complete(
        &mut self,
        token: u64,
        err: i32,
        v0: i64,
        v1: i64,
        data: &[u8],
    ) -> CallOutcome<()> {
        if self.on_async_complete.is_none() {
            return CallOutcome {
                result: Ok(()),
                soft_warned: false,
                elapsed_ns: 0,
            };
        }
        self.budgeted(HARD_TICKS, |g| {
            let (ptr, len) = if data.is_empty() {
                (0, 0)
            } else {
                g.write_bytes(data).map_err(wasmtime::Error::msg)?
            };
            let f = g.on_async_complete.as_ref().unwrap();
            f.call(&mut g.store, (token as i64, err, v0, v1, ptr, len))?;
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
    pub fn call_on_config_changed(&mut self, config: &[u8]) -> Option<bool> {
        self.on_config_changed.as_ref()?;
        let outcome = self.budgeted(HARD_TICKS, |g| {
            let (ptr, len) =
                g.write_bytes(config).map_err(wasmtime::Error::msg)?;
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

// ---------------------------------------------------------------------------
// Guest memory access for imports.
// ---------------------------------------------------------------------------

/// Structured error carried through dispatch; the message lands in the
/// per-thread last-error slot for the guest to fetch.
#[derive(Debug, Clone)]
pub struct HostError {
    pub code: ErrorCode,
    pub message: String,
}

pub fn err(code: ErrorCode, message: impl Into<String>) -> HostError {
    HostError { code, message: message.into() }
}

thread_local! {
    static LAST_ERROR: std::cell::RefCell<String> =
        const { std::cell::RefCell::new(String::new()) };
}

fn set_last_error(msg: &str) {
    LAST_ERROR.with(|e| {
        let mut e = e.borrow_mut();
        e.clear();
        e.push_str(msg);
    });
}

fn last_error() -> String {
    LAST_ERROR.with(|e| e.borrow().clone())
}

/// The one gateway to guest linear memory during an import call.
///
/// Raw pointers returned by `c_str`/`bytes` point into linear memory and
/// are valid only until the next guest re-entry; the only re-entry inside
/// an import is `give_owned` (the guest allocator), so the rule is:
/// consume every raw pointer before calling `give_owned`.
pub struct GuestMem<'a, 'b> {
    caller: &'a mut Caller<'b, StoreData>,
    memory: Memory,
    /// Debug tripwire for the C borrow contract: in debug builds, borrowed
    /// strings/bytes are COPIES freed when the import call ends, so any C
    /// code that stashes a pointer past the call becomes a use-after-free
    /// that ASAN catches in CI. Release builds pass guest memory directly
    /// (zero-copy).
    #[cfg(debug_assertions)]
    debug_copies: std::cell::RefCell<Vec<Box<[u8]>>>,
}

/// Sink context writing directly into a guest OutBuf. Counts the total
/// regardless of capacity so the needed size can be reported on overflow.
#[repr(C)]
pub struct OutSink {
    dst: *mut u8,
    cap: usize,
    written: usize,
    total: usize,
}

/// pgh_sink writing into an OutSink (guest OutBuf).
pub unsafe extern "C" fn out_sink(
    ctx: *mut std::ffi::c_void,
    ptr: *const std::os::raw::c_char,
    len: usize,
) {
    let s = &mut *(ctx as *mut OutSink);
    let n = len.min(s.cap.saturating_sub(s.written));
    if n > 0 {
        std::ptr::copy_nonoverlapping(ptr as *const u8, s.dst.add(s.written), n);
        s.written += n;
    }
    s.total = s.total.saturating_add(len);
}

/// pgh_sink collecting into a host Vec (for OwnedBuf results).
pub unsafe extern "C" fn collect_sink(
    ctx: *mut std::ffi::c_void,
    ptr: *const std::os::raw::c_char,
    len: usize,
) {
    let buf = &mut *(ctx as *mut Vec<u8>);
    buf.extend_from_slice(std::slice::from_raw_parts(ptr as *const u8, len));
}

impl<'a, 'b> GuestMem<'a, 'b> {
    fn new(caller: &'a mut Caller<'b, StoreData>) -> Result<Self, HostError> {
        let memory = caller
            .get_export("memory")
            .and_then(|e| e.into_memory())
            .ok_or_else(|| err(ErrorCode::Host, "guest has no memory"))?;
        Ok(Self {
            caller,
            memory,
            #[cfg(debug_assertions)]
            debug_copies: std::cell::RefCell::new(Vec::new()),
        })
    }

    /// Debug builds: return a call-lifetime copy instead of the guest
    /// pointer (see the field docs). No-op passthrough in release.
    #[cfg(debug_assertions)]
    fn tripwire(&self, bytes: &[u8]) -> *const u8 {
        let boxed: Box<[u8]> = bytes.into();
        let ptr = boxed.as_ptr();
        self.debug_copies.borrow_mut().push(boxed);
        ptr
    }

    pub fn data(&self) -> &StoreData {
        self.caller.data()
    }

    pub fn data_mut(&mut self) -> &mut StoreData {
        self.caller.data_mut()
    }

    /// Bounds-check ptr..ptr+len(+extra) and return the start offset.
    fn check(
        &self,
        ptr: i32,
        len: i32,
        extra: usize,
    ) -> Result<usize, HostError> {
        if ptr < 0 || len < 0 {
            return Err(err(ErrorCode::BadRequest, "negative ptr/len"));
        }
        let start = ptr as usize;
        let end = start
            .checked_add(len as usize)
            .and_then(|e| e.checked_add(extra))
            .ok_or_else(|| err(ErrorCode::BadRequest, "ptr overflow"))?;
        if end > self.memory.data_size(&*self.caller) {
            return Err(err(ErrorCode::BadRequest, "out-of-bounds buffer"));
        }
        Ok(start)
    }

    /// Borrowed C string: (ptr, len) with a NUL byte at data[len] and no
    /// interior NUL. Returns a raw pointer suitable for passing straight
    /// into a C vtable call. Valid until the next guest re-entry.
    pub fn c_str(
        &self,
        ptr: i32,
        len: i32,
    ) -> Result<*const std::os::raw::c_char, HostError> {
        let start = self.check(ptr, len, 1)?;
        let data = self.memory.data(&*self.caller);
        let bytes = &data[start..start + len as usize + 1];
        if bytes[len as usize] != 0 {
            return Err(err(
                ErrorCode::BadRequest,
                "string not NUL-terminated at data[len]",
            ));
        }
        if bytes[..len as usize].contains(&0) {
            return Err(err(ErrorCode::BadRequest, "embedded NUL in string"));
        }
        #[cfg(debug_assertions)]
        return Ok(self.tripwire(bytes) as *const std::os::raw::c_char);
        #[cfg(not(debug_assertions))]
        Ok(data[start..].as_ptr() as *const std::os::raw::c_char)
    }

    /// Optional string: ptr 0 + len 0 = absent.
    pub fn c_str_opt(
        &self,
        ptr: i32,
        len: i32,
    ) -> Result<Option<*const std::os::raw::c_char>, HostError> {
        if ptr == 0 && len == 0 {
            return Ok(None);
        }
        self.c_str(ptr, len).map(Some)
    }

    /// Borrowed raw bytes (no NUL requirements). Valid until the next
    /// guest re-entry.
    pub fn bytes(
        &self,
        ptr: i32,
        len: i32,
    ) -> Result<(*const u8, usize), HostError> {
        let start = self.check(ptr, len, 0)?;
        let data = self.memory.data(&*self.caller);
        #[cfg(debug_assertions)]
        return Ok((
            self.tripwire(&data[start..start + len as usize]),
            len as usize,
        ));
        #[cfg(not(debug_assertions))]
        Ok((data[start..].as_ptr(), len as usize))
    }

    /// Raw pointer to a guest buffer PINNED past this call (async fs
    /// input): the SDK future owns the buffer until the completion
    /// arrives, memory never moves (engine config), and instance
    /// teardown waits for in-flight jobs. Bypasses the debug tripwire
    /// deliberately - the pointer must outlive the import call.
    pub fn pinned_bytes(
        &self,
        ptr: i32,
        len: i32,
    ) -> Result<*const u8, HostError> {
        let start = self.check(ptr, len, 0)?;
        let data = self.memory.data(&*self.caller);
        Ok(data[start..].as_ptr())
    }

    /// Mutable variant for pinned output buffers (async fs_read).
    pub fn pinned_bytes_mut(
        &mut self,
        ptr: i32,
        len: i32,
    ) -> Result<*mut u8, HostError> {
        let start = self.check(ptr, len, 0)?;
        let data = self.memory.data_mut(&mut *self.caller);
        Ok(data[start..].as_mut_ptr())
    }

    /// A bounds-checked read-only slice of guest memory, for host code
    /// that consumes the bytes in place (Rust-side; C callees get the
    /// raw-pointer `bytes` above, with its debug tripwire). Valid until
    /// the next guest re-entry, which the borrow of `self` prevents.
    pub fn byte_slice(&self, ptr: i32, len: i32) -> Result<&[u8], HostError> {
        let start = self.check(ptr, len, 0)?;
        let data = self.memory.data(&*self.caller);
        Ok(&data[start..start + len as usize])
    }

    /// Copy bytes out of guest memory (owned; survives re-entry).
    pub fn read(&self, ptr: i32, len: i32) -> Result<Vec<u8>, HostError> {
        let start = self.check(ptr, len, 0)?;
        let data = self.memory.data(&*self.caller);
        Ok(data[start..start + len as usize].to_vec())
    }

    /// Copy a string out of guest memory (owned; NUL rule as `c_str`).
    pub fn read_str(&self, ptr: i32, len: i32) -> Result<String, HostError> {
        let start = self.check(ptr, len, 1)?;
        let data = self.memory.data(&*self.caller);
        if data[start + len as usize] != 0 {
            return Err(err(
                ErrorCode::BadRequest,
                "string not NUL-terminated at data[len]",
            ));
        }
        let bytes = &data[start..start + len as usize];
        if bytes.contains(&0) {
            return Err(err(ErrorCode::BadRequest, "embedded NUL in string"));
        }
        String::from_utf8(bytes.to_vec())
            .map_err(|_| err(ErrorCode::BadRequest, "invalid UTF-8"))
    }

    /// Validate a guest OutBuf and build the direct-write sink for it.
    /// The returned OutSink holds a raw pointer: consume it (and the C
    /// call using it) before any guest re-entry.
    pub fn out_sink(&mut self, out: i32, cap: i32) -> Result<OutSink, HostError> {
        let start = self.check(out, cap, 0)?;
        let data = self.memory.data_mut(&mut *self.caller);
        Ok(OutSink {
            dst: data[start..].as_mut_ptr(),
            cap: cap as usize,
            written: 0,
            total: 0,
        })
    }

    /// A bounds-checked mutable view of a guest OutBuf, for host code that
    /// fills it in place instead of copying through a host buffer. The
    /// borrow of `self` keeps it from outliving a guest re-entry, so no
    /// raw pointer discipline is needed (unlike `out_sink`).
    pub fn out_bytes_mut(
        &mut self,
        out: i32,
        cap: i32,
    ) -> Result<&mut [u8], HostError> {
        let start = self.check(out, cap, 0)?;
        let end = start + cap.max(0) as usize;
        let data = self.memory.data_mut(&mut *self.caller);
        Ok(&mut data[start..end])
    }

    /// Finish an OutBuf write: store the length (written on success, needed
    /// size on overflow) into len_out and map overflow to E_LIMIT.
    pub fn finish_out(
        &mut self,
        sink: OutSink,
        len_out: i32,
    ) -> Result<(), HostError> {
        let total = sink.total;
        self.write_u32_at(len_out, total as u32)?;
        if total > sink.cap {
            return Err(err(
                ErrorCode::Limit,
                format!("result is {total} bytes, buffer holds {}", sink.cap),
            ));
        }
        Ok(())
    }

    /// Write a little-endian u32 at a guest address.
    pub fn write_u32_at(&mut self, at: i32, value: u32) -> Result<(), HostError> {
        let start = self.check(at, 4, 0)?;
        let data = self.memory.data_mut(&mut *self.caller);
        data[start..start + 4].copy_from_slice(&value.to_le_bytes());
        Ok(())
    }

    /// Copy bytes to a guest address (fixed out-structs).
    pub fn write_at(&mut self, at: i32, bytes: &[u8]) -> Result<(), HostError> {
        let start = self.check(at, bytes.len() as i32, 0)?;
        let data = self.memory.data_mut(&mut *self.caller);
        data[start..start + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }

    /// Fill an OutBuf from a host-side byte slice (no sink involved).
    pub fn write_out(
        &mut self,
        bytes: &[u8],
        out: i32,
        cap: i32,
        len_out: i32,
    ) -> Result<(), HostError> {
        self.write_u32_at(len_out, bytes.len() as u32)?;
        if bytes.len() > cap.max(0) as usize {
            return Err(err(
                ErrorCode::Limit,
                format!("result is {} bytes, buffer holds {cap}", bytes.len()),
            ));
        }
        let start = self.check(out, bytes.len() as i32, 0)?;
        let data = self.memory.data_mut(&mut *self.caller);
        data[start..start + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }

    /// Transfer ownership of `bytes` to the guest: allocate via pgh_alloc
    /// (a guest re-entry - all raw pointers must be dead), copy the data
    /// in, and write the {ptr, len} OwnedBuf struct at `owned_out`.
    pub fn give_owned(
        &mut self,
        bytes: &[u8],
        owned_out: i32,
    ) -> Result<(), HostError> {
        // Validate the out-struct location up front (offsets stay valid:
        // growth never moves or shrinks memory).
        self.check(owned_out, 8, 0)?;
        let alloc = self
            .caller
            .get_export(exports::ALLOC)
            .and_then(|e| e.into_func())
            .ok_or_else(|| err(ErrorCode::Host, "no allocator export"))?
            .typed::<i32, i32>(&*self.caller)
            .map_err(|_| err(ErrorCode::Host, "bad allocator signature"))?;
        let len = i32::try_from(bytes.len())
            .map_err(|_| err(ErrorCode::Limit, "payload too large"))?;
        let ptr = alloc
            .call(&mut *self.caller, len)
            .map_err(|e| err(ErrorCode::Host, format!("pgh_alloc: {e:#}")))?;
        if ptr == 0 {
            return Err(err(ErrorCode::Host, "guest allocator returned NULL"));
        }
        // Memory may have grown during the re-entry; re-derive the view.
        let data = self.memory.data_mut(&mut *self.caller);
        let start = ptr as usize;
        let end = start
            .checked_add(bytes.len())
            .ok_or_else(|| err(ErrorCode::Host, "guest pointer overflow"))?;
        if end > data.len() {
            return Err(err(
                ErrorCode::Host,
                "guest allocator returned out-of-bounds pointer",
            ));
        }
        data[start..end].copy_from_slice(bytes);
        let at = owned_out as usize;
        data[at..at + 4].copy_from_slice(&(ptr as u32).to_le_bytes());
        data[at + 4..at + 8]
            .copy_from_slice(&(bytes.len() as u32).to_le_bytes());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Import registration: one typed wasm import per method.
// ---------------------------------------------------------------------------

/// Map a dispatch result to the i32 status return, recording the message.
fn ret_i32(r: Result<(), HostError>) -> i32 {
    match r {
        Ok(()) => 0,
        Err(e) => {
            set_last_error(&e.message);
            -e.code.as_num()
        }
    }
}

/// Map a dispatch result to an i64 value return (> 0) or -err.
fn ret_i64(r: Result<i64, HostError>) -> i64 {
    match r {
        Ok(v) => v,
        Err(e) => {
            set_last_error(&e.message);
            -i64::from(e.code.as_num())
        }
    }
}

/// Run a method body with a GuestMem over the caller.
fn with_mem<T>(
    caller: &mut Caller<'_, StoreData>,
    f: impl FnOnce(&mut GuestMem<'_, '_>) -> Result<T, HostError>,
) -> Result<T, HostError> {
    let mut mem = GuestMem::new(caller)?;
    f(&mut mem)
}

/// Register the `tmux` import namespace on a linker.
fn register_imports(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    use imports as im;
    let m = im::MODULE;

    linker.func_wrap(m, im::INTERN, |mut c: Caller<'_, StoreData>, ptr: i32, len: i32| -> i64 {
        ret_i64(with_mem(&mut c, |mem| dispatch::intern(mem, ptr, len)))
    })?;

    linker.func_wrap(m, im::INTERN_NAME, |mut c: Caller<'_, StoreData>, id: i32, out: i32, cap: i32, len_out: i32| -> i32 {
        ret_i32(with_mem(&mut c, |mem| dispatch::intern_name(mem, id, out, cap, len_out)))
    })?;

    linker.func_wrap(m, im::SUBSCRIBE, |mut c: Caller<'_, StoreData>, id: i32| -> i32 {
        ret_i32(with_mem(&mut c, |mem| dispatch::subscribe(mem, id, true)))
    })?;

    linker.func_wrap(m, im::UNSUBSCRIBE, |mut c: Caller<'_, StoreData>, id: i32| -> i32 {
        ret_i32(with_mem(&mut c, |mem| dispatch::subscribe(mem, id, false)))
    })?;

    linker.func_wrap(m, im::LIST, |mut c: Caller<'_, StoreData>, kind: i32, owned_out: i32| -> i32 {
        ret_i32(with_mem(&mut c, |mem| dispatch::list(mem, kind, owned_out)))
    })?;

    linker.func_wrap(m, im::RESOLVE, |mut c: Caller<'_, StoreData>, kind: i32, id: i32, owned_out: i32| -> i32 {
        ret_i32(with_mem(&mut c, |mem| dispatch::resolve(mem, kind, id, owned_out)))
    })?;

    linker.func_wrap(m, im::SELF_INFO, |mut c: Caller<'_, StoreData>, out: i32| -> i32 {
        ret_i32(with_mem(&mut c, |mem| dispatch::self_info(mem, out)))
    })?;

    linker.func_wrap(m, im::GET_OPTION, |mut c: Caller<'_, StoreData>, kind: i32, id: i32, name_ptr: i32, name_len: i32, out: i32, cap: i32, len_out: i32| -> i32 {
        ret_i32(with_mem(&mut c, |mem| {
            dispatch::get_option(mem, kind, id, name_ptr, name_len, out, cap, len_out)
        }))
    })?;

    linker.func_wrap(m, im::FORMAT_EXPAND, |mut c: Caller<'_, StoreData>, kind: i32, id: i32, fmt_ptr: i32, fmt_len: i32, out: i32, cap: i32, len_out: i32| -> i32 {
        ret_i32(with_mem(&mut c, |mem| {
            dispatch::format_expand(mem, kind, id, fmt_ptr, fmt_len, out, cap, len_out)
        }))
    })?;

    linker.func_wrap(m, im::SET_OPTION, |mut c: Caller<'_, StoreData>, kind: i32, id: i32, name_ptr: i32, name_len: i32, val_ptr: i32, val_len: i32| -> i32 {
        ret_i32(with_mem(&mut c, |mem| {
            dispatch::set_option(mem, kind, id, name_ptr, name_len, val_ptr, val_len)
        }))
    })?;

    linker.func_wrap(m, im::SEND_KEYS, |mut c: Caller<'_, StoreData>, pane: i32, keys_ptr: i32, keys_len: i32, literal: i32| -> i32 {
        ret_i32(with_mem(&mut c, |mem| {
            dispatch::send_keys(mem, pane, keys_ptr, keys_len, literal)
        }))
    })?;

    linker.func_wrap(m, im::CAPTURE_PANE, |mut c: Caller<'_, StoreData>, pane: i32, start: i32, end: i32, escapes: i32, out: i32, cap: i32, len_out: i32| -> i32 {
        ret_i32(with_mem(&mut c, |mem| {
            dispatch::capture_pane(mem, pane, start, end, escapes, out, cap, len_out)
        }))
    })?;

    linker.func_wrap(m, im::DISPLAY_MESSAGE, |mut c: Caller<'_, StoreData>, client: i32, msg_ptr: i32, msg_len: i32| -> i32 {
        ret_i32(with_mem(&mut c, |mem| {
            dispatch::display_message(mem, client, msg_ptr, msg_len)
        }))
    })?;

    linker.func_wrap(m, im::TIMER_CANCEL, |mut c: Caller<'_, StoreData>, token: i64| -> i32 {
        ret_i32(with_mem(&mut c, |mem| dispatch::timer_cancel(mem, token)))
    })?;

    linker.func_wrap(m, im::MODE_OPEN, |mut c: Caller<'_, StoreData>, window: i32, width: i32, height: i32, x: i32, y: i32, title_ptr: i32, title_len: i32| -> i64 {
        ret_i64(with_mem(&mut c, |mem| {
            dispatch::mode_open(mem, window, width, height, x, y, title_ptr, title_len)
        }))
    })?;

    linker.func_wrap(m, im::MODE_WRITE, |mut c: Caller<'_, StoreData>, mode: i64, ptr: i32, len: i32| -> i32 {
        ret_i32(with_mem(&mut c, |mem| dispatch::mode_write(mem, mode, ptr, len)))
    })?;

    linker.func_wrap(m, im::MODE_PREVIEW, |mut c: Caller<'_, StoreData>, mode: i64, pane: i64, x: i32, y: i32, w: i32, h: i32| -> i32 {
        ret_i32(with_mem(&mut c, |mem| {
            dispatch::mode_preview(mem, mode, pane, x, y, w, h)
        }))
    })?;

    linker.func_wrap(m, im::MODE_MOVE, |mut c: Caller<'_, StoreData>, mode: i64, window: i32, x: i32, y: i32| -> i32 {
        ret_i32(with_mem(&mut c, |mem| dispatch::mode_move(mem, mode, window, x, y)))
    })?;

    linker.func_wrap(m, im::MODE_CLOSE, |mut c: Caller<'_, StoreData>, mode: i64| -> i32 {
        ret_i32(with_mem(&mut c, |mem| dispatch::mode_close(mem, mode)))
    })?;

    linker.func_wrap(m, im::LAST_ERROR, |mut c: Caller<'_, StoreData>, out: i32, cap: i32, len_out: i32| -> i32 {
        let msg = last_error();
        ret_i32(with_mem(&mut c, |mem| {
            mem.write_out(msg.as_bytes(), out, cap, len_out)
        }))
    })?;

    linker.func_wrap(m, im::LOG, |mut c: Caller<'_, StoreData>, level: i32, ptr: i32, len: i32| {
        let Ok(bytes) = with_mem(&mut c, |mem| mem.read(ptr, len)) else {
            return;
        };
        let msg = String::from_utf8_lossy(&bytes).into_owned();
        let plugin = c.data().plugin.clone();
        match level {
            0 => hostlog::debug(&plugin, &msg),
            1 => hostlog::info(&plugin, &msg),
            2 => hostlog::warn(&plugin, &msg),
            _ => hostlog::error(&plugin, &msg),
        }
    })?;

    linker.func_wrap(m, im::RUN_JOB, |mut c: Caller<'_, StoreData>, cmd_ptr: i32, cmd_len: i32, cwd_ptr: i32, cwd_len: i32| -> i64 {
        ret_i64(with_mem(&mut c, |mem| {
            dispatch::run_job(mem, cmd_ptr, cmd_len, cwd_ptr, cwd_len)
        }))
    })?;

    linker.func_wrap(m, im::RUN_COMMAND, |mut c: Caller<'_, StoreData>, cmd_ptr: i32, cmd_len: i32| -> i64 {
        ret_i64(with_mem(&mut c, |mem| dispatch::run_command(mem, cmd_ptr, cmd_len)))
    })?;

    linker.func_wrap(m, im::TIMER_START, |mut c: Caller<'_, StoreData>, ms: i64| -> i64 {
        ret_i64(with_mem(&mut c, |mem| dispatch::timer_start(mem, ms)))
    })?;

    linker.func_wrap(m, im::FS_WRITE, |mut c: Caller<'_, StoreData>, path_ptr: i32, path_len: i32, data_ptr: i32, data_len: i32, append: i32| -> i64 {
        ret_i64(with_mem(&mut c, |mem| {
            dispatch::fs_write_async(mem, path_ptr, path_len, data_ptr, data_len, append)
        }))
    })?;

    linker.func_wrap(m, im::FS_READ, |mut c: Caller<'_, StoreData>, path_ptr: i32, path_len: i32, offset: i64, out_ptr: i32, out_cap: i32| -> i64 {
        ret_i64(with_mem(&mut c, |mem| {
            dispatch::fs_read_async(mem, path_ptr, path_len, offset, out_ptr, out_cap)
        }))
    })?;

    linker.func_wrap(m, im::FS_WRITE_SYNC, |mut c: Caller<'_, StoreData>, path_ptr: i32, path_len: i32, data_ptr: i32, data_len: i32, append: i32| -> i64 {
        ret_i64(with_mem(&mut c, |mem| {
            dispatch::fs_write_sync(mem, path_ptr, path_len, data_ptr, data_len, append)
        }))
    })?;

    linker.func_wrap(m, im::FS_ROOT, |mut c: Caller<'_, StoreData>, out: i32, cap: i32, len_out: i32| -> i32 {
        ret_i32(with_mem(&mut c, |mem| dispatch::fs_root(mem, out, cap, len_out)))
    })?;

    linker.func_wrap(m, im::FS_READ_SYNC, |mut c: Caller<'_, StoreData>, path_ptr: i32, path_len: i32, offset: i64, out: i32, cap: i32, len_out: i32, eof_out: i32| -> i32 {
        ret_i32(with_mem(&mut c, |mem| {
            dispatch::fs_read_sync(mem, path_ptr, path_len, offset, out, cap, len_out, eof_out)
        }))
    })?;

    Ok(())
}
