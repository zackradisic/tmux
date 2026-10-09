//! What the providers on linked servers last reported, and how it is
//! fetched.
//!
//! The calls run TOGETHER, not one after another: these are expensive
//! hops (a ProxyCommand, a Tailscale link, a box in a cloud region), so
//! serially the wait is their sum rather than their max. Each snapshot is
//! applied and reported the moment it lands, so a fast server is not held
//! back by a slow one. A server already being fetched, or fetched a
//! moment ago, is left alone ([`Remotes::claim`]).
//!
//! The row type is the consumer's; this module moves `Vec<T>` around and
//! keeps the per-server clocks.

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use tmux_plugin_sdk::abi::ErrorCode;
use tmux_plugin_sdk::prelude::*;

/// While at least one remote fetch is outstanding, repaint on this
/// cadence so the per-server spinner turns. It runs ONLY while something
/// is in flight, so an idle picker never repaints on it.
pub const SPIN_MS: u64 = 100;
/// A fetch stays invisible until it has been outstanding this long. A
/// refresh tick refetches every server, and a healthy remote answers well
/// inside this, so the spinner does not blink twice a second for nothing
/// - it appears only when a server is actually being slow.
pub const SPIN_GRACE_MS: u64 = 500;
/// A fetch outstanding this long has stopped being a blink; the header
/// says how long it has been waiting instead of only spinning.
pub const FETCH_STUCK_MS: u64 = 3000;
/// An in-flight mark older than this is not believed (the host fails a
/// service call at 30s), so a lost task cannot wedge a server forever.
pub const FETCH_STALE_MS: u64 = 35_000;
/// Spinner frames, one per [`SPIN_MS`].
pub const SPIN_FRAMES: [&str; 10] =
    ["\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}", "\u{2807}", "\u{280f}"];

/// The spinner frame for a fetch that started at `since`, once it has
/// been outstanding past the grace; `None` inside the grace.
pub fn spin_since(fetching: &HashMap<String, u64>, server: &str, now: u64) -> Option<u64> {
    let since = *fetching.get(server)?;
    (now.saturating_sub(since) >= SPIN_GRACE_MS).then_some(since)
}

/// What a provider on another server last reported.
#[derive(Debug, Clone)]
pub struct RemoteRows<T> {
    pub rows: Vec<T>,
    /// local clock - provider clock, at the last snapshot.
    pub skew_ms: i64,
    /// When the last snapshot arrived (local clock).
    pub fetched_ms: u64,
    /// When the server's link went down (local clock); None while up.
    pub down_since: Option<u64>,
    /// When the fetch now in flight started (local clock); None when
    /// nothing is outstanding. Doubles as the in-flight flag the
    /// debounce reads and as the spinner's clock for this server.
    pub fetching_since: Option<u64>,
}

impl<T> Default for RemoteRows<T> {
    fn default() -> Self {
        Self { rows: Vec::new(), skew_ms: 0, fetched_ms: 0, down_since: None, fetching_since: None }
    }
}

/// The remote rows, by server name. Shared between the plugin (which
/// feeds it from events) and the picker (which reads it).
#[derive(Debug)]
pub struct Remotes<T> {
    pub servers: HashMap<String, RemoteRows<T>>,
    /// Servers whose copy of this plugin this side does not accept, with
    /// the reason to show ("agents 0.2.0 there, 0.1.0 here").
    pub mismatch: HashMap<String, String>,
    /// A spinner task is turning; one is enough for every server.
    pub spinning: bool,
    /// When the fetch round a picker-open started began (local clock).
    /// Opening again while one is outstanding starts nothing new.
    pub open_round_since: Option<u64>,
}

impl<T> Default for Remotes<T> {
    fn default() -> Self {
        Self { servers: HashMap::new(), mismatch: HashMap::new(), spinning: false, open_round_since: None }
    }
}

impl<T> Remotes<T> {
    /// Take a provider's snapshot for a server: its rows, and its clock
    /// at the time, for the skew.
    pub fn apply(&mut self, server: &str, rows: Vec<T>, their_now_ms: i64) {
        let now = now_ms();
        let e = self.servers.entry(server.to_string()).or_default();
        e.skew_ms = now as i64 - their_now_ms;
        e.fetched_ms = now;
        e.down_since = None;
        self.mismatch.remove(server);
        e.rows = rows;
    }

