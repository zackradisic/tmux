//! Guest-side runtime plumbing: the raw per-method host imports, the
//! linear-memory allocator protocol, and buffer ownership helpers.
//!
//! Everything here is `pub(crate)` or wrapped: plugin code goes through
//! the safe typed API in `api`, which upholds the ABI's ownership rules
//! (borrowed strings live across the synchronous import call; owned
//! results are freed exactly once via [`Owned`]).

use std::alloc::{alloc as std_alloc, dealloc, Layout};

/// Raw imports, one per host method (module "tmux"). Signatures are
/// defined in tmux-plugin-abi's `imports` docs.
#[cfg(target_arch = "wasm32")]
pub(crate) mod raw {
    #[link(wasm_import_module = "tmux")]
    extern "C" {
        pub fn intern(ptr: i32, len: i32) -> i64;
        pub fn intern_name(id: i32, out: i32, cap: i32, len_out: i32) -> i32;
        pub fn subscribe(id: i32) -> i32;
        pub fn unsubscribe(id: i32) -> i32;
        pub fn list(kind: i32, owned_out: i32) -> i32;
        pub fn resolve(kind: i32, id: i32, owned_out: i32) -> i32;
        pub fn self_info(out: i32) -> i32;
        pub fn get_option(
            kind: i32, id: i32, name_ptr: i32, name_len: i32,
            out: i32, cap: i32, len_out: i32,
        ) -> i32;
        pub fn set_option(
            kind: i32, id: i32, name_ptr: i32, name_len: i32,
            val_ptr: i32, val_len: i32,
        ) -> i32;
        pub fn format_expand(
            kind: i32, id: i32, fmt_ptr: i32, fmt_len: i32,
            out: i32, cap: i32, len_out: i32,
        ) -> i32;
        pub fn send_keys(
            pane: i32, keys_ptr: i32, keys_len: i32, literal: i32,
        ) -> i32;
        pub fn capture_pane(
            pane: i32, start: i32, end: i32, escapes: i32,
            out: i32, cap: i32, len_out: i32,
        ) -> i32;
        pub fn pane_env(
            pane: i32, name_ptr: i32, name_len: i32,
            out: i32, cap: i32, len_out: i32,
        ) -> i32;
        pub fn pane_fds(pane: i32, out: i32, cap: i32, len_out: i32) -> i32;
        pub fn pane_pid(pane: i32) -> i64;
        pub fn display_message(client: i32, msg_ptr: i32, msg_len: i32) -> i32;
        #[allow(dead_code)]
        pub fn timer_cancel(token: i64) -> i32;
        pub fn mode_open(
            window: i32, width: i32, height: i32, x: i32, y: i32,
            title_ptr: i32, title_len: i32,
        ) -> i64;
        pub fn mode_write(mode: i64, ptr: i32, len: i32) -> i32;
        pub fn mode_preview(
            mode: i64, pane: i64, x: i32, y: i32, w: i32, h: i32,
        ) -> i32;
        pub fn mode_move(mode: i64, window: i32, x: i32, y: i32) -> i32;
        pub fn mode_resize(mode: i64, width: i32, height: i32) -> i32;
        pub fn mode_close(mode: i64) -> i32;
        pub fn last_error(out: i32, cap: i32, len_out: i32) -> i32;
        pub fn log(level: i32, ptr: i32, len: i32);
        pub fn run_job(
            cmd_ptr: i32, cmd_len: i32, cwd_ptr: i32, cwd_len: i32,
        ) -> i64;
        pub fn run_command(cmd_ptr: i32, cmd_len: i32) -> i64;
        pub fn timer_start(ms: i64) -> i64;
        pub fn fs_write(
            path_ptr: i32, path_len: i32, data_ptr: i32, data_len: i32,
            append: i32,
        ) -> i64;
        pub fn fs_read(
            path_ptr: i32, path_len: i32, offset: i64,
            out_ptr: i32, out_cap: i32,
        ) -> i64;
        pub fn fs_list(
            path_ptr: i32, path_len: i32, flags: i32,
            out_ptr: i32, out_cap: i32,
        ) -> i64;
        pub fn fs_write_sync(
            path_ptr: i32, path_len: i32, data_ptr: i32, data_len: i32,
            append: i32,
        ) -> i64;
        pub fn fs_read_sync(
            path_ptr: i32, path_len: i32, offset: i64,
            out: i32, cap: i32, len_out: i32, eof_out: i32,
        ) -> i32;
        pub fn fs_rename(
            from_ptr: i32, from_len: i32, to_ptr: i32, to_len: i32,
            flags: i32,
        ) -> i64;
        pub fn fs_remove(path_ptr: i32, path_len: i32) -> i64;
        pub fn fs_root(out: i32, cap: i32, len_out: i32) -> i32;
        pub fn home_dir(out: i32, cap: i32, len_out: i32) -> i32;
        pub fn time_now() -> i64;
        pub fn db_exec(
            sql_ptr: i32, sql_len: i32, params_ptr: i32, params_len: i32,
        ) -> i64;
        pub fn db_query(
            sql_ptr: i32, sql_len: i32, params_ptr: i32, params_len: i32,
        ) -> i64;
        pub fn db_batch(block_ptr: i32, block_len: i32) -> i64;
        pub fn db_exec_sync(
            sql_ptr: i32, sql_len: i32, params_ptr: i32, params_len: i32,
            out_ptr: i32,
        ) -> i32;
        pub fn db_query_sync(
            sql_ptr: i32, sql_len: i32, params_ptr: i32, params_len: i32,
            owned_out: i32,
        ) -> i32;
    }
}

