//! Event queue processing: enqueue from the C bridge, instantiate scoped
//! plugins, route events to guests, drain at safe points, and teardown.
//!
//! Events are binary buffers (header + field block, see abi-types). The
//! host routes on the fixed header alone - event id and scope ids at fixed
//! offsets - and never parses the field block, except to read the target
//! plugin name of a plugin-command event.
//!
//! Borrow discipline: the registry borrow is NEVER held across a guest
//! call. Instances are checked out of their slab slot, the guest runs, then
//! the instance is checked back in (or dropped by the failure policy).
//! Guest code may call host imports -> dispatch -> vtable -> pgh_notify,
//! which touches only the EVENTS cell.

use std::time::{Duration, Instant};

use tmux_plugin_abi::{
    patch_event_seq, EventHeader, EventScope, FieldReader, KeyRef, ScopeType,
    ValueRef,
};

use crate::abi;
use crate::hostlog;
use crate::intern as interner;
use crate::registry::{Instance, InstanceStats, ScopeId};
use crate::state::{Delivery, EVENTS, REGISTRY};

/// Default per-drain wall-clock budget when the caller passes 0.
const DEFAULT_DRAIN_BUDGET_US: u32 = 2000;

/// Does an instance at `scope` receive an event with `ev` scope?
/// Server-scoped instances see everything; scoped instances see events
/// touching their object.
pub fn scope_matches(scope: ScopeId, ev: &EventScope) -> bool {
    match scope {
        ScopeId::Server => true,
        ScopeId::Session(id) => ev.session == Some(id),
        ScopeId::Window(id) => ev.window == Some(id),
        ScopeId::Pane(id) => ev.pane == Some(id),
    }
}

/// Enqueue a raw binary event from the C bridge. ENQUEUE ONLY - the one
/// pgh entry point that vtable callbacks may legally re-enter.
pub fn enqueue_raw(bytes: Vec<u8>) {
    EVENTS.with(|e| {
        let mut q = e.borrow_mut();
        q.seq += 1;
        let seq = q.seq;
        q.deliveries.push_back(Delivery::RawEvent { bytes, seq });
    });
}

/// Queue instantiation work for a plugin (after pgh_plugin_load).
pub fn queue_instantiations(plugin: &str, scopes: Vec<ScopeId>) {
    EVENTS.with(|e| {
        let mut q = e.borrow_mut();
        for scope in scopes {
            q.deliveries.push_back(Delivery::Instantiate {
                plugin: plugin.to_string(),
                scope,
            });
        }
    });
}

/// Teardown for a dead tmux object: MARK AND QUEUE ONLY. Callable from deep
/// inside tmux teardown paths, so no guest code runs here; matching
/// instances move to the dying list and their on_unload runs at the next
/// drain.
pub fn object_destroyed(scope: ScopeId) {
    REGISTRY.with(|r| {
        let mut reg = r.borrow_mut();
        let keys: Vec<usize> = reg
            .instances
            .iter()
            .filter(|(_, slot)| {
                slot.as_ref().is_some_and(|i| i.scope_id == scope)
            })
            .map(|(k, _)| k)
            .collect();
        for key in keys {
            if let Some(inst) = reg.instances.remove(key) {
                reg.by_scope.remove(&(inst.plugin.clone(), inst.scope_id));
                reg.dying.push(inst);
            }
        }
    });
}

/// Run queued plugin work under a wall-clock budget; returns remaining
/// queued deliveries (the C side reschedules when nonzero).
pub fn drain(max_us: u32) -> u32 {
    let budget = if max_us == 0 { DEFAULT_DRAIN_BUDGET_US } else { max_us };
    let deadline = Instant::now() + Duration::from_micros(u64::from(budget));

    process_dying();

    loop {
        let delivery = EVENTS.with(|e| e.borrow_mut().deliveries.pop_front());
        let Some(delivery) = delivery else { break };

        match delivery {
            Delivery::RawEvent { mut bytes, seq } => {
                if patch_event_seq(&mut bytes, seq).is_err() {
                    hostlog::error("host", "truncated bridge event buffer");
                } else {
                    match EventHeader::parse(&bytes) {
                        Ok((header, _)) => {
                            instantiate_for_created(&header);
                            route_event(&header, &bytes);
                        }
                        Err(e) => hostlog::error(
                            "host",
                            &format!("bad bridge event buffer: {e}"),
                        ),
                    }
                }
            }
            Delivery::Instantiate { plugin, scope } => {
                instantiate_scope(&plugin, scope);
            }
            Delivery::AsyncComplete { token, err, v0, v1, data } => {
                deliver_async(token, err, v0, v1, &data);
            }
            Delivery::ModeEvent { mode_id, bytes } => {
                deliver_mode_event(mode_id, bytes);
            }
        }

        // Unloads queued by work above run in the same slice.
        process_dying();

        if Instant::now() >= deadline {
            break;
        }
    }

    EVENTS.with(|e| e.borrow().deliveries.len() as u32)
}

