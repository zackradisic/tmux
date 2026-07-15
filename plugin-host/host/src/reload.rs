//! Load/upsert and reload semantics.
//!
//! `upsert` implements the reconcile rules for one plugin:
//!   same code + same config + same caps  -> keep running untouched
//!   changed code                         -> transactional snapshot/migrate
//!   changed config                       -> on_config_changed or restart
//!   changed caps or scope                -> restart instances
//!
//! The code-reload transaction is per instance: build v2 first, migrate the
//! v1 snapshot into it, and only then unload v1. Any v2 failure leaves v1
//! running untouched. Async completions from the old generation are dropped
//! by the token generation check.

use tmux_plugin_abi::LoadDescriptor;

use crate::events::build_guest;
use crate::hostlog;
use crate::registry::{Instance, PluginState, ScopeId};
use crate::state::REGISTRY;
use crate::{events, tokens};

/// Load or update a plugin definition. Returns a short human-readable
/// outcome ("loaded", "unchanged", "code reloaded", ...).
pub fn upsert(desc: LoadDescriptor) -> Result<&'static str, String> {
    let existing = REGISTRY.with(|r| {
        let reg = r.borrow();
        reg.plugins.get(&desc.name).map(|d| {
            (d.hash, d.config.clone(), d.caps.clone(), d.scope_type)
        })
    });

    let Some((old_hash, old_config, old_caps, old_scope)) = existing else {
        // Fresh load.
        let name = desc.name.clone();
        REGISTRY.with(|r| r.borrow_mut().load(desc))?;
        let scopes = REGISTRY.with(|r| r.borrow().initial_scopes(&name));
        events::queue_instantiations(&name, scopes);
        return Ok("loaded");
    };

    // Existing definition: compare against the new descriptor.
    let bytes = std::fs::read(&desc.path)
        .map_err(|e| format!("{}: {e}", desc.path))?;
    let new_hash = blake3::hash(&bytes);
    let new_caps = crate::caps::compute(
        std::path::Path::new(&desc.path),
        &desc.caps,
    )?;

    if desc.scope != old_scope {
        // Scope change: full restart via unload + load.
        let name = desc.name.clone();
        REGISTRY.with(|r| r.borrow_mut().unload(&name));
        REGISTRY.with(|r| r.borrow_mut().load(desc))?;
        let scopes = REGISTRY.with(|r| r.borrow().initial_scopes(&name));
        events::queue_instantiations(&name, scopes);
        return Ok("scope changed, restarted");
    }

    if new_hash != old_hash {
        // Code reload: registry.load updates hash/config/caps and compiles
        // the new module, but must not tear down instances - swap them
        // transactionally instead. load() unloads, so update def by hand.
        let engine_module = REGISTRY.with(|r| {
            let mut reg = r.borrow_mut();
            reg.plugins.get(&desc.name)?;
            let engine = reg.engine.as_ref()?.engine.clone();
            Some((engine, reg.modules.contains_key(&new_hash)))
        });
        let Some((engine, cached)) = engine_module else {
            return Err("plugin/engine vanished".into());
        };
        if !cached {
            let module = wasmtime::Module::new(&engine, &bytes)
                .map_err(|e| format!("compile: {e:#}"))?;
            crate::abi::validate_module(&module)?;
            REGISTRY.with(|r| {
                r.borrow_mut().modules.insert(new_hash, module);
            });
        }
        REGISTRY.with(|r| {
            let mut reg = r.borrow_mut();
            if let Some(def) = reg.plugins.get_mut(&desc.name) {
                def.hash = new_hash;
                def.config = desc.config.clone();
                def.caps = new_caps.clone();
                def.path = std::path::PathBuf::from(&desc.path);
                def.state = PluginState::Running;
                def.failure_count = 0;
            }
        });
        swap_instances(&desc.name, true);
        return Ok("code reloaded");
    }

    if new_caps != old_caps {
        REGISTRY.with(|r| {
            let mut reg = r.borrow_mut();
            if let Some(def) = reg.plugins.get_mut(&desc.name) {
                def.caps = new_caps;
                def.config = desc.config.clone();
            }
        });
        swap_instances(&desc.name, false);
        return Ok("caps changed, restarted");
    }

    if desc.config != old_config {
        REGISTRY.with(|r| {
            let mut reg = r.borrow_mut();
            if let Some(def) = reg.plugins.get_mut(&desc.name) {
                def.config = desc.config.clone();
            }
        });
        apply_config(&desc.name, &desc.config.to_string());
        return Ok("config changed");
    }

    Ok("unchanged")
}

