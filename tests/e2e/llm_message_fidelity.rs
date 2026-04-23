//! E2E: LLM Message Fidelity
//!
//! Ensures messages are not corrupted, dropped, or reordered on the wire.
//! This module directly exercises the bug that caused "agent amnesia" —
//! assistant tool_calls disappearing during the OpenAI message build.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{mock_agent, mock_agent_with_tools};

// ── 1. Assistant tool_calls survive message building ────────────────────────

#[tokio::test]
async fn test_assistant_tool_calls_preserved_in_request() {
    let mut srv = mockito::Server::new_async().await;

    let request_count = Arc::new(AtomicUsize::new(0));
    let rc = request_count.clone();

    // Mock that inspects the request body for tool_calls.
    let _m = srv
        .mock("POST", mockito::Matcher::Any)
        .match_request(move |req| {
            let body = String::from_utf8(req.body().cloned().unwrap_or_default()).unwrap_or_default();
            let n = rc.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                // First call: assistant requests a tool.
                // We don't inspect this one deeply; just return tool call response.
                true
            } else {
                // Second call: the API request MUST contain the previous
                // assistant message with tool_calls, otherwise the API rejects
                // the tool result messages.
                body.contains("\"tool_calls\"")
            }
        })
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body_from_request(|req| {
            let body = String::from_utf8(req.body().cloned().unwrap_or_default()).unwrap_or_default();
            if body.contains("\"tool_calls\"") {
                // Second call — assistant sees tool results and replies.
                super::agent_conversation_cycle::openai_body("Done!").into_bytes()
            } else {
                // First call — assistant requests tool.
                super::agent_conversation_cycle::openai_body_with_tools(
                    "",
                    &[super::agent_conversation_cycle::tool_call(
                        "tc_1", "todo", r#"{"task":"x"}"#,
                    )],
                    "tool_calls",
                ).into_bytes()
            }
        })
        .expect_at_least(2)
        .create();

    let mut agent = mock_agent_with_tools(&srv.url());
    let reply = agent.chat("Add todo x").await.unwrap();
    assert_eq!(reply, "Done!");
}

// ── 2. Tool result paired with correct tool_call_id ─────────────────────────

#[tokio::test]
async fn test_tool_message_includes_call_id() {
    let mut srv = mockito::Server::new_async().await;

    let _m = srv
        .mock("POST", mockito::Matcher::Any)
        .match_request(|req| {
            let body = String::from_utf8(req.body().cloned().unwrap_or_default()).unwrap_or_default();
            // After the tool is executed, the follow-up request must contain
            // a "tool" role message with the matching tool_call_id.
            body.contains("\"role\":\"tool\"") && body.contains("\"tool_call_id\":\"tc_match\"")
        })
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(super::agent_conversation_cycle::openai_body("Got it."))
        .expect_at_least(1)
        .create();

    // Seed first response so agent issues a tool call.
    let _m2 = srv
        .mock("POST", mockito::Matcher::Any)
        .match_request(|req| {
            let body = String::from_utf8(req.body().cloned().unwrap_or_default()).unwrap_or_default();
            !body.contains("\"role\":\"tool\"")
        })
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(super::agent_conversation_cycle::openai_body_with_tools(
            "",
            &[super::agent_conversation_cycle::tool_call(
                "tc_match", "todo", r#"{"task":"y"}"#,
            )],
            "tool_calls",
        ))
        .expect(1)
        .create();

    let mut agent = mock_agent_with_tools(&srv.url());
    let reply = agent.chat("Add todo y").await.unwrap();
    assert_eq!(reply, "Got it.");
}

// ── 3. User message with URL sent as plain text ─────────────────────────────

#[tokio::test]
async fn test_url_in_user_message_sent_as_text() {
    let mut srv = mockito::Server::new_async().await;

    let _m = srv
        .mock("POST", mockito::Matcher::Any)
        .match_request(|req| {
            let body = String::from_utf8(req.body().cloned().unwrap_or_default()).unwrap_or_default();
            // The agent sends the URL as regular text content, not as an
            // image_url content part (that requires explicit multi-modal API).
            body.contains("https://example.com/img.png")
                && !body.contains("\"type\":\"image_url\"")
        })
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(super::agent_conversation_cycle::openai_body("I see a cat."))
        .expect(1)
        .create();

    let mut agent = mock_agent(&srv.url());
    let reply = agent.chat("Describe this image: https://example.com/img.png").await.unwrap();
    assert_eq!(reply, "I see a cat.");
}

// ── 4. System prompt not dropped ────────────────────────────────────────────

#[tokio::test]
async fn test_system_prompt_present_in_every_request() {
    let mut srv = mockito::Server::new_async().await;

    let _m = srv
        .mock("POST", mockito::Matcher::Any)
        .match_request(|req| {
            let body = String::from_utf8(req.body().cloned().unwrap_or_default()).unwrap_or_default();
            // The agent config includes a system prompt; every request should
            // start with a system role message.
            body.contains("\"role\":\"system\"")
        })
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(super::agent_conversation_cycle::openai_body("Ack."))
        .expect_at_least(1)
        .create();

    let mut agent = mock_agent(&srv.url());
    let _ = agent.chat("Hi").await.unwrap();
}

// ── 5. Message ordering preserved across turns ──────────────────────────────

#[tokio::test]
async fn test_message_order_preserved() {
    let mut srv = mockito::Server::new_async().await;

    let _m = srv
        .mock("POST", mockito::Matcher::Any)
        .match_request(|req| {
            let body = String::from_utf8(req.body().cloned().unwrap_or_default()).unwrap_or_default();
            // In a two-turn conversation the user messages must appear in order.
            let first = body.find("First message");
            let second = body.find("Second message");
            match (first, second) {
                (Some(a), Some(b)) => a < b,
                _ => false,
            }
        })
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(super::agent_conversation_cycle::openai_body("Roger."))
        .expect_at_least(1)
        .create();

    let mut agent = mock_agent(&srv.url());
    let _ = agent.chat("First message").await.unwrap();
    let _ = agent.chat("Second message").await.unwrap();
}
