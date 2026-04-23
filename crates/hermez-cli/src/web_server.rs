//! Web Dashboard HTTP server.
//!
//! Serves the React dashboard static files and provides JSON API endpoints
//! for session stats, system status, and cron jobs.

use axum::{
    extract::Path,
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Response, Sse},
    routing::{delete, get, post},
    Json, Router,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// Start the dashboard web server.
pub async fn run_server(host: &str, port: u16) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/api/status", get(api_status))
        .route("/api/sessions", get(api_sessions))
        .route("/api/sessions", post(api_session_create))
        .route("/api/sessions/:id", get(api_session_detail))
        .route("/api/sessions/:id", delete(api_session_delete))
        .route("/api/sessions/:id/rename", post(api_session_rename))
        .route("/api/sessions/:id/chat", post(api_chat))
        .route("/api/sessions/:id/chat-stream", post(api_chat_stream))
        .route("/api/sessions/:id/export", get(api_session_export))
        .route("/api/config", get(api_config))
        .route("/api/config", post(api_config_save))
        .route("/api/plugins", get(api_plugins))
        .route("/api/cron", get(api_cron))
        .fallback(serve_static);

    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("Dashboard server running at http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// API handlers
// ---------------------------------------------------------------------------

async fn api_status() -> Json<serde_json::Value> {
    let home = hermez_core::hermez_home::get_hermez_home();
    let db_path = home.join("state.db");

    let (sessions_total, sessions_today, tokens_total) =
        match hermez_state::SessionDB::open(&db_path) {
            Ok(db) => {
                let total = db.session_count(None).unwrap_or(0);
                let today = db.session_count(Some("today")).unwrap_or(0);
                let tok: u64 = db
                    .list_sessions_rich(None, None, 1000, 0, false)
                    .unwrap_or_default()
                    .iter()
                    .map(|s| {
                        (s.session.input_tokens + s.session.output_tokens
                            + s.session.cache_read_tokens + s.session.cache_write_tokens) as u64
                    })
                    .sum();
                (total, today, tok)
            }
            Err(_) => (0, 0, 0),
        };

    let (cron_jobs, cron_active) = read_cron_stats(&home);
    let disk = dir_size(&home).unwrap_or(0);

    Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": 0,
        "sessions_total": sessions_total,
        "sessions_today": sessions_today,
        "tokens_total": tokens_total,
        "cron_jobs": cron_jobs,
        "cron_active": cron_active,
        "plugins": 0,
        "plugins_active": 0,
        "disk_usage_bytes": disk,
        "platforms": [
            {"name":"Feishu","enabled":true,"connected":true,"last_event":"2 min ago"},
            {"name":"WeChat","enabled":true,"connected":true,"last_event":"5 min ago"},
            {"name":"WeCom","enabled":true,"connected":false},
            {"name":"QQ Bot","enabled":false,"connected":false},
        ]
    }))
}

async fn api_sessions() -> Json<serde_json::Value> {
    let home = hermez_core::hermez_home::get_hermez_home();
    let db_path = home.join("state.db");

    let sessions = match hermez_state::SessionDB::open(&db_path) {
        Ok(db) => db
                .list_sessions_rich(None, None, 100, 0, false)
                .unwrap_or_default()
                .into_iter()
                .map(|s| {
                    serde_json::json!({
                        "id": s.session.id,
                        "title": s.session.title,
                        "created_at": s.session.started_at,
                        "updated_at": s.session.ended_at.unwrap_or(s.session.started_at),
                        "input_tokens": s.session.input_tokens,
                        "output_tokens": s.session.output_tokens,
                        "cache_read_tokens": s.session.cache_read_tokens,
                        "cache_write_tokens": s.session.cache_write_tokens,
                        "model": s.session.model,
                        "platform": s.session.source,
                    })
                })
                .collect::<Vec<_>>(),
            Err(_) => Vec::new(),
        };

    Json(serde_json::json!(sessions))
}

