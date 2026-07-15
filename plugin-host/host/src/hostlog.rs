//! Logging through the host vtable, plus a per-server ring buffer backing
//! the `plugin-log` command.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::CString;
use std::fmt::Write as _;
use std::os::raw::c_int;

use crate::ffi::{PGH_LOG_DEBUG, PGH_LOG_ERROR, PGH_LOG_INFO, PGH_LOG_WARN};

const RING_CAPACITY: usize = 1000;

struct Entry {
    seq: u64,
    level: c_int,
    plugin: String,
    msg: String,
}

#[derive(Default)]
struct Ring {
    seq: u64,
    entries: VecDeque<Entry>,
}

thread_local! {
    static RING: RefCell<Ring> = RefCell::new(Ring::default());
}

fn log(level: c_int, plugin: &str, msg: &str) {
    RING.with(|r| {
        let mut ring = r.borrow_mut();
        ring.seq += 1;
        let seq = ring.seq;
        if ring.entries.len() >= RING_CAPACITY {
            ring.entries.pop_front();
        }
        ring.entries.push_back(Entry {
            seq,
            level,
            plugin: plugin.to_string(),
            msg: msg.to_string(),
        });
    });

    let Some(vt) = crate::vtable() else { return };
    let Ok(cplugin) = CString::new(plugin) else { return };
    // Replace interior NULs rather than dropping the message.
    let cmsg = CString::new(msg).unwrap_or_else(|_| {
        CString::new(msg.replace('\0', "\\0")).expect("NUL-free after replace")
    });
    unsafe { (vt.log)(level, cplugin.as_ptr(), cmsg.as_ptr()) };
}

/// Preformatted tail of the log ring for `plugin-log [name]`.
pub fn query(plugin: Option<&str>, limit: usize) -> String {
    let limit = if limit == 0 { 50 } else { limit };
    RING.with(|r| {
        let ring = r.borrow();
        let mut lines: Vec<&Entry> = ring
            .entries
            .iter()
            .filter(|e| plugin.is_none_or(|p| e.plugin == p))
            .collect();
        if lines.len() > limit {
            lines.drain(..lines.len() - limit);
        }
        if lines.is_empty() {
            return String::from("no log entries\n");
        }
        let mut out = String::new();
        for e in lines {
            let level = match e.level {
                PGH_LOG_DEBUG => "debug",
                PGH_LOG_INFO => "info",
                PGH_LOG_WARN => "warn",
                _ => "error",
            };
            let _ = writeln!(out, "#{} [{}] {}: {}", e.seq, level, e.plugin, e.msg);
        }
        out
    })
}

pub fn debug(plugin: &str, msg: &str) {
    log(PGH_LOG_DEBUG, plugin, msg);
}

pub fn info(plugin: &str, msg: &str) {
    log(PGH_LOG_INFO, plugin, msg);
}

pub fn warn(plugin: &str, msg: &str) {
    log(PGH_LOG_WARN, plugin, msg);
}

pub fn error(plugin: &str, msg: &str) {
    log(PGH_LOG_ERROR, plugin, msg);
}
