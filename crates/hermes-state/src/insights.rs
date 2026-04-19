//! Session Insights Engine for Hermes Agent.
//!
//! Analyzes historical session data from the SQLite state database to produce
//! comprehensive usage insights — token consumption, cost estimates, tool usage
//! patterns, activity trends, model/platform breakdowns, and session metrics.
//!
//! Mirrors the Python `agent/insights.py`.

use chrono::Datelike;
use chrono::Timelike;
use rusqlite::Connection;
use serde::Serialize;
use std::collections::HashMap;

// =========================================================================
// Data types
// =========================================================================

/// A single session row from the database.
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: String,
    pub source: String,
    pub model: String,
    pub started_at: f64,
    pub ended_at: f64,
    pub message_count: i64,
    pub tool_call_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub billing_provider: String,
    pub billing_base_url: String,
}

/// Tool usage count.
#[derive(Debug, Clone)]
pub struct ToolUsage {
    pub tool_name: String,
    pub count: i64,
}

/// Message statistics.
#[derive(Debug, Clone, Default)]
pub struct MessageStats {
    pub total_messages: i64,
    pub user_messages: i64,
    pub assistant_messages: i64,
    pub tool_messages: i64,
}

// =========================================================================
// Overview
// =========================================================================

/// High-level overview statistics.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Overview {
    pub total_sessions: usize,
    pub total_messages: i64,
    pub total_tool_calls: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_read_tokens: i64,
    pub total_cache_write_tokens: i64,
    pub total_tokens: i64,
    pub estimated_cost: f64,
    pub actual_cost: f64,
    pub total_hours: f64,
    pub avg_session_duration: f64,
    pub avg_messages_per_session: f64,
    pub avg_tokens_per_session: f64,
    pub user_messages: i64,
    pub assistant_messages: i64,
    pub tool_messages: i64,
}

/// Model breakdown entry.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ModelBreakdown {
    pub model: String,
    pub sessions: usize,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub total_tokens: i64,
    pub tool_calls: i64,
    pub cost: f64,
}

/// Platform breakdown entry.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PlatformBreakdown {
    pub platform: String,
    pub sessions: usize,
    pub messages: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub total_tokens: i64,
    pub tool_calls: i64,
}

/// Tool breakdown entry.
#[derive(Debug, Clone, Serialize)]
pub struct ToolBreakdown {
    pub tool: String,
    pub count: i64,
    pub percentage: f64,
}

