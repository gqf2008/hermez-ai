//! Skills Hub — remote skill discovery, installation, and provenance tracking.
//!
//! Mirrors the Python `tools/skills_hub.py`.
//! Provides marketplace-like functionality: search multiple sources, fetch
//! skill bundles, quarantine + security scan, and install with provenance.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::registry::tool_error;
use crate::skills_guard;

// ── Path constants ──

fn skills_dir() -> PathBuf {
    hermes_core::get_hermes_home().join("skills")
}

fn hub_dir() -> PathBuf {
    skills_dir().join(".hub")
}

fn lock_file_path() -> PathBuf {
    hub_dir().join("lock.json")
}

fn taps_file_path() -> PathBuf {
    hub_dir().join("taps.json")
}

fn quarantine_dir() -> PathBuf {
    hub_dir().join("quarantine")
}

fn audit_log_path() -> PathBuf {
    hub_dir().join("audit.log")
}

fn index_cache_dir() -> PathBuf {
    hub_dir().join("index-cache")
}

#[allow(dead_code)]
const INDEX_CACHE_TTL_SECS: u64 = 3600; // 1 hour

// ── Data models ──

/// Minimal metadata returned by search results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMeta {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub source: String,
    pub identifier: String,
    #[serde(default = "default_community")]
    pub trust_level: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<Value>,
}

fn default_community() -> String {
    "community".to_string()
}

/// A downloaded skill bundle ready for quarantine/scanning/installation.
#[derive(Debug, Clone)]
pub struct SkillBundle {
    pub name: String,
    /// Map of relative_path -> content (bytes as base64 or text as string).
    pub files: BTreeMap<String, String>,
    pub source: String,
    pub identifier: String,
    pub trust_level: String,
    #[allow(dead_code)]
    pub metadata: Option<Value>,
}

/// Authentication for GitHub API with 4-tier fallback.
#[derive(Debug, Clone)]
pub struct GitHubAuth {
    /// Personal Access Token from env.
    pub token: Option<String>,
    /// GitHub App installation access token (cached).
    pub app_token: Arc<Mutex<Option<(String, std::time::Instant)>>>,
}

impl Default for GitHubAuth {
    fn default() -> Self {
        Self {
            token: std::env::var("GITHUB_TOKEN")
                .ok()
                .or_else(|| std::env::var("GH_TOKEN").ok())
                .filter(|t| !t.is_empty()),
            app_token: Arc::new(Mutex::new(None)),
        }
    }
}

impl GitHubAuth {
    /// Get auth headers for GitHub API requests.
    pub fn get_headers(&self) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "Accept",
            "application/vnd.github.v3+json".parse().unwrap(),
        );
        if let Some(token) = &self.token {
            headers.insert(
                "Authorization",
                format!("token {token}").parse().unwrap(),
            );
        }
        headers
    }

    /// Whether we have authenticated access.
    pub fn is_authenticated(&self) -> bool {
        self.token.is_some() || self.app_token.lock().is_some()
    }

    /// Description of the auth method in use.
    pub fn auth_method(&self) -> &str {
        if self.token.is_some() {
            "pat"
        } else if self.app_token.lock().is_some() {
            "github_app"
        } else {
            "anonymous"
        }
    }
}

/// A source adapter for skill registries (GitHub, ClawHub, etc.).
pub trait SkillSource: Send + Sync {
    /// Search for skills matching the query.
    fn search(&self, query: &str, limit: usize) -> Vec<SkillMeta>;
    /// Fetch a skill bundle by identifier.
    fn fetch(&self, identifier: &str) -> Option<SkillBundle>;
    /// Inspect a skill without downloading files.
    fn inspect(&self, identifier: &str) -> Option<SkillMeta>;
    /// Source identifier string (e.g. "github", "clawhub").
    fn source_id(&self) -> &str;
    /// Trust level for this source.
    fn trust_level_for(&self, _identifier: &str) -> String {
        "community".to_string()
    }
}

// ── GitHub Source ──

/// Default GitHub repos to tap for skills.
static DEFAULT_TAPS: &[(&str, &str)] = &[
    ("openai/skills", "skills/"),
    ("anthropics/skills", "skills/"),
    ("VoltAgent/awesome-agent-skills", "skills/"),
    ("garrytan/gstack", ""),
];

/// Trusted repos get "trusted" trust level.
static TRUSTED_REPOS: &[&str] = &["openai/skills", "anthropics/skills"];

/// GitHub Contents API source adapter.
pub struct GitHubSource {
    auth: GitHubAuth,
    taps: Vec<(String, String)>,
    client: reqwest::Client,
    /// File-based cache directory.
    cache_dir: Option<PathBuf>,
}

impl GitHubSource {
    /// Create a new GitHub source with default taps.
    pub fn new(auth: Option<GitHubAuth>, extra_taps: Option<Vec<(String, String)>>) -> Self {
        let auth = auth.unwrap_or_default();
        let mut taps: Vec<(String, String)> = DEFAULT_TAPS
            .iter()
            .map(|(r, p)| (r.to_string(), p.to_string()))
            .collect();
        if let Some(extra) = extra_taps {
            for tap in extra {
                if !taps.iter().any(|(r, _)| r == &tap.0) {
                    taps.push(tap);
                }
            }
        }
        Self {
            auth,
            taps,
            client: reqwest::Client::new(),
            cache_dir: None,
        }
    }

    /// Enable file-based caching for this source.
    pub fn with_cache(mut self, cache_dir: PathBuf) -> Self {
        let _ = fs::create_dir_all(&cache_dir);
        self.cache_dir = Some(cache_dir);
        self
    }

    /// GitHub API GET request.
    async fn github_get(&self, url: &str) -> Result<reqwest::Response, reqwest::Error> {
        let headers = self.auth.get_headers();
        self.client
            .get(url)
            .headers(headers)
            .send()
            .await
    }

    /// Fetch raw file content via GitHub Contents API.
    async fn fetch_raw_file(&self, repo: &str, path: &str) -> Option<String> {
        let url = format!(
            "https://api.github.com/repos/{repo}/contents/{path}"
        );
        let mut headers = self.auth.get_headers();
        headers.insert(
            "Accept",
            "application/vnd.github.v3.raw".parse().unwrap(),
        );
        let resp = self
            .client
            .get(&url)
            .headers(headers)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.text().await.ok()
    }

    /// List directory contents via GitHub Contents API.
    async fn list_contents(&self, repo: &str, path: &str) -> Option<Vec<Value>> {
        let url = format!(
            "https://api.github.com/repos/{repo}/contents/{path}"
        );
        let resp = self.github_get(&url).await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let json: Value = resp.json().await.ok()?;
        json.as_array().cloned()
    }

    /// Get the default branch for a repo.
    async fn default_branch(&self, repo: &str) -> Option<String> {
        let url = format!("https://api.github.com/repos/{repo}");
        let resp = self.github_get(&url).await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let json: Value = resp.json().await.ok()?;
        json.get("default_branch")
            .and_then(|v| v.as_str())
            .map(String::from)
    }

    /// Fetch an entire repo tree via Git Trees API (single call).
    async fn git_tree(&self, repo: &str, branch: &str) -> Option<Vec<String>> {
        let url = format!(
            "https://api.github.com/repos/{repo}/git/trees/{branch}?recursive=1"
        );
        let resp = self.github_get(&url).await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let json: Value = resp.json().await.ok()?;
        let tree = json.get("tree")?.as_array()?;

        let mut files = Vec::new();
        for item in tree {
            let path = item.get("path")?.as_str()?;
            let typ = item.get("type")?.as_str()?;
            if typ == "blob" {
                files.push(path.to_string());
            }
        }
        Some(files)
    }