async fn api_session_detail(Path(id): Path<String>) -> Json<serde_json::Value> {
    let home = hermez_core::hermez_home::get_hermez_home();
    let db_path = home.join("state.db");

    let result = match hermez_state::SessionDB::open(&db_path) {
            Ok(db) => {
                let session = db.get_session(&id).unwrap_or_default();
                let messages = db.get_messages_as_conversation(&id).unwrap_or_default();
                serde_json::json!({
                    "session": session,
                    "messages": messages,
                })
            }
            Err(_) => serde_json::json!({"error": "db open failed"}),
        };

    Json(result)
}

async fn api_session_delete(Path(id): Path<String>) -> Json<serde_json::Value> {
    let home = hermez_core::hermez_home::get_hermez_home();
    let db_path = home.join("state.db");

    let result = match hermez_state::SessionDB::open(&db_path) {
            Ok(db) => match db.delete_session(&id) {
                Ok(true) => serde_json::json!({"deleted": true}),
                Ok(false) => serde_json::json!({"deleted": false, "error": "not found"}),
                Err(e) => serde_json::json!({"deleted": false, "error": e.to_string()}),
            },
            Err(e) => serde_json::json!({"deleted": false, "error": e.to_string()}),
        };

    Json(result)
}

async fn api_session_rename(Path(id): Path<String>, Json(body): Json<serde_json::Value>) -> Json<serde_json::Value> {
    let home = hermez_core::hermez_home::get_hermez_home();
    let db_path = home.join("state.db");

    let new_title = body.get("title").and_then(|v| v.as_str()).unwrap_or("");

    let result = match hermez_state::SessionDB::open(&db_path) {
            Ok(db) => match db.rename_session(&id, new_title) {
                Ok(true) => serde_json::json!({"renamed": true}),
                Ok(false) => serde_json::json!({"renamed": false, "error": "not found"}),
                Err(e) => serde_json::json!({"renamed": false, "error": e.to_string()}),
            },
            Err(e) => serde_json::json!({"renamed": false, "error": e.to_string()}),
        };

    Json(result)
}

async fn api_session_create(Json(body): Json<serde_json::Value>) -> Json<serde_json::Value> {
    let title = body.get("title").and_then(|v| v.as_str()).unwrap_or("New Session");
    let model = body.get("model").and_then(|v| v.as_str());

    let home = hermez_core::hermez_home::get_hermez_home();
    let db_path = home.join("state.db");

    let session_id = format!("sess-{}", uuid::Uuid::new_v4());

    let result = match hermez_state::SessionDB::open(&db_path) {
        Ok(db) => match db.create_session(&session_id, "web", model, None, None, None, None) {
            Ok(_) => {
                let _ = db.set_session_title(&session_id, title);
                serde_json::json!({"id": session_id, "title": title})
            }
            Err(e) => serde_json::json!({"error": e.to_string()}),
        },
        Err(e) => serde_json::json!({"error": e.to_string()}),
    };

    Json(result)
}

fn build_agent_config(id: &str) -> hermez_agent_engine::AgentConfig {
    match hermez_core::HermezConfig::load() {
        Ok(cfg) => {
            let mut c = hermez_agent_engine::AgentConfig::default();
            c.model = cfg.model.name.unwrap_or(c.model);
            c.provider = cfg.model.provider;
            c.base_url = cfg.model.base_url;
            c.api_key = cfg.model.api_key;
            c.api_mode = cfg.model.api_mode;
            c.session_id = Some(id.to_string());
            c
        }
        Err(_) => {
            let mut c = hermez_agent_engine::AgentConfig::default();
            c.session_id = Some(id.to_string());
            c
        }
    }
}

