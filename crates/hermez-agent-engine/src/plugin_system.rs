//! Plugin system — runtime discovery, loading, and lifecycle hooks.
//!
//! Mirrors the Python `plugins.py` architecture:
//! - PluginManifest: parsed from plugin.yaml
//! - PluginManager: discovers, loads, and manages plugins
//! - HookRegistry: calls lifecycle hooks at agent-engine boundaries
//! - PluginContext: facade given to plugins for tool/hook registration
//!
//! Supported hooks:
//!   pre_tool_call, post_tool_call, pre_llm_call, post_llm_call,
//!   on_session_start, on_session_end, on_session_finalize, on_session_reset

#![allow(dead_code)]

wasmtime::component::bindgen!({
    path: "../../wit",
    world: "hermez-plugin"
});

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hermez_tools::registry::ToolRegistry;

// ---------------------------------------------------------------------------
// Plugin manifest
// ---------------------------------------------------------------------------

/// Parsed plugin.yaml manifest.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    /// Required environment variables (simple names or rich dicts).
    #[serde(default)]
    pub requires_env: Vec<serde_yaml::Value>,
    /// Tools this plugin provides.
    #[serde(default)]
    pub provides_tools: Vec<String>,
    /// Hooks this plugin registers.
    #[serde(default)]
    pub provides_hooks: Vec<String>,
    /// Pip dependencies to install.
    #[serde(default)]
    pub pip_dependencies: Vec<String>,
    /// Manifest schema version.
    #[serde(default = "default_manifest_version")]
    pub manifest_version: i64,
    /// WASM entry point (relative to plugin dir, e.g. "plugin.wasm").
    #[serde(default)]
    pub wasm_entry: Option<String>,
    /// WebAssembly Component Model entry point (e.g. "plugin.component.wasm").
    #[serde(default)]
    pub component_entry: Option<String>,
}

fn default_manifest_version() -> i64 {
    1
}

impl PluginManifest {
    /// Read and parse a plugin.yaml from a directory.
    pub fn from_dir(dir: &Path) -> Option<Self> {
        let path = dir.join("plugin.yaml");
        if !path.exists() {
            return None;
        }
        let content = std::fs::read_to_string(&path).ok()?;
        Self::from_yaml(&content)
    }

    /// Parse from YAML string.
    pub fn from_yaml(yaml: &str) -> Option<Self> {
        serde_yaml::from_str(yaml).ok()
    }

