//! Pure generation and hot-configuration contracts, testable on the host.

#![forbid(unsafe_code)]

/// Resident ownership state for one feature generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GenerationState {
    Active,
    Previous,
    Draining,
    Loading,
    Failed,
    Retired,
}
