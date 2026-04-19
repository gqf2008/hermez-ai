//! Example Hermes WASM plugin.
//!
//! Demonstrates the Phase 2 ABI:
//!   • export `memory`
//!   • export `_plugin_register() -> i32`
//!   • export lifecycle hooks: `on_session_start(ctx_ptr, ctx_len) -> i32`
//!   • export tool handler: `handle_tool(name_ptr, name_len, args_ptr, args_len, out_ptr, out_cap) -> i32`

use std::slice;
use std::str;

// Static mutable state (single-threaded in WASM)
static mut CALL_COUNT: u32 = 0;

/// Host writes context JSON into guest memory, then calls this hook.
#[no_mangle]
pub extern "C" fn on_session_start(ctx_ptr: i32, ctx_len: i32) -> i32 {
    let _json = unsafe {
        let bytes = slice::from_raw_parts(ctx_ptr as *const u8, ctx_len as usize);
        str::from_utf8_unchecked(bytes)
    };

    unsafe {
        CALL_COUNT += 1;
    }

    0
}

/// Called once when the plugin is loaded.
#[no_mangle]
pub extern "C" fn _plugin_register() -> i32 {
    unsafe {
        CALL_COUNT = 0;
    }
    0
}

/// Tool handler — host calls this when a WASM-registered tool is invoked.
///
/// ABI:
///   name_ptr/len  → tool name string
///   args_ptr/len  → JSON arguments string
///   out_ptr/cap   → output buffer (host pre-allocated)
///
/// Returns 0 on success, non-zero on error.
#[no_mangle]
pub extern "C" fn handle_tool(
    name_ptr: i32,
    name_len: i32,
    args_ptr: i32,
    args_len: i32,
    out_ptr: i32,
    out_cap: i32,
) -> i32 {
    let name = unsafe {
        let bytes = slice::from_raw_parts(name_ptr as *const u8, name_len as usize);
        str::from_utf8_unchecked(bytes)
    };

    let args = unsafe {
        let bytes = slice::from_raw_parts(args_ptr as *const u8, args_len as usize);
        str::from_utf8_unchecked(bytes)
    };

    let result = match name {
        "greet" => format!("{{\"greeting\": \"Hello from WASM plugin! args={}\"}}", args),
        _ => format!("{{\"error\": \"Unknown tool '{}'\"}}", name),
    };

    let out = result.as_bytes();
    let to_write = out.len().min(out_cap as usize);
    unsafe {
        let dst = slice::from_raw_parts_mut(out_ptr as *mut u8, to_write);
        dst.copy_from_slice(&out[..to_write]);
    }

    0
}