    pub fn mark_down(&mut self, server: &str) {
        let e = self.servers.entry(server.to_string()).or_default();
        if e.down_since.is_none() {
            e.down_since = Some(now_ms());
        }
    }

    pub fn mark_up(&mut self, server: &str) {
        if let Some(e) = self.servers.get_mut(server) {
            e.down_since = None;
        }
    }

    /// The server's copy runs a service version this side rejects: no
    /// rows from it, one line that says why.
    pub fn mark_mismatch(&mut self, server: &str, why: String) {
        if let Some(e) = self.servers.get_mut(server) {
            e.rows.clear();
        }
        self.mismatch.insert(server.to_string(), why);
    }

    /// Claim a server for a fetch, or refuse it. Refused when one is
    /// already in flight, or when the last snapshot landed less than
    /// `min_age_ms` ago: the open, the refresh tick and a link-up event
    /// all reach for the same servers, and a second claim would mean a
    /// second ssh hop for rows we are already holding or already waiting
    /// on. A mark older than [`FETCH_STALE_MS`] outlived any call the
    /// host would still be holding, so it is not believed.
    pub fn begin_fetch(&mut self, server: &str, min_age_ms: u64) -> bool {
        let now = now_ms();
        let e = self.servers.entry(server.to_string()).or_default();
        if e.fetching_since
            .is_some_and(|t| now.saturating_sub(t) < FETCH_STALE_MS)
        {
            return false;
        }
        if now.saturating_sub(e.fetched_ms) < min_age_ms {
            return false;
        }
        e.fetching_since = Some(now);
        true
    }

    /// A claimed server's call is starting now: restart its clock, so a
    /// server that waited for a slot does not show the wait as if the
    /// remote were slow to answer.
    pub fn start_fetch(&mut self, server: &str) {
        if let Some(e) = self.servers.get_mut(server) {
            e.fetching_since = Some(now_ms());
        }
    }

    pub fn end_fetch(&mut self, server: &str) {
        if let Some(e) = self.servers.get_mut(server) {
            e.fetching_since = None;
        }
    }

    /// Per server with a fetch in flight: when it started.
    pub fn fetching(&self) -> HashMap<String, u64> {
        self.servers
            .iter()
            .filter_map(|(k, v)| v.fetching_since.map(|t| (k.clone(), t)))
            .collect()
    }

    /// Per server: local clock minus the provider's clock.
    pub fn skews(&self) -> HashMap<String, i64> {
        self.servers.iter().map(|(k, v)| (k.clone(), v.skew_ms)).collect()
    }

    /// Per server: when its link went down (local clock), while it is.
    pub fn downs(&self) -> HashMap<String, u64> {
        self.servers.iter().filter_map(|(k, v)| v.down_since.map(|t| (k.clone(), t))).collect()
    }

    /// Claim every linked server worth fetching: up, linked from this
    /// side (never an inbound peer - that would be a gated remote ->
    /// initiator call), accepted, and not fetched inside `min_age_ms`.
    /// A server whose copy is not accepted is marked mismatched with
    /// `mismatch_msg`'s text instead. Returns the claimed names.
    pub fn claim(
        &mut self,
        servers: &[ServerInfo],
        min_age_ms: u64,
        mismatch_msg: impl Fn(&ServerInfo) -> String,
    ) -> Vec<String> {
        let mut queue = Vec::new();
        for s in servers.iter().filter(|s| !s.local && s.up && s.linked) {
            if !s.accepted {
                self.mark_mismatch(&s.name, mismatch_msg(s));
                continue;
            }
            if self.begin_fetch(&s.name, min_age_ms) {
                queue.push(s.name.clone());
            }
        }
        queue
    }
}

impl<T: Clone> Remotes<T> {
    /// Every remote row, in server-name order.
    pub fn rows(&self) -> Vec<T> {
        let mut names: Vec<&String> = self.servers.keys().collect();
        names.sort();
        names
            .into_iter()
            .flat_map(|n| self.servers[n].rows.iter().cloned())
            .collect()
    }
}

/// A boxed future, so the fetch and landed callbacks can be plain
/// closures returning `Box::pin(async move { .. })`.
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// One server's fetch: its rows and its clock, or the host's error.
pub type FetchResult<T> = Result<(Vec<T>, i64), HostError>;