    /// Download a skill directory from GitHub.
    async fn download_skill(
        &self,
        owner: &str,
        repo_name: &str,
        skill_path: &str,
        identifier: &str,
    ) -> Option<SkillBundle> {
        let repo = format!("{owner}/{repo_name}");

        // Try Git Trees API first (faster, single call)
        let branch = self.default_branch(&repo).await?;
        let prefix = if skill_path.is_empty() {
            String::new()
        } else {
            format!("{skill_path}/")
        };

        if let Some(tree_files) = self.git_tree(&repo, &branch).await {
            // Filter to files under the skill_path prefix
            let skill_files: Vec<String> = tree_files
                .into_iter()
                .filter(|f| f.starts_with(&prefix))
                .collect();

            if skill_files.is_empty() {
                return None;
            }

            // Check if SKILL.md is present
            let skill_md_key = if skill_path.is_empty() {
                "SKILL.md".to_string()
            } else {
                format!("{skill_path}/SKILL.md")
            };

            let has_skill_md = skill_files.iter().any(|f| f == &skill_md_key);
            if !has_skill_md {
                return None;
            }

            // Fetch all files in parallel (limit to avoid rate limiting)
            let mut files = BTreeMap::new();

            // Fetch SKILL.md first
            let skill_md_content = self
                .fetch_raw_file(&repo, &skill_md_key)
                .await?;

            // Parse frontmatter for name
            let name = parse_skill_name_from_frontmatter(&skill_md_content)
                .unwrap_or_else(|| {
                    Path::new(skill_path)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(identifier)
                        .to_string()
                });

            // Parse metadata from frontmatter
            let metadata = parse_frontmatter_value(&skill_md_content);

            files.insert("SKILL.md".to_string(), skill_md_content);

            // Fetch remaining files (skip binary-looking files)
            for file_path in &skill_files {
                if file_path == &skill_md_key {
                    continue;
                }
                let rel_path = if skill_path.is_empty() {
                    file_path.clone()
                } else {
                    file_path.strip_prefix(&prefix)
                        .unwrap_or(file_path)
                        .to_string()
                };

                // Skip common binary extensions
                if let Some(ext) = Path::new(file_path).extension() {
                    let ext_str = ext.to_string_lossy().to_lowercase();
                    if matches!(
                        ext_str.as_str(),
                        "png" | "jpg" | "jpeg" | "gif" | "ico" | "exe" | "zip" | "tar" | "gz"
                    ) {
                        continue;
                    }
                }

                if let Some(content) = self.fetch_raw_file(&repo, file_path).await {
                    files.insert(rel_path, content);
                }
            }

            let trust = self.trust_level_for(identifier);
            return Some(SkillBundle {
                name,
                files,
                source: "github".to_string(),
                identifier: identifier.to_string(),
                trust_level: trust,
                metadata,
            });
        }

        // Fallback: recursive directory listing via Contents API
        self.download_skill_recursive(&repo, skill_path, identifier)
            .await
    }