/// Activity pattern data.
#[derive(Debug, Clone, Serialize)]
pub struct ActivityPatterns {
    pub by_day: Vec<DayCount>,
    pub by_hour: Vec<HourCount>,
    pub busiest_day: Option<DayCount>,
    pub busiest_hour: Option<HourCount>,
    pub active_days: usize,
    pub max_streak: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DayCount {
    pub day: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct HourCount {
    pub hour: usize,
    pub count: i64,
}

/// Top session entry.
#[derive(Debug, Clone, Serialize)]
pub struct TopSession {
    pub label: String,
    pub session_id: String,
    pub value: String,
    pub date: String,
}

// =========================================================================
// Complete insights report
// =========================================================================

/// Complete insights report.
#[derive(Debug, Clone, Serialize)]
pub struct InsightsReport {
    pub days: usize,
    pub source_filter: Option<String>,
    pub empty: bool,
    pub overview: Overview,
    pub models: Vec<ModelBreakdown>,
    pub platforms: Vec<PlatformBreakdown>,
    pub tools: Vec<ToolBreakdown>,
    pub activity: ActivityPatterns,
    pub top_sessions: Vec<TopSession>,
}

// =========================================================================
// Insights Engine
// =========================================================================

/// Analyzes session history and produces usage insights.
pub struct InsightsEngine {
    conn: Connection,
}

impl InsightsEngine {
    /// Create a new insights engine from a SQLite connection.
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    /// Generate a complete insights report.
    pub fn generate(&self, days: usize, source: Option<&str>) -> Result<InsightsReport, Box<dyn std::error::Error>> {
        let cutoff = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64())
            - (days as f64 * 86400.0);

        // Gather raw data
        let sessions = self.get_sessions(cutoff, source)?;
        let tool_usage = self.get_tool_usage(cutoff, source)?;
        let message_stats = self.get_message_stats(cutoff, source)?;

        if sessions.is_empty() {
            return Ok(InsightsReport {
                days,
                source_filter: source.map(|s| s.to_string()),
                empty: true,
                overview: Overview::default(),
                models: Vec::new(),
                platforms: Vec::new(),
                tools: Vec::new(),
                activity: ActivityPatterns {
                    by_day: Self::empty_day_breakdown(),
                    by_hour: Self::empty_hour_breakdown(),
                    busiest_day: None,
                    busiest_hour: None,
                    active_days: 0,
                    max_streak: 0,
                },
                top_sessions: Vec::new(),
            });
        }

        // Compute insights
        let overview = self.compute_overview(&sessions, &message_stats);
        let models = self.compute_model_breakdown(&sessions);
        let platforms = self.compute_platform_breakdown(&sessions);
        let tools = self.compute_tool_breakdown(&tool_usage);
        let activity = self.compute_activity_patterns(&sessions);
        let top_sessions = self.compute_top_sessions(&sessions);

        Ok(InsightsReport {
            days,
            source_filter: source.map(|s| s.to_string()),
            empty: false,
            overview,
            models,
            platforms,
            tools,
            activity,
            top_sessions,
        })
    }

    // =========================================================================
    // Data gathering (SQL queries)
    // =========================================================================

    fn get_sessions(&self, cutoff: f64, source: Option<&str>) -> Result<Vec<SessionRow>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT id, source, model, started_at, ended_at, \
             message_count, tool_call_count, input_tokens, output_tokens, \
             cache_read_tokens, cache_write_tokens, billing_provider, \
             billing_base_url \
             FROM sessions \
             WHERE started_at >= ?1 \
             ORDER BY started_at DESC"
        )?;

        let rows: Result<Vec<SessionRow>, rusqlite::Error> = if let Some(src) = source {
            stmt.query_map(rusqlite::params![cutoff, src], |row| {
                Ok(SessionRow {
                    id: row.get(0)?,
                    source: row.get(1)?,
                    model: row.get(2)?,
                    started_at: row.get(3)?,
                    ended_at: row.get(4)?,
                    message_count: row.get(5)?,
                    tool_call_count: row.get(6)?,
                    input_tokens: row.get(7)?,
                    output_tokens: row.get(8)?,
                    cache_read_tokens: row.get(9)?,
                    cache_write_tokens: row.get(10)?,
                    billing_provider: row.get(11).unwrap_or_default(),
                    billing_base_url: row.get(12).unwrap_or_default(),
                })
            })?
            .collect()
        } else {
            stmt.query_map(rusqlite::params![cutoff], |row| {
                Ok(SessionRow {
                    id: row.get(0)?,
                    source: row.get(1)?,
                    model: row.get(2)?,
                    started_at: row.get(3)?,
                    ended_at: row.get(4)?,
                    message_count: row.get(5)?,
                    tool_call_count: row.get(6)?,
                    input_tokens: row.get(7)?,
                    output_tokens: row.get(8)?,
                    cache_read_tokens: row.get(9)?,
                    cache_write_tokens: row.get(10)?,
                    billing_provider: row.get(11).unwrap_or_default(),
                    billing_base_url: row.get(12).unwrap_or_default(),
                })
            })?
            .collect()
        };