/// Host-target stubs so the SDK compiles for docs/tests; a plugin only
/// works on wasm32. Every call fails with -E_HOST.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod raw {
    #![allow(unused_variables, clippy::missing_safety_doc)]
    pub unsafe fn intern(ptr: i32, len: i32) -> i64 { -7 }
    pub unsafe fn intern_name(id: i32, out: i32, cap: i32, len_out: i32) -> i32 { -7 }
    pub unsafe fn subscribe(id: i32) -> i32 { -7 }
    pub unsafe fn unsubscribe(id: i32) -> i32 { -7 }
    pub unsafe fn list(kind: i32, owned_out: i32) -> i32 { -7 }
    pub unsafe fn resolve(kind: i32, id: i32, owned_out: i32) -> i32 { -7 }
    pub unsafe fn self_info(out: i32) -> i32 { -7 }
    pub unsafe fn get_option(kind: i32, id: i32, name_ptr: i32, name_len: i32, out: i32, cap: i32, len_out: i32) -> i32 { -7 }
    pub unsafe fn set_option(kind: i32, id: i32, name_ptr: i32, name_len: i32, val_ptr: i32, val_len: i32) -> i32 { -7 }
    pub unsafe fn format_expand(kind: i32, id: i32, fmt_ptr: i32, fmt_len: i32, out: i32, cap: i32, len_out: i32) -> i32 { -7 }
    pub unsafe fn send_keys(pane: i32, keys_ptr: i32, keys_len: i32, literal: i32) -> i32 { -7 }
    pub unsafe fn capture_pane(pane: i32, start: i32, end: i32, escapes: i32, out: i32, cap: i32, len_out: i32) -> i32 { -7 }
    pub unsafe fn pane_env(pane: i32, name_ptr: i32, name_len: i32, out: i32, cap: i32, len_out: i32) -> i32 { -7 }
    pub unsafe fn pane_fds(pane: i32, out: i32, cap: i32, len_out: i32) -> i32 { -7 }
    pub unsafe fn pane_pid(pane: i32) -> i64 { -7 }
    pub unsafe fn display_message(client: i32, msg_ptr: i32, msg_len: i32) -> i32 { -7 }
    #[allow(dead_code)]
    pub unsafe fn timer_cancel(token: i64) -> i32 { -7 }
    pub unsafe fn mode_open(window: i32, width: i32, height: i32, x: i32, y: i32, title_ptr: i32, title_len: i32) -> i64 { -7 }
    pub unsafe fn mode_write(mode: i64, ptr: i32, len: i32) -> i32 { -7 }
    pub unsafe fn mode_preview(mode: i64, pane: i64, x: i32, y: i32, w: i32, h: i32) -> i32 { -7 }
    pub unsafe fn mode_move(mode: i64, window: i32, x: i32, y: i32) -> i32 { -7 }
    pub unsafe fn mode_resize(mode: i64, width: i32, height: i32) -> i32 { -7 }
    pub unsafe fn mode_close(mode: i64) -> i32 { -7 }
    pub unsafe fn last_error(out: i32, cap: i32, len_out: i32) -> i32 { 0 }
    pub unsafe fn log(level: i32, ptr: i32, len: i32) {}
    pub unsafe fn run_job(cmd_ptr: i32, cmd_len: i32, cwd_ptr: i32, cwd_len: i32) -> i64 { -7 }
    pub unsafe fn run_command(cmd_ptr: i32, cmd_len: i32) -> i64 { -7 }
    pub unsafe fn timer_start(ms: i64) -> i64 { -7 }
    pub unsafe fn fs_write(path_ptr: i32, path_len: i32, data_ptr: i32, data_len: i32, append: i32) -> i64 { -7 }
    pub unsafe fn fs_read(path_ptr: i32, path_len: i32, offset: i64, out_ptr: i32, out_cap: i32) -> i64 { -7 }
    pub unsafe fn fs_list(path_ptr: i32, path_len: i32, flags: i32, out_ptr: i32, out_cap: i32) -> i64 { -7 }
    pub unsafe fn fs_write_sync(path_ptr: i32, path_len: i32, data_ptr: i32, data_len: i32, append: i32) -> i64 { -7 }
    pub unsafe fn fs_read_sync(path_ptr: i32, path_len: i32, offset: i64, out: i32, cap: i32, len_out: i32, eof_out: i32) -> i32 { -7 }
    pub unsafe fn fs_rename(from_ptr: i32, from_len: i32, to_ptr: i32, to_len: i32, flags: i32) -> i64 { -7 }
    pub unsafe fn fs_remove(path_ptr: i32, path_len: i32) -> i64 { -7 }
    pub unsafe fn fs_root(out: i32, cap: i32, len_out: i32) -> i32 { -7 }
    pub unsafe fn home_dir(out: i32, cap: i32, len_out: i32) -> i32 { -7 }
    pub unsafe fn time_now() -> i64 { 0 }
    pub unsafe fn db_exec(sql_ptr: i32, sql_len: i32, params_ptr: i32, params_len: i32) -> i64 { -7 }
    pub unsafe fn db_query(sql_ptr: i32, sql_len: i32, params_ptr: i32, params_len: i32) -> i64 { -7 }
    pub unsafe fn db_batch(block_ptr: i32, block_len: i32) -> i64 { -7 }
    pub unsafe fn db_exec_sync(sql_ptr: i32, sql_len: i32, params_ptr: i32, params_len: i32, out_ptr: i32) -> i32 { -7 }
    pub unsafe fn db_query_sync(sql_ptr: i32, sql_len: i32, params_ptr: i32, params_len: i32, owned_out: i32) -> i32 { -7 }
}