/// Run on_unload (tiny budget) for instances whose object died or whose
/// plugin was unloaded, then drop their stores.
fn process_dying() {
    loop {
        let inst = REGISTRY.with(|r| r.borrow_mut().dying.pop());
        let Some(mut inst) = inst else { break };
        let outcome = inst.guest.call_on_unload();
        if let Err(e) = &outcome.result {
            hostlog::debug(
                &inst.plugin,
                &format!("on_unload failed (ignored): {e}"),
            );
        }
        release_instance_resources(&inst);
        hostlog::debug(
            &inst.plugin,
            &format!("instance {} unloaded", inst.scope_id),
        );
        REGISTRY.with(|r| {
            let reg = r.borrow();
            if let Some(engine) = &reg.engine {
                engine.instance_removed();
            }
        });
        drop(inst);
    }
}

/// Release a dying instance's C-side resources: drop its pending tokens,
/// cancel its live timers and force-close its open modes. The mode_close
/// vtable call only *schedules* the pane teardown (deferred to a safe
/// point), so this never destroys tmux objects synchronously.
pub fn release_instance_resources(inst: &Instance) {
    // In-flight fs jobs touch the instance's pinned guest memory; block
    // until they finish before anything can drop the store. Local file
    // I/O, so bounded.
    crate::fsworker::wait_for_instance(
        &inst.plugin,
        inst.scope_id,
        inst.generation,
    );
    let timers =
        crate::tokens::purge_instance(&inst.plugin, inst.scope_id, inst.generation);
    let modes =
        crate::modes::purge_instance(&inst.plugin, inst.scope_id, inst.generation);
    if timers.is_empty() && modes.is_empty() {
        return;
    }
    let Some(vt) = crate::vtable() else { return };
    for id in timers {
        unsafe { (vt.timer_cancel)(id) };
    }
    for id in modes {
        unsafe { (vt.mode_close)(id) };
    }
}

/// Deliver an async completion to the owning instance, generation-checked.
fn deliver_async(token: u64, err: i32, v0: i64, v1: i64, data: &[u8]) {
    // Unknown token: instance already torn down (tokens purged) or the
    // token was cancelled - drop silently.
    let Some(pending) = crate::tokens::take(token) else { return };

    let key = REGISTRY.with(|r| {
        r.borrow()
            .by_scope
            .get(&(pending.plugin.clone(), pending.scope))
            .copied()
    });
    let Some(key) = key else { return };

    let inst = REGISTRY.with(|r| {
        r.borrow_mut().instances.get_mut(key).and_then(Option::take)
    });
    let Some(mut inst) = inst else { return };

    // A new generation at the same scope must not receive completions from
    // the old instance's requests.
    if inst.generation != pending.generation {
        REGISTRY.with(|r| {
            if let Some(slot) = r.borrow_mut().instances.get_mut(key) {
                *slot = Some(inst);
            }
        });
        return;
    }

    let outcome = inst.guest.call_on_async_complete(token, err, v0, v1, data);
    inst.stats.record(&outcome);
    let trapped = outcome.trapped();
    if let Err(e) = &outcome.result {
        hostlog::error(
            &inst.plugin,
            &format!("on_async_complete trapped: {e}"),
        );
    }
    check_in(key, inst, trapped, true);
}

