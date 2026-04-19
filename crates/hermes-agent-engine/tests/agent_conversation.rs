//! End-to-end integration test for the agent conversation loop.
//!
//! Verifies the full pipeline:
//!   user query → AIAgent → LLM request → mock response → agent output
//!
//! This is a smoke test — it does not exercise tool calling or context
//! compression, but it ensures the core loop wires up correctly.

use std::sync::Arc;

use hermes_agent_engine::agent::{AgentConfig, AIAgent};
use hermes_tools::registry::ToolRegistry;


/// A single-turn conversation with a mocked LLM that returns a plain
/// text response (no tool calls).
#[tokio::test]
async fn test_agent_single_turn_text_response() {
    let mut server = mockito::Server::new_async().await;

    // Mock the LLM endpoint to return a simple assistant message.
    let mock = server
        .mock("POST", mockito::Matcher::Any)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "id": "chatcmpl-e2e-1",
                "object": "chat.completion",
                "created": 1700000000,
                "model": "gpt-4o-mini",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "Hello from the test!"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
            }"#,
        )
        .create();

    let base = server.url();

    let mut config = AgentConfig::default();
    config.model = "openai/gpt-4o-mini".to_string();
    config.base_url = Some(format!("{base}/v1"));
    config.api_key = Some("test-key".to_string());
    config.api_mode = Some("openai".to_string());
    config.max_iterations = 5;
    config.skip_context_files = true;
    config.platform = Some("test".to_string());

    let registry = Arc::new(ToolRegistry::new());
    let mut agent = AIAgent::new(config, registry).unwrap();

    let response = agent.chat("Say hello").await.unwrap();

    mock.assert_async().await;
    assert_eq!(response, "Hello from the test!");
}

/// A two-turn conversation where the mock LLM returns a tool call in the
/// first response and a text message in the second.
#[tokio::test]
async fn test_agent_with_tool_call() {
    let mut server = mockito::Server::new_async().await;

    // First response: assistant requests a tool call.
    let _mock1 = server
        .mock("POST", mockito::Matcher::Any)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "id": "chatcmpl-e2e-2",
                "object": "chat.completion",
                "created": 1700000000,
                "model": "gpt-4o-mini",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "todo",
                                "arguments": "{\"task\":\"test task\"}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
            }"#,
        )
        .expect(1)
        .create();

    // Second response: assistant summarises the tool result.
    let _mock2 = server
        .mock("POST", mockito::Matcher::Any)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "id": "chatcmpl-e2e-3",
                "object": "chat.completion",
                "created": 1700000001,
                "model": "gpt-4o-mini",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "Added todo: test task"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 20, "completion_tokens": 5, "total_tokens": 25}
            }"#,
        )
        .expect(1)
        .create();

    let base = server.url();

    let mut config = AgentConfig::default();
    config.model = "openai/gpt-4o-mini".to_string();
    config.base_url = Some(format!("{base}/v1"));
    config.api_key = Some("test-key".to_string());
    config.api_mode = Some("openai".to_string());
    config.max_iterations = 5;
    config.skip_context_files = true;
    config.platform = Some("test".to_string());

    let mut registry = ToolRegistry::new();
    hermes_tools::register_all_tools(&mut registry);
    let registry = Arc::new(registry);

    let mut agent = AIAgent::new(config, registry).unwrap();

    let response = agent.chat("Add a todo").await.unwrap();
    assert!(response.contains("test task"), "Response should mention the todo task");
}