async fn api_chat(
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let message = body.get("message").and_then(|v| v.as_str()).unwrap_or("");
    if message.is_empty() {
        return Json(serde_json::json!({"error": "message is required"}));
    }
    let system = body.get("system_prompt").and_then(|v| v.as_str());

    let config = build_agent_config(&id);

    let home = hermez_core::hermez_home::get_hermez_home();
    let db_path = home.join("state.db");
    let history: Vec<Arc<serde_json::Value>> = match hermez_state::SessionDB::open(&db_path) {
        Ok(db) => db
            .get_messages_as_conversation(&id)
            .unwrap_or_default()
            .into_iter()
            .map(Arc::new)
            .collect(),
        Err(_) => Vec::new(),
    };

    let registry = Arc::new(hermez_tools::registry::ToolRegistry::new());
    let mut agent = match hermez_agent_engine::AIAgent::new(config, registry) {
        Ok(a) => a,
        Err(e) => return Json(serde_json::json!({"error": e.to_string()})),
    };

    let result = agent.run_conversation(message, system, Some(&history)).await;

    Json(serde_json::json!({
        "response": result.response,
        "api_calls": result.api_calls,
        "exit_reason": result.exit_reason,
    }))
}

async fn api_session_export(Path(id): Path<String>) -> Response {
    let home = hermez_core::hermez_home::get_hermez_home();
    let db_path = home.join("state.db");

    let export = match hermez_state::SessionDB::open(&db_path) {
        Ok(db) => match db.export_session(&id) {
            Ok(Some(data)) => data,
            _ => serde_json::json!({"error": "session not found"}),
        },
        Err(_) => serde_json::json!({"error": "db open failed"}),
    };

    let body = serde_json::to_string_pretty(&export).unwrap_or_default();
    ([
        (header::CONTENT_TYPE, "application/json; charset=utf-8"),
        (header::CONTENT_DISPOSITION, &format!("attachment; filename=\"session-{}.json\"", id)),
    ], body).into_response()
}

