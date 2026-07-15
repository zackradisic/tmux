//! Event queue processing: enqueue from the C bridge, instantiate scoped
//! plugins, route events to guests, drain at safe points, and teardown.
//!
//! Borrow discipline: the registry borrow is NEVER held across a guest
//! call. Instances are checked out of their slab slot, the guest runs, then
//! the instance is checked back in (or dropped by the failure policy).
//! Guest code may call host imports -> dispatch -> vtable -> pgh_notify,
//! which touches only the EVENTS cell.

use std::time::{Duration, Instant};

use tmux_plugin_abi::{BridgeEvent, Event, EventScope, ScopeType};

use crate::abi;
use crate::hostlog;
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

/// Lifecycle events are delivered without an explicit subscription (a
/// scoped instance always learns about its object's world changing).
fn implicit_event(name: &str) -> bool {
    name.ends_with("-created") || name.ends_with("-destroyed")
        || name == "session-closed"
}

/// Enqueue a raw event from the C bridge. ENQUEUE ONLY - the one pgh entry
/// point that vtable callbacks may legally re-enter.
pub fn enqueue_raw(json: String) {
    EVENTS.with(|e| {
        let mut q = e.borrow_mut();
        q.seq += 1;
        let seq = q.seq;
        q.deliveries.push_back(Delivery::RawEvent { json, seq });
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
            Delivery::RawEvent { json, seq } => {
                match serde_json::from_str::<BridgeEvent>(&json) {
                    Ok(bridge) => {
                        let event = bridge.into_event(seq);
                        instantiate_for_created(&event);
                        route_event(&event);
                    }
                    Err(err) => hostlog::error(
                        "host",
                        &format!("bad bridge event JSON ({err}): {json}"),
                    ),
                }
            }
            Delivery::Instantiate { plugin, scope } => {
                instantiate_scope(&plugin, scope);
            }
            Delivery::AsyncComplete { token, json, is_error } => {
                deliver_async(token, &json, is_error);
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
        cancel_instance_tokens(&inst);
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

/// Drop a dying instance's pending tokens and cancel its live C timers.
fn cancel_instance_tokens(inst: &Instance) {
    let timers =
        crate::tokens::purge_instance(&inst.plugin, inst.scope_id, inst.generation);
    if timers.is_empty() {
        return;
    }
    let Some(vt) = crate::vtable() else { return };
    for id in timers {
        unsafe { (vt.timer_cancel)(id) };
    }
}

/// Deliver an async completion to the owning instance, generation-checked.
fn deliver_async(token: u64, json: &str, is_error: bool) {
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

    let outcome = inst.guest.call_on_async_complete(token, json, is_error);
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
            cancel_instance_tokens(&inst);
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
fn instantiate_for_created(event: &Event) {
    let (scope_type, scope) = match event.event.as_str() {
        "session-created" => match event.scope.session {
            Some(id) => (ScopeType::Session, ScopeId::Session(id)),
            None => return,
        },
        "window-created" => match event.scope.window {
            Some(id) => (ScopeType::Window, ScopeId::Window(id)),
            None => return,
        },
        "pane-created" => match event.scope.pane {
            Some(id) => (ScopeType::Pane, ScopeId::Pane(id)),
            None => return,
        },
        _ => return,
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
        let config = def.config.to_string();
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

/// Fan an event out to matching, subscribed instances.
fn route_event(event: &Event) {
    let json = match serde_json::to_string(event) {
        Ok(j) => j,
        Err(e) => {
            hostlog::error("host", &format!("event serialize: {e}"));
            return;
        }
    };

    let keys: Vec<usize> = REGISTRY.with(|r| {
        let reg = r.borrow();
        reg.instances
            .iter()
            .filter(|(_, slot)| {
                slot.as_ref()
                    .is_some_and(|i| scope_matches(i.scope_id, &event.scope))
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

        let subscribed = implicit_event(&event.event)
            || inst.guest.store.data().subscriptions.contains(&event.event);

        let mut trapped = false;
        if subscribed {
            let outcome = inst.guest.call_on_event(&json);
            inst.stats.record(&outcome);
            trapped = outcome.trapped();
            if let Err(e) = &outcome.result {
                hostlog::error(
                    &inst.plugin,
                    &format!("on_event({}) trapped: {e}", event.event),
                );
            }
        }
        check_in(key, inst, trapped, subscribed);
    }
}
