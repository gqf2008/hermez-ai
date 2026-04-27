//! Session database — SQLite-backed session storage with FTS5 search.
//!
//! Mirrors the Python `hermez_state.py` SessionDB class.

use std::path::Path;
use std::sync::Arc;

use once_cell::sync::Lazy;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params, types::Value};
use rand::Rng;
use regex::Regex;

use crate::models::{Session, Message, SessionWithPreview, sanitize_title};
use crate::schema::{SCHEMA_VERSION, BASE_SCHEMA_SQL, FTS_SQL};

/// Convert a rusqlite Row into a Session struct.
fn row_to_session(r: &rusqlite::Row) -> Result<Session, rusqlite::Error> {
    Ok(Session {
        id: r.get("id").unwrap_or_default(),
        source: r.get("source").unwrap_or_default(),
        user_id: r.get("user_id").ok(),
        model: r.get("model").ok(),
        model_config: r.get("model_config").ok(),
        system_prompt: r.get("system_prompt").ok(),
        parent_session_id: r.get("parent_session_id").ok(),
        started_at: r.get("started_at").unwrap_or(0.0),
        ended_at: r.get("ended_at").ok(),
        end_reason: r.get("end_reason").ok(),
        message_count: r.get("message_count").unwrap_or(0),
        tool_call_count: r.get("tool_call_count").unwrap_or(0),
        input_tokens: r.get("input_tokens").unwrap_or(0),
        output_tokens: r.get("output_tokens").unwrap_or(0),
        cache_read_tokens: r.get("cache_read_tokens").unwrap_or(0),
        cache_write_tokens: r.get("cache_write_tokens").unwrap_or(0),
        reasoning_tokens: r.get("reasoning_tokens").unwrap_or(0),
        billing_provider: r.get("billing_provider").ok(),
        billing_base_url: r.get("billing_base_url").ok(),
        billing_mode: r.get("billing_mode").ok(),
        estimated_cost_usd: r.get("estimated_cost_usd").ok(),
        actual_cost_usd: r.get("actual_cost_usd").ok(),
        cost_status: r.get("cost_status").ok(),
        cost_source: r.get("cost_source").ok(),
        pricing_version: r.get("pricing_version").ok(),
        title: r.get("title").ok(),
    })
}

const WRITE_MAX_RETRIES: usize = 15;
const WRITE_RETRY_MIN_MS: u64 = 20;
const WRITE_RETRY_MAX_MS: u64 = 150;
const CHECKPOINT_EVERY_N_WRITES: usize = 50;

pub struct SessionDB {
    conn: Arc<Mutex<Connection>>,
    write_count: Arc<Mutex<usize>>,
}

impl SessionDB {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, StateError> {
        let db_path = path.as_ref().to_path_buf();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&db_path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        let conn = Arc::new(Mutex::new(conn));
        let write_count = Arc::new(Mutex::new(0usize));
        {
            let mut guard = conn.lock();
            init_schema(&mut guard)?;
        }
        // Auto-prune old sessions and VACUUM on startup.
        // Mirrors Python auto-prune + VACUUM at startup (session.py, commit b8663813).
        let db = Self { conn: Arc::clone(&conn), write_count: Arc::clone(&write_count) };
        if let Ok(pruned) = db.prune_sessions(30, None) {
            if pruned > 0 {
                tracing::info!("Auto-pruned {} stale session(s) on startup", pruned);
            }
        }
        {
            let guard = db.conn.lock();
            let _ = guard.execute_batch("PRAGMA optimize;");
        }

        Ok(db)
    }

    pub fn open_default() -> Result<Self, StateError> {
        Self::open(hermez_core::get_hermez_home().join("state.db"))
    }

