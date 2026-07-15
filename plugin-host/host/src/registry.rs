//! Plugin registry: definitions, instances, generations, load/unload.
//!
//! Main-thread only (see state.rs). During a guest call the Instance is
//! *taken out* of its slab slot so the registry borrow is released before
//! any guest/vtable work happens; the split into check-out / call / check-in
//! phases is what makes vtable re-entry into pgh_notify safe.

use std::collections::HashMap;
use std::ffi::c_void;
use std::fmt::Write as _;
use std::os::raw::c_char;
use std::path::PathBuf;

use serde_json::Value;
use slab::Slab;
use tmux_plugin_abi::{LoadDescriptor, ScopeType};
use wasmtime::Module;

use crate::abi::{self, Guest};
use crate::engine::EngineState;
use crate::ffi::{PGH_OBJ_PANE, PGH_OBJ_SESSION, PGH_OBJ_WINDOW};
use crate::hostlog;

/// Consecutive failures before a plugin is disabled until explicit reload.
pub const MAX_FAILURES: u32 = 3;

/// The scope a concrete instance is bound to. tmux ids are monotonic u32s,
/// never reused within a server lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScopeId {
    Server,
    Session(u32),
    Window(u32),
    Pane(u32),
}

impl std::fmt::Display for ScopeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScopeId::Server => write!(f, "server"),
            ScopeId::Session(id) => write!(f, "${id}"),
            ScopeId::Window(id) => write!(f, "@{id}"),
            ScopeId::Pane(id) => write!(f, "%{id}"),
        }
    }
}

