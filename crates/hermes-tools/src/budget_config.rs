#![allow(dead_code)]
//! Budget configuration for tool result persistence.
//!
//! Three-layer defense against context-window overflow:
//! 1. Per-tool cap
//! 2. Per-result persistence
//! 3. Per-turn aggregate budget
//!
//! Mirrors the Python `tools/budget_config.py`.

use std::collections::HashMap;
use std::sync::LazyLock;

/// Pinned thresholds that override all other config.
/// `read_file` is set to infinity to prevent persisting its results (prevents loops).
static PINNED_THRESHOLDS: LazyLock<HashMap<&'static str, f64>> = LazyLock::new(|| {
    let mut map = HashMap::new();
    map.insert("read_file", f64::INFINITY);
    map
});

/// Pinned thresholds accessor (no allocation).
fn pinned_thresholds() -> &'static HashMap<&'static str, f64> {
    &PINNED_THRESHOLDS
}

/// Default budget constants.
pub const DEFAULT_RESULT_SIZE: usize = 100_000;
pub const DEFAULT_TURN_BUDGET: usize = 200_000;
pub const DEFAULT_PREVIEW_SIZE: usize = 1_500;

/// Configuration for tool result size budgets.
#[derive(Debug, Clone)]
pub struct BudgetConfig {
    /// Default per-tool result size cap in characters.
    pub default_result_size: usize,
    /// Per-turn aggregate budget in characters.
    pub turn_budget: usize,
    /// Preview size for truncated results.
    pub preview_size: usize,
    /// Per-tool threshold overrides.
    pub tool_overrides: HashMap<String, usize>,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            default_result_size: DEFAULT_RESULT_SIZE,
            turn_budget: DEFAULT_TURN_BUDGET,
            preview_size: DEFAULT_PREVIEW_SIZE,
            tool_overrides: HashMap::new(),
        }
    }
}

impl BudgetConfig {
    /// Resolve the threshold for a specific tool.
    ///
    /// Priority chain:
    /// 1. Pinned thresholds (e.g., `read_file` = infinity)
    /// 2. Tool overrides from config
    /// 3. Registry max_result_size (if provided)
    /// 4. Default result size
    pub fn resolve_threshold(
        &self,
        tool_name: &str,
        registry_max: Option<usize>,
    ) -> usize {
        // Check pinned thresholds
        let pinned = pinned_thresholds();
        if let Some(&pinned_val) = pinned.get(tool_name) {
            if pinned_val.is_infinite() {
                return usize::MAX;
            }
            return pinned_val as usize;
        }

        // Check tool overrides
        if let Some(&override_val) = self.tool_overrides.get(tool_name) {
            return override_val;
        }

        // Check registry value
        if let Some(reg_val) = registry_max {
            return reg_val;
        }

        // Default
        self.default_result_size
    }

    /// Check if the turn budget has been exceeded.
    pub fn is_turn_budget_exceeded(&self, current_turn_total: usize) -> bool {
        current_turn_total > self.turn_budget
    }

    /// Calculate remaining turn budget.
    pub fn remaining_turn_budget(&self, current_turn_total: usize) -> usize {
        self.turn_budget.saturating_sub(current_turn_total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_resolve() {
        let config = BudgetConfig::default();
        assert_eq!(config.resolve_threshold("bash", None), DEFAULT_RESULT_SIZE);
    }

    #[test]
    fn test_pinned_read_file() {
        let config = BudgetConfig::default();
        assert_eq!(config.resolve_threshold("read_file", None), usize::MAX);
    }

    #[test]
    fn test_tool_override() {
        let mut config = BudgetConfig::default();
        config
            .tool_overrides
            .insert("bash".to_string(), 50_000);
        assert_eq!(config.resolve_threshold("bash", None), 50_000);
    }

    #[test]
    fn test_registry_max() {
        let config = BudgetConfig::default();
        assert_eq!(config.resolve_threshold("bash", Some(80_000)), 80_000);
    }

    #[test]
    fn test_override_beats_registry() {
        let mut config = BudgetConfig::default();
        config
            .tool_overrides
            .insert("bash".to_string(), 50_000);
        // Override should beat registry_max
        assert_eq!(config.resolve_threshold("bash", Some(80_000)), 50_000);
    }

    #[test]
    fn test_turn_budget() {
        let config = BudgetConfig::default();
        assert!(!config.is_turn_budget_exceeded(100_000));
        assert!(config.is_turn_budget_exceeded(250_000));
        assert_eq!(config.remaining_turn_budget(150_000), 50_000);
    }
}
