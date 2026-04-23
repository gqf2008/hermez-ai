#![allow(dead_code)]
//! Shared interrupt signaling.
//!
//! Global `AtomicBool` that any tool can poll during long-running operations.
//! Mirrors the Python `tools/interrupt.py`.

use std::sync::atomic::{AtomicBool, Ordering};

static INTERRUPT_FLAG: AtomicBool = AtomicBool::new(false);

/// Set the interrupt flag. Called by the agent loop when the user
/// requests an interrupt.
pub fn set_interrupt(active: bool) {
    INTERRUPT_FLAG.store(active, Ordering::SeqCst);
}

/// Check if an interrupt has been requested.
pub fn is_interrupted() -> bool {
    INTERRUPT_FLAG.load(Ordering::SeqCst)
}

/// Reset the interrupt flag. Called at the start of each conversation turn.
pub fn clear_interrupt() {
    INTERRUPT_FLAG.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_interrupt_lifecycle() {
        clear_interrupt();
        assert!(!is_interrupted());
        set_interrupt(true);
        assert!(is_interrupted());
        clear_interrupt();
        assert!(!is_interrupted());
    }
}
