//! Per-frame hook: replaces the long-lived payload thread with a trampoline
//! inside a function the game already calls every frame.
//!
//! Scaffold only: the real implementation lands with plan 3.

/// Console extension: handle `hook …` / `tick` style commands. `None` = "not
/// mine", let `console.rs` report an unknown command. Keeps hook code out of
/// `console.rs` (that file belongs to the api wave).
pub fn command(_line: &str) -> Option<Vec<String>> {
    None
}

/// True once the trampoline is installed and the game is ticking us.
pub fn installed() -> bool {
    false
}
