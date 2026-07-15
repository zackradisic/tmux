//! Wasmtime isolation layer.
//!
//! ALL wasmtime API usage lives in this file (and later abi.rs) so that a
//! wasmtime version bump is a localized change.
//!
//! CPU-budget model: the Engine has epoch interruption enabled and a ticker
//! thread bumps the epoch every TICK_MS milliseconds. Each call into a guest
//! sets a soft deadline; the first deadline hit logs a warning and extends to
//! the hard deadline, the second traps the guest. The ticker is the only
//! thread this crate creates and it touches nothing but the Engine (a cheap
//! atomic add), so the rest of the crate stays main-thread-only.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use wasmtime::{Config, Engine, OptLevel};

/// Epoch tick granularity.
pub const TICK_MS: u64 = 1;
/// Soft budget per guest callback (warn once), in ticks.
pub const SOFT_TICKS: u64 = 2;
/// Hard budget per guest callback (trap), in ticks.
pub const HARD_TICKS: u64 = 8;

struct TickerGate {
    stop: AtomicBool,
    /// Number of live plugin instances; the ticker parks while it is zero so
    /// an idle tmux server has zero plugin-related wakeups.
    live: Mutex<usize>,
    cv: Condvar,
}

pub struct EngineState {
    pub engine: Engine,
    gate: Arc<TickerGate>,
    ticker: Option<JoinHandle<()>>,
}

impl EngineState {
    pub fn new() -> wasmtime::Result<Self> {
        let mut cfg = Config::new();
        cfg.epoch_interruption(true);
        cfg.cranelift_opt_level(OptLevel::Speed);
        cfg.max_wasm_stack(512 * 1024);
        // No stack suspension (the "async" cargo feature is off): every
        // guest call runs to completion under an epoch deadline. Async is
        // completion-callback based at the ABI.
        // Best-effort compile cache; failure to set one up is not fatal.
        if let Ok(cache) = wasmtime::Cache::from_file(None) {
            cfg.cache(Some(cache));
        }
        let engine = Engine::new(&cfg)?;

        let gate = Arc::new(TickerGate {
            stop: AtomicBool::new(false),
            live: Mutex::new(0),
            cv: Condvar::new(),
        });
        let ticker = {
            let engine = engine.clone();
            let gate = Arc::clone(&gate);
            std::thread::Builder::new()
                .name("tmux-plugin-epoch".into())
                .spawn(move || ticker_main(engine, gate))
                .ok()
        };

        Ok(Self { engine, gate, ticker })
    }

    /// Record that a plugin instance now exists (unparks the ticker).
    pub fn instance_added(&self) {
        let mut live = self.gate.live.lock().unwrap();
        *live += 1;
        self.gate.cv.notify_one();
    }

    /// Record that a plugin instance was destroyed.
    pub fn instance_removed(&self) {
        let mut live = self.gate.live.lock().unwrap();
        debug_assert!(*live > 0);
        *live = live.saturating_sub(1);
    }

    /// Stop and join the ticker thread. Called from pgh_shutdown so the
    /// process exits ASAN/leak-clean.
    pub fn shutdown(&mut self) {
        self.gate.stop.store(true, Ordering::Relaxed);
        self.gate.cv.notify_one();
        if let Some(t) = self.ticker.take() {
            let _ = t.join();
        }
    }
}

impl Drop for EngineState {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn ticker_main(engine: Engine, gate: Arc<TickerGate>) {
    loop {
        if gate.stop.load(Ordering::Relaxed) {
            return;
        }
        {
            let mut live = gate.live.lock().unwrap();
            while *live == 0 {
                if gate.stop.load(Ordering::Relaxed) {
                    return;
                }
                // Re-check stop at least once a second even if never notified.
                let (l, _timeout) = gate
                    .cv
                    .wait_timeout(live, Duration::from_secs(1))
                    .unwrap();
                live = l;
            }
        }
        std::thread::sleep(Duration::from_millis(TICK_MS));
        engine.increment_epoch();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::{Store, UpdateDeadline};

    /// An infinite-loop guest must be interrupted by the epoch mechanism in
    /// bounded wall-clock time: soft-warn once, then trap.
    #[test]
    fn epoch_budget_traps_infinite_loop() {
        let es = EngineState::new().expect("engine");
        let wasm = wat::parse_str(
            r#"(module (func (export "spin") (loop $l br $l)))"#,
        )
        .unwrap();
        let module = wasmtime::Module::new(&es.engine, &wasm).unwrap();

        let mut store: Store<u32> = Store::new(&es.engine, 0);
        store.set_epoch_deadline(SOFT_TICKS);
        store.epoch_deadline_callback(|mut ctx| {
            let softwarned = ctx.data_mut();
            if *softwarned == 0 {
                *softwarned = 1;
                Ok(UpdateDeadline::Continue(HARD_TICKS - SOFT_TICKS))
            } else {
                Err(wasmtime::Error::msg("plugin exceeded CPU budget"))
            }
        });

        es.instance_added();
        let instance =
            wasmtime::Instance::new(&mut store, &module, &[]).unwrap();
        let spin = instance
            .get_typed_func::<(), ()>(&mut store, "spin")
            .unwrap();

        let start = std::time::Instant::now();
        let err = spin.call(&mut store, ()).unwrap_err();
        let elapsed = start.elapsed();

        assert!(
            err.root_cause().to_string().contains("CPU budget"),
            "err: {err:#}"
        );
        assert_eq!(*store.data(), 1, "soft warning fired first");
        // Generous upper bound: CI machines can stall, but an un-interrupted
        // loop would hang forever; anything bounded proves the mechanism.
        assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
        es.instance_removed();
    }
}