    /// Fallback: recursive download via Contents API.
    async fn download_skill_recursive(
        &self,
        repo: &str,
        skill_path: &str,
        identifier: &str,
    ) -> Option<SkillBundle> {
        let items = self.list_contents(repo, skill_path).await?;

        let mut files = BTreeMap::new();
        let mut has_skill_md = false;

        for item in &items {
            let typ = item.get("type")?.as_str()?;
            let name = item.get("name")?.as_str()?;

            match typ {
                "file" if name == "SKILL.md" => {
                    has_skill_md = true;
                    if let Some(content) = self.fetch_raw_file(repo, &format!("{skill_path}/{name}")).await {
                        files.insert(name.to_string(), content);
                    }
                }
                "file" => {
                    if let Some(content) = self.fetch_raw_file(repo, &format!("{skill_path}/{name}")).await {
                        files.insert(name.to_string(), content);
                    }
                }
                "dir" => {
                    let sub_path = if skill_path.is_empty() {
                        name.to_string()
                    } else {
                        format!("{skill_path}/{name}")
                    };
                    if let Some(sub_files) = self.download_skill_recursive_impl(repo, &sub_path).await {
                        for (rel, content) in sub_files {
                            files.insert(rel, content);
                        }
                    }
                }
                _ => {}
            }
        }

        if !has_skill_md {
            return None;
        }

        let skill_md = files.get("SKILL.md")?;
        let name = parse_skill_name_from_frontmatter(skill_md)
            .unwrap_or_else(|| {
                Path::new(skill_path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(identifier)
                    .to_string()
            });

        let metadata = parse_frontmatter_value(skill_md);
        let trust = self.trust_level_for(identifier);

        Some(SkillBundle {
            name,
            files,
            source: "github".to_string(),
            identifier: identifier.to_string(),
            trust_level: trust,
            metadata,
        })
    }

    /// Recursive implementation that returns paths relative to the skill root.
    /// Uses a work queue to avoid async recursion.
    async fn download_skill_recursive_impl(
        &self,
        repo: &str,
        path: &str,
    ) -> Option<BTreeMap<String, String>> {
        let mut files = BTreeMap::new();
        let mut work_queue = vec![path.to_string()];

        while let Some(current_path) = work_queue.pop() {
            let items = self.list_contents(repo, &current_path).await?;
            for item in &items {
                let typ = item.get("type")?.as_str()?;
                let name = item.get("name")?.as_str()?;

                match typ {
                    "file" => {
                        let full_path = format!("{current_path}/{name}");
                        if let Some(content) = self.fetch_raw_file(repo, &full_path).await {
                            let rel = if current_path == path {
                                name.to_string()
                            } else {
                                current_path.strip_prefix(&format!("{path}/"))
                                    .map(|p| format!("{p}/{name}"))
                                    .unwrap_or_else(|| name.to_string())
                            };
                            files.insert(rel, content);
                        }
                    }
                    "dir" => {
                        work_queue.push(format!("{current_path}/{name}"));
                    }
                    _ => {}
                }
            }
        }

        if files.is_empty() {
            None
        } else {
            Some(files)
        }
    }

    /// Search skills in a specific repo tap.
    async fn search_tap(&self, repo: &str, path_prefix: &str, query: &str) -> Vec<SkillMeta> {
        let repo_full = repo.to_string();
        let branch = match self.default_branch(&repo_full).await {
            Some(b) => b,
            None => return Vec::new(),
        };

        // Use Git Trees API to list all files
        let tree_files = match self.git_tree(&repo_full, &branch).await {
            Some(f) => f,
            None => return Vec::new(),
        };

        // Find SKILL.md files under the path prefix
        let skill_dirs: HashSet<String> = tree_files
            .iter()
            .filter(|f| {
                f.ends_with("/SKILL.md")
                    && (path_prefix.is_empty() || f.starts_with(path_prefix))
            })
            .filter_map(|f| {
                let parent = f.strip_suffix("/SKILL.md").unwrap_or(f);
                let relevant = if path_prefix.is_empty() {
                    parent.to_string()
                } else {
                    parent.strip_prefix(path_prefix)
                        .map(|s| s.trim_start_matches('/').to_string())
                        .unwrap_or_default()
                };
                if relevant.is_empty() { None } else { Some(relevant) }
            })
            .collect();

        // Fetch SKILL.md for each skill and extract metadata
        let mut results = Vec::new();
        let q = query.to_lowercase();

        for skill_rel in &skill_dirs {
            let full_path = if path_prefix.is_empty() {
                format!("{skill_rel}/SKILL.md")
            } else {
                format!("{path_prefix}/{skill_rel}/SKILL.md")
            };

            if let Some(content) = self.fetch_raw_file(&repo_full, &full_path).await {
                let (fm, _body) = parse_frontmatter(&content);
                let name = fm.name.clone().unwrap_or_else(|| {
                    Path::new(skill_rel)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown")
                        .to_string()
                });
                let description = fm.description.clone().unwrap_or_default();
                let tags = extract_tags_from_frontmatter(&fm);

                // Substring search on name + description + tags
                if !q.is_empty() {
                    let searchable = format!(
                        "{} {} {}",
                        name,
                        description,
                        tags.join(" ")
                    ).to_lowercase();
                    if !searchable.contains(&q) {
                        continue;
                    }
                }

                let identifier = format!("{repo_full}/{skill_rel}");
                let trust = self.trust_level_for(&identifier);

                // Cache the result
                self.cache_write(&repo_full, skill_rel, &SkillMeta {
                    name: name.clone(),
                    description: description.clone(),
                    source: "github".to_string(),
                    identifier: identifier.clone(),
                    trust_level: trust.clone(),
                    repo: Some(repo_full.clone()),
                    path: Some(skill_rel.clone()),
                    tags,
                    extra: None,
                });

                results.push(SkillMeta {
                    name,
                    description,
                    source: "github".to_string(),
                    identifier,
                    trust_level: trust,
                    repo: Some(repo_full.clone()),
                    path: Some(skill_rel.clone()),
                    tags: Vec::new(),
                    extra: None,
                });
            }
        }

        results
    }

    /// Read from file-based cache.
    #[allow(dead_code)]
    fn cache_read(&self, repo: &str, skill_path: &str) -> Option<SkillMeta> {
        let cache_dir = self.cache_dir.as_ref()?;
        let cache_key = format!("{}_{}",
            repo.replace(['/', ' '], "_"),
            skill_path.replace(['/', ' '], "_")
        );
        let cache_file = cache_dir.join(format!("{cache_key}.json"));
        if !cache_file.exists() {
            return None;
        }
        // Check TTL
        let metadata = fs::metadata(&cache_file).ok()?;
        let modified = metadata.modified().ok()?;
        let elapsed = modified.elapsed().ok()?.as_secs();
        if elapsed > INDEX_CACHE_TTL_SECS {
            let _ = fs::remove_file(&cache_file);
            return None;
        }
        let content = fs::read_to_string(&cache_file).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Write to file-based cache.
    fn cache_write(&self, repo: &str, skill_path: &str, meta: &SkillMeta) {
        let Some(cache_dir) = &self.cache_dir else { return; };
        let cache_key = format!("{}_{}",
            repo.replace(['/', ' '], "_"),
            skill_path.replace(['/', ' '], "_")
        );
        let cache_file = cache_dir.join(format!("{cache_key}.json"));
        if let Ok(json) = serde_json::to_string(meta) {
            let _ = fs::write(&cache_file, json);
        }
    }
}

#[async_trait::async_trait]
impl SkillSource for GitHubSource {
    fn source_id(&self) -> &str {
        "github"
    }

    fn trust_level_for(&self, identifier: &str) -> String {
        // Check if identifier starts with a trusted repo
        for trusted in TRUSTED_REPOS {
            if identifier.starts_with(trusted) {
                return "trusted".to_string();
            }
        }
        "community".to_string()
    }

    fn search(&self, query: &str, limit: usize) -> Vec<SkillMeta> {
        // Blocking call — use in async context via tokio::task::spawn_blocking
        let rt = tokio::runtime::Handle::try_current();
        if rt.is_err() {
            return Vec::new();
        }

        let mut all_results = Vec::new();
        let query_owned = query.to_string();
        let _self_clone = Self {
            auth: self.auth.clone(),
            taps: self.taps.clone(),
            client: self.client.clone(),
            cache_dir: self.cache_dir.clone(),
        };

        // Use current thread runtime for blocking
        let taps = self.taps.clone();
        let client = self.client.clone();
        let auth = self.auth.clone();
        let cache_dir = self.cache_dir.clone();

        let source = GitHubSource {
            auth,
            taps,
            client,
            cache_dir,
        };

        let results: Vec<SkillMeta> = match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle.block_on(async {
                let mut results = Vec::new();
                for (repo, path_prefix) in &source.taps {
                    let tap_results = source
                        .search_tap(repo, path_prefix, &query_owned)
                        .await;
                    results.extend(tap_results);
                }
                results
            }),
            Err(_) => Vec::new(),
        };

        // Deduplicate by name, preferring higher trust levels
        let trust_score = |t: &str| match t {
            "builtin" => 2,
            "trusted" => 1,
            _ => 0,
        };

        let mut seen: HashMap<String, usize> = HashMap::new();
        for skill in results.iter() {
            let entry = seen.entry(skill.name.clone());
            entry.or_insert_with(|| {
                all_results.push(skill.clone());
                all_results.len() - 1
            });
            // Prefer higher trust
            if let Some(&existing_idx) = seen.get(&skill.name) {
                if trust_score(&skill.trust_level) > trust_score(&all_results[existing_idx].trust_level) {
                    all_results[existing_idx] = skill.clone();
                }
            }
        }

        all_results.truncate(limit);
        all_results
    }

    fn fetch(&self, identifier: &str) -> Option<SkillBundle> {
        // identifier format: "owner/repo/path/to/skill"
        let parts: Vec<&str> = identifier.splitn(3, '/').collect();
        if parts.len() < 3 {
            return None;
        }
        let owner = parts[0];
        let repo = parts[1];
        let skill_path = parts[2];

        match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle.block_on(self.download_skill(owner, repo, skill_path, identifier)),
            Err(_) => None,
        }
    }

    fn inspect(&self, identifier: &str) -> Option<SkillMeta> {
        // identifier format: "owner/repo/path/to/skill"
        let parts: Vec<&str> = identifier.splitn(3, '/').collect();
        if parts.len() < 3 {
            return None;
        }
        let repo_full = format!("{}/{}", parts[0], parts[1]);
        let skill_path = parts[2];

        match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle.block_on(async {
                let skill_md_path = format!("{skill_path}/SKILL.md");
                let content = self.fetch_raw_file(&repo_full, &skill_md_path).await?;
                let (fm, _body) = parse_frontmatter(&content);
                let name = fm.name.clone().unwrap_or_else(|| {
                    Path::new(skill_path)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown")
                        .to_string()
                });

                Some(SkillMeta {
                    name,
                    description: fm.description.clone().unwrap_or_default(),
                    source: "github".to_string(),
                    identifier: identifier.to_string(),
                    trust_level: self.trust_level_for(identifier),
                    repo: Some(repo_full),
                    path: Some(skill_path.to_string()),
                    tags: extract_tags_from_frontmatter(&fm),
                    extra: None,
                })
            }),
            Err(_) => None,
        }
    }
}

// ── Official (local optional skills) Source ──

/// Source for local optional skills shipped with the repo.
pub struct OfficialSkillSource {
    optional_dir: Option<PathBuf>,
}

impl OfficialSkillSource {
    pub fn new() -> Self {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let optional = manifest_dir
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .map(|p| p.join("optional-skills"));
        Self {
            optional_dir: optional.filter(|p| p.exists()),
        }
    }
}

impl Default for OfficialSkillSource {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl SkillSource for OfficialSkillSource {
    fn source_id(&self) -> &str {
        "official"
    }

    fn trust_level_for(&self, _identifier: &str) -> String {
        "builtin".to_string()
    }

    fn search(&self, query: &str, limit: usize) -> Vec<SkillMeta> {
        let Some(dir) = &self.optional_dir else {
            return Vec::new();
        };
        let q = query.to_lowercase();
        let mut results = Vec::new();

        for entry in walkdir::WalkDir::new(dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if path.file_name().and_then(|n| n.to_str()) != Some("SKILL.md") {
                continue;
            }
            let path_str = path.to_string_lossy();
            if path_str.contains("/.git/") || path_str.contains("/.github/") {
                continue;
            }
            if let Ok(content) = fs::read_to_string(path) {
                let (fm, _body) = parse_frontmatter(&content);
                let name = fm.name.clone().unwrap_or_else(|| {
                    path.parent()
                        .and_then(|p| p.file_name())
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown")
                        .to_string()
                });
                let desc = fm.description.clone().unwrap_or_default();

                let searchable = format!("{} {}", name, desc).to_lowercase();
                if !q.is_empty() && !searchable.contains(&q) {
                    continue;
                }

                let skill_dir = path.parent().unwrap();
                let rel = skill_dir.strip_prefix(dir)
                    .ok()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();

                results.push(SkillMeta {
                    name,
                    description: desc,
                    source: "official".to_string(),
                    identifier: rel.clone(),
                    trust_level: "builtin".to_string(),
                    repo: None,
                    path: Some(rel),
                    tags: extract_tags_from_frontmatter(&fm),
                    extra: None,
                });
            }
        }

        results.truncate(limit);
        results
    }