    fn with_write<T, F: FnMut(&mut Connection) -> Result<T, StateError>>(&self, mut f: F) -> Result<T, StateError> {
        let mut last_err: Option<StateError> = None;
        for attempt in 0..WRITE_MAX_RETRIES {
            let mut guard = self.conn.lock();
            guard.execute_batch("BEGIN IMMEDIATE")?;
            let result = f(&mut guard);
            match result {
                Ok(val) => {
                    guard.execute_batch("COMMIT")?;
                    drop(guard);
                    let mut wc = self.write_count.lock();
                    *wc += 1;
                    if *wc % CHECKPOINT_EVERY_N_WRITES == 0 {
                        drop(wc);
                        let _ = self.try_wal_checkpoint();
                    }
                    return Ok(val);
                }
                Err(e) => {
                    let _ = guard.execute_batch("ROLLBACK");
                    drop(guard);
                    if let StateError::Sqlite(ref se) = e {
                        match se.sqlite_error_code() {
                            Some(rusqlite::ErrorCode::DatabaseBusy)
                            | Some(rusqlite::ErrorCode::DatabaseLocked) => {
                                if attempt < WRITE_MAX_RETRIES - 1 {
                                    last_err = Some(e);
                                    let jitter = rand::rng().random_range(WRITE_RETRY_MIN_MS..=WRITE_RETRY_MAX_MS);
                                    std::thread::sleep(std::time::Duration::from_millis(jitter));
                                    continue;
                                }
                            }
                            Some(rusqlite::ErrorCode::DiskFull) => {
                                return Err(StateError::Io(std::io::Error::other(
                                    "Disk full: SQLite write failed",
                                )));
                            }
                            Some(rusqlite::ErrorCode::ReadOnly) => {
                                return Err(StateError::Io(std::io::Error::other(
                                    "Database is read-only: check permissions or mount options",
                                )));
                            }
                            Some(rusqlite::ErrorCode::DatabaseCorrupt) => {
                                return Err(StateError::Io(std::io::Error::other(
                                    "Database file is corrupt: try restoring from backup",
                                )));
                            }
                            _ => {}
                        }
                    }
                    return Err(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            StateError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                Some("database is locked after max retries".to_string()),
            ))
        }))
    }

    fn try_wal_checkpoint(&self) -> Result<(), StateError> {
        let guard = self.conn.lock();
        let _ = guard.execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
        Ok(())
    }

    pub fn close(&self) {
        let _ = self.try_wal_checkpoint();
        // Try to take ownership and close the connection properly.
        // If other references exist (e.g., a concurrent with_write call),
        // the WAL checkpoint above is the best we can do.
        if let Ok(conn) = Arc::try_unwrap(self.conn.clone()) {
            drop(conn); // Connection is closed on Drop
        }
    }

    // ── Session lifecycle ──

    #[allow(clippy::too_many_arguments)]
    pub fn create_session(&self, session_id: &str, source: &str, model: Option<&str>,
        model_config: Option<&str>, system_prompt: Option<&str>,
        user_id: Option<&str>, parent_session_id: Option<&str>,
    ) -> Result<String, StateError> {
        let sid = session_id.to_string();
        let src = source.to_string();
        let mdl = model.map(String::from);
        let mc = model_config.map(String::from);
        let sp = system_prompt.map(String::from);
        let uid = user_id.map(String::from);
        let pid = parent_session_id.map(String::from);
        let started = now_epoch();
        self.with_write(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO sessions (id,source,user_id,model,model_config,\
                 system_prompt,parent_session_id,started_at) VALUES (?,?,?,?,?,?,?,?)",
                params![&sid, &src, &uid, &mdl, &mc, &sp, &pid, started],
            )?;
            Ok(sid.clone())
        })
    }

    pub fn end_session(&self, session_id: &str, end_reason: &str) -> Result<(), StateError> {
        let sid = session_id.to_string();
        let reason = end_reason.to_string();
        let ended = now_epoch();
        self.with_write(move |conn| {
            conn.execute("UPDATE sessions SET ended_at=?,end_reason=? WHERE id=?", params![ended, reason, sid])?;
            Ok(())
        })
    }

    pub fn reopen_session(&self, session_id: &str) -> Result<(), StateError> {
        let sid = session_id.to_string();
        self.with_write(move |conn| {
            conn.execute("UPDATE sessions SET ended_at=NULL,end_reason=NULL WHERE id=?", params![sid])?;
            Ok(())
        })
    }

    pub fn update_system_prompt(&self, session_id: &str, system_prompt: &str) -> Result<(), StateError> {
        let sid = session_id.to_string();
        let prompt = system_prompt.to_string();
        self.with_write(move |conn| {
            conn.execute("UPDATE sessions SET system_prompt=? WHERE id=?", params![prompt, sid])?;
            Ok(())
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_token_counts(&self, session_id: &str, input_tokens: i64, output_tokens: i64,
        model: Option<&str>, cache_read_tokens: i64, cache_write_tokens: i64,
        reasoning_tokens: i64, estimated_cost_usd: Option<f64>, actual_cost_usd: Option<f64>,
        cost_status: Option<&str>, cost_source: Option<&str>, pricing_version: Option<&str>,
        billing_provider: Option<&str>, billing_base_url: Option<&str>,
        billing_mode: Option<&str>, absolute: bool,
    ) -> Result<(), StateError> {
        let sid = session_id.to_string();
        let mdl = model.map(String::from);
        let cs = cost_status.map(String::from);
        let cso = cost_source.map(String::from);
        let pv = pricing_version.map(String::from);
        let bp = billing_provider.map(String::from);
        let bu = billing_base_url.map(String::from);
        let bm = billing_mode.map(String::from);
        let sql = if absolute {
            "UPDATE sessions SET input_tokens=?1,output_tokens=?2,cache_read_tokens=?3,\
             cache_write_tokens=?4,reasoning_tokens=?5,\
             estimated_cost_usd=COALESCE(?6,0),\
             actual_cost_usd=CASE WHEN ?7 IS NULL THEN actual_cost_usd ELSE ?7 END,\
             cost_status=COALESCE(?8,cost_status),cost_source=COALESCE(?9,cost_source),\
             pricing_version=COALESCE(?10,pricing_version),\
             billing_provider=COALESCE(billing_provider,?11),\
             billing_base_url=COALESCE(billing_base_url,?12),\
             billing_mode=COALESCE(billing_mode,?13),model=COALESCE(model,?14) WHERE id=?15"
        } else {
            "UPDATE sessions SET input_tokens=input_tokens+?1,output_tokens=output_tokens+?2,\
             cache_read_tokens=cache_read_tokens+?3,cache_write_tokens=cache_write_tokens+?4,\
             reasoning_tokens=reasoning_tokens+?5,\
             estimated_cost_usd=COALESCE(estimated_cost_usd,0)+COALESCE(?6,0),\
             actual_cost_usd=CASE WHEN ?7 IS NULL THEN actual_cost_usd \
             ELSE COALESCE(actual_cost_usd,0)+?7 END,\
             cost_status=COALESCE(?8,cost_status),cost_source=COALESCE(?9,cost_source),\
             pricing_version=COALESCE(?10,pricing_version),\
             billing_provider=COALESCE(billing_provider,?11),\
             billing_base_url=COALESCE(billing_base_url,?12),\
             billing_mode=COALESCE(billing_mode,?13),model=COALESCE(model,?14) WHERE id=?15"
        };
        self.with_write(move |conn| {
            conn.execute(sql, params![
                input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                reasoning_tokens,
                estimated_cost_usd, actual_cost_usd,
                cs, cso, pv, bp, bu, bm, mdl, sid
            ])?;
            Ok(())
        })
    }

    pub fn ensure_session(&self, session_id: &str, source: &str, model: Option<&str>) -> Result<(), StateError> {
        let sid = session_id.to_string();
        let src = source.to_string();
        let mdl = model.map(String::from);
        let started = now_epoch();
        self.with_write(move |conn| {
            conn.execute("INSERT OR IGNORE INTO sessions (id,source,model,started_at) VALUES (?,?,?,?)",
                params![sid, src, mdl, started])?;
            Ok(())
        })
    }

    pub fn get_session(&self, session_id: &str) -> Result<Option<Session>, StateError> {
        let guard = self.conn.lock();
        let mut stmt = guard.prepare_cached("SELECT * FROM sessions WHERE id=?1")?;
        let result = stmt.query_row(params![session_id], row_to_session).optional()?;
        Ok(result)
    }

    pub fn resolve_session_id(&self, prefix: &str) -> Result<Option<String>, StateError> {
        if let Some(session) = self.get_session(prefix)? {
            return Ok(Some(session.id));
        }
        let guard = self.conn.lock();
        let pattern = format!("{}%", escape_like(prefix));
        let mut stmt = guard.prepare_cached(
            "SELECT id FROM sessions WHERE id LIKE ?1 ESCAPE '\\' ORDER BY started_at DESC LIMIT 2"
        )?;
        let mut ids = Vec::new();
        let mut rows = stmt.query(params![pattern])?;
        while let Some(row) = rows.next()? {
            ids.push(row.get::<_, String>(0)?);
        }
        if ids.len() == 1 {
            Ok(Some(ids.remove(0)))
        } else {
            Ok(None)
        }
    }

    pub fn set_session_title(&self, session_id: &str, title: &str) -> Result<bool, StateError> {
        let title = sanitize_title(title);
        let sid = session_id.to_string();
        self.with_write(move |conn| {
            if let Some(ref t) = title {
                let exists: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sessions WHERE title=?1 AND id!=?2)",
                    params![t, sid], |r| r.get(0))?;
                if exists {
                    return Err(StateError::TitleConflict(t.clone()));
                }
            }
            let rows = conn.execute("UPDATE sessions SET title=?1 WHERE id=?2", params![title.as_deref(), sid])?;
            Ok(rows > 0)
        })
    }

    pub fn get_session_title(&self, session_id: &str) -> Result<Option<String>, StateError> {
        let guard = self.conn.lock();
        let mut stmt = guard.prepare_cached("SELECT title FROM sessions WHERE id=?1")?;
        let title: Option<String> = stmt.query_row(params![session_id], |r| r.get(0)).optional()?;
        Ok(title)
    }

    pub fn get_session_by_title(&self, title: &str) -> Result<Option<Session>, StateError> {
        let guard = self.conn.lock();
        let mut stmt = guard.prepare_cached("SELECT * FROM sessions WHERE title=?1")?;
        let result = stmt.query_row(params![title], row_to_session).optional()?;
        Ok(result)
    }

    pub fn resolve_session_by_title(&self, title: &str) -> Result<Option<String>, StateError> {
        // Numbered variants take priority over exact match
        let found = {
            let guard = self.conn.lock();
            let pattern = format!("{} #%", escape_like(title));
            let mut stmt = guard.prepare_cached(
                "SELECT id FROM sessions WHERE title LIKE ?1 ESCAPE '\\' ORDER BY started_at DESC"
            )?;
            let mut rows = stmt.query(params![pattern])?;
            if let Some(row) = rows.next()? {
                Some(row.get::<_, String>(0)?)
            } else {
                None
            }
        };
        if let Some(id) = found {
            return Ok(Some(id));
        }

        // Fall back to exact match
        if let Some(session) = self.get_session_by_title(title)? {
            return Ok(Some(session.id));
        }
        Ok(None)
    }

    pub fn get_next_title_in_lineage(&self, base_title: &str) -> Result<String, StateError> {
        let base = strip_title_suffix(base_title).unwrap_or(base_title).to_string();
        let existing: Vec<String> = {
            let guard = self.conn.lock();
            let pattern = format!("{} #%", escape_like(&base));
            let mut stmt = guard.prepare_cached(
                "SELECT title FROM sessions WHERE title=?1 OR title LIKE ?2 ESCAPE '\\'"
            )?;
            let mut existing = Vec::new();
            let mut rows = stmt.query(params![&base, pattern])?;
            while let Some(row) = rows.next()? {
                existing.push(row.get::<_, String>(0)?);
            }
            existing
        };
        if existing.is_empty() {
            return Ok(base);
        }
        let mut max_num: usize = 0;
        for t in &existing {
            if let Some(n) = extract_title_number(t) {
                max_num = max_num.max(n);
            }
        }
        // If there are no numbered variants but there are sessions with the base title,
        // the next one is #2 (the unnumbered original counts as #1)
        if max_num == 0 {
            max_num = 1;
        }
        Ok(format!("{} #{}", base, max_num + 1))
    }

    pub fn list_sessions_rich(&self, source: Option<&str>, exclude_sources: Option<&[String]>,
        limit: usize, offset: usize, include_children: bool,
    ) -> Result<Vec<SessionWithPreview>, StateError> {
        let mut where_clauses: Vec<String> = Vec::new();
        let mut args: Vec<Value> = Vec::new();
        if !include_children {
            where_clauses.push("s.parent_session_id IS NULL".into());
        }
        if let Some(s) = source {
            where_clauses.push("s.source=?".into());
            args.push(Value::from(s.to_string()));
        }
        if let Some(excl) = exclude_sources {
            let ph: String = excl.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            where_clauses.push(format!("s.source NOT IN ({})", ph));
            for s in excl {
                args.push(Value::from(s.clone()));
            }
        }
        let w = if where_clauses.is_empty() { String::new() } else { format!("WHERE {}", where_clauses.join(" AND ")) };
        let query = format!(
            "SELECT s.*, \
             COALESCE((SELECT SUBSTR(REPLACE(REPLACE(m.content,X'0A',' '),X'0D',' '),1,63) \
              FROM messages m WHERE m.session_id=s.id AND m.role='user' AND m.content IS NOT NULL \
              ORDER BY m.timestamp,m.id LIMIT 1),'') AS _preview_raw, \
             COALESCE((SELECT MAX(m2.timestamp) FROM messages m2 WHERE m2.session_id=s.id),s.started_at) \
             AS last_active \
             FROM sessions s {} ORDER BY s.started_at DESC LIMIT ? OFFSET ?", w);
        args.push(Value::from(limit as i64));
        args.push(Value::from(offset as i64));
        let guard = self.conn.lock();
        let mut stmt = guard.prepare(&query)?;
        let mut results = Vec::new();
        let mut rows = stmt.query(rusqlite::params_from_iter(args.iter()))?;
        while let Some(row) = rows.next()? {
            let session = row_to_session(row)?;
            let raw: String = row.get("_preview_raw").unwrap_or_default();
            let last_active: f64 = row.get("last_active").unwrap_or(session.started_at);
            let preview = format_preview(&raw);
            results.push(SessionWithPreview { session, preview, last_active });
        }
        Ok(results)
    }

    // ── Message storage ──

    #[allow(clippy::too_many_arguments)]
    pub fn append_message(&self, session_id: &str, role: &str, content: Option<&str>,
        tool_name: Option<&str>, tool_calls: Option<&str>, tool_call_id: Option<&str>,
        token_count: Option<i64>, finish_reason: Option<&str>, reasoning: Option<&str>,
        reasoning_details: Option<&str>, codex_reasoning_items: Option<&str>,
    ) -> Result<i64, StateError> {
        let sid = session_id.to_string();
        let role = role.to_string();
        let content = content.map(String::from);
        let tn = tool_name.map(String::from);
        let tc = tool_calls.map(String::from);
        let tci = tool_call_id.map(String::from);
        let fr = finish_reason.map(String::from);
        let rs = reasoning.map(String::from);
        let rd = reasoning_details.map(String::from);
        let cr = codex_reasoning_items.map(String::from);
        let ts = now_epoch();
        let num_tool_calls = tc.as_ref().map(|s| s.matches("\"function\"").count()).unwrap_or(0);
        self.with_write(move |conn| {
            let msg_id = conn.execute(
                "INSERT INTO messages (session_id,role,content,tool_call_id,tool_calls,tool_name,\
                 timestamp,token_count,finish_reason,reasoning,reasoning_details,codex_reasoning_items)\
                 VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
                params![sid, role, content, tci, tc, tn, ts, token_count, fr, rs, rd, cr],
            )? as i64;
            if num_tool_calls > 0 {
                conn.execute(
                    "UPDATE sessions SET message_count=message_count+1,tool_call_count=tool_call_count+?1 WHERE id=?2",
                    params![num_tool_calls as i64, sid])?;
            } else {
                conn.execute("UPDATE sessions SET message_count=message_count+1 WHERE id=?1", params![sid])?;
            }
            Ok(msg_id)
        })
    }

    pub fn get_messages(&self, session_id: &str) -> Result<Vec<Message>, StateError> {
        let guard = self.conn.lock();
        let mut stmt = guard.prepare_cached("SELECT * FROM messages WHERE session_id=?1 ORDER BY timestamp,id")?;
        let mut msgs = Vec::new();
        let mut rows = stmt.query(params![session_id])?;
        while let Some(row) = rows.next()? {
            msgs.push(Message {
                id: row.get("id").unwrap_or(0),
                session_id: row.get("session_id").unwrap_or_default(),
                role: row.get("role").unwrap_or_default(),
                content: row.get("content").ok(),
                tool_call_id: row.get("tool_call_id").ok(),
                tool_calls: row.get("tool_calls").ok(),
                tool_name: row.get("tool_name").ok(),
                timestamp: row.get("timestamp").unwrap_or(0.0),
                token_count: row.get("token_count").ok(),
                finish_reason: row.get("finish_reason").ok(),
                reasoning: row.get("reasoning").ok(),
                reasoning_details: row.get("reasoning_details").ok(),
                codex_reasoning_items: row.get("codex_reasoning_items").ok(),
            });
        }
        Ok(msgs)
    }

    pub fn get_messages_as_conversation(&self, session_id: &str) -> Result<Vec<serde_json::Value>, StateError> {
        let guard = self.conn.lock();
        let mut stmt = guard.prepare_cached(
            "SELECT role,content,tool_call_id,tool_calls,tool_name,reasoning,reasoning_details,codex_reasoning_items \
             FROM messages WHERE session_id=?1 ORDER BY timestamp,id")?;
        let mut msgs = Vec::new();
        let mut rows = stmt.query(params![session_id])?;
        while let Some(row) = rows.next()? {
            let role: String = row.get("role")?;
            let content: Option<String> = row.get("content")?;
            let mut msg = serde_json::json!({"role": role, "content": content});
            if let Some(tci) = row.get::<_, Option<String>>("tool_call_id")? {
                msg["tool_call_id"] = serde_json::Value::String(tci);
            }
            if let Some(tn) = row.get::<_, Option<String>>("tool_name")? {
                msg["tool_name"] = serde_json::Value::String(tn);
            }
            if let Some(tcj) = row.get::<_, Option<String>>("tool_calls")? {
                if let Ok(v) = serde_json::from_str(&tcj) { msg["tool_calls"] = v; }
            }
            if role == "assistant" {
                if let Some(r) = row.get::<_, Option<String>>("reasoning")? {
                    msg["reasoning"] = serde_json::Value::String(r);
                }
                if let Some(rd) = row.get::<_, Option<String>>("reasoning_details")? {
                    if let Ok(v) = serde_json::from_str(&rd) { msg["reasoning_details"] = v; }
                }
                if let Some(ci) = row.get::<_, Option<String>>("codex_reasoning_items")? {
                    if let Ok(v) = serde_json::from_str(&ci) { msg["codex_reasoning_items"] = v; }
                }
            }
            msgs.push(msg);
        }
        Ok(msgs)
    }

    // ── Search ──

    pub fn search_messages(&self, query: &str, source_filter: Option<&[String]>,
        exclude_sources: Option<&[String]>, role_filter: Option<&[String]>,
        limit: usize, offset: usize,
    ) -> Result<Vec<serde_json::Value>, StateError> {
        let query = sanitize_fts5_query(query);
        if query.is_empty() { return Ok(Vec::new()); }
        let mut where_clauses = vec!["messages_fts MATCH ?".to_string()];
        let mut args: Vec<Value> = vec![Value::from(query)];
        if let Some(srcs) = source_filter {
            let ph: String = srcs.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            where_clauses.push(format!("s.source IN ({})", ph));
            for s in srcs { args.push(Value::from(s.clone())); }
        }
        if let Some(excl) = exclude_sources {
            let ph: String = excl.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            where_clauses.push(format!("s.source NOT IN ({})", ph));
            for s in excl { args.push(Value::from(s.clone())); }
        }
        if let Some(roles) = role_filter {
            let ph: String = roles.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            where_clauses.push(format!("m.role IN ({})", ph));
            for r in roles { args.push(Value::from(r.clone())); }
        }
        args.push(Value::from(limit as i64));
        args.push(Value::from(offset as i64));
        let sql = format!(
            "SELECT m.id,m.session_id,m.role,snippet(messages_fts,0,'>>>','<<<','...',40) AS snippet, \
             m.timestamp,m.tool_name,s.source,s.model,s.started_at AS session_started \
             FROM messages_fts JOIN messages m ON m.id=messages_fts.rowid \
             JOIN sessions s ON s.id=m.session_id WHERE {} ORDER BY rank LIMIT ? OFFSET ?",
            where_clauses.join(" AND "));
        let guard = self.conn.lock();
        let mut stmt = guard.prepare(&sql)?;
        let mut results = Vec::new();
        let mut rows = stmt.query(rusqlite::params_from_iter(args.iter()))?;
        while let Some(row) = rows.next()? {
            let mut obj = serde_json::Map::new();
            obj.insert("id".into(), serde_json::Value::Number(row.get::<_, i64>("id")?.into()));
            obj.insert("session_id".into(), serde_json::Value::String(row.get("session_id")?));
            obj.insert("role".into(), serde_json::Value::String(row.get("role")?));
            obj.insert("snippet".into(), serde_json::Value::String(row.get("snippet")?));
            obj.insert("timestamp".into(), serde_json::Value::Number((row.get::<_, f64>("timestamp")? as i64).into()));
            if let Some(tn) = row.get::<_, Option<String>>("tool_name")? {
                obj.insert("tool_name".into(), serde_json::Value::String(tn));
            }
            obj.insert("source".into(), serde_json::Value::String(row.get("source")?));
            if let Some(m) = row.get::<_, Option<String>>("model")? {
                obj.insert("model".into(), serde_json::Value::String(m));
            }
            results.push(serde_json::Value::Object(obj));
        }
        Ok(results)
    }

    pub fn search_sessions(&self, source: Option<&str>, limit: usize, offset: usize) -> Result<Vec<Session>, StateError> {
        let guard = self.conn.lock();
        let (query, p) = if let Some(s) = source {
            ("SELECT * FROM sessions WHERE source=?1 ORDER BY started_at DESC LIMIT ?2 OFFSET ?3",
             vec![Value::from(s.to_string()), Value::from(limit as i64), Value::from(offset as i64)])
        } else {
            ("SELECT * FROM sessions ORDER BY started_at DESC LIMIT ?1 OFFSET ?2",
             vec![Value::from(limit as i64), Value::from(offset as i64)])
        };
        let mut stmt = guard.prepare_cached(query)?;
        let mut sessions = Vec::new();
        let mut rows = stmt.query(rusqlite::params_from_iter(p.iter()))?;
        while let Some(row) = rows.next()? {
            sessions.push(row_to_session(row)?);
        }
        Ok(sessions)
    }

    // ── Utility ──

    pub fn session_count(&self, source: Option<&str>) -> Result<i64, StateError> {
        let guard = self.conn.lock();
        if let Some(s) = source {
            guard.query_row("SELECT COUNT(*) FROM sessions WHERE source=?1", params![s], |r| r.get(0))
        } else {
            guard.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        }.map_err(StateError::from)
    }

    pub fn message_count(&self, session_id: Option<&str>) -> Result<i64, StateError> {
        let guard = self.conn.lock();
        if let Some(sid) = session_id {
            guard.query_row("SELECT COUNT(*) FROM messages WHERE session_id=?1", params![sid], |r| r.get(0))
        } else {
            guard.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        }.map_err(StateError::from)
    }

    pub fn export_session(&self, session_id: &str) -> Result<Option<serde_json::Value>, StateError> {
        let Some(session) = self.get_session(session_id)? else { return Ok(None) };
        let messages = self.get_messages(session_id)?;
        let mut obj = serde_json::to_value(session)?.as_object().cloned().unwrap_or_default();
        obj.insert("messages".into(), serde_json::to_value(messages)?);
        Ok(Some(serde_json::Value::Object(obj)))
    }

    /// Export all sessions (with messages) as a list of dicts.
    /// Suitable for writing to a JSONL file for backup/analysis.
    pub fn export_all(&self, source: Option<&str>) -> Result<Vec<serde_json::Value>, StateError> {
        let sessions = self.search_sessions(source, 100000, 0)?;
        let mut results = Vec::new();
        for session in sessions {
            if let Ok(messages) = self.get_messages(&session.id) {
                let mut obj = serde_json::to_value(&session)?.as_object().cloned().unwrap_or_default();
                obj.insert("messages".into(), serde_json::to_value(messages)?);
                results.push(serde_json::Value::Object(obj));
            }
        }
        Ok(results)
    }

    pub fn delete_session(&self, session_id: &str) -> Result<bool, StateError> {
        let sid = session_id.to_string();
        self.with_write(move |conn| {
            let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)", params![sid], |r| r.get(0))?;
            if !exists { return Ok(false); }
            conn.execute("UPDATE sessions SET parent_session_id=NULL WHERE parent_session_id=?1", params![sid])?;
            conn.execute("DELETE FROM messages WHERE session_id=?1", params![sid])?;
            conn.execute("DELETE FROM sessions WHERE id=?1", params![sid])?;
            Ok(true)
        })
    }

    /// Rename a session's title.
    pub fn rename_session(&self, session_id: &str, new_title: &str) -> Result<bool, StateError> {
        let sid = session_id.to_string();
        let title = new_title.to_string();
        self.with_write(move |conn| {
            let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)", params![sid], |r| r.get(0))?;
            if !exists { return Ok(false); }
            conn.execute("UPDATE sessions SET title=?1 WHERE id=?2", params![title, sid])?;
            Ok(true)
        })
    }

    pub fn clear_messages(&self, session_id: &str) -> Result<(), StateError> {
        let sid = session_id.to_string();
        self.with_write(move |conn| {
            conn.execute("DELETE FROM messages WHERE session_id=?1", params![sid])?;
            conn.execute("UPDATE sessions SET message_count=0,tool_call_count=0 WHERE id=?1", params![sid])?;
            Ok(())
        })
    }

    pub fn prune_sessions(&self, older_than_days: i64, source: Option<&str>) -> Result<usize, StateError> {
        let cutoff = now_epoch() - (older_than_days as f64 * 86400.0);
        let sql = if source.is_some() {
            "DELETE FROM messages WHERE session_id IN (SELECT id FROM sessions WHERE started_at<?1 AND source=?2)"
        } else {
            "DELETE FROM messages WHERE session_id IN (SELECT id FROM sessions WHERE started_at<?1)"
        };
        self.with_write(move |conn| {
            if let Some(s) = source {
                Ok(conn.execute(sql, params![cutoff, s])?)
            } else {
                Ok(conn.execute(sql, params![cutoff])?)
            }
        })
    }

    /// Delete sessions and their messages older than a given number of days.
    /// Returns the number of sessions deleted.
    pub fn prune_old_sessions(&self, older_than_days: i64, source: Option<&str>) -> Result<usize, StateError> {
        let cutoff = now_epoch() - (older_than_days as f64 * 86400.0);
        self.with_write(move |conn| {
            // Get session IDs to delete
            let ids: Vec<String> = if let Some(s) = source {
                let mut stmt = conn.prepare("SELECT id FROM sessions WHERE started_at<?1 AND source=?2")?;
                let rows = stmt.query_map(params![cutoff, s], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
                rows
            } else {
                let mut stmt = conn.prepare("SELECT id FROM sessions WHERE started_at<?1")?;
                let rows = stmt.query_map(params![cutoff], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
                rows
            };

            let count = ids.len();
            if count == 0 {
                return Ok(0);
            }

            // Use a transaction for atomic batch delete
            let tx = conn.transaction()?;
            let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            // Delete messages for all old sessions in one query
            let sql = format!("DELETE FROM messages WHERE session_id IN ({})", placeholders);
            {
                let mut stmt = tx.prepare(&sql)?;
                let id_params: Vec<&dyn rusqlite::ToSql> = ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
                stmt.execute(rusqlite::params_from_iter(id_params))?;
            }
            // Clear parent references
            let sql2 = format!("UPDATE sessions SET parent_session_id=NULL WHERE parent_session_id IN ({})", placeholders);
            {
                let mut stmt2 = tx.prepare(&sql2)?;
                let id_params2: Vec<&dyn rusqlite::ToSql> = ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
                stmt2.execute(rusqlite::params_from_iter(id_params2))?;
            }
            // Delete sessions
            let sql3 = format!("DELETE FROM sessions WHERE id IN ({})", placeholders);
            {
                let mut stmt3 = tx.prepare(&sql3)?;
                let id_params3: Vec<&dyn rusqlite::ToSql> = ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
                stmt3.execute(rusqlite::params_from_iter(id_params3))?;
            }
            tx.commit()?;

            Ok(count)
        })
    }
}

// ── Schema initialization ──

fn init_schema(conn: &mut Connection) -> Result<(), StateError> {
    conn.execute_batch(BASE_SCHEMA_SQL)?;
    let current_version: i64 = conn.query_row("SELECT version FROM schema_version LIMIT 1", [], |r| r.get(0))
        .or_else(|_| -> Result<i64, StateError> {
            conn.execute("INSERT INTO schema_version (version) VALUES (?1)", [SCHEMA_VERSION])?;
            Ok(SCHEMA_VERSION)
        })?;
    if current_version < 2 {
        let _ = conn.execute("ALTER TABLE messages ADD COLUMN finish_reason TEXT", []);
        conn.execute("UPDATE schema_version SET version=2", [])?;
    }
    if current_version < 3 {
        let _ = conn.execute("ALTER TABLE sessions ADD COLUMN title TEXT", []);
        conn.execute("UPDATE schema_version SET version=3", [])?;
    }
    if current_version < 4 {
        let _ = conn.execute("CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_title_unique ON sessions(title) WHERE title IS NOT NULL", []);
        conn.execute("UPDATE schema_version SET version=4", [])?;
    }
    if current_version < 5 {
        for (name, ctype) in &[("cache_read_tokens","INTEGER DEFAULT 0"),("cache_write_tokens","INTEGER DEFAULT 0"),
            ("reasoning_tokens","INTEGER DEFAULT 0"),("billing_provider","TEXT"),("billing_base_url","TEXT"),
            ("billing_mode","TEXT"),("estimated_cost_usd","REAL"),("actual_cost_usd","REAL"),
            ("cost_status","TEXT"),("cost_source","TEXT"),("pricing_version","TEXT")] {
            let _ = conn.execute(&format!("ALTER TABLE sessions ADD COLUMN \"{}\" {}", name, ctype), []);
        }
        conn.execute("UPDATE schema_version SET version=5", [])?;
    }
    if current_version < 6 {
        for col in &["reasoning", "reasoning_details", "codex_reasoning_items"] {
            let _ = conn.execute(&format!("ALTER TABLE messages ADD COLUMN \"{}\" TEXT", col), []);
        }
        conn.execute("UPDATE schema_version SET version=6", [])?;
    }
    let _ = conn.execute("CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_title_unique ON sessions(title) WHERE title IS NOT NULL", []);
    let fts_exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='messages_fts'", [], |r| r.get(0)).unwrap_or(0);
    if fts_exists == 0 {
        conn.execute_batch(FTS_SQL)?;
    }
    Ok(())
}

// ── Helpers ──

/// Current time as a Unix epoch timestamp (seconds).
pub fn now_epoch() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs_f64()
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

fn format_preview(raw: &str) -> String {
    let text = raw.replace(['\n', '\r'], " ").trim().to_string();
    if text.len() > 60 { format!("{}...", &text[..60]) } else { text }
}

fn strip_title_suffix(title: &str) -> Option<&str> {
    if let Some(pos) = title.rfind(" #") {
        let after = &title[pos + 2..];
        if !after.is_empty() && after.chars().all(|c| c.is_ascii_digit()) {
            return Some(&title[..pos]);
        }
    }
    None
}

fn extract_title_number(title: &str) -> Option<usize> {
    if let Some(pos) = title.rfind(" #") {
        title[pos + 2..].parse::<usize>().ok()
    } else {
        None
    }
}

// Static regexes for FTS5 sanitization — compiled once at startup.
static RE_QUOTED: Lazy<Regex> = Lazy::new(|| Regex::new(r#""[^"]*""#).unwrap());
static RE_SPECIAL: Lazy<Regex> = Lazy::new(|| Regex::new(r#"[+{}()"^]"#).unwrap());
static RE_STARS: Lazy<Regex> = Lazy::new(|| Regex::new(r"\*+").unwrap());
static RE_LEADING_STAR: Lazy<Regex> = Lazy::new(|| Regex::new(r"(^|\s)\*").unwrap());
static RE_BOOL_START: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^(AND|OR|NOT)\b\s*").unwrap());
static RE_BOOL_END: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\s+(AND|OR|NOT)\s*$").unwrap());
static RE_DOTTED: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(\w+(?:[.-]\w+)+)\b").unwrap());

fn sanitize_fts5_query(query: &str) -> String {
    let mut quoted_parts: Vec<String> = Vec::new();
    let mut sanitized = query.to_string();

    // Extract quoted phrases
    for cap in RE_QUOTED.find_iter(query) {
        let idx = quoted_parts.len();
        quoted_parts.push(cap.as_str().to_string());
        sanitized = sanitized.replace(cap.as_str(), &format!("\u{0000}Q{}\u{0000}", idx));
    }

    sanitized = RE_SPECIAL.replace_all(&sanitized, " ").to_string();
    sanitized = RE_STARS.replace_all(&sanitized, "*").to_string();
    sanitized = RE_LEADING_STAR.replace_all(&sanitized, "$1").to_string();
    sanitized = RE_BOOL_START.replace_all(&sanitized, "").to_string();
    sanitized = RE_BOOL_END.replace_all(&sanitized, "").to_string();
    sanitized = RE_DOTTED.replace_all(&sanitized, r#""$1""#).to_string();

    for (i, quoted) in quoted_parts.iter().enumerate() {
        sanitized = sanitized.replace(&format!("\u{0000}Q{}\u{0000}", i), quoted);
    }
    sanitized.trim().to_string()
}

// ── Error type ──

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("Title conflict: '{0}' is already in use")]
    TitleConflict(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_db() -> (SessionDB, TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let db = SessionDB::open(tmp.path().join("state.db")).unwrap();
        (db, tmp)
    }

    #[test]
    fn test_create_and_get_session() {
        let (db, _tmp) = make_db();
        db.create_session("test-1", "cli", Some("anthropic/opus"), None, None, None, None).unwrap();
        let s = db.get_session("test-1").unwrap().unwrap();
        assert_eq!(s.id, "test-1");
        assert_eq!(s.source, "cli");
        assert_eq!(s.model.as_deref(), Some("anthropic/opus"));
    }

    #[test]
    fn test_end_and_reopen_session() {
        let (db, _tmp) = make_db();
        db.create_session("s1", "cli", None, None, None, None, None).unwrap();
        db.end_session("s1", "completed").unwrap();
        let s = db.get_session("s1").unwrap().unwrap();
        assert!(s.ended_at.is_some());
        db.reopen_session("s1").unwrap();
        let s = db.get_session("s1").unwrap().unwrap();
        assert!(s.ended_at.is_none());
    }

    #[test]
    fn test_append_and_get_messages() {
        let (db, _tmp) = make_db();
        db.create_session("s1", "cli", None, None, None, None, None).unwrap();
        db.append_message("s1", "user", Some("Hello"), None, None, None, None, None, None, None, None).unwrap();
        db.append_message("s1", "assistant", Some("Hi"), None, Some(r#"[{"function":{"name":"todo"}}]"#), None, None, None, None, None, None).unwrap();
        let msgs = db.get_messages("s1").unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        let s = db.get_session("s1").unwrap().unwrap();
        assert_eq!(s.message_count, 2);
    }

    #[test]
    fn test_session_title() {
        let (db, _tmp) = make_db();
        db.create_session("s1", "cli", None, None, None, None, None).unwrap();
        assert!(db.set_session_title("s1", "My Chat").unwrap());
        assert_eq!(db.get_session_title("s1").unwrap(), Some("My Chat".to_string()));
        db.create_session("s2", "cli", None, None, None, None, None).unwrap();
        assert!(db.set_session_title("s2", "My Chat").is_err());
    }

    #[test]
    fn test_list_sessions_rich() {
        let (db, _tmp) = make_db();
        db.create_session("s1", "cli", None, None, None, None, None).unwrap();
        db.create_session("s2", "telegram", None, None, None, None, None).unwrap();
        db.append_message("s1", "user", Some("Hello world"), None, None, None, None, None, None, None, None).unwrap();
        let sessions = db.list_sessions_rich(None, None, 20, 0, false).unwrap();
        assert_eq!(sessions.len(), 2);
        let cli = db.list_sessions_rich(Some("cli"), None, 20, 0, false).unwrap();
        assert_eq!(cli.len(), 1);
    }

    #[test]
    fn test_delete_session() {
        let (db, _tmp) = make_db();
        db.create_session("parent", "cli", None, None, None, None, None).unwrap();
        db.create_session("child", "cli", None, None, None, None, Some("parent")).unwrap();
        db.append_message("parent", "user", Some("Hi"), None, None, None, None, None, None, None, None).unwrap();
        assert!(db.delete_session("parent").unwrap());
        assert!(db.get_session("parent").unwrap().is_none());
        let child = db.get_session("child").unwrap().unwrap();
        assert!(child.parent_session_id.is_none());
    }

    #[test]
    fn test_session_count() {
        let (db, _tmp) = make_db();
        assert_eq!(db.session_count(None).unwrap(), 0);
        db.create_session("s1", "cli", None, None, None, None, None).unwrap();
        db.create_session("s2", "telegram", None, None, None, None, None).unwrap();
        assert_eq!(db.session_count(None).unwrap(), 2);
        assert_eq!(db.session_count(Some("cli")).unwrap(), 1);
    }

    #[test]
    fn test_title_lineage() {
        let (db, _tmp) = make_db();
        db.create_session("s1", "cli", None, None, None, None, None).unwrap();
        db.set_session_title("s1", "my chat").unwrap();
        assert_eq!(db.get_next_title_in_lineage("my chat").unwrap(), "my chat #2");
        // Create session with #2 title
        db.create_session("s2", "cli", None, None, None, None, None).unwrap();
        db.set_session_title("s2", "my chat #2").unwrap();
        assert_eq!(db.get_next_title_in_lineage("my chat #2").unwrap(), "my chat #3");
    }

    #[test]
    fn test_resolve_session_by_title() {
        let (db, _tmp) = make_db();
        db.create_session("s1", "cli", None, None, None, None, None).unwrap();
        db.set_session_title("s1", "chat").unwrap();
        db.create_session("s2", "cli", None, None, None, None, None).unwrap();
        db.set_session_title("s2", "chat #2").unwrap();
        db.create_session("s3", "cli", None, None, None, None, None).unwrap();
        db.set_session_title("s3", "chat #3").unwrap();
        let resolved = db.resolve_session_by_title("chat").unwrap();
        assert_eq!(resolved, Some("s3".to_string()));
    }

    #[test]
    fn test_clear_messages() {
        let (db, _tmp) = make_db();
        db.create_session("s1", "cli", None, None, None, None, None).unwrap();
        for _ in 0..5 {
            db.append_message("s1", "user", Some("msg"), None, None, None, None, None, None, None, None).unwrap();
        }
        assert_eq!(db.message_count(Some("s1")).unwrap(), 5);
        db.clear_messages("s1").unwrap();
        assert_eq!(db.message_count(Some("s1")).unwrap(), 0);
    }

    #[test]
    fn test_export_session() {
        let (db, _tmp) = make_db();
        db.create_session("s1", "cli", Some("model"), None, Some("sys prompt"), None, None).unwrap();
        db.append_message("s1", "user", Some("Hello"), None, None, None, None, None, None, None, None).unwrap();
        db.append_message("s1", "assistant", Some("Hi"), None, None, None, None, None, None, None, None).unwrap();
        let exp = db.export_session("s1").unwrap().unwrap();
        assert_eq!(exp["source"], "cli");
        assert_eq!(exp["model"], "model");
        assert_eq!(exp["system_prompt"], "sys prompt");
        let msgs = exp["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "Hello");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"], "Hi");

        // Non-existent session
        let missing = db.export_session("nope").unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn test_update_token_counts_incremental() {
        let (db, _tmp) = make_db();
        db.create_session("s1", "cli", None, None, None, None, None).unwrap();
        db.update_token_counts("s1", 100, 50, None, 10, 5, 0, None, None, None, None, None, None, None, None, false).unwrap();
        db.update_token_counts("s1", 100, 50, None, 10, 5, 0, None, None, None, None, None, None, None, None, false).unwrap();
        let s = db.get_session("s1").unwrap().unwrap();
        assert_eq!(s.input_tokens, 200);
        assert_eq!(s.output_tokens, 100);
    }

    #[test]
    fn test_update_token_counts_absolute() {
        let (db, _tmp) = make_db();
        db.create_session("s1", "cli", None, None, None, None, None).unwrap();
        db.update_token_counts("s1", 100, 50, None, 10, 5, 0, Some(0.01), Some(0.02), None, None, None, None, None, None, true).unwrap();
        let s = db.get_session("s1").unwrap().unwrap();
        assert_eq!(s.input_tokens, 100);
        assert_eq!(s.estimated_cost_usd, Some(0.01));
        assert_eq!(s.actual_cost_usd, Some(0.02));
    }

    #[test]
    fn test_resolve_session_id_prefix() {
        let (db, _tmp) = make_db();
        db.create_session("abc123", "cli", None, None, None, None, None).unwrap();
        assert_eq!(db.resolve_session_id("abc123").unwrap(), Some("abc123".to_string()));
    }

    #[test]
    fn test_sanitize_fts5_query() {
        assert_eq!(sanitize_fts5_query("hello world"), "hello world");
        let q = sanitize_fts5_query(r#""exact phrase""#);
        assert!(q.contains(r#""exact phrase""#));
        let q = sanitize_fts5_query("chat-send");
        assert!(q.contains(r#""chat-send""#));
    }

    #[test]
    fn test_get_messages_as_conversation() {
        let (db, _tmp) = make_db();
        db.create_session("s1", "cli", None, None, None, None, None).unwrap();
        db.append_message("s1", "user", Some("Hello"), None, None, None, None, None, None, None, None).unwrap();
        db.append_message("s1", "assistant", Some("Hi"), None, None, None, None, None, Some("reasoning"), None, None).unwrap();
        let conv = db.get_messages_as_conversation("s1").unwrap();
        assert_eq!(conv.len(), 2);
        assert_eq!(conv[0]["role"], "user");
        assert_eq!(conv[1]["reasoning"], "reasoning");
    }

    #[test]
    fn test_ensure_session() {
        let (db, _tmp) = make_db();
        db.ensure_session("auto", "unknown", Some("model")).unwrap();
        let s = db.get_session("auto").unwrap().unwrap();
        assert_eq!(s.source, "unknown");
        db.ensure_session("auto", "cli", None).unwrap(); // no-op
    }

    #[test]
    fn test_sanitize_title() {
        use crate::models::sanitize_title;
        assert_eq!(sanitize_title("hello world"), Some("hello world".to_string()));
        assert_eq!(sanitize_title(""), None);
        assert_eq!(sanitize_title("   "), None);
        assert_eq!(sanitize_title("hello   world"), Some("hello world".to_string()));
    }

    #[test]
    fn test_search_sessions() {
        let (db, _tmp) = make_db();
        db.create_session("s1", "cli", Some("model-a"), None, None, None, None).unwrap();
        db.create_session("s2", "telegram", Some("model-b"), None, None, None, None).unwrap();
        db.create_session("s3", "cli", Some("model-c"), None, None, None, None).unwrap();

        // All sessions
        let all = db.search_sessions(None, 10, 0).unwrap();
        assert_eq!(all.len(), 3);

        // Filter by source
        let cli = db.search_sessions(Some("cli"), 10, 0).unwrap();
        assert_eq!(cli.len(), 2);
        assert!(cli.iter().all(|s| s.source == "cli"));

        // Limit
        let limited = db.search_sessions(None, 2, 0).unwrap();
        assert_eq!(limited.len(), 2);

        // Offset
        let offset = db.search_sessions(None, 10, 2).unwrap();
        assert_eq!(offset.len(), 1);
    }

    #[test]
    fn test_search_messages_fts5() {
        let (db, _tmp) = make_db();
        db.create_session("s1", "cli", Some("model-a"), None, None, None, None).unwrap();
        db.create_session("s2", "telegram", Some("model-b"), None, None, None, None).unwrap();
        db.append_message("s1", "user", Some("Hello world"), None, None, None, None, None, None, None, None).unwrap();
        db.append_message("s1", "assistant", Some("Hi there"), None, None, None, None, None, None, None, None).unwrap();
        db.append_message("s2", "user", Some("Hello from telegram"), None, None, None, None, None, None, None, None).unwrap();

        // Search for "hello"
        let results = db.search_messages("hello", None, None, None, 10, 0).unwrap();
        assert_eq!(results.len(), 2);

        // Filter by source
        let cli_only = db.search_messages("hello", Some(&["cli".to_string()]), None, None, 10, 0).unwrap();
        assert_eq!(cli_only.len(), 1);
        assert_eq!(cli_only[0]["source"], "cli");

        // Exclude source
        let no_telegram = db.search_messages("hello", None, Some(&["telegram".to_string()]), None, 10, 0).unwrap();
        assert_eq!(no_telegram.len(), 1);

        // Filter by role
        let user_only = db.search_messages("hello", None, None, Some(&["user".to_string()]), 10, 0).unwrap();
        assert_eq!(user_only.len(), 2);
        let assistant_only = db.search_messages("hello", None, None, Some(&["assistant".to_string()]), 10, 0).unwrap();
        assert_eq!(assistant_only.len(), 0); // "Hi there" doesn't match "hello"

        // Empty query returns empty
        let empty = db.search_messages("", None, None, None, 10, 0).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn test_prune_sessions() {
        let (db, _tmp) = make_db();
        db.create_session("s1", "cli", Some("model"), None, None, None, None).unwrap();
        db.create_session("s2", "cli", Some("model"), None, None, None, None).unwrap();
        db.append_message("s1", "user", Some("Hello"), None, None, None, None, None, None, None, None).unwrap();
        db.append_message("s1", "assistant", Some("Hi"), None, None, None, None, None, None, None, None).unwrap();
        db.append_message("s2", "user", Some("Hey"), None, None, None, None, None, None, None, None).unwrap();

        // Prune with 1 day should not delete anything (sessions just created)
        let pruned = db.prune_sessions(1, Some("cli")).unwrap();
        assert_eq!(pruned, 0);

        // Prune with negative days (cutoff in the future) — should delete everything
        let pruned = db.prune_sessions(-1, Some("cli")).unwrap();
        assert_eq!(pruned, 3); // 3 messages deleted

        // Verify messages are gone
        let m1 = db.get_messages("s1").unwrap();
        assert!(m1.is_empty());
        let m2 = db.get_messages("s2").unwrap();
        assert!(m2.is_empty());
    }

    #[test]
    fn test_update_system_prompt() {
        let (db, _tmp) = make_db();
        db.create_session("s1", "cli", None, None, Some("original"), None, None).unwrap();
        let s = db.get_session("s1").unwrap().unwrap();
        assert_eq!(s.system_prompt.as_deref(), Some("original"));
        db.update_system_prompt("s1", "updated prompt").unwrap();
        let s = db.get_session("s1").unwrap().unwrap();
        assert_eq!(s.system_prompt.as_deref(), Some("updated prompt"));
    }

    #[test]
    fn test_get_session_by_title() {
        let (db, _tmp) = make_db();
        db.create_session("s1", "cli", None, None, None, None, None).unwrap();
        db.set_session_title("s1", "my session").unwrap();
        let s = db.get_session_by_title("my session").unwrap().unwrap();
        assert_eq!(s.id, "s1");
        // Non-existent title
        assert!(db.get_session_by_title("nope").unwrap().is_none());
    }

    #[test]
    fn test_get_next_title_in_lineage_empty() {
        let (db, _tmp) = make_db();
        // No sessions with this title exist
        assert_eq!(db.get_next_title_in_lineage("fresh").unwrap(), "fresh");
    }

    #[test]
    fn test_resolve_session_id_ambiguous() {
        let (db, _tmp) = make_db();
        db.create_session("abc", "cli", None, None, None, None, None).unwrap();
        db.create_session("abcd", "cli", None, None, None, None, None).unwrap();
        // Ambiguous prefix — multiple matches
        assert!(db.resolve_session_id("ab").unwrap().is_none());
        // Exact match
        assert_eq!(db.resolve_session_id("abc").unwrap(), Some("abc".to_string()));
        assert_eq!(db.resolve_session_id("abcd").unwrap(), Some("abcd".to_string()));
        // Unique prefix
        db.create_session("abcde", "cli", None, None, None, None, None).unwrap();
        // "abcde" is unique prefix of "abcd"? No, "abcd" matches "abcd" exactly and is prefix of "abcde"
        // Let's test a truly unique prefix
        db.create_session("z_unique", "cli", None, None, None, None, None).unwrap();
        assert_eq!(db.resolve_session_id("z_").unwrap(), Some("z_unique".to_string()));
    }

    #[test]
    fn test_search_messages_role_filter() {
        let (db, _tmp) = make_db();
        db.create_session("s1", "cli", None, None, None, None, None).unwrap();
        db.append_message("s1", "user", Some("query"), None, None, None, None, None, None, None, None).unwrap();
        db.append_message("s1", "tool", Some("result from query tool"), None, None, None, None, None, None, None, None).unwrap();
        db.append_message("s1", "assistant", Some("here is the answer"), None, None, None, None, None, None, None, None).unwrap();

        let tool_results = db.search_messages("query", None, None, Some(&["tool".to_string()]), 10, 0).unwrap();
        assert_eq!(tool_results.len(), 1);
        assert_eq!(tool_results[0]["role"], "tool");
    }

    #[test]
    fn test_search_messages_empty_query() {
        let (db, _tmp) = make_db();
        db.create_session("s1", "cli", None, None, None, None, None).unwrap();
        db.append_message("s1", "user", Some("hello"), None, None, None, None, None, None, None, None).unwrap();
        // Empty query
        assert!(db.search_messages("", None, None, None, 10, 0).unwrap().is_empty());
        // Special chars only
        assert!(db.search_messages("++{{}}", None, None, None, 10, 0).unwrap().is_empty());
    }

    #[test]
    fn test_append_message_with_tool_calls_updates_count() {
        let (db, _tmp) = make_db();
        db.create_session("s1", "cli", None, None, None, None, None).unwrap();
        // Message with 2 tool calls (two "function" entries)
        let tool_calls_json = r#"[{"function":{"name":"todo"}},{"function":{"name":"read_file"}}]"#;
        db.append_message("s1", "assistant", Some("calling tools"), None, Some(tool_calls_json), None, None, None, None, None, None).unwrap();
        let s = db.get_session("s1").unwrap().unwrap();
        assert_eq!(s.message_count, 1);
        assert_eq!(s.tool_call_count, 2);
    }

    #[test]
    fn test_delete_nonexistent_session() {
        let (db, _tmp) = make_db();
        assert!(!db.delete_session("nonexistent").unwrap());
    }

    #[test]
    fn test_prune_sessions_no_source() {
        let (db, _tmp) = make_db();
        db.create_session("s1", "cli", None, None, None, None, None).unwrap();
        db.append_message("s1", "user", Some("hello"), None, None, None, None, None, None, None, None).unwrap();
        // Negative days = cutoff in future = everything pruned
        let pruned = db.prune_sessions(-1, None).unwrap();
        assert_eq!(pruned, 1);
    }

    #[test]
    fn test_open_creates_parent_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("subdir").join("nested").join("state.db");
        let db = SessionDB::open(&db_path).unwrap();
        assert!(db_path.exists());
        assert!(db.session_count(None).is_ok());
        db.close();
    }

    #[test]
    fn test_schema_version() {
        let (db, _tmp) = make_db();
        let guard = db.conn.lock();
        let version: i64 = guard.query_row("SELECT version FROM schema_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, 6);
    }
}
