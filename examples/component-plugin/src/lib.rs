//! Example Hermez Component Model plugin.
//!
//! Uses wit-bindgen (via cargo-component) for type-safe host-guest interfaces.
//! Build: cargo component build --release

#[allow(warnings)]
mod bindings;

use bindings::exports::hermez::plugin::plugin::Guest;
use bindings::hermez::plugin::host;

struct HermezPlugin;

impl Guest for HermezPlugin {
    fn register() {
        host::log("info", "Component plugin 'example-component-plugin' registered");
    }

    fn on_session_start(ctx: String) {
        host::log("debug", &format!("on_session_start: {}", ctx));
    }

    fn on_session_end(ctx: String) {
        host::log("debug", &format!("on_session_end: {}", ctx));
    }

    fn handle_tool(name: String, args: String) -> Result<String, String> {
        host::log("info", &format!("handle_tool: {} with args {}", name, args));
        match name.as_str() {
            "greet" => Ok(format!(
                "{{\"greeting\": \"Hello from Component Model! name={}, args={}\"}}",
                "example-component-plugin", args
            )),
            _ => Err(format!("Unknown tool: '{}'", name)),
        }
    }
}

bindings::export!(HermezPlugin with_types_in bindings);
