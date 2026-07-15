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

use serde_json::Value;
use slab::Slab;
use tmux_plugin_abi::{ErrorCode, HostError};

pub type HostResult = Result<Value, HostError>;

#[derive(Default)]
struct TokenSlot {
    waker: Option<Waker>,
    result: Option<HostResult>,
}

#[derive(Default)]
struct Executor {
    /// `None` while the task is checked out for polling.
    tasks: Slab<Option<Pin<Box<dyn Future<Output = ()>>>>>,
    ready: VecDeque<usize>,
    waiting: HashMap<u64, TokenSlot>,
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
                    data: Value::Null,
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
pub fn complete(token: u64, payload: &[u8], is_error: bool) {
    let result: HostResult = if is_error {
        Err(serde_json::from_slice::<HostError>(payload).unwrap_or(HostError {
            code: ErrorCode::Host,
            message: String::from_utf8_lossy(payload).into_owned(),
            data: Value::Null,
        }))
    } else {
        Ok(serde_json::from_slice::<Value>(payload)
            .unwrap_or(Value::Null))
    };

    let waker = EXEC.with(|e| {
        let mut ex = e.borrow_mut();
        let slot = ex.waiting.entry(token).or_default();
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