        rows
    }

    fn get_tool_usage(&self, cutoff: f64, source: Option<&str>) -> Result<Vec<ToolUsage>, rusqlite::Error> {
        let mut tool_counts: HashMap<String, i64> = HashMap::new();

        // Source 1: explicit tool_name on tool response messages
        if let Ok(mut stmt) = self.conn.prepare(
            "SELECT m.tool_name, COUNT(*) as count \
             FROM messages m \
             JOIN sessions s ON s.id = m.session_id \
             WHERE s.started_at >= ?1 \
               AND m.role = 'tool' AND m.tool_name IS NOT NULL \
             GROUP BY m.tool_name \
             ORDER BY count DESC"
        ) {
            let rows: Result<Vec<_>, _> = if let Some(src) = source {
                stmt.query_map(rusqlite::params![cutoff, src], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?
                .collect()
            } else {
                stmt.query_map(rusqlite::params![cutoff], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?
                .collect()
            };

            for row in rows.unwrap_or_default() {
                *tool_counts.entry(row.0).or_insert(0) += row.1;
            }
        }

        // Source 2: extract from tool_calls JSON on assistant messages
        let mut tool_calls_counts: HashMap<String, i64> = HashMap::new();
        if let Ok(mut stmt) = self.conn.prepare(
            "SELECT m.tool_calls \
             FROM messages m \
             JOIN sessions s ON s.id = m.session_id \
             WHERE s.started_at >= ?1 \
               AND m.role = 'assistant' AND m.tool_calls IS NOT NULL"
        ) {
            let rows: Result<Vec<_>, _> = if let Some(src) = source {
                stmt.query_map(rusqlite::params![cutoff, src], |row| {
                    row.get::<_, String>(0)
                })?
                .collect()
            } else {
                stmt.query_map(rusqlite::params![cutoff], |row| {
                    row.get::<_, String>(0)
                })?
                .collect()
            };

            for row in rows.unwrap_or_default() {
                if let Ok(calls) = serde_json::from_str::<Vec<serde_json::Value>>(&row) {
                    for call in calls {
                        if let Some(func) = call.get("function").and_then(|v| v.as_object()) {
                            if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                                *tool_calls_counts.entry(name.to_string()).or_insert(0) += 1;
                            }
                        }
                    }
                }
            }
        }

        // Merge: prefer tool_name source, supplement with tool_calls source
        if tool_counts.is_empty() && !tool_calls_counts.is_empty() {
            tool_counts = tool_calls_counts;
        } else if !tool_counts.is_empty() && !tool_calls_counts.is_empty() {
            for (tool, count) in &tool_calls_counts {
                let existing = tool_counts.get(tool).copied().unwrap_or(0);
                if *count > existing {
                    tool_counts.insert(tool.clone(), *count);
                }
            }
        }

        let mut result: Vec<ToolUsage> = tool_counts
            .into_iter()
            .map(|(tool_name, count)| ToolUsage { tool_name, count })
            .collect();
        result.sort_by(|a, b| b.count.cmp(&a.count));

        Ok(result)
    }

    fn get_message_stats(&self, cutoff: f64, source: Option<&str>) -> Result<MessageStats, rusqlite::Error> {
        let row = if let Some(src) = source {
            let mut stmt = self.conn.prepare(
                "SELECT \
                 COUNT(*) as total_messages, \
                 SUM(CASE WHEN m.role = 'user' THEN 1 ELSE 0 END) as user_messages, \
                 SUM(CASE WHEN m.role = 'assistant' THEN 1 ELSE 0 END) as assistant_messages, \
                 SUM(CASE WHEN m.role = 'tool' THEN 1 ELSE 0 END) as tool_messages \
                 FROM messages m \
                 JOIN sessions s ON s.id = m.session_id \
                 WHERE s.started_at >= ?1 AND s.source = ?2"
            )?;
            stmt.query_row(rusqlite::params![cutoff, src], |row| {
                Ok(MessageStats {
                    total_messages: row.get(0).unwrap_or(0),
                    user_messages: row.get(1).unwrap_or(0),
                    assistant_messages: row.get(2).unwrap_or(0),
                    tool_messages: row.get(3).unwrap_or(0),
                })
            })
        } else {
            let mut stmt = self.conn.prepare(
                "SELECT \
                 COUNT(*) as total_messages, \
                 SUM(CASE WHEN m.role = 'user' THEN 1 ELSE 0 END) as user_messages, \
                 SUM(CASE WHEN m.role = 'assistant' THEN 1 ELSE 0 END) as assistant_messages, \
                 SUM(CASE WHEN m.role = 'tool' THEN 1 ELSE 0 END) as tool_messages \
                 FROM messages m \
                 JOIN sessions s ON s.id = m.session_id \
                 WHERE s.started_at >= ?1"
            )?;
            stmt.query_row(rusqlite::params![cutoff], |row| {
                Ok(MessageStats {
                    total_messages: row.get(0).unwrap_or(0),
                    user_messages: row.get(1).unwrap_or(0),
                    assistant_messages: row.get(2).unwrap_or(0),
                    tool_messages: row.get(3).unwrap_or(0),
                })
            })
        };

        row.or_else(|_| Ok(MessageStats::default()))
    }

    // =========================================================================
    // Computation
    // =========================================================================

    fn compute_overview(&self, sessions: &[SessionRow], message_stats: &MessageStats) -> Overview {
        let total_input: i64 = sessions.iter().map(|s| s.input_tokens).sum();
        let total_output: i64 = sessions.iter().map(|s| s.output_tokens).sum();
        let total_cache_read: i64 = sessions.iter().map(|s| s.cache_read_tokens).sum();
        let total_cache_write: i64 = sessions.iter().map(|s| s.cache_write_tokens).sum();
        let total_tokens = total_input + total_output + total_cache_read + total_cache_write;
        let total_tool_calls: i64 = sessions.iter().map(|s| s.tool_call_count).sum();
        let total_messages: i64 = sessions.iter().map(|s| s.message_count).sum();

        // Session duration stats
        let durations: Vec<f64> = sessions
            .iter()
            .filter_map(|s| {
                if s.ended_at > s.started_at {
                    Some(s.ended_at - s.started_at)
                } else {
                    None
                }
            })
            .collect();

        let total_hours = durations.iter().sum::<f64>() / 3600.0;
        let avg_duration = if durations.is_empty() {
            0.0
        } else {
            durations.iter().sum::<f64>() / durations.len() as f64
        };

        let total_cost = 0.0; // Placeholder — full pricing integration would go here
        let actual_cost = 0.0; // Placeholder

        Overview {
            total_sessions: sessions.len(),
            total_messages,
            total_tool_calls,
            total_input_tokens: total_input,
            total_output_tokens: total_output,
            total_cache_read_tokens: total_cache_read,
            total_cache_write_tokens: total_cache_write,
            total_tokens,
            estimated_cost: total_cost,
            actual_cost,
            total_hours,
            avg_session_duration: avg_duration,
            avg_messages_per_session: if sessions.is_empty() { 0.0 } else { total_messages as f64 / sessions.len() as f64 },
            avg_tokens_per_session: if sessions.is_empty() { 0.0 } else { total_tokens as f64 / sessions.len() as f64 },
            user_messages: message_stats.user_messages,
            assistant_messages: message_stats.assistant_messages,
            tool_messages: message_stats.tool_messages,
        }
    }

    fn compute_model_breakdown(&self, sessions: &[SessionRow]) -> Vec<ModelBreakdown> {
        let mut model_data: HashMap<String, ModelBreakdown> = HashMap::new();

        for s in sessions {
            let display_model = if s.model.contains('/') {
                s.model.split('/').next_back().unwrap_or(&s.model)
            } else {
                &s.model
            };
            let display_model = if display_model.is_empty() { "unknown" } else { display_model };

            let entry = model_data.entry(display_model.to_string()).or_default();
            entry.model = display_model.to_string();
            entry.sessions += 1;
            entry.input_tokens += s.input_tokens;
            entry.output_tokens += s.output_tokens;
            entry.cache_read_tokens += s.cache_read_tokens;
            entry.cache_write_tokens += s.cache_write_tokens;
            entry.total_tokens += s.input_tokens + s.output_tokens + s.cache_read_tokens + s.cache_write_tokens;
            entry.tool_calls += s.tool_call_count;
        }

        let mut result: Vec<ModelBreakdown> = model_data.into_values().collect();
        result.sort_by(|a, b| {
            (b.total_tokens, b.sessions as i64).cmp(&(a.total_tokens, a.sessions as i64))
        });
        result
    }

    fn compute_platform_breakdown(&self, sessions: &[SessionRow]) -> Vec<PlatformBreakdown> {
        let mut platform_data: HashMap<String, PlatformBreakdown> = HashMap::new();

        for s in sessions {
            let source = if s.source.is_empty() { "unknown" } else { &s.source };
            let entry = platform_data.entry(source.to_string()).or_default();
            entry.platform = source.to_string();
            entry.sessions += 1;
            entry.messages += s.message_count;
            entry.input_tokens += s.input_tokens;
            entry.output_tokens += s.output_tokens;
            entry.cache_read_tokens += s.cache_read_tokens;
            entry.cache_write_tokens += s.cache_write_tokens;
            entry.total_tokens += s.input_tokens + s.output_tokens + s.cache_read_tokens + s.cache_write_tokens;
            entry.tool_calls += s.tool_call_count;
        }

        let mut result: Vec<PlatformBreakdown> = platform_data.into_values().collect();
        result.sort_by(|a, b| b.sessions.cmp(&a.sessions));
        result
    }

    fn compute_tool_breakdown(&self, tool_usage: &[ToolUsage]) -> Vec<ToolBreakdown> {
        let total_calls: i64 = tool_usage.iter().map(|t| t.count).sum();
        tool_usage
            .iter()
            .map(|t| {
                let pct = if total_calls > 0 {
                    t.count as f64 / total_calls as f64 * 100.0
                } else {
                    0.0
                };
                ToolBreakdown {
                    tool: t.tool_name.clone(),
                    count: t.count,
                    percentage: pct,
                }
            })
            .collect()
    }

    fn compute_activity_patterns(&self, sessions: &[SessionRow]) -> ActivityPatterns {
        let day_names = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
        let mut day_counts = [0i64; 7];
        let mut hour_counts = [0i64; 24];
        let mut daily_counts: HashMap<String, i64> = HashMap::new();

        for s in sessions {
            let dt = chrono::DateTime::from_timestamp(s.started_at as i64, 0);
            if let Some(dt) = dt {
                day_counts[dt.weekday().num_days_from_monday() as usize % 7] += 1;
                hour_counts[dt.time().hour() as usize] += 1;
                daily_counts
                    .entry(dt.format("%Y-%m-%d").to_string())
                    .and_modify(|c| *c += 1)
                    .or_insert(1);
            }
        }

        let by_day: Vec<DayCount> = (0..7)
            .map(|i| DayCount {
                day: day_names[i].to_string(),
                count: day_counts[i],
            })
            .collect();

        let by_hour: Vec<HourCount> = (0..24)
            .map(|i| HourCount { hour: i, count: hour_counts[i] })
            .collect();

        let busiest_day = by_day.iter().max_by_key(|d| d.count).cloned();
        let busiest_hour = by_hour.iter().max_by_key(|h| h.count).cloned();

        // Streak calculation
        let max_streak = if daily_counts.is_empty() {
            0
        } else {
            let mut all_dates: Vec<String> = daily_counts.keys().cloned().collect();
            all_dates.sort();
            let mut current_streak = 1;
            let mut max_streak = 1;
            for i in 1..all_dates.len() {
                let d1 = chrono::NaiveDate::parse_from_str(&all_dates[i - 1], "%Y-%m-%d");
                let d2 = chrono::NaiveDate::parse_from_str(&all_dates[i], "%Y-%m-%d");
                if let (Ok(d1), Ok(d2)) = (d1, d2) {
                    if (d2 - d1).num_days() == 1 {
                        current_streak += 1;
                        max_streak = max_streak.max(current_streak);
                    } else {
                        current_streak = 1;
                    }
                }
            }
            max_streak
        };

        ActivityPatterns {
            by_day,
            by_hour,
            busiest_day,
            busiest_hour,
            active_days: daily_counts.len(),
            max_streak,
        }
    }

    fn compute_top_sessions(&self, sessions: &[SessionRow]) -> Vec<TopSession> {
        let mut top = Vec::new();

        // Longest by duration
        if let Some(longest) = sessions.iter().filter(|s| s.ended_at > s.started_at).max_by(|a, b| {
            let da = a.ended_at - a.started_at;
            let db = b.ended_at - b.started_at;
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        }) {
            let dur = longest.ended_at - longest.started_at;
            top.push(TopSession {
                label: "Longest session".to_string(),
                session_id: longest.id.chars().take(16).collect(),
                value: format_duration_compact(dur),
                date: format_timestamp_short(longest.started_at),
            });
        }

        // Most messages
        if let Some(most_msgs) = sessions.iter().max_by_key(|s| s.message_count) {
            if most_msgs.message_count > 0 {
                top.push(TopSession {
                    label: "Most messages".to_string(),
                    session_id: most_msgs.id.chars().take(16).collect(),
                    value: format!("{} msgs", most_msgs.message_count),
                    date: format_timestamp_short(most_msgs.started_at),
                });
            }
        }

        // Most tokens
        if let Some(most_tokens) = sessions.iter().max_by_key(|s| s.input_tokens + s.output_tokens) {
            let token_total = most_tokens.input_tokens + most_tokens.output_tokens;
            if token_total > 0 {
                top.push(TopSession {
                    label: "Most tokens".to_string(),
                    session_id: most_tokens.id.chars().take(16).collect(),
                    value: format!("{} tokens", token_total),
                    date: format_timestamp_short(most_tokens.started_at),
                });
            }
        }

        // Most tool calls
        if let Some(most_tools) = sessions.iter().max_by_key(|s| s.tool_call_count) {
            if most_tools.tool_call_count > 0 {
                top.push(TopSession {
                    label: "Most tool calls".to_string(),
                    session_id: most_tools.id.chars().take(16).collect(),
                    value: format!("{} calls", most_tools.tool_call_count),
                    date: format_timestamp_short(most_tools.started_at),
                });
            }
        }

        top
    }

    // =========================================================================
    // Formatting
    // =========================================================================

    /// Format the insights report for terminal display (CLI).
    pub fn format_terminal(&self, report: &InsightsReport) -> String {
        if report.empty {
            let src = match &report.source_filter {
                Some(s) => format!(" (source: {})", s),
                None => String::new(),
            };
            return format!("  No sessions found in the last {} days{}.", report.days, src);
        }

        let mut lines: Vec<String> = Vec::new();
        let o = &report.overview;
        let days = report.days;

        lines.push(String::new());
        lines.push("  ╔══════════════════════════════════════════════════════════╗".to_string());
        lines.push("  ║                    📊 Hermes Insights                    ║".to_string());
        let period_label = match &report.source_filter {
            Some(s) => format!("Last {} days ({})", days, s),
            None => format!("Last {} days", days),
        };
        let padding = 58usize.saturating_sub(period_label.len() + 2);
        let left_pad = padding / 2;
        let right_pad = padding - left_pad;
        lines.push(format!("  ║{:width$} {} {:width2$}║", "", period_label, "", width = left_pad, width2 = right_pad));
        lines.push("  ╚══════════════════════════════════════════════════════════╝".to_string());
        lines.push(String::new());

        // Overview
        lines.push("  📋 Overview".to_string());
        lines.push(format!("  {}", "─".repeat(56)));
        lines.push(format!("  Sessions:          {:<12}  Messages:        {}", o.total_sessions, o.total_messages));
        lines.push(format!("  Tool calls:        {:<12}  User messages:   {}", o.total_tool_calls, o.user_messages));
        lines.push(format!("  Input tokens:      {:<12}  Output tokens:   {}", o.total_input_tokens, o.total_output_tokens));
        let cache_total = o.total_cache_read_tokens + o.total_cache_write_tokens;
        if cache_total > 0 {
            lines.push(format!("  Cache read:        {:<12}  Cache write:     {}", o.total_cache_read_tokens, o.total_cache_write_tokens));
        }
        lines.push(format!("  Total tokens:      {:<12}  Est. cost:       ${:.2}", o.total_tokens, o.estimated_cost));
        if o.total_hours > 0.0 {
            lines.push(format!("  Active time:       ~{:<11}  Avg session:     ~{}", format_duration_compact(o.total_hours * 3600.0), format_duration_compact(o.avg_session_duration)));
        }
        lines.push(format!("  Avg msgs/session:  {:.1}", o.avg_messages_per_session));
        lines.push(String::new());

        // Model breakdown
        if !report.models.is_empty() {
            lines.push("  🤖 Models Used".to_string());
            lines.push(format!("  {}", "─".repeat(56)));
            lines.push(format!("  {:<30} {:>8} {:>12} {:>8}", "Model", "Sessions", "Tokens", "Cost"));
            for m in &report.models {
                let model_name = if m.model.len() > 28 { &m.model[..28] } else { &m.model };
                lines.push(format!("  {:<30} {:>8} {:>12} {:>8}", model_name, m.sessions, m.total_tokens, format!("${:.2}", m.cost)));
            }
            lines.push(String::new());
        }

        // Platform breakdown
        if report.platforms.len() > 1 || (!report.platforms.is_empty() && report.platforms[0].platform != "cli") {
            lines.push("  📱 Platforms".to_string());
            lines.push(format!("  {}", "─".repeat(56)));
            lines.push(format!("  {:<14} {:>8} {:>10} {:>14}", "Platform", "Sessions", "Messages", "Tokens"));
            for p in &report.platforms {
                lines.push(format!("  {:<14} {:>8} {:>10} {:>14}", p.platform, p.sessions, p.messages, p.total_tokens));
            }
            lines.push(String::new());
        }

        // Tool usage
        if !report.tools.is_empty() {
            lines.push("  🔧 Top Tools".to_string());
            lines.push(format!("  {}", "─".repeat(56)));
            lines.push(format!("  {:<28} {:>8} {:>8}", "Tool", "Calls", "%"));
            for t in report.tools.iter().take(15) {
                lines.push(format!("  {:<28} {:>8} {:>7.1}%", t.tool, t.count, t.percentage));
            }
            if report.tools.len() > 15 {
                lines.push(format!("  ... and {} more tools", report.tools.len() - 15));
            }
            lines.push(String::new());
        }

        // Activity patterns
        let act = &report.activity;
        if !act.by_day.is_empty() {
            lines.push("  📅 Activity Patterns".to_string());
            lines.push(format!("  {}", "─".repeat(56)));

            // Day of week chart
            let day_values: Vec<i64> = act.by_day.iter().map(|d| d.count).collect();
            let peak = *day_values.iter().max().unwrap_or(&1);
            let max_width = 15;
            for d in act.by_day.iter() {
                let bar_len = if peak == 0 {
                    0
                } else {
                    ((d.count as f64 / peak as f64 * max_width as f64) as usize).max(1)
                };
                let bar = if d.count > 0 { "█".repeat(bar_len) } else { String::new() };
                lines.push(format!("  {}  {:<15} {}", d.day, bar, d.count));
            }

            lines.push(String::new());

            // Peak hours
            let mut busy_hours: Vec<&HourCount> = act.by_hour.iter().filter(|h| h.count > 0).collect();
            busy_hours.sort_by(|a, b| b.count.cmp(&a.count));
            busy_hours.truncate(5);
            if !busy_hours.is_empty() {
                let hour_strs: Vec<String> = busy_hours.iter().map(|h| {
                    let ampm = if h.hour < 12 { "AM" } else { "PM" };
                    let display_hr = if h.hour % 12 == 0 { 12 } else { h.hour % 12 };
                    format!("{}{} ({})", display_hr, ampm, h.count)
                }).collect();
                lines.push(format!("  Peak hours: {}", hour_strs.join(", ")));
            }

            if act.active_days > 0 {
                lines.push(format!("  Active days: {}", act.active_days));
            }
            if act.max_streak > 1 {
                lines.push(format!("  Best streak: {} consecutive days", act.max_streak));
            }
            lines.push(String::new());
        }

        // Notable sessions
        if !report.top_sessions.is_empty() {
            lines.push("  🏆 Notable Sessions".to_string());
            lines.push(format!("  {}", "─".repeat(56)));
            for ts in &report.top_sessions {
                lines.push(format!("  {:<20} {:<18} ({}, {})", ts.label, ts.value, ts.date, ts.session_id));
            }
            lines.push(String::new());
        }

        lines.join("\n")
    }

    // =========================================================================
    // Helpers
    // =========================================================================

    fn empty_day_breakdown() -> Vec<DayCount> {
        let day_names = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
        day_names.iter().map(|&d| DayCount { day: d.to_string(), count: 0 }).collect()
    }

    fn empty_hour_breakdown() -> Vec<HourCount> {
        (0..24).map(|h| HourCount { hour: h, count: 0 }).collect()
    }
}