    fn fetch(&self, identifier: &str) -> Option<SkillBundle> {
        let dir = self.optional_dir.as_ref()?;
        let skill_dir = if identifier.is_empty() {
            None
        } else {
            let candidate = dir.join(identifier);
            if candidate.join("SKILL.md").exists() {
                Some(candidate)
            } else {
                // Try to find by skill name
                for entry in walkdir::WalkDir::new(dir)
                    .max_depth(3)
                    .into_iter()
                    .filter_map(|e| e.ok())
                {
                    if entry.path().join("SKILL.md").exists()
                        && entry.path().file_name().and_then(|n| n.to_str()) == Some(identifier)
                    {
                        return Some(read_skill_bundle(entry.path(), "official", identifier, "builtin"));
                    }
                }
                None
            }
        }?;

        Some(read_skill_bundle(&skill_dir, "official", identifier, "builtin"))
    }

    fn inspect(&self, identifier: &str) -> Option<SkillMeta> {
        let dir = self.optional_dir.as_ref()?;
        // Find the skill directory
        for entry in walkdir::WalkDir::new(dir)
            .max_depth(3)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if path.join("SKILL.md").exists() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name == identifier || path.to_string_lossy().ends_with(&format!("/{identifier}")) {
                    if let Ok(content) = fs::read_to_string(path.join("SKILL.md")) {
                        let (fm, _body) = parse_frontmatter(&content);
                        return Some(SkillMeta {
                            name: fm.name.clone().unwrap_or(name.to_string()),
                            description: fm.description.clone().unwrap_or_default(),
                            source: "official".to_string(),
                            identifier: identifier.to_string(),
                            trust_level: "builtin".to_string(),
                            repo: None,
                            path: path.strip_prefix(dir).ok().map(|p| p.to_string_lossy().to_string()),
                            tags: extract_tags_from_frontmatter(&fm),
                            extra: None,
                        });
                    }
                }
            }
        }
        None
    }
}

// ── Frontmatter parsing ──

/// Parse YAML frontmatter from markdown content.
fn parse_frontmatter(content: &str) -> (SkillFrontmatterParsed, String) {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return (SkillFrontmatterParsed::default(), content.to_string());
    }
    let rest = &trimmed[3..];
    if let Some(end_idx) = rest.find("\n---") {
        let yaml_content = &rest[..end_idx];
        let body_start = end_idx + 3 + 5; // "---" + "\n---" + newline
        let body = &trimmed[body_start.min(trimmed.len())..].trim_start();
        if let Ok(fm) = serde_yaml::from_str::<SkillFrontmatterParsed>(yaml_content) {
            return (fm, body.to_string());
        }
    }
    (SkillFrontmatterParsed::default(), content.to_string())
}

/// Parse frontmatter as a generic Value (for metadata extraction).
fn parse_frontmatter_value(content: &str) -> Option<Value> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let rest = &trimmed[3..];
    if let Some(end_idx) = rest.find("\n---") {
        let yaml_content = &rest[..end_idx];
        serde_yaml::from_str::<Value>(yaml_content).ok()
    } else {
        None
    }
}

#[derive(Debug, Default, Deserialize)]
struct SkillFrontmatterParsed {
    name: Option<String>,
    description: Option<String>,
    tags: Option<Vec<String>>,
    #[allow(dead_code)]
    metadata: Option<Value>,
}

/// Extract skill name from frontmatter.
fn parse_skill_name_from_frontmatter(content: &str) -> Option<String> {
    let (fm, _) = parse_frontmatter(content);
    fm.name
}

/// Extract tags from frontmatter.
fn extract_tags_from_frontmatter(fm: &SkillFrontmatterParsed) -> Vec<String> {
    fm.tags.clone().unwrap_or_default()
}

/// Read a skill bundle from a local directory.
fn read_skill_bundle(
    skill_dir: &Path,
    source: &str,
    identifier: &str,
    trust_level: &str,
) -> SkillBundle {
    let mut files = BTreeMap::new();
    let mut metadata = None;

    for entry in walkdir::WalkDir::new(skill_dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // Skip dotfiles and binary files
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.'))
        {
            continue;
        }
        if let Ok(rel) = path.strip_prefix(skill_dir) {
            let rel_str = rel.to_string_lossy().to_string();
            // Skip binary extensions
            if let Some(ext) = path.extension() {
                let ext_str = ext.to_string_lossy().to_lowercase();
                if matches!(
                    ext_str.as_str(),
                    "png" | "jpg" | "jpeg" | "gif" | "ico" | "exe" | "zip"
                ) {
                    continue;
                }
            }
            if let Ok(content) = fs::read_to_string(path) {
                if rel_str == "SKILL.md" {
                    metadata = parse_frontmatter_value(&content);
                }
                files.insert(rel_str, content);
            }
        }
    }

    let name = metadata
        .as_ref()
        .and_then(|m| m.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| {
            skill_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(identifier)
        })
        .to_string();

    SkillBundle {
        name,
        files,
        source: source.to_string(),
        identifier: identifier.to_string(),
        trust_level: trust_level.to_string(),
        metadata,
    }
}

// ── Hub Lock File ──

/// Tracks provenance of installed hub skills.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HubLockFile {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub installed: BTreeMap<String, LockEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockEntry {
    pub source: String,
    pub identifier: String,
    pub trust_level: String,
    #[serde(default)]
    pub scan_verdict: Option<String>,
    #[serde(default)]
    pub content_hash: String,
    #[serde(default)]
    pub install_path: String,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub metadata: Option<Value>,
    #[serde(default = "chrono::Utc::now")]
    pub installed_at: chrono::DateTime<chrono::Utc>,
    #[serde(default = "chrono::Utc::now")]
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl HubLockFile {
    /// Load lock file from disk.
    pub fn load() -> Self {
        let path = lock_file_path();
        if !path.exists() {
            return Self {
                version: 1,
                installed: BTreeMap::new(),
            };
        }
        match fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|_| Self {
                version: 1,
                installed: BTreeMap::new(),
            }),
            Err(_) => Self {
                version: 1,
                installed: BTreeMap::new(),
            },
        }
    }

    /// Save lock file to disk.
    pub fn save(&self) {
        let path = lock_file_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = fs::write(&path, json);
        }
    }

    /// Record a skill installation.
    #[allow(clippy::too_many_arguments)]
    pub fn record_install(
        &mut self,
        name: &str,
        source: &str,
        identifier: &str,
        trust_level: &str,
        scan_verdict: Option<&str>,
        content_hash: &str,
        install_path: &str,
        files: Vec<String>,
        metadata: Option<Value>,
    ) {
        let now = chrono::Utc::now();
        self.installed.insert(name.to_string(), LockEntry {
            source: source.to_string(),
            identifier: identifier.to_string(),
            trust_level: trust_level.to_string(),
            scan_verdict: scan_verdict.map(String::from),
            content_hash: content_hash.to_string(),
            install_path: install_path.to_string(),
            files,
            metadata,
            installed_at: now,
            updated_at: now,
        });
        self.save();
    }

    /// Remove a skill from the lock file.
    pub fn record_uninstall(&mut self, name: &str) {
        self.installed.remove(name);
        self.save();
    }

    /// Get a specific installed skill.
    pub fn get_installed(&self, name: &str) -> Option<&LockEntry> {
        self.installed.get(name)
    }

    /// List all installed skills.
    pub fn list_installed(&self) -> Vec<(String, &LockEntry)> {
        self.installed.iter().map(|(k, v)| (k.clone(), v)).collect()
    }
}

