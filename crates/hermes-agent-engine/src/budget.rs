#![allow(dead_code)]
//! Iteration budget for agent turns.
//!
//! Mirrors the Python `IterationBudget` in `run_agent.py`.
//! Thread-safe counter that limits total LLM API calls across
//! parent agent + all subagents.
//!
//! Supports grace call: when budget is exhausted, the agent gets
//! one final API call before exiting (mirrors Python `_budget_grace_call`).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Thread-safe iteration budget.
pub struct IterationBudget {
    pub max_total: usize,
    used: AtomicUsize,
    /// Grace call flag — set when budget is exhausted, consumed on next iteration.
    grace_call: AtomicBool,
}

impl IterationBudget {
    /// Create a new budget.
    pub fn new(max_total: usize) -> Self {
        Self {
            max_total,
            used: AtomicUsize::new(0),
            grace_call: AtomicBool::new(false),
        }
    }

    /// Try to consume one iteration. Returns `true` if allowed.
    ///
    /// Uses `fetch_update` to atomically check-and-increment, avoiding
    /// the TOCTOU race between `load` and `fetch_add`.
    pub fn consume(&self) -> bool {
        self.used
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                if current >= self.max_total {
                    None
                } else {
                    Some(current + 1)
                }
            })
            .is_ok()
    }

    /// Give back one iteration (e.g., for programmatic tool calls).
    ///
    /// Uses saturating subtraction to prevent underflow if called
    /// when no iterations have been used.
    pub fn refund(&self) {
        let mut current = self.used.load(Ordering::SeqCst);
        while current > 0 {
            match self.used.compare_exchange_weak(
                current,
                current - 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return,
                Err(new) => current = new,
            }
        }
    }

    /// Number of iterations used.
    pub fn used(&self) -> usize {
        self.used.load(Ordering::SeqCst)
    }

    /// Number of iterations remaining.
    pub fn remaining(&self) -> usize {
        self.max_total.saturating_sub(self.used())
    }

    /// Reset the budget to zero.
    pub fn reset(&self) {
        self.used.store(0, Ordering::SeqCst);
    }

    // --- Grace call mechanism (mirrors Python `_budget_grace_call`) ---

    /// Check if a grace call is pending.
    ///
    /// Returns `true` when the budget is exhausted but a grace call
    /// was previously set. Consumes the grace flag so only one grace
    /// iteration is allowed.
    pub fn take_grace_call(&self) -> bool {
        self.grace_call
            .fetch_and(false, Ordering::SeqCst)
    }

    /// Set the grace call flag.
    ///
    /// Called when the budget is exhausted — gives the agent one final
    /// chance to produce output. Mirrors Python: `_budget_grace_call = True`.
    pub fn set_grace_call(&self) {
        self.grace_call.store(true, Ordering::SeqCst);
    }

    /// Check if a grace call is currently pending (non-consuming).
    pub fn has_grace_call(&self) -> bool {
        self.grace_call.load(Ordering::SeqCst)
    }
}

impl std::fmt::Debug for IterationBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IterationBudget")
            .field("max_total", &self.max_total)
            .field("used", &self.used())
            .field("remaining", &self.remaining())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_budget_consume() {
        let budget = IterationBudget::new(3);
        assert!(budget.consume());
        assert!(budget.consume());
        assert!(budget.consume());
        assert!(!budget.consume()); // exhausted
    }

    #[test]
    fn test_budget_remaining() {
        let budget = IterationBudget::new(10);
        assert_eq!(budget.remaining(), 10);
        budget.consume();
        budget.consume();
        assert_eq!(budget.remaining(), 8);
        assert_eq!(budget.used(), 2);
    }

    #[test]
    fn test_budget_refund() {
        let budget = IterationBudget::new(3);
        budget.consume();
        budget.consume();
        assert_eq!(budget.used(), 2);
        budget.refund();
        assert_eq!(budget.used(), 1);
        // Can now consume one more
        assert!(budget.consume());
        assert!(budget.consume());
        assert!(!budget.consume());
    }

    #[test]
    fn test_budget_reset() {
        let budget = IterationBudget::new(5);
        for _ in 0..5 {
            budget.consume();
        }
        assert_eq!(budget.remaining(), 0);
        budget.reset();
        assert_eq!(budget.remaining(), 5);
    }

    #[test]
    fn test_budget_debug() {
        let budget = IterationBudget::new(10);
        let debug = format!("{:?}", budget);
        assert!(debug.contains("max_total"));
        assert!(debug.contains("remaining"));
    }

    #[test]
    fn test_budget_zero_max() {
        let budget = IterationBudget::new(0);
        assert!(!budget.consume());
        assert_eq!(budget.remaining(), 0);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn test_budget_refund_at_zero() {
        // Refunding when nothing has been used should not panic or underflow
        let budget = IterationBudget::new(5);
        budget.refund();
        budget.refund();
        assert_eq!(budget.used(), 0);
        assert_eq!(budget.remaining(), 5);
    }

    #[test]
    fn test_budget_concurrent_races() {
        use std::sync::Arc;
        use std::thread;

        let budget = Arc::new(IterationBudget::new(100));
        let mut handles = vec![];

        // Spawn 10 threads, each consuming 15 times (150 total, but only 100 allowed)
        for _ in 0..10 {
            let b = Arc::clone(&budget);
            handles.push(thread::spawn(move || {
                let mut consumed = 0;
                for _ in 0..15 {
                    if b.consume() {
                        consumed += 1;
                    }
                }
                consumed
            }));
        }

        let total_consumed: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert_eq!(total_consumed, 100);
        assert_eq!(budget.remaining(), 0);
    }

    #[test]
    fn test_grace_call_set_and_take() {
        let budget = IterationBudget::new(2);
        budget.consume();
        budget.consume();
        assert!(!budget.consume()); // exhausted
        assert_eq!(budget.remaining(), 0);

        // Set grace call
        budget.set_grace_call();
        assert!(budget.has_grace_call());

        // take_grace_call consumes the flag
        assert!(budget.take_grace_call());
        assert!(!budget.has_grace_call()); // consumed

        // Second take returns false
        assert!(!budget.take_grace_call());
    }

    #[test]
    fn test_grace_call_single_use() {
        let budget = IterationBudget::new(1);
        budget.consume();
        budget.set_grace_call();

        // First take consumes the flag
        assert!(budget.take_grace_call());
        // Second take is false — single use
        assert!(!budget.take_grace_call());
    }

    #[test]
    fn test_grace_call_default_false() {
        let budget = IterationBudget::new(10);
        assert!(!budget.has_grace_call());
        assert!(!budget.take_grace_call());
    }
}
