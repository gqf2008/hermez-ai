//! E2E: Agent Conversation Cycle
//!
//! Covers the core agent loop — the most critical path.
//! If these pass, the agent can talk, remember, call tools, and recover.

use super::{mock_agent, mock_agent_with_tools};

// ── 1. Single-turn text response ────────────────────────────────────────────

#[tokio::test]
async fn test_single_turn_plain_text() {
    let mut srv = mockito::Server::new_async().await;
    let _m = srv
        .mock("POST", mockito::Matcher::Any)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(openai_body("Hello from the test!"))
        .create();

    let mut agent = mock_agent(&srv.url());
    let reply = agent.chat("Say hello").await.unwrap();

    assert_eq!(reply, "Hello from the test!");
}

// ── 2. Tool call round-trip ─────────────────────────────────────────────────

#[tokio::test]
async fn test_tool_call_round_trip() {
    let mut srv = mockito::Server::new_async().await;

    // First LLM call: assistant requests a tool.
    let _m1 = srv
        .mock("POST", mockito::Matcher::Any)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(openai_body_with_tools(
            "",
            &[tool_call("call_1", "todo", r#"{"task":"buy milk"}"#)],
            "tool_calls",
        ))
        .expect(1)
        .create();

    // Second LLM call: assistant sees the tool result and replies.
    let _m2 = srv
        .mock("POST", mockito::Matcher::Any)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(openai_body("Added todo: buy milk"))
        .expect(1)
        .create();

    let mut agent = mock_agent_with_tools(&srv.url());
    let reply = agent.chat("Add a todo to buy milk").await.unwrap();

    assert!(reply.contains("buy milk"), "Agent should mention the todo");
}

// ── 3. Multi-turn context retention ─────────────────────────────────────────

#[tokio::test]
async fn test_multi_turn_context_retention() {
    let mut srv = mockito::Server::new_async().await;

    // Turn 1: agent answers a question.
    let _m1 = srv
        .mock("POST", mockito::Matcher::Any)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(openai_body("The capital of France is Paris."))
        .expect(1)
        .create();

    let mut agent = mock_agent(&srv.url());
    let result1 = agent.run_conversation("What is the capital of France?", None, None).await;
    assert!(result1.response.contains("Paris"));

    // Turn 2: agent must remember the previous topic.
    let _m2 = srv
        .mock("POST", mockito::Matcher::Any)
        .match_request(move |req| {
            let body = String::from_utf8(req.body().cloned().unwrap_or_default()).unwrap_or_default();
            body.contains("Paris")
        })
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(openai_body("The Eiffel Tower is in Paris."))
        .expect(1)
        .create();

    let history = result1.messages.clone();
    let result2 = agent.run_conversation("What famous landmark is there?", None, Some(&history)).await;

    assert!(result2.response.contains("Eiffel Tower"));
}

// ── 4. Tool-loop detection ──────────────────────────────────────────────────

#[tokio::test]
async fn test_tool_loop_breaks() {
    let mut srv = mockito::Server::new_async().await;

    // Mock always returns the *same* tool call.
    let _m = srv
        .mock("POST", mockito::Matcher::Any)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(openai_body_with_tools(
            "",
            &[tool_call("call_loop", "todo", r#"{"task":"loop"}"#)],
            "tool_calls",
        ))
        .expect_at_least(5)
        .create();

    let mut agent = mock_agent_with_tools(&srv.url());
    let reply = agent.chat("Trigger a loop").await.unwrap();

    assert!(
        reply.contains("循环") || reply.contains("loop"),
        "Agent should break the loop and warn the user"
    );
}

// ── 5. Truncated response continuation ──────────────────────────────────────

#[tokio::test]
async fn test_truncated_response_continues() {
    let mut srv = mockito::Server::new_async().await;

    // First call: truncated (finish_reason=length).
    let _m1 = srv
        .mock("POST", mockito::Matcher::Any)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(openai_body_truncated("This is partial..."))
        .expect(1)
        .create();

    // Second call: continuation.
    let _m2 = srv
        .mock("POST", mockito::Matcher::Any)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(openai_body("This is the rest of the answer."))
        .expect(1)
        .create();

    let mut agent = mock_agent(&srv.url());
    let reply = agent.chat("Tell me a long story").await.unwrap();

    assert!(
        reply.contains("partial") || reply.contains("rest"),
        "Agent should stitch the truncated parts together"
    );
}

// ── Helpers ─────────────────────────────────────────────────────────────────

pub fn openai_body(content: &str) -> String {
    format!(
        r#"{{
            "id": "chatcmpl-e2e",
            "object": "chat.completion",
            "created": 1700000000,
            "model": "gpt-4o-mini",
            "choices": [{{
                "index": 0,
                "message": {{"role": "assistant", "content": "{}"}},
                "finish_reason": "stop"
            }}],
            "usage": {{"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}}
        }}"#,
        content.replace('"', "\\\"")
    )
}

pub fn openai_body_truncated(content: &str) -> String {
    format!(
        r#"{{
            "id": "chatcmpl-e2e",
            "object": "chat.completion",
            "created": 1700000000,
            "model": "gpt-4o-mini",
            "choices": [{{
                "index": 0,
                "message": {{"role": "assistant", "content": "{}"}},
                "finish_reason": "length"
            }}],
            "usage": {{"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}}
        }}"#,
        content.replace('"', "\\\"")
    )
}

pub fn openai_body_with_tools(
    content: &str,
    tool_calls: &[(String, String, String)],
    finish_reason: &str,
) -> String {
    let tc_json = tool_calls
        .iter()
        .map(|(id, name, args)| {
            format!(
                r#"{{"id":"{}","type":"function","function":{{"name":"{}","arguments":"{}"}}}}"#,
                id, name, args.replace('"', "\\\"")
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"{{
            "id": "chatcmpl-e2e",
            "object": "chat.completion",
            "created": 1700000000,
            "model": "gpt-4o-mini",
            "choices": [{{
                "index": 0,
                "message": {{
                    "role": "assistant",
                    "content": "{}",
                    "tool_calls": [{}]
                }},
                "finish_reason": "{}"
            }}],
            "usage": {{"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}}
        }}"#,
        content.replace('"', "\\\"")
        .replace("\n", "\\n"),
        tc_json,
        finish_reason
    )
}

pub fn tool_call(id: &str, name: &str, args: &str) -> (String, String, String) {
    (id.to_string(), name.to_string(), args.to_string())
}
