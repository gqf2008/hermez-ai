//! Toolset distributions for batch data generation.
//!
//! Mirrors the Python `toolset_distributions.py`.
//! Each distribution maps toolset names to selection probabilities (0–100%).
//! Used to sample active toolsets per prompt during RL/data generation runs.

use std::collections::HashMap;

/// A toolset distribution: probabilities for each toolset (0–100%).
#[derive(Debug, Clone)]
pub struct Distribution {
    pub description: &'static str,
    pub toolsets: HashMap<&'static str, u8>,
}

/// All defined distributions.
pub fn distributions() -> HashMap<&'static str, Distribution> {
    let mut map = HashMap::new();

    map.insert("default", Distribution {
        description: "All toolsets enabled",
        toolsets: HashMap::from([
            ("web", 100u8), ("terminal", 100), ("file", 100),
            ("browser", 100), ("vision", 100), ("image_gen", 100), ("moa", 100),
        ]),
    });

    map.insert("research", Distribution {
        description: "Web-heavy distribution for research tasks",
        toolsets: HashMap::from([
            ("web", 90u8), ("browser", 70), ("vision", 50), ("moa", 40),
        ]),
    });

    map.insert("science", Distribution {
        description: "Science-focused: web, terminal, file, vision",
        toolsets: HashMap::from([
            ("web", 94u8), ("terminal", 94), ("file", 94),
            ("vision", 65), ("browser", 50),
        ]),
    });

    map.insert("development", Distribution {
        description: "Code development: terminal + file + moa",
        toolsets: HashMap::from([
            ("terminal", 80u8), ("file", 80), ("moa", 60), ("web", 30),
        ]),
    });

    map.insert("safe", Distribution {
        description: "All toolsets except terminal",
        toolsets: HashMap::from([
            ("web", 100u8), ("file", 100), ("browser", 100),
            ("vision", 100), ("image_gen", 100), ("moa", 100),
        ]),
    });

    map.insert("balanced", Distribution {
        description: "All toolsets at 50% probability",
        toolsets: HashMap::from([
            ("web", 50u8), ("terminal", 50), ("file", 50),
            ("browser", 50), ("vision", 50), ("image_gen", 50), ("moa", 50),
        ]),
    });

    map.insert("minimal", Distribution {
        description: "Web search only",
        toolsets: HashMap::from([("web", 100u8)]),
    });

    map.insert("terminal_only", Distribution {
        description: "Terminal and file only",
        toolsets: HashMap::from([("terminal", 100u8), ("file", 100)]),
    });

    map.insert("terminal_web", Distribution {
        description: "Terminal, web, and file",
        toolsets: HashMap::from([
            ("terminal", 100u8), ("web", 100), ("file", 100),
        ]),
    });

    map.insert("creative", Distribution {
        description: "Image generation and vision heavy",
        toolsets: HashMap::from([
            ("image_gen", 90u8), ("vision", 90), ("web", 30),
        ]),
    });

    map.insert("reasoning", Distribution {
        description: "MoA-heavy for complex reasoning",
        toolsets: HashMap::from([
            ("moa", 90u8), ("web", 30), ("terminal", 20),
        ]),
    });

    map.insert("browser_use", Distribution {
        description: "Browser automation focused",
        toolsets: HashMap::from([
            ("browser", 100u8), ("web", 80), ("vision", 70),
        ]),
    });

    map.insert("browser_only", Distribution {
        description: "Browser toolset only",
        toolsets: HashMap::from([("browser", 100u8)]),
    });

    map.insert("mixed_tasks", Distribution {
        description: "Mixed browser, terminal, file tasks",
        toolsets: HashMap::from([
            ("browser", 92u8), ("terminal", 92), ("file", 92),
            ("web", 35), ("vision", 15), ("image_gen", 15),
        ]),
    });

    map
}

