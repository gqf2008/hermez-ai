//! ACP Registry — Service discovery and registration for multi-agent collaboration.
//!
//! Provides an in-memory registry where ACP servers (agents) can:
//! - `register` — announce their capabilities and endpoint
//! - `discover` — find agents by capability or name
//! - `heartbeat` — keep their registration alive
//! - `deregister` — remove themselves on shutdown
//!
//! The registry can run standalone (`hermes-acp --registry`) or be embedded.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A registered agent in the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredAgent {
    /// Unique agent ID (UUID).
    pub agent_id: String,
    /// Human-readable name.
    pub name: String,
    /// Agent version.
    pub version: String,
    /// Capabilities this agent provides.
    pub capabilities: Vec<String>,
    /// Transport endpoint: "stdio", "tcp://host:port", or "unix:/path".
    pub endpoint: String,
    /// Last heartbeat timestamp (milliseconds since epoch).
    pub last_heartbeat: u64,
    /// Registration timestamp.
    pub registered_at: u64,
    /// Extra metadata.
    pub metadata: HashMap<String, serde_json::Value>,
}

/// Request to register an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub name: String,
    pub version: String,
    pub capabilities: Vec<String>,
    pub endpoint: String,
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

/// Request to update heartbeat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    pub agent_id: String,
}

/// Request to deregister an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeregisterRequest {
    pub agent_id: String,
}

/// Request to discover agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoverRequest {
    /// Filter by capability (optional).
    #[serde(default)]
    pub capability: Option<String>,
    /// Filter by name substring (optional).
    #[serde(default)]
    pub name: Option<String>,
}

/// Response from a discover query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoverResponse {
    pub agents: Vec<RegisteredAgent>,
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// In-memory agent registry with automatic expiry.
pub struct AgentRegistry {
    agents: RwLock<HashMap<String, RegisteredAgent>>,
    /// How long before an agent is considered stale (no heartbeat).
    heartbeat_timeout: Duration,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            agents: RwLock::new(HashMap::new()),
            heartbeat_timeout: Duration::from_secs(60),
        }
    }

    pub fn with_timeout(heartbeat_timeout: Duration) -> Self {
        Self {
            agents: RwLock::new(HashMap::new()),
            heartbeat_timeout,
        }
    }

    /// Register a new agent. Returns the assigned agent_id.
    pub async fn register(&self, req: RegisterRequest) -> String {
        let agent_id = uuid::Uuid::new_v4().to_string();
        let now = Instant::now().elapsed().as_secs();
        // Use chrono for real timestamp
        let now = chrono::Utc::now().timestamp_millis() as u64;

        let agent = RegisteredAgent {
            agent_id: agent_id.clone(),
            name: req.name,
            version: req.version,
            capabilities: req.capabilities,
            endpoint: req.endpoint,
            last_heartbeat: now,
            registered_at: now,
            metadata: req.metadata,
        };

        let mut agents = self.agents.write().await;
        agents.insert(agent_id.clone(), agent);
        tracing::info!("Registered agent {} ({})", agent_id, agents.len());
        agent_id
    }

    /// Update heartbeat for an agent.
    pub async fn heartbeat(&self, agent_id: &str) -> bool {
        let mut agents = self.agents.write().await;
        if let Some(agent) = agents.get_mut(agent_id) {
            agent.last_heartbeat = chrono::Utc::now().timestamp() as u64;
            true
        } else {
            false
        }
    }

    /// Deregister an agent.
    pub async fn deregister(&self, agent_id: &str) -> bool {
        let mut agents = self.agents.write().await;
        let removed = agents.remove(agent_id).is_some();
        if removed {
            tracing::info!("Deregistered agent {}", agent_id);
        }
        removed
    }

    /// Discover agents matching filters.
    pub async fn discover(&self, req: &DiscoverRequest) -> DiscoverResponse {
        let agents = self.agents.read().await;
        let now = chrono::Utc::now().timestamp_millis() as u64;
        let timeout_millis = self.heartbeat_timeout.as_millis() as u64;

        let mut results = Vec::new();
        for agent in agents.values() {
            // Skip stale agents
            if now - agent.last_heartbeat > timeout_millis {
                continue;
            }

            // Filter by capability
            if let Some(ref cap) = req.capability {
                if !agent.capabilities.iter().any(|c| c.eq_ignore_ascii_case(cap)) {
                    continue;
                }
            }

            // Filter by name
            if let Some(ref name) = req.name {
                if !agent.name.to_lowercase().contains(&name.to_lowercase()) {
                    continue;
                }
            }

            results.push(agent.clone());
        }

        DiscoverResponse { agents: results }
    }

    /// Get a single agent by ID.
    pub async fn get(&self, agent_id: &str) -> Option<RegisteredAgent> {
        let agents = self.agents.read().await;
        agents.get(agent_id).cloned()
    }

    /// List all non-stale agents.
    pub async fn list_all(&self) -> Vec<RegisteredAgent> {
        self.discover(&DiscoverRequest {
            capability: None,
            name: None,
        })
        .await
        .agents
    }

    /// Remove stale agents (no heartbeat within timeout).
    pub async fn purge_stale(&self) -> usize {
        let mut agents = self.agents.write().await;
        let now = chrono::Utc::now().timestamp_millis() as u64;
        let timeout_millis = self.heartbeat_timeout.as_millis() as u64;
        let before = agents.len();
        agents.retain(|_id, agent| now - agent.last_heartbeat <= timeout_millis);
        let removed = before - agents.len();
        if removed > 0 {
            tracing::info!("Purged {} stale agents", removed);
        }
        removed
    }

    /// Number of registered agents (including stale).
    pub async fn count(&self) -> usize {
        self.agents.read().await.len()
    }
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Registry server (JSON-RPC interface)
// ---------------------------------------------------------------------------

