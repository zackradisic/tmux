//! Single-threaded local executor over the completion-callback ABI.
//!
//! There is no stack suspension at the wasm boundary: every entry into the
//! guest runs to completion. Async plugin code works by parking futures
//! here; when the host delivers `pgh_on_async_complete(token, ...)`, the
//! matching future's waker fires and `run_until_stalled` polls whatever
//! became ready - all within that same budgeted guest callback.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use slab::Slab;
use tmux_plugin_abi::{ErrorCode, HostError};

/// A successful async completion: two per-method scalars plus an optional
/// data buffer (job output; empty for commands and timers).
#[derive(Debug, Default)]
pub struct Completion {
    pub v0: i64,
    pub v1: i64,
    pub data: Vec<u8>,
}

pub type HostResult = Result<Completion, HostError>;

#[derive(Default)]
struct TokenSlot {
    waker: Option<Waker>,
    result: Option<HostResult>,
    /// The future was dropped before its completion arrived; the
    /// completion cleans up (including any pinned buffer).
    abandoned: bool,
}

#[derive(Default)]
struct Executor {
    /// `None` while the task is checked out for polling.
    tasks: Slab<Option<Pin<Box<dyn Future<Output = ()>>>>>,
    ready: VecDeque<usize>,
    waiting: HashMap<u64, TokenSlot>,
    /// Buffers that must stay alive until a token's completion arrives
    /// (async fs: the host worker reads/writes them directly). Keyed by
    /// token; dropped here if the awaiting future was cancelled.
    pinned: HashMap<u64, Vec<u8>>,
}

thread_local! {
    static EXEC: RefCell<Executor> = RefCell::new(Executor::default());
}

/// Spawn a detached task. It is polled during `run_until_stalled`, which
/// the SDK glue runs at the end of every guest callback.
pub fn spawn(fut: impl Future<Output = ()> + 'static) {
    EXEC.with(|e| {
        let mut ex = e.borrow_mut();
        let id = ex.tasks.insert(Some(Box::pin(fut)));
        ex.ready.push_back(id);
    });
}

/// Register interest in a host token before its completion can arrive.
pub(crate) fn register_token(token: u64) {
    EXEC.with(|e| {
        e.borrow_mut().waiting.insert(token, TokenSlot::default());
    });
}

/// Pin a buffer until `token`'s completion arrives (the host reads or
/// writes it directly). The buffer's heap allocation must not move:
/// take raw pointers BEFORE calling this (moving the Vec struct into
/// the map does not move its heap storage).
pub(crate) fn pin_buffer(token: u64, buf: Vec<u8>) {
    EXEC.with(|e| {
        e.borrow_mut().pinned.insert(token, buf);
    });
}

/// Reclaim a pinned buffer after its completion arrived.
pub(crate) fn take_buffer(token: u64) -> Option<Vec<u8>> {
    EXEC.with(|e| e.borrow_mut().pinned.remove(&token))
}

/// Future resolving to a host async result.
pub(crate) struct HostFuture {
    token: u64,
}

impl HostFuture {
    pub(crate) fn new(token: u64) -> Self {
        register_token(token);
        Self { token }
    }
}

impl Drop for HostFuture {
    fn drop(&mut self) {
        // Cancellation: the completion has not been consumed. Mark the
        // slot abandoned so complete() cleans up - the pinned buffer (if
        // any) must survive until then, because the host worker may
        // still be using it.
        EXEC.with(|e| {
            let mut ex = e.borrow_mut();
            if let Some(slot) = ex.waiting.get_mut(&self.token) {
                if slot.result.is_some() {
                    ex.waiting.remove(&self.token);
                    ex.pinned.remove(&self.token);
                } else {
                    slot.waker = None;
                    slot.abandoned = true;
                }
            }
        });
    }
}

impl Future for HostFuture {
    type Output = HostResult;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        EXEC.with(|e| {
            let mut ex = e.borrow_mut();
            let Some(slot) = ex.waiting.get_mut(&self.token) else {
                // Completion already consumed or never registered.
                return Poll::Ready(Err(HostError {
                    code: ErrorCode::Cancelled,
                    message: "completion lost".into(),
                }));
            };
            if let Some(result) = slot.result.take() {
                ex.waiting.remove(&self.token);
                Poll::Ready(result)
            } else {
                slot.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        })
    }
}

/// Called from `pgh_on_async_complete`: fill the slot and wake the task.
/// `err` = 0 on success (v0/v1/data are the payload) or an ErrorCode
/// number (data = the error message bytes).
pub fn complete(token: u64, err: i32, v0: i64, v1: i64, data: &[u8]) {
    let result: HostResult = if err != 0 {
        Err(HostError {
            code: ErrorCode::from_num(err),
            message: String::from_utf8_lossy(data).into_owned(),
        })
    } else {
        Ok(Completion { v0, v1, data: data.to_vec() })
    };

    let waker = EXEC.with(|e| {
        let mut ex = e.borrow_mut();
        let slot = ex.waiting.entry(token).or_default();
        if slot.abandoned {
            // The future was cancelled: consume the completion and free
            // the pinned buffer (safe now - the host is done with it).
            ex.waiting.remove(&token);
            ex.pinned.remove(&token);
            return None;
        }
        slot.result = Some(result);
        slot.waker.take()
    });
    if let Some(w) = waker {
        w.wake();
    }
}

fn make_waker(task: usize) -> Waker {
    unsafe fn clone(p: *const ()) -> RawWaker {
        RawWaker::new(p, &VTABLE)
    }
    unsafe fn wake(p: *const ()) {
        let task = p as usize;
        EXEC.with(|e| e.borrow_mut().ready.push_back(task));
    }
    unsafe fn drop_raw(_: *const ()) {}
    static VTABLE: RawWakerVTable =
        RawWakerVTable::new(clone, wake, wake, drop_raw);
    unsafe { Waker::from_raw(RawWaker::new(task as *const (), &VTABLE)) }
}

/// Poll every ready task until nothing more can run. The whole loop runs
/// inside the host's epoch budget; runaway async code traps like any other
/// guest code.
pub fn run_until_stalled() {
    loop {
        let Some(id) = EXEC.with(|e| e.borrow_mut().ready.pop_front()) else {
            return;
        };
        // Check the task out so polling can re-enter the executor (spawn,
        // token registration, wakes) without a double borrow.
        let fut = EXEC.with(|e| {
            e.borrow_mut().tasks.get_mut(id).and_then(Option::take)
        });
        let Some(mut fut) = fut else { continue };

        let waker = make_waker(id);
        let mut cx = Context::from_waker(&waker);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(()) => {
                EXEC.with(|e| {
                    e.borrow_mut().tasks.try_remove(id);
                });
            }
            Poll::Pending => {
                EXEC.with(|e| {
                    if let Some(slot) = e.borrow_mut().tasks.get_mut(id) {
                        *slot = Some(fut);
                    }
                });
            }
        }
    }
}
