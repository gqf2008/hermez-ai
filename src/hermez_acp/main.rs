//! Hermez ACP Adapter — Agent Client Protocol server.
//!
//! Replaces the Python `hermez-acp` command (acp_adapter.entry:main).
//! Runs a JSON-RPC server on stdin/stdout for IDE integration.

use std::sync::Arc;

use axum::{extract::State, routing::post, Json, Router};
use clap::Parser;
use tracing_subscriber::EnvFilter;

mod client;
mod protocol;
mod registry;
mod server;
mod session;

#[derive(Parser)]
#[command(name = "hermez-acp", about = "Hermez ACP Server", version)]
struct Cli {
    /// Enable verbose logging to stderr
    #[arg(short, long)]
    verbose: bool,
    /// Run as a registry server for multi-agent discovery
    #[arg(long)]
    registry: bool,
    /// Registry listen address (e.g. 127.0.0.1:8080)
    #[arg(long, default_value = "127.0.0.1:8080")]
    registry_addr: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Load .env for API keys
    load_dotenv();

    // Logging goes to stderr — stdout is reserved for ACP JSON-RPC transport
    let log_level = if cli.verbose {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive(format!("hermez_acp={log_level}").parse()?)
                .add_directive("httpx=warn".parse()?)
                .add_directive("openai=warn".parse()?),
        )
        .with_writer(std::io::stderr)
        .init();

    tracing::info!(
        "hermez-acp v{} starting",
        env!("CARGO_PKG_VERSION")
    );

    if cli.registry {
        run_registry(&cli.registry_addr).await
    } else {
        run_acp_server().await
    }
}

/// Run as an ACP registry server (HTTP API for agent discovery).
async fn run_registry(addr: &str) -> anyhow::Result<()> {
    tracing::info!("Starting ACP registry on http://{addr}");

    let reg = Arc::new(registry::AgentRegistry::new());
    let reg_clone = reg.clone();

    // Spawn heartbeat purge task
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            reg_clone.purge_stale().await;
        }
    });

    let app = Router::new()
        .route("/register", post(registry_register))
        .route("/heartbeat", post(registry_heartbeat))
        .route("/deregister", post(registry_deregister))
        .route("/discover", post(registry_discover))
        .route("/list", post(registry_list))
        .route("/get", post(registry_get))
        .with_state(reg);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("ACP registry listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn registry_register(
    State(reg): State<Arc<registry::AgentRegistry>>,
    Json(req): Json<registry::RegisterRequest>,
) -> Json<serde_json::Value> {
    let agent_id = reg.register(req).await;
    Json(serde_json::json!({ "agent_id": agent_id }))
}

async fn registry_heartbeat(
    State(reg): State<Arc<registry::AgentRegistry>>,
    Json(req): Json<registry::HeartbeatRequest>,
) -> Json<serde_json::Value> {
    let ok = reg.heartbeat(&req.agent_id).await;
    Json(serde_json::json!({ "ok": ok }))
}

async fn registry_deregister(
    State(reg): State<Arc<registry::AgentRegistry>>,
    Json(req): Json<registry::DeregisterRequest>,
) -> Json<serde_json::Value> {
    let ok = reg.deregister(&req.agent_id).await;
    Json(serde_json::json!({ "ok": ok }))
}

async fn registry_discover(
    State(reg): State<Arc<registry::AgentRegistry>>,
    Json(req): Json<registry::DiscoverRequest>,
) -> Json<serde_json::Value> {
    let resp = reg.discover(&req).await;
    match serde_json::to_value(resp) {
        Ok(v) => Json(v),
        Err(_) => Json(serde_json::json!({"error": "serialization failed"})),
    }
}

async fn registry_list(
    State(reg): State<Arc<registry::AgentRegistry>>,
) -> Json<serde_json::Value> {
    let agents = reg.list_all().await;
    match serde_json::to_value(agents) {
        Ok(v) => Json(v),
        Err(_) => Json(serde_json::json!({"error": "serialization failed"})),
    }
}

