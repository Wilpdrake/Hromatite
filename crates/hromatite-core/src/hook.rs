//! Per-frame hook: replaces the long-lived payload thread with a trampoline
//! inside a function the game already calls every frame.
//!
//! Scaffold only: the real implementation lands with plan 3.

/// True once the trampoline is installed and the game is ticking us.
pub fn installed() -> bool {
    false
}
