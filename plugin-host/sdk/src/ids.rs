//! Typed weak handles. tmux ids are monotonic and never reused; a handle
//! may refer to a dead object, in which case host calls return
//! `E_NO_SUCH_OBJECT`.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! id_type {
    ($name:ident, $sigil:literal) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub u32);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($sigil, "{}"), self.0)
            }
        }

        impl From<u32> for $name {
            fn from(id: u32) -> Self {
                Self(id)
            }
        }
    };
}

id_type!(SessionId, "$");
id_type!(WindowId, "@");
id_type!(PaneId, "%");
id_type!(ClientId, "#");

/// Handle to an open plugin UI mode (from `mode_open`). Dead after
/// `mode-closed`; host calls on a closed mode return `E_NO_SUCH_OBJECT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModeId(pub u64);

impl fmt::Display for ModeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mode:{}", self.0)
    }
}

impl From<u64> for ModeId {
    fn from(id: u64) -> Self {
        Self(id)
    }
}
