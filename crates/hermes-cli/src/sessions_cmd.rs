#![allow(dead_code)]
//! Session management commands for the Hermes CLI.
//!
//! Mirrors the Python `hermes sessions` subcommand.

use console::style;
use hermes_state::SessionDB;

/// List recent sessions.
pub fn cmd_sessions_list(
    db: &SessionDB,
    limit: usize,
    source: Option<&str>,
    _verbose: bool,
) -> anyhow::Result<()> {
    let sessions = db.list_sessions_rich(source, None, limit, 0, true)?;

    if sessions.is_empty() {
        println!("{}", style("No sessions found.").dim());
        return Ok(());
    }

    println!(
        "{}",
        style(format!("{:^8}  {:^20}  {:12}  {:>6}  {:>6}  {:>5}  {}",
            "ID", "Title", "Model", "In", "Out", "Calls", "Preview"))
        .bold()
    );
    println!("{}", style("-".repeat(100)).dim());

    for sp in &sessions {
        let session = &sp.session;
        let short_id = &session.id[..8.min(session.id.len())];
        let title = session.title.as_deref().unwrap_or("(untitled)");
        let model = session
            .model
            .as_deref()
            .unwrap_or("(default)")
            .split('/')
            .next_back()
            .unwrap_or("?");
        let preview: String = sp.preview.chars().take(50).collect();

        println!(
            "{:<8}  {:20}  {:12}  {:>6}  {:>6}  {:>5}  {}",
            style(short_id).cyan(),
            style(title).dim(),
            model,
            session.input_tokens,
            session.output_tokens,
            session.tool_call_count,
            style(preview).dim(),
        );
    }

    println!("\n{} session(s) shown", sessions.len());
    Ok(())
}