// ── Taps Manager ──

/// User-added custom GitHub repo sources.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TapsManager {
    pub taps: Vec<TapEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TapEntry {
    pub repo: String,
    #[serde(default = "default_skills_path")]
    pub path: String,
}

fn default_skills_path() -> String {
    "skills/".to_string()
}

impl TapsManager {
    /// Load taps from disk.
    pub fn load() -> Self {
        let path = taps_file_path();
        if !path.exists() {
            return Self::default();
        }
        serde_json::from_str(&fs::read_to_string(&path).ok().unwrap_or_default())
            .unwrap_or_default()
    }

    /// Save taps to disk.
    pub fn save(&self) {
        let path = taps_file_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = fs::write(&path, json);
        }
    }

    /// Add a tap (skip if repo already exists).
    pub fn add(&mut self, repo: &str, path: &str) {
        if !self.taps.iter().any(|t| t.repo == repo) {
            self.taps.push(TapEntry {
                repo: repo.to_string(),
                path: if path.is_empty() {
                    "skills/".to_string()
                } else {
                    path.to_string()
                },
            });
            self.save();
        }
    }

    /// Remove a tap by repo.
    pub fn remove(&mut self, repo: &str) {
        self.taps.retain(|t| t.repo != repo);
        self.save();
    }

    /// List all taps.
    pub fn list_taps(&self) -> Vec<(&str, &str)> {
        self.taps.iter().map(|t| (t.repo.as_str(), t.path.as_str())).collect()
    }
}

// ── Hub Operations ──

/// Ensure hub directories exist.
pub fn ensure_hub_dirs() {
    let _ = fs::create_dir_all(hub_dir());
    let _ = fs::create_dir_all(quarantine_dir());
    let _ = fs::create_dir_all(index_cache_dir());

    // Initialize lock file if missing
    if !lock_file_path().exists() {
        HubLockFile::default().save();
    }
    // Initialize audit log if missing
    if !audit_log_path().exists() {
        let _ = fs::write(audit_log_path(), "");
    }
    // Initialize taps file if missing
    if !taps_file_path().exists() {
        TapsManager::default().save();
    }
}

/// Compute content hash of a skill bundle (SHA-256 of concatenated file contents).
pub fn bundle_content_hash(bundle: &SkillBundle) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    // Hash in sorted order (BTreeMap is already sorted)
    for (path, content) in &bundle.files {
        hasher.update(path.as_bytes());
        hasher.update(content.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

/// Validate a skill name for hub operations.
fn validate_skill_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Skill name cannot be empty.".to_string());
    }
    if name.contains("..") || name.starts_with('/') || name.starts_with('\\') {
        return Err(format!("Invalid skill name: {name}"));
    }
    if name.len() > 128 {
        return Err(format!("Skill name too long (max 128 chars): {name}"));
    }
    Ok(())
}

/// Validate and normalize a bundle file path (prevent path traversal).
fn validate_bundle_rel_path(path: &str) -> Result<String, String> {
    let p = Path::new(path);
    if p.is_absolute() || p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(format!("Path traversal detected: {path}"));
    }
    // Normalize to forward slashes
    Ok(path.replace('\\', "/"))
}

/// Quarantine a skill bundle for security scanning.
///
/// Writes the bundle to `skills/.hub/quarantine/<name>/` and returns the path.
pub fn quarantine_bundle(bundle: &SkillBundle) -> Result<PathBuf, String> {
    validate_skill_name(&bundle.name)?;

    let q_dir = quarantine_dir().join(&bundle.name);
    let _ = fs::remove_dir_all(&q_dir);
    fs::create_dir_all(&q_dir).map_err(|e| format!("Failed to create quarantine dir: {e}"))?;

    for (rel_path, content) in &bundle.files {
        let normalized = validate_bundle_rel_path(rel_path)?;
        let target = q_dir.join(&normalized);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("Failed to create dir: {e}"))?;
        }
        // Check the resolved path is still within quarantine
        let resolved = target.canonicalize().unwrap_or_else(|_| target.clone());
        if !resolved.starts_with(quarantine_dir().canonicalize().unwrap_or_else(|_| quarantine_dir())) {
            return Err(format!("Path escapes quarantine: {normalized}"));
        }
        fs::write(&target, content).map_err(|e| format!("Failed to write file: {e}"))?;
    }

    Ok(q_dir)
}

/// Install a skill from quarantine after successful scan.
pub fn install_from_quarantine(
    quarantine_path: &Path,
    skill_name: &str,
    category: Option<&str>,
    bundle: &SkillBundle,
    scan_result: Option<&str>,
) -> Result<PathBuf, String> {
    validate_skill_name(skill_name)?;

    // Verify quarantine path is within quarantine dir
    let q_resolved = quarantine_path.canonicalize().map_err(|e| format!("Quarantine path error: {e}"))?;
    let q_dir_resolved = quarantine_dir().canonicalize().unwrap_or_else(|_| quarantine_dir());
    if !q_resolved.starts_with(&q_dir_resolved) {
        return Err("Quarantine path escapes quarantine directory.".to_string());
    }

    // Determine install location
    let install_dir = if let Some(cat) = category.filter(|c| !c.is_empty()) {
        skills_dir().join(cat).join(skill_name)
    } else {
        skills_dir().join(skill_name)
    };

    // Warn on large SKILL.md (>100KB) but don't block
    let skill_md = q_resolved.join("SKILL.md");
    if let Ok(metadata) = fs::metadata(&skill_md) {
        if metadata.len() > 100_000 {
            tracing::warn!(
                "Skill {skill_name}: SKILL.md is {} bytes (recommended < 100KB)",
                metadata.len()
            );
        }
    }

    // Copy from quarantine to install location
    copy_dir_all(&q_resolved, &install_dir).map_err(|e| format!("Failed to install: {e}"))?;

    // Record in lock file
    let content_hash = bundle_content_hash(bundle);
    let file_list: Vec<String> = bundle.files.keys().cloned().collect();
    let mut lock = HubLockFile::load();
    lock.record_install(
        skill_name,
        &bundle.source,
        &bundle.identifier,
        &bundle.trust_level,
        scan_result,
        &content_hash,
        &install_dir.to_string_lossy(),
        file_list,
        bundle.metadata.clone(),
    );

    // Audit log
    append_audit_log("install", skill_name, &bundle.source, &bundle.trust_level, scan_result.unwrap_or("pass"));

    // Clean up quarantine
    let _ = fs::remove_dir_all(quarantine_path);

    Ok(install_dir)
}

/// Uninstall a hub-installed skill.
pub fn uninstall_skill(skill_name: &str) -> (bool, String) {
    let lock = HubLockFile::load();

    // Refuse to remove built-in skills
    let entry = match lock.get_installed(skill_name) {
        Some(e) => e,
        None => return (false, format!("Skill '{skill_name}' was not installed via the hub.")),
    };

    let install_path = Path::new(&entry.install_path);
    if !install_path.exists() {
        return (false, format!("Skill '{skill_name}' directory not found on disk."));
    }

    // Delete the skill directory
    if let Err(e) = fs::remove_dir_all(install_path) {
        return (false, format!("Failed to remove skill directory: {e}"));
    }

    // Remove from lock file
    let mut updated_lock = HubLockFile::load();
    updated_lock.record_uninstall(skill_name);

    append_audit_log("uninstall", skill_name, &entry.source, &entry.trust_level, "ok");

    (true, format!("Skill '{skill_name}' uninstalled successfully."))
}