/// Deliver a mode event to the instance owning the mode, generation-
/// checked. Mode events are targeted (never broadcast) and need no
/// subscription; the C side builds the complete event buffer (including
/// the mode field).
fn deliver_mode_event(mode_id: u64, mut bytes: Vec<u8>) {
    let header = match EventHeader::parse(&bytes) {
        Ok((h, _)) => h,
        Err(e) => {
            hostlog::error("host", &format!("bad mode event buffer: {e}"));
            return;
        }
    };
    // Unknown mode: owner already torn down (modes purged) or the mode was
    // closed - drop silently. mode-closed is terminal: the C-side registry
    // entry is already gone, so drop ours too.
    let owner = if header.event_id == interner::intern("mode-closed") {
        crate::modes::take(mode_id)
    } else {
        crate::modes::owner_of(mode_id)
    };
    let Some(owner) = owner else { return };

    let key = REGISTRY.with(|r| {
        r.borrow()
            .by_scope
            .get(&(owner.plugin.clone(), owner.scope))
            .copied()
    });
    let Some(key) = key else { return };

    let inst = REGISTRY.with(|r| {
        r.borrow_mut().instances.get_mut(key).and_then(Option::take)
    });
    let Some(mut inst) = inst else { return };

    // A new generation at the same scope must not receive events from the
    // old instance's modes.
    if inst.generation != owner.generation {
        REGISTRY.with(|r| {
            if let Some(slot) = r.borrow_mut().instances.get_mut(key) {
                *slot = Some(inst);
            }
        });
        return;
    }

    let seq = EVENTS.with(|e| {
        let mut q = e.borrow_mut();
        q.seq += 1;
        q.seq
    });
    let _ = patch_event_seq(&mut bytes, seq);

    let outcome = inst.guest.call_on_event(&bytes);
    inst.stats.record(&outcome);
    let trapped = outcome.trapped();
    if let Err(e) = &outcome.result {
        hostlog::error(
            &inst.plugin,
            &format!("on_event(mode {mode_id}) trapped: {e}"),
        );
    }
    check_in(key, inst, trapped, true);
}

/// Return a checked-out instance to its slot, or apply the failure policy
/// if the guest trapped. `ran` = a guest call actually happened.
fn check_in(key: usize, inst: Instance, trapped: bool, ran: bool) {
    REGISTRY.with(|r| {
        let mut reg = r.borrow_mut();
        if trapped {
            let plugin = inst.plugin.clone();
            reg.by_scope.remove(&(inst.plugin.clone(), inst.scope_id));
            reg.instances.try_remove(key);
            if let Some(engine) = &reg.engine {
                engine.instance_removed();
            }
            drop(reg);
            release_instance_resources(&inst);
            REGISTRY.with(|r2| {
                r2.borrow_mut()
                    .record_failure(&plugin, "guest trap in callback");
            });
        } else {
            if ran {
                reg.record_success(&inst.plugin);
            }
            if let Some(slot) = reg.instances.get_mut(key) {
                *slot = Some(inst);
            }
        }
    });
}

/// Eagerly create scoped instances when an object-creation event arrives.
fn instantiate_for_created(header: &EventHeader) {
    let id = header.event_id;
    let (scope_type, scope) = if id == interner::intern("session-created") {
        match header.scope.session {
            Some(id) => (ScopeType::Session, ScopeId::Session(id)),
            None => return,
        }
    } else if id == interner::intern("window-created") {
        match header.scope.window {
            Some(id) => (ScopeType::Window, ScopeId::Window(id)),
            None => return,
        }
    } else if id == interner::intern("pane-created") {
        match header.scope.pane {
            Some(id) => (ScopeType::Pane, ScopeId::Pane(id)),
            None => return,
        }
    } else {
        return;
    };

    let plugins: Vec<String> = REGISTRY.with(|r| {
        let reg = r.borrow();
        reg.plugins
            .values()
            .filter(|d| {
                d.scope_type == scope_type
                    && d.state == crate::registry::PluginState::Running
                    && !reg.by_scope.contains_key(&(d.name.clone(), scope))
            })
            .map(|d| d.name.clone())
            .collect()
    });
    for plugin in plugins {
        instantiate_scope(&plugin, scope);
    }
}

/// Build (instantiate + handshake + init) a fresh guest for a plugin at a
/// scope, WITHOUT inserting it into the registry. The registry borrow is
/// released before any guest code runs; Engine/Module handles are cheap Arc
/// clones. Used by scoped instantiation and by reloads.
pub fn build_guest(
    plugin: &str,
    scope: ScopeId,
) -> Result<Option<(abi::Guest, u64, InstanceStats)>, String> {
    // Phase 1 (borrow): gather engine/module handles and a generation.
    let setup = REGISTRY.with(|r| {
        let mut reg = r.borrow_mut();
        let Some(def) = reg.plugins.get(plugin) else {
            return Ok(None);
        };
        if def.state != crate::registry::PluginState::Running {
            return Ok(None);
        }
        let Some(module) = reg.modules.get(&def.hash).cloned() else {
            return Err(format!("no compiled module for {plugin}"));
        };
        let config = abi::encode_config(&def.config);
        let caps = def.caps.clone();
        let Some(engine) = reg.engine.as_ref().map(|e| e.engine.clone())
        else {
            return Err("no engine".to_string());
        };
        reg.next_generation += 1;
        Ok(Some((engine, module, config, caps, reg.next_generation)))
    })?;
    let Some((engine, module, config, caps, generation)) = setup else {
        return Ok(None);
    };

    // Phase 2 (no borrow): instantiate + handshake + init under budget.
    let mut guest =
        abi::instantiate(&engine, &module, plugin, generation, scope, caps)
            .map_err(|e| format!("instantiate for {scope}: {e}"))?;
    let outcome = guest.call_init(&config);
    let mut stats = InstanceStats::default();
    stats.record(&outcome);
    if let Err(e) = &outcome.result {
        return Err(format!("init for {scope}: {e}"));
    }
    Ok(Some((guest, generation, stats)))
}