/// Sample toolsets from a distribution by name.
///
/// Each toolset is independently sampled: roll a random 0–99 against
/// the probability. At least one toolset is always returned (falls back
/// to the highest-probability toolset if none pass).
pub fn sample_toolsets(distribution_name: &str) -> Option<Vec<String>> {
    let dists = distributions();
    let dist = dists.get(distribution_name)?;

    let mut result = Vec::new();
    let mut best: Option<(&str, u8)> = None;

    for (&name, &prob) in &dist.toolsets {
        if rand::random::<u8>() < prob {
            result.push(name.to_string());
        }
        if best.is_none_or(|(_, bp)| prob > bp) {
            best = Some((name, prob));
        }
    }

    // Ensure at least one toolset
    if result.is_empty() {
        if let Some((name, _)) = best {
            result.push(name.to_string());
        }
    }

    Some(result)
}

/// List all available distribution names with descriptions.
pub fn list_distributions() -> Vec<(&'static str, &'static str)> {
    let mut list: Vec<_> = distributions()
        .into_iter()
        .map(|(name, dist)| (name, dist.description))
        .collect();
    list.sort_by_key(|&(name, _)| name);
    list
}

/// Validate that a distribution name exists.
pub fn validate_distribution(name: &str) -> bool {
    distributions().contains_key(name)
}

/// The set of all possible tool names across all toolsets.
///
/// Used for schema normalization: ensures every tool appears in tool_stats
/// with at least zero counts, so HuggingFace dataset loaders see consistent
/// columns.
pub fn all_possible_tools() -> Vec<String> {
    let toolsets = hermez_tools::toolsets_def::toolsets();
    let mut tools = std::collections::HashSet::new();
    for ts in toolsets.values() {
        for &t in ts.tools {
            tools.insert(t.to_string());
        }
    }
    tools.into_iter().collect()
}

/// Normalize tool stats to include all possible tools with zero defaults.
///
/// Ensures every tool in `all_possible_tools()` has an entry in the stats
/// map, filling missing ones with `{calls: 0, success: 0, errors: 0}`.
pub fn normalize_tool_stats(
    stats: serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    let possible = all_possible_tools();
    let mut result = stats;

    for tool in &possible {
        result.entry(tool.clone()).or_insert_with(|| {
            serde_json::json!({"calls": 0, "success": 0, "errors": 0})
        });
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_distributions_nonempty() {
        let dists = distributions();
        assert!(!dists.is_empty());
    }

    #[test]
    fn test_sample_always_returns_at_least_one() {
        for _ in 0..100 {
            let result = sample_toolsets("minimal").unwrap();
            assert!(!result.is_empty());
        }
    }

    #[test]
    fn test_sample_unknown_distribution() {
        assert!(sample_toolsets("nonexistent").is_none());
    }

    #[test]
    fn test_validate_distribution() {
        assert!(validate_distribution("default"));
        assert!(validate_distribution("research"));
        assert!(!validate_distribution("nonexistent"));
    }

    #[test]
    fn test_list_distributions() {
        let list = list_distributions();
        assert!(!list.is_empty());
        // Check sorted order
        for i in 1..list.len() {
            assert!(list[i].0 >= list[i - 1].0);
        }
    }

    #[test]
    fn test_all_possible_tools_nonempty() {
        let tools = all_possible_tools();
        assert!(!tools.is_empty());
        assert!(tools.contains(&"web_search".to_string()));
        assert!(tools.contains(&"terminal".to_string()));
    }

    #[test]
    fn test_normalize_tool_stats() {
        let mut stats = serde_json::Map::new();
        stats.insert("web_search".to_string(), serde_json::json!({"calls": 5, "success": 4, "errors": 1}));

        let normalized = normalize_tool_stats(stats);
        let possible = all_possible_tools();

        // All possible tools should be present
        for tool in &possible {
            assert!(normalized.contains_key(tool));
        }

        // Original stats should be preserved
        let ws = normalized.get("web_search").unwrap();
        assert_eq!(ws["calls"], 5);
        assert_eq!(ws["success"], 4);
        assert_eq!(ws["errors"], 1);
    }
}
