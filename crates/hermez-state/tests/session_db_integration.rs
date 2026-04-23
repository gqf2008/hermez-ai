//! Integration tests for hermez-state session database.

#[test]
fn test_session_db_create_and_get() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("test_sessions.db");
    let db = hermez_state::SessionDB::open(&db_path).unwrap();

    db.create_session("sess-test-1", "cli", Some("anthropic/claude-opus-4"), None, None, None, None).unwrap();

    let session = db.get_session("sess-test-1").unwrap();
    assert!(session.is_some());
    assert_eq!(session.unwrap().id, "sess-test-1");
}

#[test]
fn test_session_db_title_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("test_title.db");
    let db = hermez_state::SessionDB::open(&db_path).unwrap();

    db.create_session("sess-title", "cli", None, None, None, None, None).unwrap();
    db.set_session_title("sess-title", "My Test Session").unwrap();

    let title = db.get_session_title("sess-title").unwrap();
    assert_eq!(title, Some("My Test Session".to_string()));
}