/// Export sessions to JSONL.
pub fn cmd_sessions_export(
    db: &SessionDB,
    path: &str,
    source: Option<&str>,
    session_id: Option<&str>,
) -> anyhow::Result<()> {
    // Single session export
    if let Some(sid) = session_id {
        let resolved = db.resolve_session_id(sid)?;
        let sid = resolved.as_deref().unwrap_or(sid);
        let export = db.export_session(sid)?;
        if let Some(data) = export {
            let json = serde_json::to_string_pretty(&data)?;
            if path == "-" {
                println!("{json}");
            } else {
                std::fs::write(path, json)?;
                println!("Exported session {sid} to {path}");
            }
        } else {
            println!("Session {sid} not found.");
        }
        return Ok(());
    }

    // Bulk export with optional source filter
    let sessions = db.list_sessions_rich(source, None, 10000, 0, false)?;
    let output: Vec<_> = sessions.iter().map(|s| {
        serde_json::json!({
            "id": s.session.id,
            "title": s.session.title,
            "source": s.session.source,
            "created_at": s.session.started_at,
            "input_tokens": s.session.input_tokens,
            "output_tokens": s.session.output_tokens,
        })
    }).collect();

    let jsonl: String = output.iter()
        .map(|v| serde_json::to_string(v).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");

    if path == "-" {
        println!("{jsonl}");
    } else {
        std::fs::write(path, jsonl)?;
        println!("Exported {} sessions to {path}", output.len());
    }
    Ok(())
}

/// Delete a session.
pub fn cmd_sessions_delete(
    db: &SessionDB,
    session_id: &str,
    force: bool,
) -> anyhow::Result<()> {
    let resolved = db.resolve_session_id(session_id)?;
    let sid = resolved.as_deref().unwrap_or(session_id);

    if !force {
        print!("Delete session {}? [y/N] ", style(sid).yellow());
        std::io::Write::flush(&mut std::io::stdout())?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    match db.delete_session(sid) {
        Ok(true) => println!("{} Deleted session {}", style("[OK]").green(), style(sid).cyan()),
        Ok(false) => println!("{}", style(format!("Session {} not found.", sid)).red()),
        Err(e) => println!("{}", style(format!("Error deleting session: {e}")).red()),
    }
    Ok(())
}

/// Search sessions by query (FTS5).
pub fn cmd_sessions_search(
    db: &SessionDB,
    query: &str,
    limit: usize,
) -> anyhow::Result<()> {
    let sessions = db.search_sessions(None, limit, 0)?;

    if sessions.is_empty() {
        println!("{}", style("No matching sessions found.").dim());
        return Ok(());
    }

    println!(
        "{}",
        style(format!("{:^8}  {:20}  {:12}  {}",
            "ID", "Title", "Model", "Query Match"))
        .bold()
    );
    println!("{}", style("-".repeat(80)).dim());

    for session in &sessions {
        let short_id = &session.id[..8.min(session.id.len())];
        let title = session.title.as_deref().unwrap_or("(untitled)");
        let model = session
            .model
            .as_deref()
            .unwrap_or("(default)")
            .split('/')
            .next_back()
            .unwrap_or("?");

        // Get matching message preview
        let matches = db.search_messages(query, None, None, None, 1, 0)?;
        let preview = matches
            .first()
            .and_then(|m| m.get("content").and_then(|v| v.as_str()))
            .map(|c| {
                let truncated: String = c.chars().take(60).collect();
                truncated
            })
            .unwrap_or_default();

        println!(
            "{:<8}  {:20}  {:12}  {}",
            style(short_id).cyan(),
            style(title).dim(),
            model,
            style(preview).dim(),
        );
    }

    println!("\n{} session(s) matched", sessions.len());
    Ok(())
}

/// Show session statistics.
pub fn cmd_sessions_stats(db: &SessionDB, source: Option<&str>) -> anyhow::Result<()> {
    let total_sessions = db.session_count(source)?;
    let total_messages = db.message_count(None)?;

    println!("{}", style("Session Statistics").bold());
    println!("{}", "-".repeat(40));
    println!(
        "Total sessions: {}",
        style(total_sessions).cyan()
    );
    println!(
        "Total messages: {}",
        style(total_messages).cyan()
    );

    if total_sessions > 0 {
        let sessions = db.list_sessions_rich(source, None, 1, 0, false)?;
        if let Some(sp) = sessions.first() {
            let s = &sp.session;
            println!(
                "Sources: {}",
                style(s.source.clone()).cyan()
            );
        }
    }

    Ok(())
}

/// Rename a session's title.
pub fn cmd_sessions_rename(
    db: &SessionDB,
    session_id: &str,
    title: &str,
) -> anyhow::Result<()> {
    match db.rename_session(session_id, title)? {
        true => {
            println!("  {} Session '{}' renamed to: {}", style("✓").green(), session_id, style(title).cyan());
        }
        false => {
            println!("  {} Session '{}' not found.", style("✗").red(), session_id);
        }
    }
    Ok(())
}

/// Prune (delete) old sessions.
pub fn cmd_sessions_prune(
    db: &SessionDB,
    older_than_days: i64,
    source: Option<&str>,
    force: bool,
) -> anyhow::Result<()> {
    let cyan = style("◆").cyan();
    let green = style("✓").green();
    let yellow = style("⚠").yellow();

    // Show what would be deleted
    let cutoff_sessions = db.list_sessions_rich(source, None, 1000, 0, false)?;
    let cutoff_count = cutoff_sessions.iter()
        .filter(|sp| {
            let age_days = (hermes_state::now_epoch() - sp.session.started_at) / 86400.0;
            age_days >= older_than_days as f64
        })
        .count();

    println!();
    println!("{} Prune Old Sessions", cyan);
    println!();
    println!("  Criteria: older than {older_than_days} days");
    if let Some(s) = source {
        println!("  Source: {s}");
    }
    println!("  Sessions to delete: {cutoff_count}");
    println!();

    if !force && cutoff_count > 0 {
        print!("  Delete these sessions? [y/N] ");
        std::io::Write::flush(&mut std::io::stdout())?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("  Cancelled.");
            return Ok(());
        }
    }

    if cutoff_count == 0 {
        println!("  {} No sessions older than {older_than_days} days.", yellow);
        println!();
        return Ok(());
    }

    let deleted = db.prune_old_sessions(older_than_days, source)?;
    println!("  {} Deleted {deleted} old session(s)", green);
    println!();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_empty_db() {
        let db = SessionDB::open(":memory:").unwrap();
        let result = cmd_sessions_list(&db, 10, None, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_export_not_found() {
        let db = SessionDB::open(":memory:").unwrap();
        let tmp = std::env::temp_dir().join("hermes_test_export.jsonl");
        let result = cmd_sessions_export(&db, &tmp.to_string_lossy(), None, Some("nonexistent"));
        assert!(result.is_ok());
        let _ = std::fs::remove_file(tmp);
    }

    #[test]
    fn test_delete_not_found() {
        let db = SessionDB::open(":memory:").unwrap();
        let result = cmd_sessions_delete(&db, "nonexistent", true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_stats_empty() {
        let db = SessionDB::open(":memory:").unwrap();
        let result = cmd_sessions_stats(&db, None);
        assert!(result.is_ok());
    }
}
