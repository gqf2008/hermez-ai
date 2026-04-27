# Architectural Plan — Remaining 3 Items

## 1. MCP OAuth PKCE

### Current State
- `crates/hermez-tools/src/mcp_client/` has no OAuth module
- `rmcp` crate (v0.1.5) does not include OAuth transport
- Python has `tools/mcp_tool.py` with `MCPOAuthManager` + full PKCE flow

### Plan
**New file**: `crates/hermez-tools/src/mcp_client/oauth.rs` (~400 lines)

**Dependencies**: Add `oauth2 = "4.4"` to workspace, with `reqwest` feature for HTTP client

**Components**:
1. `McpOAuthConfig` struct: `client_id`, `client_secret`, `auth_url`, `token_url`, `redirect_port`
2. `McpOAuthManager::new(config)` — creates oauth2 client, generates PKCE challenge
3. `authorize()` — opens browser via `webbrowser` crate, starts local redirect server, waits for callback
4. `exchange_code(code)` — exchanges authorization code for token
5. `refresh_token(refresh_token)` — refreshes access token
6. `get_access_token()` — returns current token, auto-refreshes if expired
7. Disk persistence to `~/.hermez/mcp-oauth/{server_name}.json`

**Integration point**: `McpServerHandle::connect_http/connect_sse` — before connecting, if `config.oauth` is Some, call `manager.get_access_token()` and inject as `Authorization: Bearer` header

**Effort**: 3-4 days

---

## 2. Browser CDP Supervisor

### Current State
- `crates/hermez-tools/src/browser/session.rs` manages browser daemon sessions
- No dialog detection/response mechanism
- Python has `browser_supervisor.SupervisorRegistry` with WebSocket event loop

### Plan
**New file**: `crates/hermez-tools/src/browser/supervisor.rs` (~300 lines)

**Dependencies**: No new deps needed — uses existing `tokio-tungstenite` or raw TCP for CDP

**Components**:
1. `CdpSupervisor` struct: `dialog_policy: DialogPolicy`, `active_sessions: HashMap<String, SupervisorHandle>`
2. `DialogPolicy` enum: `AutoDismiss`, `AutoAccept`, `MustRespond`
3. `start_supervisor(session_name, ws_url)` — spawns tokio task that:
   - Connects to browser CDP WebSocket
   - Listens for `Page.javascriptDialogOpening` events
   - Auto-responds based on policy: `Page.handleJavaScriptDialog { accept: true/false }`
   - Logs dialog text for debugging
4. `stop_supervisor(session_name)` — closes WebSocket, removes handle
5. `SupervisorRegistry` — singleton tracking all active supervisors

**Integration point**: `BrowserSessionManager` — when creating a new browser session, also start supervisor. On session close, stop supervisor.

**Effort**: 2-3 days

---

## 3. MCP Sampling (server→client LLM requests)

### Current State
- `rmcp` crate already supports `sampling/createMessage`:
  - `CreateMessageRequestParam` model type
  - `CreateMessageResult` response type
  - `enable_sampling()` on client capabilities
  - Client handler trait with `create_message()` method

### Plan
**Modified files**:
- `crates/hermez-tools/src/mcp_client/server.rs` — add sampling handler
- `crates/hermez-tools/src/mcp_client/mod.rs` — wire into connection flow

**Components**:
1. `create_sampling_client()` — returns a `rmcp::service::RoleServer` that handles sampling
2. Implement `rmcp::handler::client::ClientHandler` trait for a `SamplingDelegate` struct:
   - `create_message(request, context)` → calls `hermez_agent_engine` to run an LLM call
   - Returns `CreateMessageResult` with model response
3. Enable sampling in client capabilities: `ClientCapabilities::default().enable_sampling()`
4. In `connect_stdio`/`connect_sse`, pass sampling delegate to `rmcp::serve_client()`

**Integration point**: The sampling delegate needs access to `AIAgent` or `LlmRequest`. Simplest approach: store an `AgentConfig` + `ToolRegistry` clone on the delegate, create a one-shot agent per sampling request.

**Effort**: 2-3 days

---

## Execution Order

1. **Browser CDP Supervisor** (highest impact for browser tool reliability)
2. **MCP OAuth PKCE** (enables authenticated MCP servers)
3. **MCP Sampling** (enables MCP servers that need LLM access)

Total combined effort: 7-10 days