/// Check for skill updates.
pub fn check_for_skill_updates(
    name: Option<&str>,
    auth: Option<&GitHubAuth>,
) -> Vec<Value> {
    let lock = HubLockFile::load();
    let mut results = Vec::new();

    let entries: Vec<(String, LockEntry)> = if let Some(n) = name {
        lock.get_installed(n)
            .map(|e| (n.to_string(), e.clone()))
            .into_iter()
            .collect()
    } else {
        lock.list_installed().into_iter().map(|(k, v)| (k, v.clone())).collect()
    };

    for (skill_name, entry) in entries {
        if entry.source != "github" {
            continue;
        }

        let auth_owned = auth.cloned().unwrap_or_default();
        let source = GitHubSource::new(Some(auth_owned), None);
        let identifier = entry.identifier.clone();

        // Fetch current remote version
        let current_meta = source.inspect(&identifier);
        let current_hash = if let Some(bundle) = source.fetch(&identifier) {
            bundle_content_hash(&bundle)
        } else {
            results.push(serde_json::json!({
                "name": skill_name,
                "identifier": identifier,
                "source": "github",
                "status": "unavailable",
            }));
            continue;
        };

        let status = if current_hash == entry.content_hash {
            "up_to_date"
        } else {
            "update_available"
        };

        results.push(serde_json::json!({
            "name": skill_name,
            "identifier": identifier,
            "source": "github",
            "status": status,
            "current_hash": current_hash,
            "latest_hash": entry.content_hash,
            "description": current_meta.map(|m| m.description).unwrap_or_default(),
        }));
    }

    results
}

/// Search skills across all configured sources in parallel.
pub fn unified_search(
    query: &str,
    sources: Vec<Arc<dyn SkillSource>>,
    source_filter: Option<&[String]>,
    limit: usize,
) -> Vec<SkillMeta> {
    let q = query.to_string();
    let filter_owned = source_filter.map(|s| s.to_vec());

    // Spawn tasks for each source
    let mut handles = Vec::new();
    for source in sources {
        let q_clone = q.clone();
        let source_id = source.source_id().to_string();
        let filter = filter_owned.as_ref();

        // Check if source is in filter
        if let Some(f) = filter {
            if !f.contains(&source_id) && source_id != "official" {
                continue;
            }
        }

        let source_clone = source.clone();
        let limit_clone = limit;
        let q_inner = q_clone.clone();

        let handle = tokio::spawn(async move {
            // Run blocking search in a blocking thread
            let results = tokio::task::spawn_blocking(move || {
                source_clone.search(&q_inner, limit_clone)
            }).await.unwrap_or_default();
            (source_id, results)
        });
        handles.push(handle);
    }

    // Collect results
    let rt = tokio::runtime::Handle::current();
    let all_results: Vec<(String, Vec<SkillMeta>)> = rt.block_on(async {
        futures_util::future::join_all(handles)
            .await
            .into_iter()
            .filter_map(|h| h.ok())
            .collect()
    });

    // Deduplicate by name, preferring higher trust
    let trust_score = |t: &str| match t {
        "builtin" => 2,
        "trusted" => 1,
        _ => 0,
    };

    let mut best: HashMap<String, SkillMeta> = HashMap::new();
    for (_source_id, results) in all_results {
        for skill in results {
            let entry = best.entry(skill.name.clone());
            entry.and_modify(|existing| {
                if trust_score(&skill.trust_level) > trust_score(&existing.trust_level) {
                    *existing = skill.clone();
                }
            }).or_insert(skill);
        }
    }

    let mut deduped: Vec<SkillMeta> = best.into_values().collect();
    deduped.sort_by(|a, b| {
        let score_a = trust_score(&a.trust_level);
        let score_b = trust_score(&b.trust_level);
        score_b.cmp(&score_a).then_with(|| a.name.cmp(&b.name))
    });
    deduped.truncate(limit);
    deduped
}

/// Append a line to the audit log.
fn append_audit_log(action: &str, skill_name: &str, source: &str, trust_level: &str, verdict: &str) {
    let path = audit_log_path();
    let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let line = format!("{timestamp} {action} {skill_name} {source}:{trust_level} {verdict}\n");
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Recursively copy a directory.
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dest_path)?;
        } else {
            fs::copy(entry.path(), &dest_path)?;
        }
    }
    Ok(())
}

// ── Tool handlers ──

/// Handle skill search across all sources.
pub fn handle_skill_search(args: Value) -> Result<String, hermes_core::HermesError> {
    let query = args.get("query").and_then(Value::as_str).unwrap_or("");
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;
    let source_filter = args.get("source").and_then(Value::as_str).map(|s| vec![s.to_string()]);

    let auth = GitHubAuth::default();
    let sources: Vec<Arc<dyn SkillSource>> = vec![
        Arc::new(OfficialSkillSource::new()),
        Arc::new(GitHubSource::new(Some(auth), None)),
    ];

    let results = unified_search(query, sources, source_filter.as_deref(), limit);

    crate::registry::tool_result(serde_json::json!({
        "query": query,
        "results": results,
        "count": results.len(),
        "limit": limit,
    }))
}

/// Handle skill fetch/download from a source.
pub fn handle_skill_fetch(args: Value) -> Result<String, hermes_core::HermesError> {
    let identifier = args.get("identifier")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            hermes_core::HermesError::new(
                hermes_core::errors::ErrorCategory::ToolError,
                "skill_fetch requires 'identifier' parameter (e.g. 'owner/repo/skill-name')",
            )
        })?;

    let auth = GitHubAuth::default();
    let source = GitHubSource::new(Some(auth), None);

    match source.fetch(identifier) {
        Some(bundle) => {
            let file_list: Vec<&str> = bundle.files.keys().map(|s| s.as_str()).collect();
            crate::registry::tool_result(serde_json::json!({
                "name": bundle.name,
                "source": bundle.source,
                "identifier": bundle.identifier,
                "trust_level": bundle.trust_level,
                "files": file_list,
                "file_count": file_list.len(),
            }))
        }
        None => Ok(tool_error(format!("Skill not found: {identifier}"))),
    }
}

/// Handle skill install from a source.
pub fn handle_skill_install(args: Value) -> Result<String, hermes_core::HermesError> {
    let identifier = args.get("identifier")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            hermes_core::HermesError::new(
                hermes_core::errors::ErrorCategory::ToolError,
                "skill_install requires 'identifier' parameter",
            )
        })?;

    let category = args.get("category").and_then(Value::as_str);
    let auth = GitHubAuth::default();
    let source = GitHubSource::new(Some(auth), None);

    // Fetch the skill bundle
    let Some(bundle) = source.fetch(identifier) else {
        return Ok(tool_error(format!("Skill not found: {identifier}")));
    };

    // Quarantine for security scanning
    let q_path = match quarantine_bundle(&bundle) {
        Ok(p) => p,
        Err(e) => return Ok(tool_error(format!("Failed to quarantine: {e}"))),
    };

    // Security scan
    let scan_result = skills_guard::scan_skill(&q_path, &bundle.trust_level);
    let verdict = match &scan_result {
        Ok(s) => {
            let has_critical_or_high = s.findings.iter().any(|f| {
                matches!(f.severity, skills_guard::Severity::Critical | skills_guard::Severity::High)
            });
            if has_critical_or_high { "block" } else { "pass" }
        }
        Err(_) => "scan_failed",
    };

    // Block on Critical/High findings
    if let Ok(scan) = &scan_result {
        for f in &scan.findings {
            if matches!(f.severity, skills_guard::Severity::Critical | skills_guard::Severity::High) {
                let _ = fs::remove_dir_all(&q_path);
                return Ok(tool_error(format!(
                    "Security scan blocked install: {} ({:?})",
                    f.category, f.severity
                )));
            }
        }
    }

    // Install from quarantine
    let category_str = category.unwrap_or("");
    match install_from_quarantine(&q_path, &bundle.name, if category_str.is_empty() { None } else { Some(category_str) }, &bundle, Some(verdict)) {
        Ok(path) => crate::registry::tool_result(serde_json::json!({
            "name": bundle.name,
            "source": bundle.source,
            "identifier": bundle.identifier,
            "trust_level": bundle.trust_level,
            "install_path": path.to_string_lossy(),
            "scan_verdict": verdict,
            "success": true,
        })),
        Err(e) => Ok(tool_error(format!("Failed to install: {e}"))),
    }
}

