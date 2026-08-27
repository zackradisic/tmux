//! Hand-written tmux plugin against the raw C-like ABI (no SDK), kept as
//! the conformance guinea pig: typed per-method imports, NUL-included
//! strings, OutBuf results with grow-on-E_LIMIT, OwnedBuf transfers and
//! binary event buffers, all by hand.
//!
//! Build: cargo build -p hello-raw --target wasm32-unknown-unknown --release

use std::alloc::{alloc, dealloc, Layout};
use std::cell::RefCell;

#[link(wasm_import_module = "tmux")]
extern "C" {
    fn intern(ptr: i32, len: i32) -> i64;
    fn intern_name(id: i32, out: i32, cap: i32, len_out: i32) -> i32;
    fn subscribe(id: i32) -> i32;
    fn list(kind: i32, owned_out: i32) -> i32;
    fn set_option(
        kind: i32, id: i32, name_ptr: i32, name_len: i32,
        val_ptr: i32, val_len: i32,
    ) -> i32;
    fn get_option(
        kind: i32, id: i32, name_ptr: i32, name_len: i32,
        out: i32, cap: i32, len_out: i32,
    ) -> i32;
    fn capture_pane(
        pane: i32, start: i32, end: i32, escapes: i32,
        out: i32, cap: i32, len_out: i32,
    ) -> i32;
    fn format_expand(
        kind: i32, id: i32, fmt_ptr: i32, fmt_len: i32,
        out: i32, cap: i32, len_out: i32,
    ) -> i32;
    fn display_message(client: i32, msg_ptr: i32, msg_len: i32) -> i32;
    fn log(level: i32, ptr: i32, len: i32);
}

thread_local! {
    static EVENTS_SEEN: RefCell<u64> = const { RefCell::new(0) };
}

fn logf(level: i32, msg: &str) {
    unsafe { log(level, msg.as_ptr() as i32, msg.len() as i32) };
}

/// A Str argument: (ptr, len) with the NUL at data[len]. c-string
/// literals carry the NUL in the data segment already.
fn s(cstr: &std::ffi::CStr) -> (i32, i32) {
    let b = cstr.to_bytes_with_nul();
    (b.as_ptr() as i32, (b.len() - 1) as i32)
}

/// Intern an event/key name.
fn intern_str(name: &str) -> u32 {
    let id = unsafe { intern(name.as_ptr() as i32, name.len() as i32) };
    if id > 0 {
        id as u32
    } else {
        0
    }
}

#[no_mangle]
pub extern "C" fn pgh_abi_version() -> i32 {
    1
}

#[no_mangle]
pub extern "C" fn pgh_alloc(size: i32) -> i32 {
    if size <= 0 {
        return 8; // nonzero dangling; never dereferenced for len 0
    }
    unsafe {
        let layout = Layout::from_size_align_unchecked(size as usize, 8);
        alloc(layout) as i32
    }
}

#[no_mangle]
pub extern "C" fn pgh_free(ptr: i32, size: i32) {
    if size <= 0 || ptr == 0 {
        return;
    }
    unsafe {
        let layout = Layout::from_size_align_unchecked(size as usize, 8);
        dealloc(ptr as *mut u8, layout);
    }
}