/// Work a claimed queue off at most `max_inflight` calls at a time.
/// `fetch` makes one server's call; `landed` runs after each server is
/// applied (or marked down), to repaint. Oldest rows first, so the
/// group most out of date comes back first when there are more servers
/// than slots. An `Unreachable` error marks the server down (an old
/// remote, or no provider yet: nothing to show, nothing to break); a
/// `Version` error marks it mismatched with the host's message.
pub async fn fetch_all<T: 'static>(
    remotes: Rc<RefCell<Remotes<T>>>,
    mut queue: Vec<String>,
    max_inflight: usize,
    fetch: impl Fn(String) -> BoxFut<'static, FetchResult<T>> + 'static,
    landed: impl Fn(String) -> BoxFut<'static, ()> + 'static,
) {
    if queue.is_empty() {
        return;
    }
    queue.sort_by_key(|n| remotes.borrow().servers.get(n).map(|e| e.fetched_ms).unwrap_or(0));
    queue.reverse(); // workers pop from the end
    let n = max_inflight.max(1).min(queue.len());
    let queue = Rc::new(RefCell::new(queue));
    let fetch = Rc::new(fetch);
    let landed = Rc::new(landed);
    let futs: Vec<BoxFut<'static, ()>> = (0..n)
        .map(|_| {
            let remotes = Rc::clone(&remotes);
            let queue = Rc::clone(&queue);
            let fetch = Rc::clone(&fetch);
            let landed = Rc::clone(&landed);
            Box::pin(async move {
                loop {
                    let next = queue.borrow_mut().pop();
                    let Some(server) = next else { return };
                    remotes.borrow_mut().start_fetch(&server);
                    let res = fetch(server.clone()).await;
                    {
                        let mut r = remotes.borrow_mut();
                        r.end_fetch(&server);
                        match res {
                            Ok((rows, their_now)) => r.apply(&server, rows, their_now),
                            Err(e) => {
                                if e.code == ErrorCode::Unreachable {
                                    r.mark_down(&server);
                                } else if e.code == ErrorCode::Version {
                                    r.mark_mismatch(&server, e.message.clone());
                                }
                            }
                        }
                    }
                    landed(server).await;
                }
            }) as BoxFut<'static, ()>
        })
        .collect();
    JoinAll { futs }.await;
}

/// Poll a handful of futures together to completion. The guest carries no
/// futures crate; this is the whole of it - re-poll whatever is still
/// pending on each wake, finish when nothing is.
pub struct JoinAll<'a> {
    pub futs: Vec<BoxFut<'a, ()>>,
}

impl Future for JoinAll<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let futs = &mut self.get_mut().futs;
        futs.retain_mut(|f| f.as_mut().poll(cx).is_pending());
        if futs.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

/// Turn the per-server spinner while any fetch is outstanding. One task
/// at a time (the `spinning` flag), and it ends the moment nothing is in
/// flight, so an idle picker is not repainting forever on a 100ms tick.
/// `tick` gets the in-flight servers and the time and repaints; it
/// returns false to stop (the picker closed).
pub fn start_spinner<T: 'static>(
    remotes: &Rc<RefCell<Remotes<T>>>,
    mut tick: impl FnMut(HashMap<String, u64>, u64) -> bool + 'static,
) {
    {
        let mut r = remotes.borrow_mut();
        if r.spinning {
            return;
        }
        r.spinning = true;
    }
    let remotes = Rc::clone(remotes);
    spawn(async move {
        loop {
            if sleep_ms(SPIN_MS).await.is_err() {
                break;
            }
            let fetching = remotes.borrow().fetching();
            if fetching.is_empty() {
                break;
            }
            let now = now_ms();
            // Nothing has been outstanding long enough to draw yet: keep
            // ticking (one of these may get slow) but do not repaint. A
            // fetch that finishes inside the grace costs no renders at
            // all, which is the whole point - the refresh tick refetches
            // every server and must not blink a spinner each time.
            if !fetching.values().any(|t| now.saturating_sub(*t) >= SPIN_GRACE_MS) {
                continue;
            }
            if !tick(fetching, now) {
                break;
            }
        }
        remotes.borrow_mut().spinning = false;
    });
}
