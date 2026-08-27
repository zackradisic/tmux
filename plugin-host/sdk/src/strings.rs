//! Zero-copy string arguments.
//!
//! The ABI's `Str` shape is (ptr, len) with a NUL byte at `data[len]`, so
//! the host can hand the pointer straight to C without copying. Three ways
//! to satisfy it, cheapest first:
//!
//! - `c"..."` literals (`&CStr`): NUL lives in the data segment. Zero work.
//! - [`TmuxString`]: a growable string that maintains the trailing NUL as
//!   an invariant while you build it (`write!` into it). Zero work per
//!   call.
//! - `&str` / `String`: one copy per call to append the NUL. Fine for cold
//!   paths; build a `TmuxString` for hot ones.

use std::ffi::CStr;
use std::fmt;

/// A string argument in ABI form: bytes with a trailing NUL.
pub enum TmuxStrRef<'a> {
    Borrowed(&'a [u8]), // includes the trailing NUL
    Owned(Vec<u8>),     // includes the trailing NUL
}

impl TmuxStrRef<'_> {
    /// (ptr, len) with len excluding the NUL - the wire form.
    pub(crate) fn parts(&self) -> (i32, i32) {
        let bytes = match self {
            TmuxStrRef::Borrowed(b) => b,
            TmuxStrRef::Owned(v) => v.as_slice(),
        };
        (bytes.as_ptr() as i32, (bytes.len() - 1) as i32)
    }
}

/// Anything that can cross the ABI as a `Str`.
pub trait AsTmuxStr {
    fn to_tmux(&self) -> TmuxStrRef<'_>;
}

impl AsTmuxStr for CStr {
    fn to_tmux(&self) -> TmuxStrRef<'_> {
        TmuxStrRef::Borrowed(self.to_bytes_with_nul())
    }
}

impl AsTmuxStr for &CStr {
    fn to_tmux(&self) -> TmuxStrRef<'_> {
        TmuxStrRef::Borrowed(self.to_bytes_with_nul())
    }
}

impl AsTmuxStr for str {
    fn to_tmux(&self) -> TmuxStrRef<'_> {
        let mut v = Vec::with_capacity(self.len() + 1);
        v.extend_from_slice(self.as_bytes());
        v.push(0);
        TmuxStrRef::Owned(v)
    }
}

impl AsTmuxStr for &str {
    fn to_tmux(&self) -> TmuxStrRef<'_> {
        (**self).to_tmux()
    }
}

impl AsTmuxStr for String {
    fn to_tmux(&self) -> TmuxStrRef<'_> {
        self.as_str().to_tmux()
    }
}

impl AsTmuxStr for &String {
    fn to_tmux(&self) -> TmuxStrRef<'_> {
        self.as_str().to_tmux()
    }
}

impl AsTmuxStr for TmuxString {
    fn to_tmux(&self) -> TmuxStrRef<'_> {
        TmuxStrRef::Borrowed(&self.buf)
    }
}

impl AsTmuxStr for &TmuxString {
    fn to_tmux(&self) -> TmuxStrRef<'_> {
        TmuxStrRef::Borrowed(&self.buf)
    }
}

/// A growable string that keeps a trailing NUL as an invariant, so passing
/// it across the ABI is zero-copy. Build with `write!` or [`push_str`];
/// interior NULs are rejected by the host at call time.
///
/// [`push_str`]: TmuxString::push_str
pub struct TmuxString {
    /// Always non-empty; last byte is 0. `len()` excludes it.
    buf: Vec<u8>,
}

impl TmuxString {
    pub fn new() -> Self {
        Self { buf: vec![0] }
    }

    pub fn with_capacity(cap: usize) -> Self {
        let mut buf = Vec::with_capacity(cap + 1);
        buf.push(0);
        Self { buf }
    }

    pub fn push_str(&mut self, s: &str) {
        self.buf.pop();
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.buf.push(0);
    }

    pub fn len(&self) -> usize {
        self.buf.len() - 1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn as_str(&self) -> &str {
        // Invariant: only &str data is ever appended.
        unsafe { std::str::from_utf8_unchecked(&self.buf[..self.buf.len() - 1]) }
    }
}

impl Default for TmuxString {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Write for TmuxString {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.push_str(s);
        Ok(())
    }
}

impl fmt::Display for TmuxString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<String> for TmuxString {
    fn from(s: String) -> Self {
        let mut buf = s.into_bytes();
        buf.push(0);
        Self { buf }
    }
}

impl From<&str> for TmuxString {
    fn from(s: &str) -> Self {
        let mut t = TmuxString::with_capacity(s.len());
        t.push_str(s);
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    #[test]
    fn nul_invariant() {
        let mut s = TmuxString::new();
        assert_eq!(s.to_tmux().parts().1, 0);
        write!(s, "hello {}", 42).unwrap();
        assert_eq!(s.as_str(), "hello 42");
        let (_, len) = s.to_tmux().parts();
        assert_eq!(len as usize, s.len());
        s.clear();
        assert!(s.is_empty());
    }
}
