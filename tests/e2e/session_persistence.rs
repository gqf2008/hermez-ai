//! E2E: Session Persistence
//!
//! If these pass, conversation history can be saved to SQLite and restored.

use hermez_state::session_db::SessionDB;

// ── 1. Session create + get round-trip ──────────────────────────────────────

#[test]
fn test_session_create_and_get() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let db = SessionDB::open(tmp.path()).unwrap();

    let sid = db
        .create_session("sess-1", "test", Some("gpt-4o"), None, None, Some("u1"), None)
        .unwrap();
    assert_eq!(sid, "sess-1");

    let sess = db.get_session("sess-1").unwrap().expect("session exists");
    assert_eq!(sess.id, "sess-1");
    assert_eq!(sess.source, "test");
    assert_eq!(sess.model.as_deref(), Some("gpt-4o"));
    assert_eq!(sess.user_id.as_deref(), Some("u1"));
}

// ── 2. Message append + retrieve round-trip ─────────────────────────────────

#[test]
fn test_message_append_and_get() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let db = SessionDB::open(tmp.path()).unwrap();

    db.create_session("sess-msg", "test", None, None, None, None, None)
        .unwrap();

    db.append_message("sess-msg", "user", Some("Hello"), None, None, None, None, None, None, None, None)
        .unwrap();
    db.append_message("sess-msg", "assistant", Some("Hi there"), None, None, None, None, None, None, None, None)
        .unwrap();

    let msgs = db.get_messages("sess-msg").unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].role, "user");
    assert_eq!(msgs[0].content.as_deref(), Some("Hello"));
    assert_eq!(msgs[1].role, "assistant");
    assert_eq!(msgs[1].content.as_deref(), Some("Hi there"));
}

// ── 3. Conversation JSON export matches expected shape ──────────────────────

#[test]
fn test_conversation_export_format() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let db = SessionDB::open(tmp.path()).unwrap();

    db.create_session("sess-fmt", "test", None, None, None, None, None)
        .unwrap();

    db.append_message("sess-fmt", "system", Some("You are a helper."), None, None, None, None, None, None, None, None)
        .unwrap();
    db.append_message("sess-fmt", "user", Some("What's 2+2?"), None, None, None, None, None, None, None, None)
        .unwrap();
    db.append_message("sess-fmt", "assistant", Some("4"), None, None, None, None, None, None, None, None)
        .unwrap();

    let conv = db.get_messages_as_conversation("sess-fmt").unwrap();
    assert_eq!(conv.len(), 3);

    let first = conv.first().unwrap();
    assert_eq!(first.get("role").and_then(|v| v.as_str()), Some("system"));
    assert_eq!(first.get("content").and_then(|v| v.as_str()), Some("You are a helper."));

    let last = conv.last().unwrap();
    assert_eq!(last.get("role").and_then(|v| v.as_str()), Some("assistant"));
    assert_eq!(last.get("content").and_then(|v| v.as_str()), Some("4"));
}

// ── 4. Session title set and retrieve ───────────────────────────────────────

#[test]
fn test_session_title_round_trip() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let db = SessionDB::open(tmp.path()).unwrap();

    db.create_session("sess-title", "test", None, None, None, None, None)
        .unwrap();

    db.set_session_title("sess-title", "My Test Session").unwrap();

    let title = db.get_session_title("sess-title").unwrap();
    assert_eq!(title, Some("My Test Session".to_string()));

    let by_title = db.get_session_by_title("My Test Session").unwrap();
    assert!(by_title.is_some());
    assert_eq!(by_title.unwrap().id, "sess-title");
}

// ── 5. Session end + reopen ─────────────────────────────────────────────────

#[test]
fn test_session_end_and_reopen() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let db = SessionDB::open(tmp.path()).unwrap();

    db.create_session("sess-end", "test", None, None, None, None, None)
        .unwrap();

    db.end_session("sess-end", "user_closed").unwrap();
    let ended = db.get_session("sess-end").unwrap().unwrap();
    assert!(ended.ended_at.is_some());
    assert_eq!(ended.end_reason.as_deref(), Some("user_closed"));

    db.reopen_session("sess-end").unwrap();
    let reopened = db.get_session("sess-end").unwrap().unwrap();
    assert!(reopened.ended_at.is_none());
    assert!(reopened.end_reason.is_none());
}

// ── 6. Message search (FTS5) ────────────────────────────────────────────────

#[test]
fn test_message_search() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let db = SessionDB::open(tmp.path()).unwrap();

    db.create_session("sess-search", "test", None, None, None, None, None)
        .unwrap();

    db.append_message("sess-search", "user", Some("Where is the Eiffel Tower?"), None, None, None, None, None, None, None, None)
        .unwrap();
    db.append_message(
        "sess-search",
        "assistant",
        Some("The Eiffel Tower is in Paris, France."),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .unwrap();

    let results = db.search_messages("Paris", None, None, None, 10, 0).unwrap();
    assert!(!results.is_empty(), "FTS search should find Paris");
    assert!(results.iter().any(|m| {
        m.get("snippet")
            .and_then(|v| v.as_str())
            .map(|s| s.contains("Paris"))
            .unwrap_or(false)
    }));
}

// ── 7. Clear messages preserves session ─────────────────────────────────────

#[test]
fn test_clear_messages_keeps_session() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let db = SessionDB::open(tmp.path()).unwrap();

    db.create_session("sess-clear", "test", None, None, None, None, None)
        .unwrap();
    db.append_message("sess-clear", "user", Some("Hi"), None, None, None, None, None, None, None, None)
        .unwrap();

    db.clear_messages("sess-clear").unwrap();

    let msgs = db.get_messages("sess-clear").unwrap();
    assert!(msgs.is_empty());

    let sess = db.get_session("sess-clear").unwrap();
    assert!(sess.is_some());
}