async fn registry_get(
    State(reg): State<Arc<registry::AgentRegistry>>,
    Json(params): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let agent_id = params.get("agent_id").and_then(|v| v.as_str()).unwrap_or("");
    match reg.get(agent_id).await {
        Some(agent) => match serde_json::to_value(agent) {
            Ok(v) => Json(v),
            Err(_) => Json(serde_json::Value::Null),
        },
        None => Json(serde_json::Value::Null),
    }
}

/// Run the ACP server (JSON-RPC over stdin/stdout).
async fn run_acp_server() -> anyhow::Result<()> {
    // Create session manager
    let session_manager = Arc::new(session::SessionManager::new());

    // Create channel for session updates → stdout
    let (update_tx, mut update_rx) =
        tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();

    // Create ACP server
    let acp_server = Arc::new(server::AcpServer::new(session_manager.clone(), update_tx.clone()));

    // Spawn a task to forward session updates to stdout
    let stdout_task = tokio::spawn(async move {
        while let Some(update) = update_rx.recv().await {
            let notification = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": update,
            });
            let _ = writeln_json(&notification);
        }
    });

    // Run the JSON-RPC event loop on stdin/stdout
    let result = run_jsonrpc(acp_server).await;

    // Drop the sender to signal the stdout task to exit
    drop(update_tx);
    let _ = stdout_task.await;

    result
}

/// Minimal .env loader — reads key=value pairs from ~/.hermez/.env or ./.env
fn load_dotenv() {
    let paths: Vec<String> = if let Ok(home) = std::env::var("HERMEZ_HOME") {
        vec![format!("{home}/.env"), ".env".to_string()]
    } else if let Some(dir) = dirs::home_dir() {
        vec![
            format!("{}/.hermez/.env", dir.display()),
            ".env".to_string(),
        ]
    } else {
        vec![".env".to_string()]
    };

    for path in &paths {
        if load_dotenv_file(path) {
            tracing::info!("Loaded .env from {path}");
            break;
        }
    }
}

#[allow(clippy::lines_filter_map_ok)]
fn load_dotenv_file(path: &str) -> bool {
    use std::io::BufRead;
    if let Ok(file) = std::fs::File::open(path) {
        let reader = std::io::BufReader::new(file);
        for line in reader.lines().filter_map(|r| r.ok()) {
            let line = line.trim().to_string();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                std::env::set_var(key.trim(), value.trim().trim_matches('"'));
            }
        }
        true
    } else {
        false
    }
}

/// Write a single JSON object to stdout followed by a newline.
fn writeln_json(value: &serde_json::Value) -> std::io::Result<()> {
    use std::io::Write;
    let json = serde_json::to_string(value)?;
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    writeln!(lock, "{json}")?;
    lock.flush()?;
    Ok(())
}

/// Run the JSON-RPC event loop: read requests from stdin, dispatch, write responses to stdout.
async fn run_jsonrpc(acp_server: Arc<server::AcpServer>) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            break; // EOF
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Parse JSON-RPC request
        let request: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("Invalid JSON-RPC request: {e}");
                let error_response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": {
                        "code": -32700,
                        "message": format!("Parse error: {e}")
                    }
                });
                let _ = writeln_json(&error_response);
                continue;
            }
        };

        // Only handle requests with an "id" (client → server)
        if let Some(obj) = request.as_object() {
            if let Some(id) = obj.get("id").cloned() {
                let method = obj
                    .get("method")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let params = obj
                    .get("params")
                    .and_then(|v| v.as_object())
                    .cloned()
                    .unwrap_or_default();

                match acp_server.dispatch(&method, &params).await {
                    Ok(result) => {
                        let response = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": result,
                        });
                        let _ = writeln_json(&response);
                    }
                    Err(e) => {
                        let error_response = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {
                                "code": -32603,
                                "message": e,
                            }
                        });
                        let _ = writeln_json(&error_response);
                    }
                }
            }
        }
    }

    tracing::info!("ACP stdin closed, shutting down");
    Ok(())
}