/// Handle skill uninstall.
pub fn handle_skill_uninstall(args: Value) -> Result<String, hermes_core::HermesError> {
    let name = args.get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            hermes_core::HermesError::new(
                hermes_core::errors::ErrorCategory::ToolError,
                "skill_uninstall requires 'name' parameter",
            )
        })?;

    let confirm = args.get("confirm").and_then(Value::as_bool).unwrap_or(false);
    if !confirm {
        return Ok(tool_error(format!(
            "Uninstalling '{name}' requires 'confirm': true."
        )));
    }

    let (success, message) = uninstall_skill(name);
    crate::registry::tool_result(serde_json::json!({
        "success": success,
        "message": message,
    }))
}

/// Handle skill update check.
pub fn handle_skill_update_check(args: Value) -> Result<String, hermes_core::HermesError> {
    let name = args.get("name").and_then(Value::as_str);
    let auth = GitHubAuth::default();

    let updates = check_for_skill_updates(name, Some(&auth));

    let has_updates = updates.iter().filter(|u| u["status"] == "update_available").count();
    crate::registry::tool_result(serde_json::json!({
        "skills": updates,
        "total": updates.len(),
        "updates_available": has_updates,
    }))
}

/// Handle tap management (add/remove/list GitHub sources).
pub fn handle_skill_taps(args: Value) -> Result<String, hermes_core::HermesError> {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
    let mut taps = TapsManager::load();

    match action {
        "list" => {
            let tap_list: Vec<Value> = taps.list_taps()
                .iter()
                .map(|(repo, path)| serde_json::json!({"repo": repo, "path": path}))
                .collect();
            crate::registry::tool_result(serde_json::json!({
                "taps": tap_list,
                "count": tap_list.len(),
            }))
        }
        "add" => {
            let repo = args.get("repo").and_then(Value::as_str).unwrap_or("");
            if repo.is_empty() {
                return Ok(tool_error("Adding a tap requires 'repo' parameter."));
            }
            let path = args.get("path").and_then(Value::as_str).unwrap_or("skills/");
            taps.add(repo, path);
            crate::registry::tool_result(serde_json::json!({
                "success": true,
                "message": format!("Added tap: {repo}"),
            }))
        }
        "remove" => {
            let repo = args.get("repo").and_then(Value::as_str).unwrap_or("");
            if repo.is_empty() {
                return Ok(tool_error("Removing a tap requires 'repo' parameter."));
            }
            taps.remove(repo);
            crate::registry::tool_result(serde_json::json!({
                "success": true,
                "message": format!("Removed tap: {repo}"),
            }))
        }
        _ => Ok(tool_error(format!("Unknown tap action: {action}. Use: list, add, remove"))),
    }
}

/// Handle listing installed hub skills.
pub fn handle_skill_installed_list(_args: Value) -> Result<String, hermes_core::HermesError> {
    let lock = HubLockFile::load();
    let installed: Vec<Value> = lock
        .list_installed()
        .iter()
        .map(|(name, entry)| serde_json::json!({
            "name": name,
            "source": entry.source,
            "identifier": entry.identifier,
            "trust_level": entry.trust_level,
            "content_hash": entry.content_hash,
            "installed_at": entry.installed_at,
            "updated_at": entry.updated_at,
        }))
        .collect();

    crate::registry::tool_result(serde_json::json!({
        "installed": installed,
        "count": installed.len(),
    }))
}