/// Format seconds into a human-readable duration string.
pub fn format_duration_compact(seconds: f64) -> String {
    if seconds < 60.0 {
        format!("{:.0}s", seconds)
    } else if seconds < 3600.0 {
        let mins = (seconds / 60.0) as u64;
        let secs = (seconds % 60.0) as u64;
        format!("{}m {}s", mins, secs)
    } else if seconds < 86400.0 {
        let hrs = (seconds / 3600.0) as u64;
        let mins = ((seconds % 3600.0) / 60.0) as u64;
        format!("{}h {}m", hrs, mins)
    } else {
        let days = (seconds / 86400.0) as u64;
        let hrs = ((seconds % 86400.0) / 3600.0) as u64;
        format!("{}d {}h", days, hrs)
    }
}

/// Format a Unix timestamp as a short date string.
fn format_timestamp_short(ts: f64) -> String {
    let dt = chrono::DateTime::from_timestamp(ts as i64, 0);
    match dt {
        Some(dt) => dt.format("%b %d").to_string(),
        None => "?".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_duration_compact() {
        assert_eq!(format_duration_compact(30.0), "30s");
        assert_eq!(format_duration_compact(90.0), "1m 30s");
        assert_eq!(format_duration_compact(3661.0), "1h 1m");
        assert_eq!(format_duration_compact(90000.0), "1d 1h");
    }

    #[test]
    fn test_format_timestamp_short() {
        let result = format_timestamp_short(1700000000.0);
        assert!(!result.is_empty());
    }

    #[test]
    fn test_insights_engine_empty() {
        // Create an in-memory DB with the required tables
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY, source TEXT, model TEXT,
                started_at REAL, ended_at REAL,
                message_count INTEGER, tool_call_count INTEGER,
                input_tokens INTEGER, output_tokens INTEGER,
                cache_read_tokens INTEGER, cache_write_tokens INTEGER,
                billing_provider TEXT, billing_base_url TEXT
            )",
            [],
        ).unwrap();
        conn.execute(
            "CREATE TABLE messages (
                id TEXT PRIMARY KEY, session_id TEXT, role TEXT,
                content TEXT, created_at REAL
            )",
            [],
        ).unwrap();
        conn.execute(
            "CREATE TABLE IF NOT EXISTS tool_calls (
                id INTEGER PRIMARY KEY, session_id TEXT, tool_name TEXT,
                args TEXT, result TEXT, created_at REAL
            )",
            [],
        ).unwrap();

        let engine = InsightsEngine::new(conn);
        let report = engine.generate(30, None).unwrap();
        assert!(report.empty);
    }

    #[test]
    fn test_compute_activity_patterns_empty() {
        let patterns = InsightsEngine::compute_activity_patterns(
            &InsightsEngine { conn: Connection::open_in_memory().unwrap() },
            &[],
        );
        assert_eq!(patterns.by_day.len(), 7);
        assert_eq!(patterns.by_hour.len(), 24);
        assert_eq!(patterns.active_days, 0);
        assert_eq!(patterns.max_streak, 0);
    }

    #[test]
    fn test_compute_activity_patterns_single_session() {
        let sessions = vec![SessionRow {
            id: "test".to_string(),
            source: "cli".to_string(),
            model: "test".to_string(),
            started_at: 1700000000.0,
            ended_at: 1700003600.0,
            message_count: 10,
            tool_call_count: 5,
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            billing_provider: "test".to_string(),
            billing_base_url: "".to_string(),
        }];

        let engine = InsightsEngine::new(Connection::open_in_memory().unwrap());
        let patterns = engine.compute_activity_patterns(&sessions);
        assert_eq!(patterns.active_days, 1);
    }
}
