//! Guarded calls: VEH-based exception handling for calling functions we have
//! not proven safe.
//!
//! `catch_unwind` cannot catch an access violation; only a vectored exception
//! handler can turn a fault inside a game function into an `Err`. The handler is
//! armed only while our own frame is on the stack (a flag + return-address
//! window), so the game's own exceptions are never swallowed.
//!
//! Scaffold only: the real implementation lands with plan 2.

/// True once the handler is installed in this process.
pub fn installed() -> bool {
    false
}