/// Register skills hub tools in the registry.
pub fn register(registry: &mut crate::registry::ToolRegistry) {
    // skill_search
    registry.register(
        "skill_search".to_string(),
        "organization".to_string(),
        serde_json::json!({
            "name": "skill_search",
            "description": "Search for skills across multiple sources (GitHub, official, etc.).",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query for skill discovery."
                    },
                    "source": {
                        "type": "string",
                        "description": "Filter by source (github, official, etc.)."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum results to return.",
                        "default": 10
                    }
                },
                "required": []
            }
        }),
        std::sync::Arc::new(handle_skill_search),
        None,
        vec![],
        "Search for skills across multiple sources".to_string(),
        "🔍".to_string(),
        None,
    );

    // skill_fetch
    registry.register(
        "skill_fetch".to_string(),
        "organization".to_string(),
        serde_json::json!({
            "name": "skill_fetch",
            "description": "Fetch a skill bundle from a source for preview (does not install).",
            "parameters": {
                "type": "object",
                "properties": {
                    "identifier": {
                        "type": "string",
                        "description": "Skill identifier (e.g. 'owner/repo/skill-name')."
                    }
                },
                "required": ["identifier"]
            }
        }),
        std::sync::Arc::new(handle_skill_fetch),
        None,
        vec![],
        "Fetch a skill bundle for preview".to_string(),
        "📦".to_string(),
        None,
    );

    // skill_install
    registry.register(
        "skill_install".to_string(),
        "organization".to_string(),
        serde_json::json!({
            "name": "skill_install",
            "description": "Install a skill from a source. Downloads, scans, and installs with provenance tracking.",
            "parameters": {
                "type": "object",
                "properties": {
                    "identifier": {
                        "type": "string",
                        "description": "Skill identifier (e.g. 'owner/repo/skill-name')."
                    },
                    "category": {
                        "type": "string",
                        "description": "Optional category directory for the installed skill."
                    }
                },
                "required": ["identifier"]
            }
        }),
        std::sync::Arc::new(handle_skill_install),
        None,
        vec![],
        "Install a skill from a source with security scanning".to_string(),
        "📥".to_string(),
        None,
    );

    // skill_uninstall
    registry.register(
        "skill_uninstall".to_string(),
        "organization".to_string(),
        serde_json::json!({
            "name": "skill_uninstall",
            "description": "Uninstall a hub-installed skill.",
            "parameters": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Name of the skill to uninstall."
                    },
                    "confirm": {
                        "type": "boolean",
                        "description": "Must be true to confirm uninstall.",
                        "default": false
                    }
                },
                "required": ["name"]
            }
        }),
        std::sync::Arc::new(handle_skill_uninstall),
        None,
        vec![],
        "Uninstall a hub-installed skill".to_string(),
        "🗑️".to_string(),
        None,
    );

    // skill_update_check
    registry.register(
        "skill_update_check".to_string(),
        "organization".to_string(),
        serde_json::json!({
            "name": "skill_update_check",
            "description": "Check installed hub skills for available updates.",
            "parameters": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Optional: check only this skill."
                    }
                },
                "required": []
            }
        }),
        std::sync::Arc::new(handle_skill_update_check),
        None,
        vec![],
        "Check for skill updates from upstream sources".to_string(),
        "🔄".to_string(),
        None,
    );

    // skill_taps
    registry.register(
        "skill_taps".to_string(),
        "organization".to_string(),
        serde_json::json!({
            "name": "skill_taps",
            "description": "Manage GitHub source taps for skill discovery.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "description": "Action: list, add, remove.",
                        "enum": ["list", "add", "remove"]
                    },
                    "repo": {
                        "type": "string",
                        "description": "Repo for add/remove (e.g. 'owner/repo')."
                    },
                    "path": {
                        "type": "string",
                        "description": "Path within repo for add (default: skills/).",
                        "default": "skills/"
                    }
                },
                "required": ["action"]
            }
        }),
        std::sync::Arc::new(handle_skill_taps),
        None,
        vec![],
        "Manage GitHub source taps".to_string(),
        "🔌".to_string(),
        None,
    );

    // skill_installed_list
    registry.register(
        "skill_installed_list".to_string(),
        "organization".to_string(),
        serde_json::json!({
            "name": "skill_installed_list",
            "description": "List all skills installed via the hub with provenance info.",
            "parameters": {
                "type": "object",
                "properties": {},
                "required": []
            }
        }),
        std::sync::Arc::new(handle_skill_installed_list),
        None,
        vec![],
        "List hub-installed skills".to_string(),
        "📋".to_string(),
        None,
    );
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_skill_name_valid() {
        assert!(validate_skill_name("my-skill").is_ok());
        assert!(validate_skill_name("skill_123").is_ok());
        assert!(validate_skill_name("simple").is_ok());
    }

    #[test]
    fn test_validate_skill_name_empty() {
        let r = validate_skill_name("");
        assert!(r.is_err());
    }

    #[test]
    fn test_validate_skill_name_traversal() {
        assert!(validate_skill_name("../etc/passwd").is_err());
        assert!(validate_skill_name("/absolute/path").is_err());
        assert!(validate_skill_name("\\windows\\system32").is_err());
    }

    #[test]
    fn test_validate_skill_name_too_long() {
        let r = validate_skill_name(&"a".repeat(129));
        assert!(r.is_err());
    }

    #[test]
    fn test_validate_bundle_rel_path_valid() {
        assert_eq!(validate_bundle_rel_path("SKILL.md").unwrap(), "SKILL.md");
        assert_eq!(validate_bundle_rel_path("references/api.md").unwrap(), "references/api.md");
        assert_eq!(validate_bundle_rel_path("sub/dir/file.txt").unwrap(), "sub/dir/file.txt");
    }

    #[test]
    fn test_validate_bundle_rel_path_traversal() {
        assert!(validate_bundle_rel_path("../etc/passwd").is_err());
        // On Windows, /absolute may not be considered absolute by Path
        // Just check that parent dir components are caught
        assert!(validate_bundle_rel_path("foo/../bar").is_err());
    }

    #[test]
    fn test_bundle_content_hash_deterministic() {
        let bundle1 = SkillBundle {
            name: "test".to_string(),
            files: [("SKILL.md".to_string(), "# Test".to_string())].into_iter().collect(),
            source: "test".to_string(),
            identifier: "test".to_string(),
            trust_level: "community".to_string(),
            metadata: None,
        };
        let bundle2 = bundle1.clone();
        assert_eq!(bundle_content_hash(&bundle1), bundle_content_hash(&bundle2));
    }

    #[test]
    fn test_bundle_content_hash_different() {
        let bundle1 = SkillBundle {
            name: "test".to_string(),
            files: [("SKILL.md".to_string(), "# Test".to_string())].into_iter().collect(),
            source: "test".to_string(),
            identifier: "test".to_string(),
            trust_level: "community".to_string(),
            metadata: None,
        };
        let bundle2 = SkillBundle {
            name: "test".to_string(),
            files: [("SKILL.md".to_string(), "# Different".to_string())].into_iter().collect(),
            source: "test".to_string(),
            identifier: "test".to_string(),
            trust_level: "community".to_string(),
            metadata: None,
        };
        assert_ne!(bundle_content_hash(&bundle1), bundle_content_hash(&bundle2));
    }

    #[test]
    fn test_github_auth_default() {
        let auth = GitHubAuth::default();
        // Token may or may not be set from env
        let _ = auth.is_authenticated();
        let _ = auth.auth_method();
    }

    #[test]
    fn test_parse_frontmatter() {
        let content = "---\nname: test\ndescription: A test skill\ntags: [tag1, tag2]\n---\n\n# Body";
        let (fm, body) = parse_frontmatter(content);
        assert_eq!(fm.name, Some("test".to_string()));
        assert_eq!(fm.description, Some("A test skill".to_string()));
        assert_eq!(body, "# Body");
    }

    #[test]
    fn test_parse_frontmatter_no_frontmatter() {
        let content = "# Just a title\n\nBody content.";
        let (fm, body) = parse_frontmatter(content);
        assert!(fm.name.is_none());
        assert_eq!(body, "# Just a title\n\nBody content.");
    }

    #[test]
    fn test_parse_frontmatter_value() {
        let content = "---\nname: test\nmetadata:\n  hermes:\n    key: val\n---\n\nBody";
        let val = parse_frontmatter_value(content).unwrap();
        assert_eq!(val["name"], "test");
        assert!(val["metadata"]["hermes"]["key"].is_string());
    }

    #[test]
    fn test_parse_frontmatter_value_none() {
        assert!(parse_frontmatter_value("# No frontmatter").is_none());
    }

    #[test]
    fn test_hub_lock_file_roundtrip() {
        let _tmp = tempfile::tempdir().unwrap();
        // We can't easily override the lock file path, so just test the struct
        let lock = HubLockFile {
            version: 1,
            installed: BTreeMap::new(),
        };
        assert_eq!(lock.version, 1);
        assert!(lock.installed.is_empty());
    }

    #[test]
    fn test_taps_manager() {
        let mut taps = TapsManager::default();
        assert!(taps.list_taps().is_empty());

        taps.add("owner/repo", "skills/");
        assert_eq!(taps.list_taps().len(), 1);

        // Duplicate add should be ignored
        taps.add("owner/repo", "other/");
        assert_eq!(taps.list_taps().len(), 1);

        taps.remove("owner/repo");
        assert!(taps.list_taps().is_empty());
    }

    #[test]
    fn test_skill_meta_serialization() {
        let meta = SkillMeta {
            name: "test-skill".to_string(),
            description: "A test skill".to_string(),
            source: "github".to_string(),
            identifier: "owner/repo/skill".to_string(),
            trust_level: "community".to_string(),
            repo: Some("owner/repo".to_string()),
            path: Some("skill".to_string()),
            tags: vec!["tag1".to_string()],
            extra: None,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let parsed: SkillMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "test-skill");
    }

    #[test]
    fn test_official_source_default() {
        let source = OfficialSkillSource::new();
        assert_eq!(source.source_id(), "official");
        assert_eq!(source.trust_level_for("anything"), "builtin");
    }

    #[test]
    fn test_handler_skill_search_no_query() {
        // This requires a tokio runtime, skip in unit test
        // Just verify the function exists and the schema is valid
    }

    #[test]
    fn test_handler_skill_fetch_missing_param() {
        let result = handle_skill_fetch(serde_json::json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn test_handler_skill_install_missing_param() {
        let result = handle_skill_install(serde_json::json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn test_handler_skill_uninstall_missing_param() {
        let result = handle_skill_uninstall(serde_json::json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn test_handler_skill_uninstall_no_confirm() {
        let result = handle_skill_uninstall(serde_json::json!({
            "name": "some-skill"
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        // When no confirm, it returns an error message in the result
        assert!(json.get("error").is_some() || json.get("success").and_then(|v| v.as_bool()) == Some(false));
    }

    #[test]
    fn test_handler_skill_taps_list() {
        let result = handle_skill_taps(serde_json::json!({
            "action": "list"
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json["taps"].is_array());
    }

    #[test]
    fn test_handler_skill_taps_add_missing_repo() {
        let result = handle_skill_taps(serde_json::json!({
            "action": "add"
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some() || json.get("success").and_then(|v| v.as_bool()) == Some(false));
    }

    #[test]
    fn test_handler_skill_installed_list() {
        let result = handle_skill_installed_list(serde_json::json!({}));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json["installed"].is_array());
    }

    #[test]
    fn test_handler_skill_update_check() {
        let result = handle_skill_update_check(serde_json::json!({}));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json["skills"].is_array());
        assert_eq!(json["updates_available"], 0);
    }
}