/// Create and init one instance and insert it into the registry.
fn instantiate_scope(plugin: &str, scope: ScopeId) {
    let exists = REGISTRY.with(|r| {
        r.borrow().by_scope.contains_key(&(plugin.to_string(), scope))
    });
    if exists {
        return;
    }

    let built = match build_guest(plugin, scope) {
        Ok(Some(b)) => b,
        Ok(None) => return,
        Err(e) => {
            fail(plugin, &e);
            return;
        }
    };
    let (guest, generation, stats) = built;

    // Phase 3 (borrow): check in.
    REGISTRY.with(|r| {
        let mut reg = r.borrow_mut();
        if !reg.is_running(plugin) {
            return; // disabled while init ran
        }
        let key = reg.instances.insert(Some(Instance {
            plugin: plugin.to_string(),
            generation,
            scope_id: scope,
            guest,
            stats,
        }));
        reg.by_scope.insert((plugin.to_string(), scope), key);
        reg.record_success(plugin);
        if let Some(engine) = &reg.engine {
            engine.instance_added();
        }
        hostlog::debug(
            plugin,
            &format!("instance {scope} started (generation {generation})"),
        );
    });
}

fn fail(plugin: &str, what: &str) {
    REGISTRY.with(|r| {
        r.borrow_mut().record_failure(plugin, what);
    });
}

/// For a plugin-command event, read the target plugin name from the field
/// block (the one field the host ever reads out of an event).
fn plugin_command_target(bytes: &[u8]) -> Option<String> {
    let (_, cursor) = EventHeader::parse(bytes).ok()?;
    let plugin_key = interner::intern("plugin");
    for field in FieldReader::from_cursor(cursor).ok()? {
        let (key, value) = field.ok()?;
        if key == KeyRef::Id(plugin_key) {
            if let ValueRef::Str(s) = value {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// Fan an event out to matching, subscribed instances.
fn route_event(header: &EventHeader, bytes: &[u8]) {
    let implicit = interner::implicit(header.event_id);
    // plugin-command events are addressed to one plugin by name; others
    // must not see them even when subscribed.
    let command_target = if header.event_id == interner::intern("plugin-command")
    {
        match plugin_command_target(bytes) {
            Some(t) => Some(t),
            None => return, // malformed: no target, deliver to nobody
        }
    } else {
        None
    };

    let keys: Vec<usize> = REGISTRY.with(|r| {
        let reg = r.borrow();
        reg.instances
            .iter()
            .filter(|(_, slot)| {
                slot.as_ref()
                    .is_some_and(|i| scope_matches(i.scope_id, &header.scope))
            })
            .map(|(k, _)| k)
            .collect()
    });

    for key in keys {
        // Check out (the slot stays, holding None, so the key is stable).
        let inst = REGISTRY.with(|r| {
            r.borrow_mut().instances.get_mut(key).and_then(Option::take)
        });
        let Some(mut inst) = inst else { continue };

        let subscribed = (implicit
            || inst
                .guest
                .store
                .data()
                .subscriptions
                .contains(&header.event_id))
            && command_target
                .as_deref()
                .is_none_or(|t| t == inst.plugin.as_str());

        let mut trapped = false;
        if subscribed {
            let outcome = inst.guest.call_on_event(bytes);
            inst.stats.record(&outcome);
            trapped = outcome.trapped();
            if let Err(e) = &outcome.result {
                hostlog::error(
                    &inst.plugin,
                    &format!("on_event({}) trapped: {e}", header.event_id),
                );
            }
        }
        check_in(key, inst, trapped, subscribed);
    }
}
