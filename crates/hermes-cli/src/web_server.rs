//! Web Dashboard HTTP server.
//!
//! Serves the React dashboard static files and provides JSON API endpoints
//! for session stats, system status, and cron jobs.

use axum::{
    extract::Path,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use std::path::PathBuf;

/// Start the dashboard web server.
pub async fn run_server(host: &str, port: u16) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/api/status", get(api_status))
        .route("/api/sessions", get(api_sessions))
        .route("/api/cron", get(api_cron))
        .route("/assets/{*file}", get(serve_asset))
        .fallback(serve_index);

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
    let home = hermes_core::hermes_home::get_hermes_home();
    let db_path = home.join("sessions.db");

    let (sessions_total, sessions_today, tokens_total) =
        if db_path.exists() {
            match hermes_state::SessionDB::open(&db_path) {
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
            }
        } else {
            (0, 0, 0)
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
    let home = hermes_core::hermes_home::get_hermes_home();
    let db_path = home.join("sessions.db");

    let sessions = if db_path.exists() {
        match hermes_state::SessionDB::open(&db_path) {
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
        }
    } else {
        Vec::new()
    };

    Json(serde_json::json!(sessions))
}

async fn api_cron() -> Json<serde_json::Value> {
    let home = hermes_core::hermes_home::get_hermes_home();
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

async fn serve_index() -> Response {
    serve_file("index.html")
}

async fn serve_asset(Path(file): Path<String>) -> Response {
    serve_file(&format!("assets/{file}"))
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
