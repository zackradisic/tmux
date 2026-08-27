//! Name interning: event names and payload keys become stable u32 ids.
//!
//! One table serves both sides: the C bridge interns names through
//! `pgh_intern` when it builds event buffers, and guests intern through the
//! `intern` import to subscribe and to compare payload keys. Ids start at 1
//! (0 marks an inline key in a field block), are stable for the server
//! lifetime, and are never reused.
//!
//! Event routing needs one derived bit per name — whether the event is
//! delivered without a subscription — computed once at intern time.

use std::cell::RefCell;
use std::collections::HashMap;

struct InternTable {
    by_name: HashMap<String, u32>,
    names: Vec<String>,
    implicit: Vec<bool>,
}

impl InternTable {
    fn new() -> Self {
        Self {
            by_name: HashMap::new(),
            names: Vec::new(),
            implicit: Vec::new(),
        }
    }
}

thread_local! {
    static TABLE: RefCell<InternTable> = RefCell::new(InternTable::new());
}

/// Lifecycle events are delivered without an explicit subscription (a
/// scoped instance always learns about its object's world changing).
fn is_implicit(name: &str) -> bool {
    name.ends_with("-created")
        || name.ends_with("-destroyed")
        || name == "session-closed"
}

/// Intern a name, returning its stable id (>= 1).
pub fn intern(name: &str) -> u32 {
    TABLE.with(|t| {
        let mut t = t.borrow_mut();
        if let Some(&id) = t.by_name.get(name) {
            return id;
        }
        t.names.push(name.to_string());
        t.implicit.push(is_implicit(name));
        let id = t.names.len() as u32; // ids start at 1
        t.by_name.insert(name.to_string(), id);
        id
    })
}

/// Reverse lookup. None for unknown ids (including 0).
pub fn name_of(id: u32) -> Option<String> {
    TABLE.with(|t| {
        t.borrow().names.get((id as usize).checked_sub(1)?).cloned()
    })
}

/// Is this event id delivered without a subscription?
pub fn implicit(id: u32) -> bool {
    TABLE.with(|t| {
        t.borrow()
            .implicit
            .get((id as usize).wrapping_sub(1))
            .copied()
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_is_stable_and_reverse_maps() {
        let a = intern("pane-focus-in");
        let b = intern("pane-focus-in");
        assert_eq!(a, b);
        assert!(a >= 1);
        assert_eq!(name_of(a).as_deref(), Some("pane-focus-in"));
        assert_eq!(name_of(0), None);
        assert!(!implicit(a));
        let c = intern("pane-created");
        assert!(implicit(c));
        assert!(implicit(intern("session-closed")));
    }
}