/// Reload a plugin from disk (scope reload / explicit reload-plugin): even
/// with unchanged code, every instance is swapped with state migration and
/// a new generation.
pub fn reload(name: &str) -> Result<(), String> {
    let desc = REGISTRY.with(|r| {
        let reg = r.borrow();
        reg.plugins.get(name).map(|d| {
            (d.path.clone(), d.config.clone())
        })
    });
    let Some((path, _)) = desc else {
        return Err(format!("unknown plugin: {name}"));
    };

    let bytes =
        std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let new_hash = blake3::hash(&bytes);
    let need_compile = REGISTRY.with(|r| {
        let reg = r.borrow();
        !reg.modules.contains_key(&new_hash)
    });
    if need_compile {
        let engine = REGISTRY
            .with(|r| r.borrow().engine.as_ref().map(|e| e.engine.clone()))
            .ok_or("no engine")?;
        let module = wasmtime::Module::new(&engine, &bytes)
            .map_err(|e| format!("compile: {e:#}"))?;
        crate::abi::validate_module(&module)?;
        REGISTRY.with(|r| {
            r.borrow_mut().modules.insert(new_hash, module);
        });
    }
    REGISTRY.with(|r| {
        let mut reg = r.borrow_mut();
        if let Some(def) = reg.plugins.get_mut(name) {
            def.hash = new_hash;
            def.state = PluginState::Running;
            def.failure_count = 0;
        }
    });
    swap_instances(name, true);
    Ok(())
}

/// Enable or disable a plugin. Disabling tears down instances (on_unload at
/// the next drain); enabling re-instantiates for current objects.
pub fn set_enabled(name: &str, enabled: bool) -> Result<(), String> {
    let known =
        REGISTRY.with(|r| r.borrow().plugins.contains_key(name));
    if !known {
        return Err(format!("unknown plugin: {name}"));
    }
    if enabled {
        REGISTRY.with(|r| {
            let mut reg = r.borrow_mut();
            if let Some(def) = reg.plugins.get_mut(name) {
                def.state = PluginState::Running;
                def.failure_count = 0;
            }
        });
        let scopes = REGISTRY.with(|r| r.borrow().initial_scopes(name));
        events::queue_instantiations(name, scopes);
    } else {
        REGISTRY.with(|r| {
            let mut reg = r.borrow_mut();
            if let Some(def) = reg.plugins.get_mut(name) {
                def.state = PluginState::Disabled {
                    reason: "disabled by user".into(),
                };
            }
            let keys: Vec<usize> = reg
                .instances
                .iter()
                .filter(|(_, s)| {
                    s.as_ref().is_some_and(|i| i.plugin == name)
                })
                .map(|(k, _)| k)
                .collect();
            for key in keys {
                if let Some(inst) = reg.instances.remove(key) {
                    reg.by_scope
                        .remove(&(inst.plugin.clone(), inst.scope_id));
                    reg.dying.push(inst);
                }
            }
        });
    }
    Ok(())
}

