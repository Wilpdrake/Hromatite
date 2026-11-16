//! Wall-clock milliseconds. The Python model also timestamps records in UNIX ms,
//! so both logs can be interleaved and compared directly.

use std::time::{SystemTime, UNIX_EPOCH};

/// UNIX epoch milliseconds, or `0` if the system clock predates 1970.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}
