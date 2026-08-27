//! Guest-side events: a typed view over the binary event buffer, plus the
//! name-interning cache shared with subscriptions.
//!
//! Event names and payload keys are u32 ids interned by the host. The SDK
//! caches name<->id both ways so `event.is("pane-focus-in")` and
//! `event.get_str("session_name")` cost one host call per distinct name
//! per instance lifetime, then hashmap lookups.

use std::cell::RefCell;
use std::collections::HashMap;

use tmux_plugin_abi::{
    Cursor, EventHeader, EventScope, FieldReader, KeyRef, ValueRef, WireError,
};

use crate::runtime::raw;

thread_local! {
    static BY_NAME: RefCell<HashMap<String, u32>> = RefCell::new(HashMap::new());
    static BY_ID: RefCell<HashMap<u32, String>> = RefCell::new(HashMap::new());
}

/// Intern a name through the host, cached. Returns 0 only if the host is
/// unreachable (never for a real name).
pub fn intern(name: &str) -> u32 {
    if let Some(id) =
        BY_NAME.with(|m| m.borrow().get(name).copied())
    {
        return id;
    }
    let id = unsafe {
        raw::intern(name.as_ptr() as i32, name.len() as i32)
    };
    let id = if id > 0 { id as u32 } else { 0 };
    if id != 0 {
        BY_NAME.with(|m| m.borrow_mut().insert(name.to_string(), id));
        BY_ID.with(|m| m.borrow_mut().insert(id, name.to_string()));
    }
    id
}

/// Reverse lookup of an interned id, cached.
pub fn intern_name(id: u32) -> Option<String> {
    if let Some(name) = BY_ID.with(|m| m.borrow().get(&id).cloned()) {
        return Some(name);
    }
    let mut buf = vec![0u8; 128];
    loop {
        let mut len: u32 = 0;
        let rc = unsafe {
            raw::intern_name(
                id as i32,
                buf.as_mut_ptr() as i32,
                buf.len() as i32,
                &mut len as *mut u32 as i32,
            )
        };
        if rc == 0 {
            buf.truncate(len as usize);
            let name = String::from_utf8_lossy(&buf).into_owned();
            BY_ID.with(|m| m.borrow_mut().insert(id, name.clone()));
            BY_NAME.with(|m| m.borrow_mut().insert(name.clone(), id));
            return Some(name);
        }
        if -rc == tmux_plugin_abi::ErrorCode::Limit.as_num()
            && len as usize > buf.len()
        {
            buf.resize(len as usize, 0);
            continue;
        }
        return None;
    }
}

/// One event as delivered to `Plugin::on_event`. Owns the raw buffer;
/// field accessors borrow from it (no copies).
pub struct Event {
    bytes: Vec<u8>,
    /// Interned event name id; compare with [`Event::is`] or
    /// [`crate::event::intern`].
    pub id: u32,
    pub seq: u64,
    pub scope: EventScope,
}

impl Event {
    /// Parse a delivered event buffer. None = malformed (host bug).
    pub fn parse(bytes: Vec<u8>) -> Option<Event> {
        let (header, _) = EventHeader::parse(&bytes).ok()?;
        Some(Event {
            bytes,
            id: header.event_id,
            seq: header.seq,
            scope: header.scope,
        })
    }

    /// The event's name (one host lookup per distinct id, then cached).
    pub fn name(&self) -> String {
        intern_name(self.id).unwrap_or_else(|| format!("event#{}", self.id))
    }

    /// Is this event `name`?
    pub fn is(&self, name: &str) -> bool {
        self.id == intern(name)
    }

    fn fields(&self) -> Option<FieldReader<'_>> {
        let mut cursor = Cursor::new(&self.bytes);
        // Skip the fixed header.
        for _ in 0..tmux_plugin_abi::EVENT_HEADER_LEN / 4 {
            cursor.u32().ok()?;
        }
        FieldReader::from_cursor(cursor).ok()
    }

    /// Look up one payload field by key name.
    pub fn get(&self, key: &str) -> Option<ValueRef<'_>> {
        let id = intern(key);
        let fields = self.fields()?;
        for field in fields {
            let (k, v) = field.ok()?;
            let matches = match k {
                KeyRef::Id(kid) => kid == id,
                KeyRef::Name(name) => name == key,
            };
            if matches {
                return Some(v);
            }
        }
        None
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key)?.as_str()
    }

    pub fn get_i64(&self, key: &str) -> Option<i64> {
        self.get(key)?.as_i64()
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.get(key)?.as_bool()
    }

    /// Iterate every payload field as (key name, value). Key names resolve
    /// through the intern cache.
    pub fn iter(&self) -> impl Iterator<Item = (String, ValueRef<'_>)> {
        self.fields()
            .into_iter()
            .flatten()
            .filter_map(|f| f.ok())
            .map(|(k, v)| {
                let name = match k {
                    KeyRef::Id(id) => intern_name(id)
                        .unwrap_or_else(|| format!("key#{id}")),
                    KeyRef::Name(n) => n.to_string(),
                };
                (name, v)
            })
    }
}

/// Decode a config field block into a serde_json value for
/// `Plugin::Config` deserialization (JSON-tagged fields carry nested
/// values as embedded JSON text).
pub fn config_value(bytes: &[u8]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    let Ok(reader) = FieldReader::new(bytes) else {
        return serde_json::Value::Object(map);
    };
    for field in reader {
        let Ok((key, value)) = field else { break };
        let key = match key {
            KeyRef::Name(n) => n.to_string(),
            KeyRef::Id(id) => match intern_name(id) {
                Some(n) => n,
                None => continue,
            },
        };
        let value = match value {
            ValueRef::Null => serde_json::Value::Null,
            ValueRef::Bool(b) => b.into(),
            ValueRef::I64(v) => v.into(),
            ValueRef::F64(v) => serde_json::Number::from_f64(v)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            ValueRef::Str(s) => s.into(),
            ValueRef::Json(s) => serde_json::from_str(s)
                .unwrap_or(serde_json::Value::Null),
        };
        map.insert(key, value);
    }
    serde_json::Value::Object(map)
}

/// Suppress an unused warning on non-wasm targets.
#[allow(dead_code)]
fn _wire_error_is_public(_: WireError) {}