#[no_mangle]
pub extern "C" fn pgh_init(cfg_ptr: i32, cfg_len: i32) -> i32 {
    pgh_free(cfg_ptr, cfg_len); // config not used; free the transfer

    for name in
        ["session-created", "window-linked", "window-renamed", "pane-created"]
    {
        let id = intern_str(name);
        if id == 0 || unsafe { subscribe(id as i32) } != 0 {
            logf(3, &format!("subscribe {name} failed"));
            return 1;
        }
    }

    // OwnedBuf transfer: list sessions, report the byte count, free it.
    let mut owned: [u32; 2] = [0, 0];
    let rc = unsafe { list(0, owned.as_mut_ptr() as i32) };
    if rc == 0 {
        logf(1, &format!("sessions list buffer: {} bytes", owned[1]));
        pgh_free(owned[0] as i32, owned[1] as i32);
    }

    // Sync effects: user option + status message, zero-copy strings.
    let (np, nl) = s(c"@hello");
    let (vp, vl) = s(c"world");
    if unsafe { set_option(-1, 0, np, nl, vp, vl) } != 0 {
        logf(3, "set_option failed");
    }
    let (mp, ml) = s(c"hello-raw is alive");
    if unsafe { display_message(-1, mp, ml) } != 0 {
        logf(2, "display_message failed");
    }

    // format_expand round trip at server scope.
    let (fp, fl) = s(c"host=#{host_short} version=#{version}");
    let mut out = vec![0u8; 256];
    let mut flen: u32 = 0;
    let rc = unsafe {
        format_expand(
            -1,
            0,
            fp,
            fl,
            out.as_mut_ptr() as i32,
            out.len() as i32,
            &mut flen as *mut u32 as i32,
        )
    };
    out.truncate(flen as usize);
    logf(
        1,
        &format!(
            "format_expand({rc}): {}",
            String::from_utf8_lossy(&out)
        ),
    );
    0
}

#[no_mangle]
pub extern "C" fn pgh_on_event(ptr: i32, len: i32) {
    // Parse the binary event header by hand: u32 event id, u64 seq, four
    // u32 scope ids (0xffffffff = none).
    let bytes = unsafe {
        std::slice::from_raw_parts(ptr as *const u8, len as usize).to_vec()
    };
    pgh_free(ptr, len);
    if bytes.len() < 28 {
        return;
    }
    let u32_at = |o: usize| {
        u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap())
    };
    let event_id = u32_at(0);
    let pane = u32_at(24);

    let count = EVENTS_SEEN.with(|c| {
        let mut c = c.borrow_mut();
        *c += 1;
        *c
    });

    // Reverse-lookup the name through an OutBuf with grow-on-E_LIMIT.
    let mut name_buf = vec![0u8; 64];
    let mut name = String::from("?");
    loop {
        let mut need: u32 = 0;
        let rc = unsafe {
            intern_name(
                event_id as i32,
                name_buf.as_mut_ptr() as i32,
                name_buf.len() as i32,
                &mut need as *mut u32 as i32,
            )
        };
        if rc == 0 {
            name_buf.truncate(need as usize);
            name = String::from_utf8_lossy(&name_buf).into_owned();
            break;
        }
        if rc == -6 && need as usize > name_buf.len() {
            name_buf.resize(need as usize, 0);
            continue;
        }
        break;
    }
    logf(1, &format!("event #{count}: {name}"));

    // On pane creation, prove capture_pane + get_option round-trips.
    if name == "pane-created" && pane != u32::MAX {
        let mut out = vec![0u8; 4096];
        let mut got: u32 = 0;
        let rc = unsafe {
            capture_pane(
                pane as i32,
                0,
                3,
                0,
                out.as_mut_ptr() as i32,
                out.len() as i32,
                &mut got as *mut u32 as i32,
            )
        };
        logf(1, &format!("capture({rc}): {got} bytes"));

        let (np, nl) = s(c"@hello");
        let mut val = vec![0u8; 64];
        let mut vlen: u32 = 0;
        let rc = unsafe {
            get_option(
                -1,
                0,
                np,
                nl,
                val.as_mut_ptr() as i32,
                val.len() as i32,
                &mut vlen as *mut u32 as i32,
            )
        };
        val.truncate(vlen as usize);
        logf(
            1,
            &format!(
                "get_option @hello ({rc}): {}",
                String::from_utf8_lossy(&val)
            ),
        );
    }
}

#[no_mangle]
pub extern "C" fn pgh_on_unload() {
    let count = EVENTS_SEEN.with(|c| *c.borrow());
    logf(1, &format!("goodbye after {count} events"));
}