    /// Validate the manifest.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.is_empty() {
            return Err("Plugin manifest missing 'name'".into());
        }
        if self.name.contains('/') || self.name.contains('\\') || self.name.contains("..") {
            return Err(format!("Invalid plugin name: '{}'", self.name));
        }
        if self.manifest_version > 1 {
            return Err(format!(
                "Unsupported manifest version {} (max supported: 1)",
                self.manifest_version
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Loaded plugin
// ---------------------------------------------------------------------------

/// Runtime state for a loaded plugin.
#[derive(Clone)]
pub struct LoadedPlugin {
    pub manifest: PluginManifest,
    pub path: PathBuf,
    pub enabled: bool,
    pub hooks_registered: Vec<String>,
    pub tools_registered: Vec<String>,
    pub error: Option<String>,
    /// Active WASM runtime (if this plugin has a wasm_entry).
    pub wasm_runtime: Option<Arc<WasmPluginRuntime>>,
    /// Active Component Model runtime (if this plugin has a component_entry).
    pub component_runtime: Option<Arc<ComponentPluginRuntime>>,
}

impl std::fmt::Debug for LoadedPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedPlugin")
            .field("manifest", &self.manifest)
            .field("path", &self.path)
            .field("enabled", &self.enabled)
            .field("hooks_registered", &self.hooks_registered)
            .field("tools_registered", &self.tools_registered)
            .field("error", &self.error)
            .field("wasm_runtime", &self.wasm_runtime.is_some())
            .field("component_runtime", &self.component_runtime.is_some())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Plugin manager
// ---------------------------------------------------------------------------

/// Discovers and manages plugins from the plugins directory.
pub struct PluginManager {
    plugins_dir: PathBuf,
    plugins: Mutex<Vec<LoadedPlugin>>,
    disabled: Vec<String>,
}

impl PluginManager {
    pub fn new() -> Self {
        let plugins_dir = hermez_core::get_hermez_home().join("plugins");
        Self {
            plugins_dir,
            plugins: Mutex::new(Vec::new()),
            disabled: Self::load_disabled_list(),
        }
    }

    pub fn with_dir(plugins_dir: PathBuf) -> Self {
        Self {
            plugins_dir,
            plugins: Mutex::new(Vec::new()),
            disabled: Self::load_disabled_list(),
        }
    }

    /// Discover and load all plugins from the plugins directory.
    pub fn discover(&self) -> Vec<LoadedPlugin> {
        let mut plugins = Vec::new();
        if !self.plugins_dir.exists() {
            return plugins;
        }

        let entries = match std::fs::read_dir(&self.plugins_dir) {
            Ok(e) => e,
            Err(_) => return plugins,
        };

        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let Some(manifest) = PluginManifest::from_dir(&path) else {
                // No plugin.yaml — skip
                continue;
            };

            if let Err(e) = manifest.validate() {
                plugins.push(LoadedPlugin {
                    manifest,
                    path,
                    enabled: false,
                    hooks_registered: Vec::new(),
                    tools_registered: Vec::new(),
                    error: Some(e),
                    wasm_runtime: None,
                    component_runtime: None,
                });
                continue;
            }

            let enabled = !self.disabled.contains(&manifest.name);
            plugins.push(LoadedPlugin {
                manifest: manifest.clone(),
                path: path.clone(),
                enabled,
                hooks_registered: manifest.provides_hooks.clone(),
                tools_registered: manifest.provides_tools.clone(),
                error: None,
                wasm_runtime: None,
                component_runtime: None,
            });
        }

        let mut guard = self.plugins.lock().unwrap();
        *guard = plugins.clone();
        plugins
    }

    /// Get all discovered plugins.
    pub fn list(&self) -> Vec<LoadedPlugin> {
        let guard = self.plugins.lock().unwrap();
        if guard.is_empty() {
            drop(guard);
            self.discover()
        } else {
            guard.clone()
        }
    }

    /// Get a single plugin by name.
    pub fn get(&self, name: &str) -> Option<LoadedPlugin> {
        self.list().into_iter().find(|p| p.manifest.name == name)
    }

    /// Check if a plugin is installed.
    pub fn is_installed(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// Check if a plugin is enabled.
    pub fn is_enabled(&self, name: &str) -> bool {
        self.get(name).map(|p| p.enabled).unwrap_or(false)
    }

    /// Load disabled plugins list from config.
    fn load_disabled_list() -> Vec<String> {
        if let Ok(config) = hermez_core::config::HermezConfig::load() {
            config.plugins.disabled.clone()
        } else {
            Vec::new()
        }
    }

    /// Get the plugins directory path.
    pub fn plugins_dir(&self) -> &Path {
        &self.plugins_dir
    }

    /// Auto-discover and load all enabled plugins.
    ///
    /// For each discovered plugin, creates a `PluginContext` and invokes
    /// the plugin's `register()` lifecycle if a `register.rs` / `register.py`
    /// entry point exists.  Also registers declared hooks into the global
    /// hook registry.
    pub fn auto_load(&self, tool_registry: Option<Arc<ToolRegistry>>) -> Vec<LoadedPlugin> {
        let plugins = self.discover();
        let mut loaded = Vec::new();

        for plugin in &plugins {
            if !plugin.enabled {
                tracing::debug!("Plugin '{}' is disabled, skipping auto-load", plugin.manifest.name);
                loaded.push(plugin.clone());
                continue;
            }

            // Check for Component Model entry point (Wassette-aligned, Phase 2+)
            let mut component_runtime: Option<Arc<ComponentPluginRuntime>> = None;
            if let Some(ref component_entry) = plugin.manifest.component_entry {
                let component_path = plugin.path.join(component_entry);
                if component_path.exists() {
                    match ComponentPluginRuntime::from_file(&component_path, &plugin.manifest.name) {
                        Ok(runtime) => {
                            let rt = Arc::new(runtime);
                            if let Err(e) = rt.instantiate() {
                                tracing::warn!("Component plugin '{}' instantiate failed: {}", plugin.manifest.name, e);
                            } else {
                                tracing::info!("Component plugin '{}' loaded from {}", plugin.manifest.name, component_path.display());
                                component_runtime = Some(rt);
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Failed to load Component plugin '{}': {}", plugin.manifest.name, e);
                        }
                    }
                } else {
                    tracing::warn!("Component entry '{}' not found for plugin '{}'", component_path.display(), plugin.manifest.name);
                }
            }

            // Check for Core WASM entry point (fallback)
            let mut wasm_runtime: Option<Arc<WasmPluginRuntime>> = None;
            if component_runtime.is_none() {
                if let Some(ref wasm_entry) = plugin.manifest.wasm_entry {
                    let wasm_path = plugin.path.join(wasm_entry);
                    if wasm_path.exists() {
                        match WasmPluginRuntime::from_file(&wasm_path, &plugin.manifest.name) {
                            Ok(runtime) => {
                                let rt = Arc::new(runtime);
                                if let Err(e) = rt.register() {
                                    tracing::warn!("WASM plugin '{}' register failed: {}", plugin.manifest.name, e);
                                } else {
                                    tracing::info!("WASM plugin '{}' loaded from {}", plugin.manifest.name, wasm_path.display());
                                    wasm_runtime = Some(rt);
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Failed to load WASM plugin '{}': {}", plugin.manifest.name, e);
                            }
                        }
                    } else {
                        tracing::warn!("WASM entry '{}' not found for plugin '{}'", wasm_path.display(), plugin.manifest.name);
                    }
                }
            }

            // Register declared tools (WASM / Component plugins get proxy handlers)
            for tool_name in &plugin.manifest.provides_tools {
                if let Some(ref rt) = component_runtime {
                    // Component model tool: register a proxy handler
                    let comp_name = tool_name.clone();
                    let comp_rt: Arc<ComponentPluginRuntime> = rt.clone();
                    let handler = Arc::new(move |args: serde_json::Value| -> hermez_core::Result<String> {
                        let args_str = args.to_string();
                        comp_rt.invoke_tool(&comp_name, &args_str)
                            .map_err(|e| hermez_core::HermezError::with_source(
                                hermez_core::errors::ErrorCategory::ToolError,
                                format!("Component tool error: {e}"),
                                e,
                            ))
                    });
                    if let Some(ref tr) = tool_registry {
                        let tools_dir = plugin.path.join("tools");
                        let schema_path = tools_dir.join(format!("{}.json", tool_name));
                        let schema = if schema_path.exists() {
                            if let Ok(content) = std::fs::read_to_string(&schema_path) {
                                serde_json::from_str(&content).unwrap_or_else(|_| {
                                    serde_json::json!({"name": tool_name, "description": format!("Component tool from plugin {}", plugin.manifest.name)})
                                })
                            } else {
                                serde_json::json!({"name": tool_name, "description": format!("Component tool from plugin {}", plugin.manifest.name)})
                            }
                        } else {
                            serde_json::json!({"name": tool_name, "description": format!("Component tool from plugin {}", plugin.manifest.name)})
                        };
                        tr.register(
                            tool_name.clone(),
                            plugin.manifest.name.clone(),
                            schema,
                            handler,
                            None,
                            Vec::new(),
                            format!("Component tool '{}' from plugin '{}'", tool_name, plugin.manifest.name),
                            "🔌".to_string(),
                            None,
                        );
                        tracing::info!("Registered Component tool '{}' from plugin '{}'", tool_name, plugin.manifest.name);
                    }
                } else if let Some(ref rt) = wasm_runtime {
                    // Core WASM tool: register a proxy handler that delegates to the guest
                    let wasm_name = tool_name.clone();
                    let wasm_rt: Arc<WasmPluginRuntime> = rt.clone();
                    let handler = Arc::new(move |args: serde_json::Value| -> hermez_core::Result<String> {
                        let args_str = args.to_string();
                        wasm_rt.invoke_tool(&wasm_name, &args_str)
                            .map_err(|e| hermez_core::HermezError::with_source(
                                hermez_core::errors::ErrorCategory::ToolError,
                                format!("WASM tool error: {e}"),
                                e,
                            ))
                    });
                    if let Some(ref tr) = tool_registry {
                        let tools_dir = plugin.path.join("tools");
                        let schema_path = tools_dir.join(format!("{}.json", tool_name));
                        let schema = if schema_path.exists() {
                            if let Ok(content) = std::fs::read_to_string(&schema_path) {
                                serde_json::from_str(&content).unwrap_or_else(|_| {
                                    serde_json::json!({"name": tool_name, "description": format!("WASM tool from plugin {}", plugin.manifest.name)})
                                })
                            } else {
                                serde_json::json!({"name": tool_name, "description": format!("WASM tool from plugin {}", plugin.manifest.name)})
                            }
                        } else {
                            serde_json::json!({"name": tool_name, "description": format!("WASM tool from plugin {}", plugin.manifest.name)})
                        };
                        tr.register(
                            tool_name.clone(),
                            plugin.manifest.name.clone(),
                            schema,
                            handler,
                            None,
                            Vec::new(),
                            format!("WASM tool '{}' from plugin '{}'", tool_name, plugin.manifest.name),
                            "🔌".to_string(),
                            None,
                        );
                        tracing::info!("Registered WASM tool '{}' from plugin '{}'", tool_name, plugin.manifest.name);
                    }
                } else {
                    // Non-WASM plugin: just log the declaration
                    let ctx = if let Some(ref tr) = tool_registry {
                        PluginContext::with_tools(plugin.manifest.clone(), global_hooks(), tr.clone())
                    } else {
                        PluginContext::new(plugin.manifest.clone(), global_hooks())
                    };
                    ctx.register_tool(tool_name);
                }
            }

            // Check for legacy register scripts (non-WASM plugins)
            if wasm_runtime.is_none() {
                let register_rs = plugin.path.join("register.rs");
                let register_py = plugin.path.join("register.py");
                let register_sh = plugin.path.join("register.sh");

                if register_sh.exists() {
                    let _ = std::process::Command::new("sh")
                        .arg(&register_sh)
                        .current_dir(&plugin.path)
                        .output();
                } else if register_py.exists() {
                    let _ = std::process::Command::new("python3")
                        .arg(&register_py)
                        .current_dir(&plugin.path)
                        .output();
                } else if register_rs.exists() {
                    tracing::debug!(
                        "Plugin '{}' has register.rs (not yet auto-compiled)",
                        plugin.manifest.name
                    );
                }
            }

            // Fire plugin load hook
            let mut hook_ctx = HashMap::new();
            hook_ctx.insert("plugin_name".into(), serde_json::json!(&plugin.manifest.name));
            hook_ctx.insert("plugin_version".into(), serde_json::json!(&plugin.manifest.version));
            hook_ctx.insert("wasm".into(), serde_json::json!(wasm_runtime.is_some()));
            global_hooks().invoke("on_plugin_load", &hook_ctx);

            loaded.push(LoadedPlugin {
                manifest: plugin.manifest.clone(),
                path: plugin.path.clone(),
                enabled: plugin.enabled,
                hooks_registered: plugin.hooks_registered.clone(),
                tools_registered: plugin.tools_registered.clone(),
                error: plugin.error.clone(),
                wasm_runtime,
                component_runtime,
            });
        }

        loaded
    }
}

impl Default for PluginManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Hook registry
// ---------------------------------------------------------------------------

/// Valid lifecycle hooks.
pub const VALID_HOOKS: &[&str] = &[
    "pre_tool_call",
    "post_tool_call",
    "pre_llm_call",
    "post_llm_call",
    "pre_api_request",
    "post_api_request",
    "on_session_start",
    "on_session_end",
    "on_session_finalize",
    "on_session_reset",
];

/// Callback type for hooks.
pub type HookCallback = Arc<dyn Fn(&str, &HashMap<String, serde_json::Value>) + Send + Sync>;

/// Registry of hook callbacks.
pub struct HookRegistry {
    hooks: Mutex<HashMap<String, Vec<HookCallback>>>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self {
            hooks: Mutex::new(HashMap::new()),
        }
    }

    /// Register a callback for a hook.
    pub fn register(&self, hook_name: &str, callback: HookCallback) {
        if !VALID_HOOKS.contains(&hook_name) {
            tracing::warn!("Registering unknown hook: {}", hook_name);
        }
        let mut hooks = self.hooks.lock().unwrap();
        hooks.entry(hook_name.to_string()).or_default().push(callback);
    }

    /// Unregister all callbacks for a hook.
    pub fn unregister(&self, hook_name: &str) {
        let mut hooks = self.hooks.lock().unwrap();
        hooks.remove(hook_name);
    }

    /// Invoke all callbacks for a hook.
    pub fn invoke(&self, hook_name: &str, context: &HashMap<String, serde_json::Value>) {
        let hooks = self.hooks.lock().unwrap();
        if let Some(callbacks) = hooks.get(hook_name) {
            for cb in callbacks {
                cb(hook_name, context);
            }
        }
    }

    /// Check if a hook has any registered callbacks.
    pub fn has_hooks(&self, hook_name: &str) -> bool {
        let hooks = self.hooks.lock().unwrap();
        hooks.get(hook_name).map(|v| !v.is_empty()).unwrap_or(false)
    }

    /// List all registered hooks and their callback counts.
    pub fn list(&self) -> HashMap<String, usize> {
        let hooks = self.hooks.lock().unwrap();
        hooks
            .iter()
            .map(|(k, v)| (k.clone(), v.len()))
            .collect()
    }
}

impl Default for HookRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Plugin context
// ---------------------------------------------------------------------------

/// Facade given to plugins for registration.
pub struct PluginContext {
    pub manifest: PluginManifest,
    hook_registry: Arc<HookRegistry>,
    tool_registry: Option<Arc<ToolRegistry>>,
}

impl PluginContext {
    pub fn new(manifest: PluginManifest, hook_registry: Arc<HookRegistry>) -> Self {
        Self {
            manifest,
            hook_registry,
            tool_registry: None,
        }
    }

    /// Create a context with tool registry access.
    pub fn with_tools(manifest: PluginManifest, hook_registry: Arc<HookRegistry>, tool_registry: Arc<ToolRegistry>) -> Self {
        Self {
            manifest,
            hook_registry,
            tool_registry: Some(tool_registry),
        }
    }

    /// Register a hook callback.
    pub fn register_hook(&self, hook_name: &str, callback: HookCallback) {
        if !VALID_HOOKS.contains(&hook_name) {
            tracing::warn!("Plugin '{}' registering unknown hook '{}'", self.manifest.name, hook_name);
        }
        self.hook_registry.register(hook_name, callback);
        tracing::info!(
            "Plugin '{}' registered hook '{}'",
            self.manifest.name,
            hook_name
        );
    }

    /// Register a tool declaration from the plugin.
    ///
    /// Note: actual tool schema/handler binding happens at the call-site
    /// (e.g. `AIAgent` or `ToolRegistry` owner) because `ToolRegistry::register`
    /// requires `&mut self`.
    pub fn register_tool(&self, tool_name: &str) {
        tracing::info!(
            "Plugin '{}' declared tool '{}'",
            self.manifest.name,
            tool_name
        );
    }

    /// Inject a message into the current conversation (placeholder).
    pub fn inject_message(&self, _message: &str) {
        tracing::info!("Plugin '{}' injected message", self.manifest.name);
    }

    /// Get the plugin manifest.
    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
}

// ---------------------------------------------------------------------------
// WASM Plugin Runtime (Phase 1)
// ---------------------------------------------------------------------------

/// WASI context for a plugin instance (WASI Preview 1 for wasm32-wasip1).
struct PluginWasiCtx {
    wasi: wasmtime_wasi::preview1::WasiP1Ctx,
}

/// Runtime for a single WASM plugin module.
///
/// Phase 1: loads `.wasm`, calls `_plugin_register`, exposes lifecycle hooks.
/// The `Engine` and `Module` are `Send + Sync`; a fresh `Store` is created per call.
pub struct WasmPluginRuntime {
    engine: wasmtime::Engine,
    module: wasmtime::Module,
    pub plugin_name: String,
}

impl WasmPluginRuntime {
    /// Compile a `.wasm` file into a runtime module.
    pub fn from_file(path: &Path, plugin_name: &str) -> anyhow::Result<Self> {
        let engine = wasmtime::Engine::default();
        let bytes = std::fs::read(path)
            .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;
        let module = wasmtime::Module::new(&engine, &bytes)
            .map_err(|e| anyhow::anyhow!("WASM compile error for {}: {}", plugin_name, e))?;
        Ok(Self {
            engine,
            module,
            plugin_name: plugin_name.to_string(),
        })
    }

    /// Create a new Store with WASI Preview 1 context for this plugin.
    fn new_store(&self) -> anyhow::Result<wasmtime::Store<PluginWasiCtx>> {
        let mut wasi_config = wasmtime_wasi::WasiCtxBuilder::new();
        wasi_config.inherit_stdout().inherit_stderr();
        let wasi = wasi_config.build_p1();
        let ctx = PluginWasiCtx { wasi };
        Ok(wasmtime::Store::new(&self.engine, ctx))
    }

    /// Link WASI Preview 1 imports into a linker.
    fn link_wasi(&self, linker: &mut wasmtime::Linker<PluginWasiCtx>) -> anyhow::Result<()> {
        wasmtime_wasi::preview1::add_to_linker_sync(linker, |cx: &mut PluginWasiCtx| &mut cx.wasi)
            .map_err(|e| anyhow::anyhow!("WASI link error: {}", e))?;
        Ok(())
    }

    /// Instantiate the module with WASI and call `_plugin_register` if it exists.
    pub fn register(&self) -> anyhow::Result<()> {
        let mut store = self.new_store()?;
        let mut linker = wasmtime::Linker::new(&self.engine);
        self.link_wasi(&mut linker)?;
        let instance = linker.instantiate(&mut store, &self.module)
            .map_err(|e| anyhow::anyhow!("WASM instantiate error for {}: {}", self.plugin_name, e))?;

        if let Ok(register_fn) = instance.get_typed_func::<(), ()>(&mut store, "_plugin_register") {
            register_fn.call(&mut store, ())
                .map_err(|e| anyhow::anyhow!("_plugin_register failed: {}", e))?;
            tracing::info!("WASM plugin '{}' registered", self.plugin_name);
        } else {
            tracing::debug!("WASM plugin '{}' has no _plugin_register export", self.plugin_name);
        }
        Ok(())
    }

    /// Call a lifecycle hook export (e.g. `on_session_start`, `pre_tool_call`).
    ///
    /// The guest receives a pointer to a JSON blob in shared memory.
    /// Phase 1 uses a simple linear-memory ABI:
    ///   guest:  fn hook_name(ctx_ptr: i32, ctx_len: i32) -> i32
    pub fn call_hook(&self, hook_name: &str, context: &HashMap<String, serde_json::Value>) -> anyhow::Result<()> {
        let mut store = self.new_store()?;
        let mut linker = wasmtime::Linker::new(&self.engine);
        self.link_wasi(&mut linker)?;
        let instance = linker.instantiate(&mut store, &self.module)
            .map_err(|e| anyhow::anyhow!("WASM instantiate error: {}", e))?;

        let hook_fn = match instance.get_typed_func::<(i32, i32), i32>(&mut store, hook_name) {
            Ok(f) => f,
            Err(_) => {
                // Hook not exported — silent skip (plugin doesn't implement it)
                return Ok(());
            }
        };

        let ctx_json = serde_json::to_string(context)?;
        let ctx_bytes = ctx_json.as_bytes();
        let ctx_len = ctx_bytes.len() as i32;

        // Allocate memory in the guest for the JSON payload
        let memory = instance.get_memory(&mut store, "memory")
            .ok_or_else(|| anyhow::anyhow!("WASM plugin '{}' missing 'memory' export", self.plugin_name))?;

        // Simple allocation: grow memory by 1 page (64KB) and use the start
        let page_size = 64 * 1024;
        let current_pages = memory.size(&store);
        memory.grow(&mut store, 1)
            .map_err(|e| anyhow::anyhow!("WASM memory grow failed: {}", e))?;
        let ptr = (current_pages * page_size) as i32;

        memory.write(&mut store, ptr as usize, ctx_bytes)
            .map_err(|e| anyhow::anyhow!("WASM memory write failed: {}", e))?;

        let result = hook_fn.call(&mut store, (ptr, ctx_len))
            .map_err(|e| anyhow::anyhow!("WASM hook '{}' call failed: {}", hook_name, e))?;

        if result != 0 {
            tracing::warn!("WASM hook '{}' returned error code {}", hook_name, result);
        } else {
            tracing::debug!("WASM hook '{}' succeeded", hook_name);
        }

        Ok(())
    }

    /// Invoke a tool handler exported by the WASM plugin.
    ///
    /// Expected guest export: `handle_tool(tool_name_ptr, tool_name_len, args_ptr, args_len, out_ptr, out_cap) -> i32`
    /// Returns 0 on success, writes result JSON to `out_ptr`.
    pub fn invoke_tool(&self, tool_name: &str, args: &str) -> anyhow::Result<String> {
        let mut store = self.new_store()?;
        let mut linker = wasmtime::Linker::new(&self.engine);
        self.link_wasi(&mut linker)?;
        let instance = linker.instantiate(&mut store, &self.module)
            .map_err(|e| anyhow::anyhow!("WASM instantiate error: {}", e))?;

        let handle_fn = match instance.get_typed_func::<(i32, i32, i32, i32, i32, i32), i32>(&mut store, "handle_tool") {
            Ok(f) => f,
            Err(_) => {
                anyhow::bail!("WASM plugin '{}' does not export 'handle_tool'", self.plugin_name);
            }
        };

        let memory = instance.get_memory(&mut store, "memory")
            .ok_or_else(|| anyhow::anyhow!("WASM plugin '{}' missing 'memory' export", self.plugin_name))?;

        let name_bytes = tool_name.as_bytes();
        let args_bytes = args.as_bytes();
        let name_len = name_bytes.len() as i32;
        let args_len = args_bytes.len() as i32;

        // Allocate memory: grow by 2 pages (128KB) for input + output
        let page_size = 64 * 1024;
        let current_pages = memory.size(&store);
        memory.grow(&mut store, 2)
            .map_err(|e| anyhow::anyhow!("WASM memory grow failed: {}", e))?;

        let name_ptr = (current_pages * page_size) as i32;
        let args_ptr = name_ptr + name_len + 64; // small padding
        let out_ptr = args_ptr + args_len + 64;
        let out_cap = page_size as i32; // 64KB output buffer

        memory.write(&mut store, name_ptr as usize, name_bytes)
            .map_err(|e| anyhow::anyhow!("WASM memory write failed: {}", e))?;
        memory.write(&mut store, args_ptr as usize, args_bytes)
            .map_err(|e| anyhow::anyhow!("WASM memory write failed: {}", e))?;

        let result = handle_fn.call(&mut store, (name_ptr, name_len, args_ptr, args_len, out_ptr, out_cap))
            .map_err(|e| anyhow::anyhow!("WASM handle_tool failed: {}", e))?;

        if result != 0 {
            anyhow::bail!("WASM plugin tool '{}' returned error code {}", tool_name, result);
        }

        // Read result from guest memory
        let mut out_bytes = vec![0u8; out_cap as usize];
        memory.read(&store, out_ptr as usize, &mut out_bytes)
            .map_err(|e| anyhow::anyhow!("WASM memory read failed: {}", e))?;
        // Trim null terminator
        let len = out_bytes.iter().position(|&b| b == 0).unwrap_or(out_bytes.len());
        let result_str = String::from_utf8_lossy(&out_bytes[..len]).to_string();

        Ok(result_str)
    }
}

// ---------------------------------------------------------------------------
// Component Model Host State
// ---------------------------------------------------------------------------

/// Host-side state for component model plugins.
/// Implements both the custom `host` interface and WASI preview2.
struct ComponentHostState {
    ctx: wasmtime_wasi::WasiCtx,
    table: wasmtime::component::ResourceTable,
}

impl ComponentHostState {
    fn new() -> Self {
        Self {
            ctx: wasmtime_wasi::WasiCtxBuilder::new().build(),
            table: wasmtime::component::ResourceTable::new(),
        }
    }
}

impl wasmtime_wasi::WasiView for ComponentHostState {
    fn ctx(&mut self) -> &mut wasmtime_wasi::WasiCtx {
        &mut self.ctx
    }
    fn table(&mut self) -> &mut wasmtime::component::ResourceTable {
        &mut self.table
    }
}

impl hermez::plugin::host::Host for ComponentHostState {
    fn log(&mut self, level: String, message: String) {
        match level.as_str() {
            "trace" => tracing::trace!(target: "wasm_plugin", "{}", message),
            "debug" => tracing::debug!(target: "wasm_plugin", "{}", message),
            "info" => tracing::info!(target: "wasm_plugin", "{}", message),
            "warn" => tracing::warn!(target: "wasm_plugin", "{}", message),
            "error" => tracing::error!(target: "wasm_plugin", "{}", message),
            _ => tracing::info!(target: "wasm_plugin", "{}", message),
        }
    }

    fn get_config(&mut self, key: String) -> Option<String> {
        // TODO: integrate with Hermez config system (~/.hermez/config.yaml)
        tracing::debug!("Component plugin get_config: {}", key);
        None
    }

    fn invoke_tool(&mut self, name: String, args: String) -> Result<String, String> {
        // TODO: integrate with ToolRegistry dispatch for cross-plugin tool calls
        tracing::debug!("Component plugin invoke_tool: {} {}", name, args);
        Err(format!("Tool '{}' not yet available in component runtime", name))
    }
}

/// Active instance of a component plugin.
struct ComponentInstance {
    store: wasmtime::Store<ComponentHostState>,
    bindings: HermezPlugin,
}

// ---------------------------------------------------------------------------
// WebAssembly Component Model Runtime (Phase 3 — Wassette-aligned)
// ---------------------------------------------------------------------------

/// Runtime for WebAssembly Component Model plugins.
///
/// This aligns with the Wassette architecture:
///   • Uses wasmtime::component (not Core WASM)
///   • Supports WIT interfaces for type-safe host-guest communication
///   • Loads .wasm components (not modules)
///   • Planned: OCI registry fetch, deny-by-default permissions
pub struct ComponentPluginRuntime {
    engine: wasmtime::Engine,
    component: wasmtime::component::Component,
    pub plugin_name: String,
    instance: parking_lot::Mutex<Option<ComponentInstance>>,
}

impl ComponentPluginRuntime {
    /// Compile a WebAssembly Component from file.
    pub fn from_file(path: &Path, plugin_name: &str) -> anyhow::Result<Self> {
        let engine = wasmtime::Engine::default();
        let bytes = std::fs::read(path)
            .map_err(|e| anyhow::anyhow!("Failed to read component {}: {}", path.display(), e))?;
        let component = wasmtime::component::Component::new(&engine, &bytes)
            .map_err(|e| anyhow::anyhow!("Component compile error for {}: {}", plugin_name, e))?;
        Ok(Self {
            engine,
            component,
            plugin_name: plugin_name.to_string(),
            instance: parking_lot::Mutex::new(None),
        })
    }

    /// Instantiate the component and call its `register` hook.
    ///
    /// Uses the WIT-generated bindings to set up host imports (log,
    /// get_config, invoke_tool) and instantiate the guest exports.
    pub fn instantiate(&self) -> anyhow::Result<()> {
        let mut store = wasmtime::Store::new(&self.engine, ComponentHostState::new());
        let mut linker = wasmtime::component::Linker::new(&self.engine);

        // Register WASI preview2 imports (required by cargo-component-built guests)
        wasmtime_wasi::add_to_linker_sync(&mut linker)
            .map_err(|e| anyhow::anyhow!("Failed to add WASI to linker: {}", e))?;

        // Register custom host imports (log, get_config, invoke_tool)
        hermez::plugin::host::add_to_linker(&mut linker, |state| state)
            .map_err(|e| anyhow::anyhow!("Failed to add host imports to linker: {}", e))?;

        let bindings = HermezPlugin::instantiate(&mut store, &self.component, &linker)
            .map_err(|e| anyhow::anyhow!("Component instantiate error: {}", e))?;

        // Call the guest register hook
        bindings.interface0.call_register(&mut store)
            .map_err(|e| anyhow::anyhow!("Component register error: {}", e))?;

        *self.instance.lock() = Some(ComponentInstance { store, bindings });

        tracing::info!("Component plugin '{}' ready", self.plugin_name);
        Ok(())
    }

    /// Call a lifecycle hook on the instantiated component.
    pub fn call_hook(&self, hook_name: &str, context: &HashMap<String, serde_json::Value>) -> anyhow::Result<()> {
        let mut guard = self.instance.lock();
        let instance = guard.as_mut()
            .ok_or_else(|| anyhow::anyhow!("Component plugin '{}' not instantiated", self.plugin_name))?;

        let ctx_json = serde_json::to_string(context)
            .map_err(|e| anyhow::anyhow!("Failed to serialize context: {}", e))?;

        match hook_name {
            "on_session_start" => {
                instance.bindings.interface0.call_on_session_start(&mut instance.store, &ctx_json)
                    .map_err(|e| anyhow::anyhow!("on_session_start error: {}", e))?;
            }
            "on_session_end" => {
                instance.bindings.interface0.call_on_session_end(&mut instance.store, &ctx_json)
                    .map_err(|e| anyhow::anyhow!("on_session_end error: {}", e))?;
            }
            _ => {
                tracing::debug!("Component plugin '{}': unhandled hook '{}'", self.plugin_name, hook_name);
            }
        }

        Ok(())
    }

    /// Invoke a tool exported by the component.
    pub fn invoke_tool(&self, tool_name: &str, args: &str) -> anyhow::Result<String> {
        let mut guard = self.instance.lock();
        let instance = guard.as_mut()
            .ok_or_else(|| anyhow::anyhow!("Component plugin '{}' not instantiated", self.plugin_name))?;

        instance.bindings.interface0.call_handle_tool(&mut instance.store, tool_name, args)
            .map_err(|e| anyhow::anyhow!("handle_tool error: {}", e))?
            .map_err(|e| anyhow::anyhow!("Tool error: {}", e))
    }
}

// ---------------------------------------------------------------------------
// Global singleton (lazy)
// ---------------------------------------------------------------------------

use once_cell::sync::Lazy;

static GLOBAL_HOOK_REGISTRY: Lazy<Arc<HookRegistry>> = Lazy::new(|| {
    Arc::new(HookRegistry::new())
});

/// Get the global hook registry.
pub fn global_hooks() -> Arc<HookRegistry> {
    GLOBAL_HOOK_REGISTRY.clone()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manifest_parse() {
        let yaml = r#"
name: test-plugin
version: 1.0.0
description: A test plugin
author: Test Author
provides_hooks:
  - on_session_start
  - on_session_end
provides_tools:
  - my_tool
manifest_version: 1
"#;
        let manifest = PluginManifest::from_yaml(yaml).unwrap();
        assert_eq!(manifest.name, "test-plugin");
        assert_eq!(manifest.version, "1.0.0");
        assert_eq!(manifest.provides_hooks.len(), 2);
        assert_eq!(manifest.provides_tools.len(), 1);
    }

    #[test]
    fn test_manifest_validate_ok() {
        let manifest = PluginManifest {
            name: "good-plugin".into(),
            version: "1.0.0".into(),
            ..Default::default()
        };
        assert!(manifest.validate().is_ok());
    }

    #[test]
    fn test_manifest_validate_empty_name() {
        let manifest = PluginManifest {
            name: "".into(),
            ..Default::default()
        };
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn test_manifest_validate_bad_name() {
        let manifest = PluginManifest {
            name: "bad/name".into(),
            ..Default::default()
        };
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn test_manifest_validate_version() {
        let manifest = PluginManifest {
            name: "test".into(),
            manifest_version: 999,
            ..Default::default()
        };
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn test_hook_registry_register_invoke() {
        let reg = HookRegistry::new();
        let called = Arc::new(Mutex::new(false));
        let called_clone = called.clone();
        reg.register("on_session_start", Arc::new(move |_name, _ctx| {
            *called_clone.lock().unwrap() = true;
        }));

        reg.invoke("on_session_start", &HashMap::new());
        assert!(*called.lock().unwrap());
    }

    #[test]
    fn test_hook_registry_no_hooks() {
        let reg = HookRegistry::new();
        assert!(!reg.has_hooks("on_session_start"));
        // Should not panic
        reg.invoke("on_session_start", &HashMap::new());
    }

    #[test]
    fn test_hook_registry_multiple_callbacks() {
        let reg = HookRegistry::new();
        let count = Arc::new(Mutex::new(0));
        for _ in 0..3 {
            let c = count.clone();
            reg.register("pre_tool_call", Arc::new(move |_name, _ctx| {
                *c.lock().unwrap() += 1;
            }));
        }
        reg.invoke("pre_tool_call", &HashMap::new());
        assert_eq!(*count.lock().unwrap(), 3);
    }

    #[test]
    fn test_plugin_context_register() {
        let hooks = Arc::new(HookRegistry::new());
        let manifest = PluginManifest {
            name: "ctx-test".into(),
            ..Default::default()
        };
        let ctx = PluginContext::new(manifest, hooks.clone());
        let called = Arc::new(Mutex::new(false));
        let called_clone = called.clone();
        ctx.register_hook("on_session_end", Arc::new(move |_n, _c| {
            *called_clone.lock().unwrap() = true;
        }));
        hooks.invoke("on_session_end", &HashMap::new());
        assert!(*called.lock().unwrap());
    }

    #[test]
    fn test_valid_hooks_list() {
        assert!(VALID_HOOKS.contains(&"pre_tool_call"));
        assert!(VALID_HOOKS.contains(&"on_session_start"));
        assert!(!VALID_HOOKS.contains(&"nonexistent"));
    }

    #[test]
    fn test_wasm_runtime_load_example_plugin() {
        let wasm_path = std::path::Path::new("../../plugins/example-wasm-plugin/plugin.wasm");
        if !wasm_path.exists() {
            // Example plugin not built — skip this test
            return;
        }

        let rt = WasmPluginRuntime::from_file(wasm_path, "example-wasm-plugin").unwrap();
        // _plugin_register should succeed
        rt.register().unwrap();

        // on_session_start should succeed with empty context
        let mut ctx = HashMap::new();
        ctx.insert("turn_number".into(), serde_json::json!(1));
        rt.call_hook("on_session_start", &ctx).unwrap();

        // unknown_hook should silently skip (no panic)
        rt.call_hook("nonexistent_hook", &ctx).unwrap();
    }

    #[test]
    fn test_component_runtime_load_example_plugin() {
        let component_path = std::path::Path::new("../../plugins/example-component-plugin/plugin.wasm");
        if !component_path.exists() {
            // Example component plugin not built — skip this test
            return;
        }

        let rt = ComponentPluginRuntime::from_file(component_path, "example-component-plugin").unwrap();
        // instantiate should succeed (calls register hook internally)
        rt.instantiate().unwrap();

        // on_session_start should succeed with a context
        let mut ctx = HashMap::new();
        ctx.insert("turn_number".into(), serde_json::json!(1));
        rt.call_hook("on_session_start", &ctx).unwrap();

        // on_session_end should succeed
        rt.call_hook("on_session_end", &ctx).unwrap();

        // handle_tool should return a greeting
        let result = rt.invoke_tool("greet", r#"{"name":"World"}"#).unwrap();
        assert!(result.contains("Hello from Component Model!"));

        // unknown_hook should silently skip (no panic)
        rt.call_hook("nonexistent_hook", &ctx).unwrap();
    }

    #[test]
    fn test_wasm_manifest_with_entry() {
        let yaml = r#"
name: wasm-test-plugin
version: 1.0.0
wasm_entry: plugin.wasm
provides_hooks:
  - on_session_start
manifest_version: 1
"#;
        let manifest = PluginManifest::from_yaml(yaml).unwrap();
        assert_eq!(manifest.wasm_entry, Some("plugin.wasm".into()));
    }
}