/// Swap every live instance of a plugin for a freshly-built one. With
/// `migrate`, v1 state is snapshotted and offered to v2; a migration
/// failure keeps v1 running (per-instance atomicity).
fn swap_instances(name: &str, migrate: bool) {
    let keys: Vec<(usize, ScopeId)> = REGISTRY.with(|r| {
        let reg = r.borrow();
        reg.instances
            .iter()
            .filter_map(|(k, s)| {
                s.as_ref()
                    .filter(|i| i.plugin == name)
                    .map(|i| (k, i.scope_id))
            })
            .collect()
    });

    for (key, scope) in keys {
        // Check out v1.
        let inst = REGISTRY.with(|r| {
            r.borrow_mut().instances.get_mut(key).and_then(Option::take)
        });
        let Some(mut old) = inst else { continue };

        let snapshot = if migrate { old.guest.call_snapshot() } else { None };

        // Build v2 (instantiate + init).
        let (mut guest, generation, mut stats) = match build_guest(name, scope)
        {
            Ok(Some(b)) => b,
            Ok(None) => {
                // Definition vanished/disabled meanwhile: keep v1.
                REGISTRY.with(|r| {
                    if let Some(slot) = r.borrow_mut().instances.get_mut(key) {
                        *slot = Some(old);
                    }
                });
                continue;
            }
            Err(e) => {
                // v2 failed: keep v1 running untouched.
                hostlog::error(
                    name,
                    &format!("reload failed, keeping old instance: {e}"),
                );
                REGISTRY.with(|r| {
                    if let Some(slot) = r.borrow_mut().instances.get_mut(key) {
                        *slot = Some(old);
                    }
                });
                continue;
            }
        };

        // Migrate state; failure keeps v1.
        if let Some((version, bytes)) = &snapshot {
            if let Err(e) = guest.call_migrate(*version, bytes) {
                hostlog::error(
                    name,
                    &format!(
                        "migration to new code failed for {scope}: {e}; keeping old instance"
                    ),
                );
                REGISTRY.with(|r| {
                    if let Some(slot) =
                        r.borrow_mut().instances.get_mut(key)
                    {
                        *slot = Some(old);
                    }
                });
                continue;
            }
            stats.callbacks += 1;
        }

        // Commit: unload v1, swap in v2 under a new generation.
        let _ = old.guest.call_on_unload();
        let timers =
            tokens::purge_instance(&old.plugin, old.scope_id, old.generation);
        if let Some(vt) = crate::vtable() {
            for id in timers {
                unsafe { (vt.timer_cancel)(id) };
            }
        }
        REGISTRY.with(|r| {
            let mut reg = r.borrow_mut();
            if let Some(slot) = reg.instances.get_mut(key) {
                *slot = Some(Instance {
                    plugin: name.to_string(),
                    generation,
                    scope_id: scope,
                    guest,
                    stats,
                });
            }
            // by_scope key -> same slab key; engine live count unchanged.
        });
        hostlog::info(
            name,
            &format!("instance {scope} reloaded (generation {generation})"),
        );
        drop(old);
    }
}

/// Offer changed config to each instance; restart those that refuse.
fn apply_config(name: &str, config_json: &str) {
    let keys: Vec<(usize, ScopeId)> = REGISTRY.with(|r| {
        let reg = r.borrow();
        reg.instances
            .iter()
            .filter_map(|(k, s)| {
                s.as_ref()
                    .filter(|i| i.plugin == name)
                    .map(|i| (k, i.scope_id))
            })
            .collect()
    });

    let mut restart: Vec<(usize, ScopeId)> = Vec::new();
    for (key, scope) in keys {
        let inst = REGISTRY.with(|r| {
            r.borrow_mut().instances.get_mut(key).and_then(Option::take)
        });
        let Some(mut inst) = inst else { continue };
        let absorbed =
            inst.guest.call_on_config_changed(config_json).unwrap_or(false);
        REGISTRY.with(|r| {
            if let Some(slot) = r.borrow_mut().instances.get_mut(key) {
                *slot = Some(inst);
            }
        });
        if !absorbed {
            restart.push((key, scope));
        }
    }

    if !restart.is_empty() {
        // Rebuild refusing instances with the new config (fresh init).
        swap_instances_subset(name, &restart);
    }
}

fn swap_instances_subset(name: &str, subset: &[(usize, ScopeId)]) {
    for &(key, scope) in subset {
        let inst = REGISTRY.with(|r| {
            r.borrow_mut().instances.get_mut(key).and_then(Option::take)
        });
        let Some(mut old) = inst else { continue };
        let built = match build_guest(name, scope) {
            Ok(Some(b)) => b,
            _ => {
                REGISTRY.with(|r| {
                    if let Some(slot) =
                        r.borrow_mut().instances.get_mut(key)
                    {
                        *slot = Some(old);
                    }
                });
                continue;
            }
        };
        let (guest, generation, stats) = built;
        let _ = old.guest.call_on_unload();
        let timers =
            tokens::purge_instance(&old.plugin, old.scope_id, old.generation);
        if let Some(vt) = crate::vtable() {
            for id in timers {
                unsafe { (vt.timer_cancel)(id) };
            }
        }
        REGISTRY.with(|r| {
            if let Some(slot) = r.borrow_mut().instances.get_mut(key) {
                *slot = Some(Instance {
                    plugin: name.to_string(),
                    generation,
                    scope_id: scope,
                    guest,
                    stats,
                });
            }
        });
    }
}