/// ABI allocator: 8-aligned, size echoed back on free.
pub fn alloc(size: i32) -> i32 {
    if size <= 0 {
        return 8; // nonzero dangling; never dereferenced for len 0
    }
    unsafe {
        let layout = Layout::from_size_align_unchecked(size as usize, 8);
        std_alloc(layout) as i32
    }
}

pub fn free(ptr: i32, size: i32) {
    if size <= 0 || ptr == 0 {
        return;
    }
    unsafe {
        let layout = Layout::from_size_align_unchecked(size as usize, 8);
        dealloc(ptr as *mut u8, layout);
    }
}

/// Copy a host-written buffer out of linear memory and free it.
pub fn take_buf(ptr: i32, len: i32) -> Vec<u8> {
    if ptr == 0 || len <= 0 {
        return Vec::new();
    }
    let bytes = unsafe {
        std::slice::from_raw_parts(ptr as *const u8, len as usize).to_vec()
    };
    free(ptr, len);
    bytes
}

/// Allocate a guest buffer and copy `bytes` into it; ownership passes to
/// the host (which frees it with pgh_free).
pub fn give_buf(bytes: &[u8]) -> (i32, i32) {
    let len = bytes.len() as i32;
    let ptr = alloc(len);
    if ptr != 0 && len > 0 {
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                ptr as *mut u8,
                bytes.len(),
            );
        }
    }
    (ptr, len)
}

/// Write a little-endian u32 into an out-slot the host provided.
///
/// # Safety
/// `at` must point at 4 writable bytes in linear memory.
pub unsafe fn write_u32_slot(at: i32, value: u32) {
    std::ptr::write_unaligned(at as *mut u32, value.to_le());
}

/// A host-transferred allocation (OwnedBuf): freed exactly once on drop.
/// The only form OwnedBuf results ever take - leaking one requires raw
/// FFI. Dereferences to the bytes in place (no extra copy).
pub struct Owned {
    ptr: u32,
    len: u32,
}

impl Owned {
    /// Wrap the {ptr, len} pair the host wrote into an out-struct.
    pub(crate) fn from_out_struct(out: [u32; 2]) -> Self {
        Self { ptr: out[0], len: out[1] }
    }
}

impl std::ops::Deref for Owned {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        if self.ptr == 0 || self.len == 0 {
            return &[];
        }
        unsafe {
            std::slice::from_raw_parts(self.ptr as *const u8, self.len as usize)
        }
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        free(self.ptr as i32, self.len as i32);
    }
}

pub fn log(level: i32, msg: &str) {
    unsafe { raw::log(level, msg.as_ptr() as i32, msg.len() as i32) };
}

/// Panics become wasm traps (host failure policy); log the message first so
/// `plugin-log` shows why.
pub fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            log(3, &format!("plugin panic: {info}"));
        }));
    });
}