/// JSON-RPC handler for the registry.
pub struct RegistryServer {
    registry: Arc<AgentRegistry>,
}

impl RegistryServer {
    pub fn new(registry: Arc<AgentRegistry>) -> Self {
        Self { registry }
    }

    pub async fn dispatch(
        &self,
        method: &str,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        match method {
            "registry/register" => {
                let req: RegisterRequest =
                    serde_json::from_value(serde_json::Value::Object(params.clone()))
                        .map_err(|e| e.to_string())?;
                let agent_id = self.registry.register(req).await;
                Ok(serde_json::json!({ "agent_id": agent_id }))
            }
            "registry/heartbeat" => {
                let req: HeartbeatRequest =
                    serde_json::from_value(serde_json::Value::Object(params.clone()))
                        .map_err(|e| e.to_string())?;
                let ok = self.registry.heartbeat(&req.agent_id).await;
                Ok(serde_json::json!({ "ok": ok }))
            }
            "registry/deregister" => {
                let req: DeregisterRequest =
                    serde_json::from_value(serde_json::Value::Object(params.clone()))
                        .map_err(|e| e.to_string())?;
                let ok = self.registry.deregister(&req.agent_id).await;
                Ok(serde_json::json!({ "ok": ok }))
            }
            "registry/discover" => {
                let req: DiscoverRequest =
                    serde_json::from_value(serde_json::Value::Object(params.clone()))
                        .map_err(|e| e.to_string())?;
                let resp = self.registry.discover(&req).await;
                serde_json::to_value(resp).map_err(|e| e.to_string())
            }
            "registry/list" => {
                let agents = self.registry.list_all().await;
                serde_json::to_value(agents).map_err(|e| e.to_string())
            }
            "registry/get" => {
                let agent_id = params
                    .get("agent_id")
                    .and_then(|v| v.as_str())
                    .ok_or("missing agent_id")?;
                match self.registry.get(agent_id).await {
                    Some(agent) => serde_json::to_value(agent).map_err(|e| e.to_string()),
                    None => Ok(serde_json::Value::Null),
                }
            }
            other => Err(format!("Unknown registry method: {other}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Registry client (used by agents to register themselves)
// ---------------------------------------------------------------------------

/// Client for talking to a remote registry.
pub struct RegistryClient {
    endpoint: String,
}

impl RegistryClient {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
        }
    }

    /// Register this agent with the registry.
    pub async fn register(&self, req: RegisterRequest) -> Result<String, String> {
        // For MVP, use HTTP POST to the registry endpoint
        let client = reqwest::Client::new();
        let url = format!("{}/register", self.endpoint);
        let resp = client
            .post(&url)
            .json(&req)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        body.get("agent_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| "missing agent_id in response".to_string())
    }

    /// Send a heartbeat.
    pub async fn heartbeat(&self, agent_id: &str) -> Result<bool, String> {
        let client = reqwest::Client::new();
        let url = format!("{}/heartbeat", self.endpoint);
        let resp = client
            .post(&url)
            .json(&serde_json::json!({ "agent_id": agent_id }))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        Ok(body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false))
    }

    /// Deregister.
    pub async fn deregister(&self, agent_id: &str) -> Result<bool, String> {
        let client = reqwest::Client::new();
        let url = format!("{}/deregister", self.endpoint);
        let resp = client
            .post(&url)
            .json(&serde_json::json!({ "agent_id": agent_id }))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        Ok(body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false))
    }

    /// Discover agents.
    pub async fn discover(&self, req: &DiscoverRequest) -> Result<DiscoverResponse, String> {
        let client = reqwest::Client::new();
        let url = format!("{}/discover", self.endpoint);
        let resp = client
            .post(&url)
            .json(req)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        resp.json().await.map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_registry_register_and_get() {
        let reg = AgentRegistry::new();
        let req = RegisterRequest {
            name: "test-agent".into(),
            version: "1.0.0".into(),
            capabilities: vec!["memory".into(), "search".into()],
            endpoint: "stdio".into(),
            metadata: HashMap::new(),
        };
        let id = reg.register(req).await;
        assert!(!id.is_empty());

        let agent = reg.get(&id).await.unwrap();
        assert_eq!(agent.name, "test-agent");
        assert_eq!(agent.capabilities.len(), 2);
    }

    #[tokio::test]
    async fn test_registry_heartbeat() {
        let reg = AgentRegistry::new();
        let req = RegisterRequest {
            name: "test-agent".into(),
            version: "1.0.0".into(),
            capabilities: vec![],
            endpoint: "stdio".into(),
            metadata: HashMap::new(),
        };
        let id = reg.register(req).await;
        assert!(reg.heartbeat(&id).await);
        assert!(!reg.heartbeat("nonexistent").await);
    }

    #[tokio::test]
    async fn test_registry_deregister() {
        let reg = AgentRegistry::new();
        let req = RegisterRequest {
            name: "test-agent".into(),
            version: "1.0.0".into(),
            capabilities: vec![],
            endpoint: "stdio".into(),
            metadata: HashMap::new(),
        };
        let id = reg.register(req).await;
        assert!(reg.deregister(&id).await);
        assert!(!reg.deregister(&id).await);
    }

    #[tokio::test]
    async fn test_registry_discover_by_capability() {
        let reg = AgentRegistry::new();

        let req1 = RegisterRequest {
            name: "memory-agent".into(),
            version: "1.0.0".into(),
            capabilities: vec!["memory".into()],
            endpoint: "stdio".into(),
            metadata: HashMap::new(),
        };
        reg.register(req1).await;

        let req2 = RegisterRequest {
            name: "search-agent".into(),
            version: "1.0.0".into(),
            capabilities: vec!["search".into()],
            endpoint: "tcp://localhost:8080".into(),
            metadata: HashMap::new(),
        };
        reg.register(req2).await;

        let results = reg
            .discover(&DiscoverRequest {
                capability: Some("memory".into()),
                name: None,
            })
            .await;
        assert_eq!(results.agents.len(), 1);
        assert_eq!(results.agents[0].name, "memory-agent");
    }

    #[tokio::test]
    async fn test_registry_discover_by_name() {
        let reg = AgentRegistry::new();

        let req = RegisterRequest {
            name: "my-special-agent".into(),
            version: "1.0.0".into(),
            capabilities: vec![],
            endpoint: "stdio".into(),
            metadata: HashMap::new(),
        };
        reg.register(req).await;

        let results = reg
            .discover(&DiscoverRequest {
                capability: None,
                name: Some("special".into()),
            })
            .await;
        assert_eq!(results.agents.len(), 1);
    }

    #[tokio::test]
    async fn test_registry_server_dispatch() {
        let reg = Arc::new(AgentRegistry::new());
        let server = RegistryServer::new(reg);

        let params = serde_json::json!({
            "name": "server-test",
            "version": "1.0.0",
            "capabilities": ["test"],
            "endpoint": "stdio",
        });
        let result = server.dispatch("registry/register", params.as_object().unwrap()).await;
        assert!(result.is_ok());
        let body = result.unwrap();
        assert!(body.get("agent_id").is_some());
    }

    #[tokio::test]
    async fn test_registry_purge_stale() {
        let reg = AgentRegistry::with_timeout(Duration::from_secs(0));
        let req = RegisterRequest {
            name: "stale-agent".into(),
            version: "1.0.0".into(),
            capabilities: vec![],
            endpoint: "stdio".into(),
            metadata: HashMap::new(),
        };
        reg.register(req).await;
        // Wait a tiny bit so the agent becomes stale
        tokio::time::sleep(Duration::from_millis(100)).await;
        let removed = reg.purge_stale().await;
        assert_eq!(removed, 1);
        assert_eq!(reg.count().await, 0);
    }
}