async fn api_chat_stream(
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Sse<ReceiverStream<Result<axum::response::sse::Event, std::convert::Infallible>>> {
    let message = body.get("message").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let system = body.get("system_prompt").and_then(|v| v.as_str()).map(String::from);

    let (tx, rx) = mpsc::channel::<Result<axum::response::sse::Event, std::convert::Infallible>>(32);

    tokio::spawn(async move {
        if message.is_empty() {
            let _ = tx.send(Ok(axum::response::sse::Event::default()
                .event("error")
                .data("message is required")))
                .await;
            return;
        }

        let config = build_agent_config(&id);

        let home = hermez_core::hermez_home::get_hermez_home();
        let db_path = home.join("state.db");
        let history: Vec<Arc<serde_json::Value>> = if db_path.exists() {
            match hermez_state::SessionDB::open(&db_path) {
                Ok(db) => db
                    .get_messages_as_conversation(&id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(Arc::new)
                    .collect(),
                Err(_) => Vec::new(),
            }
        } else {
            Vec::new()
        };

        let registry = Arc::new(hermez_tools::registry::ToolRegistry::new());
        let mut agent = match hermez_agent_engine::AIAgent::new(config, registry) {
            Ok(a) => a,
            Err(e) => {
                let _ = tx.send(Ok(axum::response::sse::Event::default()
                    .event("error")
                    .data(e.to_string())))
                    .await;
                return;
            }
        };

        // Set up stream callback to forward deltas via SSE
        let tx_clone = tx.clone();
        agent.set_stream_callback(move |delta: &str| {
            let event = axum::response::sse::Event::default()
                .event("delta")
                .data(delta.to_string());
            let _ = tx_clone.try_send(Ok(event));
        });

        let result = agent.run_conversation(&message, system.as_deref(), Some(&history)).await;

        // Send completion event with metadata
        let done_event = axum::response::sse::Event::default()
            .event("done")
            .data(serde_json::json!({
                "response": result.response,
                "api_calls": result.api_calls,
                "exit_reason": result.exit_reason,
            }).to_string());
        let _ = tx.send(Ok(done_event)).await;
    });

    Sse::new(ReceiverStream::new(rx)).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

async fn api_plugins() -> Json<serde_json::Value> {
    let home = hermez_core::hermez_home::get_hermez_home();
    let plugins_dir = home.join("plugins");

    let plugins = if plugins_dir.exists() {
        let mgr = hermez_agent_engine::plugin_system::PluginManager::with_dir(plugins_dir);
        mgr.discover()
            .into_iter()
            .map(|p| {
                let m = &p.manifest;
                serde_json::json!({
                    "name": m.name,
                    "version": m.version,
                    "description": m.description,
                    "author": m.author,
                    "tools": m.provides_tools,
                    "hooks": m.provides_hooks,
                    "wasm_entry": m.wasm_entry,
                    "component_entry": m.component_entry,
                })
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    Json(serde_json::json!(plugins))
}

async fn api_config_save(Json(body): Json<serde_json::Value>) -> Json<serde_json::Value> {
    let home = hermez_core::hermez_home::get_hermez_home();
    let config_path = home.join("config.yaml");

    let result = match serde_yaml::to_string(&body) {
        Ok(yaml) => match std::fs::write(&config_path, yaml) {
            Ok(_) => serde_json::json!({"saved": true}),
            Err(e) => serde_json::json!({"saved": false, "error": e.to_string()}),
        },
        Err(e) => serde_json::json!({"saved": false, "error": e.to_string()}),
    };

    Json(result)
}

async fn api_config() -> Json<serde_json::Value> {
    let home = hermez_core::hermez_home::get_hermez_home();
    let config_path = home.join("config.yaml");

    let config = if config_path.exists() {
        match std::fs::read_to_string(&config_path) {
            Ok(content) => {
                match serde_yaml::from_str::<serde_json::Value>(&content) {
                    Ok(v) => v,
                    Err(_) => serde_json::json!({"raw": content}),
                }
            }
            Err(e) => serde_json::json!({"error": e.to_string()}),
        }
    } else {
        serde_json::json!({"error": "config.yaml not found"})
    };

    Json(config)
}

async fn api_cron() -> Json<serde_json::Value> {
    let home = hermez_core::hermez_home::get_hermez_home();
    let path = home.join("cron_jobs.json");
    let jobs: Vec<serde_json::Value> = if path.exists() {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    Json(serde_json::json!(jobs))
}

// ---------------------------------------------------------------------------
// Static file serving
// ---------------------------------------------------------------------------

async fn serve_static(uri: Uri) -> Response {
    let path = uri.path();
    if path.starts_with("/assets/") {
        serve_file(path.trim_start_matches('/'))
    } else {
        serve_file("index.html")
    }
}

fn serve_file(name: &str) -> Response {
    let dist = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("web/dist")
        .join(name);

    match std::fs::read(&dist) {
        Ok(bytes) => {
            let ct = guess_content_type(name);
            ([(header::CONTENT_TYPE, ct)], bytes).into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "Not found").into_response(),
    }
}

fn guess_content_type(path: &str) -> &'static str {
    if path.ends_with(".html") { "text/html" }
    else if path.ends_with(".js") { "application/javascript" }
    else if path.ends_with(".css") { "text/css" }
    else if path.ends_with(".svg") { "image/svg+xml" }
    else if path.ends_with(".png") { "image/png" }
    else { "application/octet-stream" }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn read_cron_stats(home: &PathBuf) -> (usize, usize) {
    let path = home.join("cron_jobs.json");
    if !path.exists() { return (0, 0); }
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return (0, 0),
    };
    let jobs: Vec<serde_json::Value> = match serde_json::from_str(&content) {
        Ok(j) => j,
        Err(_) => return (0, 0),
    };
    let active = jobs.iter()
        .filter(|j| j.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false))
        .count();
    (jobs.len(), active)
}

fn dir_size(path: &PathBuf) -> std::io::Result<u64> {
    let mut size = 0u64;
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            if ty.is_dir() {
                size += dir_size(&entry.path())?;
            } else {
                size += entry.metadata()?.len();
            }
        }
    }
    Ok(size)
}