impl ScopeId {
    pub fn to_json(self) -> Value {
        match self {
            ScopeId::Server => serde_json::json!({ "type": "server" }),
            ScopeId::Session(id) => {
                serde_json::json!({ "type": "session", "id": id })
            }
            ScopeId::Window(id) => {
                serde_json::json!({ "type": "window", "id": id })
            }
            ScopeId::Pane(id) => serde_json::json!({ "type": "pane", "id": id }),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PluginState {
    Running,
    Disabled { reason: String },
    LoadFailed { error: String },
}

pub struct PluginDef {
    pub name: String,
    pub path: PathBuf,
    pub hash: blake3::Hash,
    pub scope_type: ScopeType,
    pub config: Value,
    pub caps: crate::caps::EffectiveCaps,
    pub state: PluginState,
    /// Consecutive failures; reset by any clean callback return.
    pub failure_count: u32,
}

pub struct Instance {
    pub plugin: String,
    pub generation: u64,
    pub scope_id: ScopeId,
    pub guest: Guest,
    pub stats: InstanceStats,
}

#[derive(Debug, Default, Clone)]
pub struct InstanceStats {
    pub callbacks: u64,
    pub soft_overruns: u64,
    pub traps: u64,
    pub total_ns: u64,
}

impl InstanceStats {
    pub fn record<T>(&mut self, outcome: &abi::CallOutcome<T>) {
        self.callbacks += 1;
        self.total_ns += outcome.elapsed_ns;
        if outcome.soft_warned {
            self.soft_overruns += 1;
        }
        if outcome.trapped() {
            self.traps += 1;
        }
    }
}

/// Slot wrapper: `None` while the Instance is checked out for a guest call.
pub type InstanceSlot = Option<Instance>;

pub struct Registry {
    /// Lazily created on first plugin load (a wasmtime engine is not free
    /// and most servers run no plugins).
    pub engine: Option<EngineState>,
    /// Compiled-module cache keyed by file content hash; the same hash
    /// drives code-change detection on reload.
    pub modules: HashMap<blake3::Hash, Module>,
    pub plugins: HashMap<String, PluginDef>,
    pub instances: Slab<InstanceSlot>,
    pub by_scope: HashMap<(String, ScopeId), usize>,
    /// Instances awaiting guest on_unload + drop at the next drain
    /// (pgh_object_destroyed and unload are mark-and-queue only).
    pub dying: Vec<Instance>,
    pub next_generation: u64,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            engine: None,
            modules: HashMap::new(),
            plugins: HashMap::new(),
            instances: Slab::new(),
            by_scope: HashMap::new(),
            dying: Vec::new(),
            next_generation: 1,
        }
    }

    fn ensure_engine(&mut self) -> Result<(), String> {
        if self.engine.is_none() {
            self.engine = Some(
                EngineState::new().map_err(|e| format!("wasmtime engine: {e}"))?,
            );
        }
        Ok(())
    }

    /// Load (or replace) a plugin definition: read + hash + compile +
    /// validate the module and register the definition. Instantiation is
    /// queued by the caller (events::queue_instantiations) and happens at
    /// the next drain - guest code never runs inside pgh_plugin_load.
    pub fn load(&mut self, desc: LoadDescriptor) -> Result<(), String> {
        let bytes = std::fs::read(&desc.path)
            .map_err(|e| format!("{}: {e}", desc.path))?;
        let hash = blake3::hash(&bytes);
        let caps =
            crate::caps::compute(std::path::Path::new(&desc.path), &desc.caps)?;

        self.ensure_engine()?;
        let engine = self.engine.as_ref().unwrap().engine.clone();

        if !self.modules.contains_key(&hash) {
            let module = Module::new(&engine, &bytes)
                .map_err(|e| format!("compile: {e:#}"))?;
            abi::validate_module(&module)?;
            self.modules.insert(hash, module);
        }

        // Replacing an existing definition tears down its instances first
        // (simple restart semantics; transactional code reload is M8).
        if self.plugins.contains_key(&desc.name) {
            self.unload(&desc.name);
        }

        self.plugins.insert(
            desc.name.clone(),
            PluginDef {
                name: desc.name.clone(),
                path: PathBuf::from(&desc.path),
                hash,
                scope_type: desc.scope,
                config: desc.config,
                caps,
                state: PluginState::Running,
                failure_count: 0,
            },
        );
        hostlog::info(
            &desc.name,
            &format!("loaded ({}, scope {})", desc.path, desc.scope),
        );
        Ok(())
    }

    /// Remove a plugin definition; its instances move to `dying` for
    /// on_unload at the next drain. Returns false if unknown.
    pub fn unload(&mut self, name: &str) -> bool {
        if self.plugins.remove(name).is_none() {
            return false;
        }
        let keys: Vec<usize> = self
            .instances
            .iter()
            .filter(|(_, slot)| {
                slot.as_ref().is_some_and(|i| i.plugin == name)
            })
            .map(|(k, _)| k)
            .collect();
        for key in keys {
            if let Some(inst) = self.instances.remove(key) {
                self.by_scope.remove(&(inst.plugin.clone(), inst.scope_id));
                self.dying.push(inst);
            }
        }
        hostlog::info(name, "unloaded");
        true
    }

    /// Scope ids a fresh definition should be instantiated for right now:
    /// the server scope, or every currently-live object of the scope kind
    /// (enumerated through the vtable).
    pub fn initial_scopes(&self, name: &str) -> Vec<ScopeId> {
        let Some(def) = self.plugins.get(name) else {
            return Vec::new();
        };
        match def.scope_type {
            ScopeType::Server => vec![ScopeId::Server],
            ScopeType::Session => enumerate_ids(PGH_OBJ_SESSION)
                .into_iter()
                .map(ScopeId::Session)
                .collect(),
            ScopeType::Window => enumerate_ids(PGH_OBJ_WINDOW)
                .into_iter()
                .map(ScopeId::Window)
                .collect(),
            ScopeType::Pane => enumerate_ids(PGH_OBJ_PANE)
                .into_iter()
                .map(ScopeId::Pane)
                .collect(),
        }
    }

    /// Record a failure for a plugin; disables it after MAX_FAILURES
    /// consecutive ones. Returns true if the plugin was disabled.
    pub fn record_failure(&mut self, name: &str, what: &str) -> bool {
        let Some(def) = self.plugins.get_mut(name) else {
            return false;
        };
        def.failure_count += 1;
        hostlog::error(
            name,
            &format!("{what} (failure {}/{})", def.failure_count, MAX_FAILURES),
        );
        if def.failure_count >= MAX_FAILURES
            && def.state == PluginState::Running
        {
            let reason = format!("{} consecutive failures", def.failure_count);
            def.state = PluginState::Disabled { reason: reason.clone() };
            hostlog::error(name, "disabled until explicit reload");
            notify_state_changed(name, "disabled", &reason);
            return true;
        }
        false
    }

    pub fn record_success(&mut self, name: &str) {
        if let Some(def) = self.plugins.get_mut(name) {
            def.failure_count = 0;
        }
    }

    pub fn is_running(&self, name: &str) -> bool {
        self.plugins
            .get(name)
            .is_some_and(|d| d.state == PluginState::Running)
    }

    /// Human-readable plugin listing for `show-plugins` / `show-plugins -v`.
    /// Returned as preformatted text: the C side never parses JSON.
    pub fn query_text(&self, verbose: bool) -> String {
        if self.plugins.is_empty() {
            return String::from("no plugins loaded\n");
        }
        let mut out = String::new();
        let mut names: Vec<&String> = self.plugins.keys().collect();
        names.sort();
        for name in names {
            let def = &self.plugins[name];
            let state = match &def.state {
                PluginState::Running => "running".to_string(),
                PluginState::Disabled { reason } => format!("disabled ({reason})"),
                PluginState::LoadFailed { error } => format!("load failed ({error})"),
            };
            let ninstances = self
                .instances
                .iter()
                .filter(|(_, slot)| {
                    slot.as_ref().is_some_and(|i| &i.plugin == name)
                })
                .count();
            let _ = writeln!(
                out,
                "{}: scope {}, {}, {} instance{}, path {}",
                def.name,
                def.scope_type,
                state,
                ninstances,
                if ninstances == 1 { "" } else { "s" },
                def.path.display()
            );
            if verbose {
                let _ = writeln!(out, "  caps: {}", def.caps.describe());
                for (_, slot) in self.instances.iter() {
                    let Some(inst) = slot.as_ref() else { continue };
                    if &inst.plugin != name {
                        continue;
                    }
                    let _ = writeln!(
                        out,
                        "  instance {}: generation {}, {} callbacks, {} soft overruns, {} traps, {:.2}ms total",
                        inst.scope_id,
                        inst.generation,
                        inst.stats.callbacks,
                        inst.stats.soft_overruns,
                        inst.stats.traps,
                        inst.stats.total_ns as f64 / 1e6,
                    );
                }
            }
        }
        out
    }
}

/// Tell the user a plugin changed state (status line + message log).
pub fn notify_state_changed(plugin: &str, state: &str, reason: &str) {
    let Some(vt) = crate::vtable() else { return };
    let (Ok(p), Ok(s), Ok(r)) = (
        std::ffi::CString::new(plugin),
        std::ffi::CString::new(state),
        std::ffi::CString::new(reason),
    ) else {
        return;
    };
    unsafe { (vt.plugin_state_changed)(p.as_ptr(), s.as_ptr(), r.as_ptr()) };
}

/// Enumerate live object ids of a kind through the vtable.
fn enumerate_ids(kind: i32) -> Vec<u32> {
    #[derive(serde::Deserialize)]
    struct IdOnly {
        id: u32,
    }
    unsafe extern "C" fn sink(ctx: *mut c_void, ptr: *const c_char, len: usize) {
        let buf = &mut *(ctx as *mut Vec<u8>);
        buf.extend_from_slice(std::slice::from_raw_parts(ptr as *const u8, len));
    }
    let Some(vt) = crate::vtable() else { return Vec::new() };
    let mut buf: Vec<u8> = Vec::new();
    unsafe { (vt.list_objects)(kind, sink, &mut buf as *mut Vec<u8> as *mut c_void) };
    let parsed: Result<Vec<IdOnly>, _> =
        serde_json::from_str(&String::from_utf8_lossy(&buf));
    match parsed {
        Ok(objs) => objs.into_iter().map(|o| o.id).collect(),
        Err(e) => {
            hostlog::error("host", &format!("enumerate objects: {e}"));
            Vec::new()
        }
    }
}
