//! Hand-written ABI-v1 tmux plugin, used as the conformance guinea pig
//! until the SDK exists. Logs lifecycle events and proves sync host calls
//! (subscribe, list_sessions) work.
//!
//! Build: cargo build -p hello-raw --target wasm32-unknown-unknown --release

use std::alloc::{alloc, dealloc, Layout};
use std::cell::RefCell;

#[link(wasm_import_module = "tmux")]
extern "C" {
    fn host_call(req_ptr: i32, req_len: i32, out_ptr: i32, out_len_ptr: i32) -> i32;
    fn host_log(level: i32, ptr: i32, len: i32);
}

thread_local! {
    static EVENTS_SEEN: RefCell<u64> = const { RefCell::new(0) };
}

fn log(level: i32, msg: &str) {
    unsafe { host_log(level, msg.as_ptr() as i32, msg.len() as i32) };
}

/// Synchronous host call; returns (status, response JSON).
fn call(req: &str) -> (i32, String) {
    let mut out_ptr: u32 = 0;
    let mut out_len: u32 = 0;
    let status = unsafe {
        host_call(
            req.as_ptr() as i32,
            req.len() as i32,
            &mut out_ptr as *mut u32 as i32,
            &mut out_len as *mut u32 as i32,
        )
    };
    if status > 1 || out_ptr == 0 {
        return (status, String::new());
    }
    let resp = unsafe {
        let bytes =
            std::slice::from_raw_parts(out_ptr as *const u8, out_len as usize)
                .to_vec();
        pgh_free(out_ptr as i32, out_len as i32);
        String::from_utf8_lossy(&bytes).into_owned()
    };
    (status, resp)
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
    let config = unsafe {
        String::from_utf8_lossy(std::slice::from_raw_parts(
            cfg_ptr as *const u8,
            cfg_len as usize,
        ))
        .into_owned()
    };
    pgh_free(cfg_ptr, cfg_len);
    log(1, &format!("hello-raw initialized, config: {config}"));

    let (status, resp) = call(
        r#"{"method":"subscribe","params":{"events":["session-created","window-linked","window-renamed","pane-created"]}}"#,
    );
    if status != 0 {
        log(3, &format!("subscribe failed: {resp}"));
        return 1;
    }

    let (status, resp) = call(r#"{"method":"list_sessions","params":{}}"#);
    if status == 0 {
        log(1, &format!("sessions at startup: {resp}"));
    }

    // Exercise the sync effects: user option + status message.
    let (status, resp) = call(
        r#"{"method":"set_option","params":{"name":"@hello","value":"world"}}"#,
    );
    if status != 0 {
        log(3, &format!("set_option failed: {resp}"));
    }
    let (status, resp) = call(
        r#"{"method":"display_message","params":{"message":"hello-raw is alive"}}"#,
    );
    if status != 0 {
        log(2, &format!("display_message: {resp}"));
    }
    0
}

#[no_mangle]
pub extern "C" fn pgh_on_event(ptr: i32, len: i32) {
    let json = unsafe {
        String::from_utf8_lossy(std::slice::from_raw_parts(
            ptr as *const u8,
            len as usize,
        ))
        .into_owned()
    };
    pgh_free(ptr, len);

    let count = EVENTS_SEEN.with(|c| {
        let mut c = c.borrow_mut();
        *c += 1;
        *c
    });

    let parsed = serde_json::from_str::<serde_json::Value>(&json).ok();
    let name = parsed
        .as_ref()
        .and_then(|v| v.get("event").and_then(|e| e.as_str()).map(String::from))
        .unwrap_or_else(|| "?".into());
    log(1, &format!("event #{count}: {name} ({json})"));

    // On pane creation, prove capture_pane + get_option round-trips.
    if name == "pane-created" {
        if let Some(pane) = parsed
            .as_ref()
            .and_then(|v| v.pointer("/scope/pane").and_then(|p| p.as_u64()))
        {
            let (status, resp) = call(&format!(
                r#"{{"method":"capture_pane","params":{{"pane":{pane},"start":0,"end":3}}}}"#
            ));
            log(1, &format!("capture({status}): {resp}"));
        }
        let (status, resp) =
            call(r#"{"method":"get_option","params":{"name":"@hello"}}"#);
        log(1, &format!("get_option @hello ({status}): {resp}"));
    }
}

#[no_mangle]
pub extern "C" fn pgh_on_unload() {
    let count = EVENTS_SEEN.with(|c| *c.borrow());
    log(1, &format!("goodbye after {count} events"));
}
