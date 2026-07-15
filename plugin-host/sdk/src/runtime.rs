//! Guest-side runtime plumbing: raw host imports, the linear-memory
//! allocator protocol, and buffer helpers.

use std::alloc::{alloc as std_alloc, dealloc, Layout};

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "tmux")]
extern "C" {
    fn host_call(req_ptr: i32, req_len: i32, out_ptr: i32, out_len_ptr: i32) -> i32;
    fn host_request(req_ptr: i32, req_len: i32) -> i64;
    fn host_log(level: i32, ptr: i32, len: i32);
}

// Stubs so the SDK compiles for host targets (docs, tests); a plugin only
// works on wasm32.
#[cfg(not(target_arch = "wasm32"))]
unsafe fn host_call(_: i32, _: i32, _: i32, _: i32) -> i32 {
    2
}
#[cfg(not(target_arch = "wasm32"))]
unsafe fn host_request(_: i32, _: i32) -> i64 {
    -7
}
#[cfg(not(target_arch = "wasm32"))]
unsafe fn host_log(_: i32, _: i32, _: i32) {}

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

pub fn log(level: i32, msg: &str) {
    unsafe { host_log(level, msg.as_ptr() as i32, msg.len() as i32) };
}

/// Raw synchronous host call: returns (status, response bytes).
pub fn raw_host_call(request: &str) -> (i32, Vec<u8>) {
    let mut out_ptr: u32 = 0;
    let mut out_len: u32 = 0;
    let status = unsafe {
        host_call(
            request.as_ptr() as i32,
            request.len() as i32,
            &mut out_ptr as *mut u32 as i32,
            &mut out_len as *mut u32 as i32,
        )
    };
    if status > 1 || out_ptr == 0 {
        return (status, Vec::new());
    }
    (status, take_buf(out_ptr as i32, out_len as i32))
}

/// Raw asynchronous host request: token > 0 or negative error code.
pub fn raw_host_request(request: &str) -> i64 {
    unsafe { host_request(request.as_ptr() as i32, request.len() as i32) }
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
