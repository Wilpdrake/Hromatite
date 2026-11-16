//! Anti-cheat session epochs: one EAC session per VRChat room.
//!
//! Verified from a real VRChat log: 18 world joins produced 18 `BeginSession`
//! and 17 `EndSession` calls, in the order `OnLeftRoom` -> `EndSession` ->
//! `BeginSession`. The anti-cheat client therefore has a *new session id per
//! room*, and the heartbeat cadence restarts with it.
//!
//! This model keeps two clocks:
//!
//! * `seq` / `hb_seq` / `hb_in_epoch` are **per epoch** — a new session starts
//!   its own frame sequence, as the SDK does.
//! * `last_hb_ms` deliberately **survives across epochs**, so the first
//!   heartbeat after a world load reports the full stall as `dt_ms` and the log
//!   shows the world transition as a `gap` record.
//!
//! `dt > 2 * interval_ms` is the gap threshold (same rule as `session.py`).

/// Default cadence of the reference model. The real EAC interval is unknown;
/// 5 s is what `session.py` assumes and what the log format was designed around.
pub const DEFAULT_INTERVAL_MS: u64 = 5_000;
/// Minimum accepted interval for manual configuration.
pub const MIN_INTERVAL_MS: u64 = 200;
/// Maximum accepted interval for manual configuration.
pub const MAX_INTERVAL_MS: u64 = 60_000;

/// One heartbeat, as observed by the session model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Beat {
    pub epoch: u64,
    pub session_id: u64,
    /// Frame sequence inside this epoch (starts at 1).
    pub seq: u64,
    /// Payload sequence inside this epoch (starts at 1).
    pub hb_in_epoch: u32,
    /// Milliseconds since the previous heartbeat of *any* epoch.
    pub dt_ms: Option<u64>,
    /// True when `dt_ms > 2 * interval_ms`.
    pub gap: bool,
    pub timestamp_ms: u64,
    pub nonce: u64,
}

/// Result of `begin`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Begin {
    pub epoch: u64,
    pub session_id: u64,
    pub timestamp_ms: u64,
}

/// Result of `end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct End {
    pub epoch: u64,
    pub heartbeats: u32,
    pub total_heartbeats: u64,
    pub timestamp_ms: u64,
}

/// Snapshot for `eac.status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub epoch: u64,
    pub session_id: u64,
    pub running: bool,
    pub seq: u64,
    pub hb_in_epoch: u32,
    pub total_heartbeats: u64,
    pub interval_ms: u64,
    pub last_dt_ms: Option<u64>,
    pub gaps: u64,
}

/// Epoch state machine. Pure: every method takes the clock as a parameter.
#[derive(Clone, Debug)]
pub struct EacSession {
    interval_ms: u64,
    epoch: u64,
    session_id: u64,
    running: bool,
    seq: u64,
    hb_seq: u64,
    hb_in_epoch: u32,
    total_heartbeats: u64,
    last_hb_ms: Option<u64>,
    last_beat: Option<Beat>,
    gaps: u64,
    nonce_counter: u64,
}

impl Default for EacSession {
    fn default() -> Self {
        Self::new(DEFAULT_INTERVAL_MS)
    }
}

impl EacSession {
    /// Create an idle session with the given nominal cadence.
    pub fn new(interval_ms: u64) -> Self {
        Self {
            interval_ms: interval_ms.clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS),
            epoch: 0,
            session_id: 0,
            running: false,
            seq: 0,
            hb_seq: 0,
            hb_in_epoch: 0,
            total_heartbeats: 0,
            last_hb_ms: None,
            last_beat: None,
            gaps: 0,
            nonce_counter: 0,
        }
    }

    /// Nominal interval in milliseconds.
    pub fn interval_ms(&self) -> u64 {
        self.interval_ms
    }

    /// Change the cadence; clamped to `MIN_INTERVAL_MS..=MAX_INTERVAL_MS`.
    pub fn set_interval_ms(&mut self, interval_ms: u64) {
        self.interval_ms = interval_ms.clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS);
    }

    /// Current epoch (0 before the first `begin`).
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Current session id (0 before the first `begin`).
    pub fn session_id(&self) -> u64 {
        self.session_id
    }

    /// True between `begin` and `end`.
    pub fn running(&self) -> bool {
        self.running
    }

    /// Snapshot of all counters.
    pub fn status(&self) -> Status {
        Status {
            epoch: self.epoch,
            session_id: self.session_id,
            running: self.running,
            seq: self.seq,
            hb_in_epoch: self.hb_in_epoch,
            total_heartbeats: self.total_heartbeats,
            interval_ms: self.interval_ms,
            last_dt_ms: self.last_beat.and_then(|beat| beat.dt_ms),
            gaps: self.gaps,
        }
    }

    /// Start a new epoch. An already-running epoch is reported by the caller
    /// (the model simply opens the next one and resets per-epoch counters).
    pub fn begin(&mut self, now_ms: u64, session_id: Option<u64>) -> Begin {
        self.epoch = self.epoch.saturating_add(1);
        self.session_id = session_id.unwrap_or_else(|| derive_session_id(now_ms, self.epoch));
        self.running = true;
        self.seq = 0;
        self.hb_seq = 0;
        self.hb_in_epoch = 0;
        Begin {
            epoch: self.epoch,
            session_id: self.session_id,
            timestamp_ms: now_ms,
        }
    }

    /// Close the current epoch. `None` when nothing was running.
    pub fn end(&mut self, now_ms: u64) -> Option<End> {
        if !self.running {
            return None;
        }
        self.running = false;
        Some(End {
            epoch: self.epoch,
            heartbeats: self.hb_in_epoch,
            total_heartbeats: self.total_heartbeats,
            timestamp_ms: now_ms,
        })
    }

    /// True when a heartbeat is due at `now_ms` (or none was ever sent).
    pub fn due(&self, now_ms: u64) -> bool {
        self.running
            && self
                .last_hb_ms
                .is_none_or(|last| now_ms.saturating_sub(last) >= self.interval_ms)
    }

    /// Record one heartbeat. Caller decides *when*; this only models the result.
    pub fn beat(&mut self, now_ms: u64) -> Beat {
        let dt_ms = self.last_hb_ms.map(|last| now_ms.saturating_sub(last));
        let gap = dt_ms.is_some_and(|dt| dt > 2 * self.interval_ms);
        self.seq = self.seq.saturating_add(1);
        self.hb_seq = self.hb_seq.saturating_add(1);
        self.hb_in_epoch = self.hb_in_epoch.saturating_add(1);
        self.total_heartbeats = self.total_heartbeats.saturating_add(1);
        self.last_hb_ms = Some(now_ms);
        self.nonce_counter = self.nonce_counter.wrapping_add(1);
        let nonce = now_ms ^ self.nonce_counter.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let beat = Beat {
            epoch: self.epoch,
            session_id: self.session_id,
            seq: self.seq,
            hb_in_epoch: self.hb_in_epoch,
            dt_ms,
            gap,
            timestamp_ms: now_ms,
            nonce,
        };
        self.last_beat = Some(beat);
        if gap {
            self.gaps = self.gaps.saturating_add(1);
        }
        beat
    }

    /// Payload sequence inside the epoch (what the 120-byte body carries).
    pub fn hb_seq(&self) -> u64 {
        self.hb_seq
    }
}

/// Placeholder session id when the caller has none. **HYPOTHESIS:** the real
/// id comes from the EOS socket handshake; until phase 2 hooks install, any
/// non-zero 64-bit value is as good as another.
pub fn derive_session_id(now_ms: u64, epoch: u64) -> u64 {
    now_ms.wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(17)
        ^ epoch.wrapping_mul(0xbf58_476d_1ce4_e5b9)
        | 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gap_is_measured_across_epochs_so_a_world_load_is_visible() {
        let mut session = EacSession::new(5_000);
        let begin = session.begin(1_000, Some(0xaa));
        assert_eq!(begin.epoch, 1);
        let first = session.beat(1_500);
        assert_eq!(first.dt_ms, None);
        assert!(!first.gap);
        assert_eq!(first.seq, 1);
        assert_eq!(first.hb_in_epoch, 1);

        let second = session.beat(6_500);
        assert_eq!(second.dt_ms, Some(5_000));
        assert!(!second.gap, "exactly one interval is not a gap");

        let end = session.end(7_000).expect("running session");
        assert_eq!(end.heartbeats, 2);

        // A world load: new epoch, heartbeat arrives 12.5 s after the last one.
        session.begin(19_000, Some(0xbb));
        let after_world = session.beat(19_000);
        assert_eq!(after_world.epoch, 2);
        assert_eq!(after_world.seq, 1, "per-epoch frame sequence restarts");
        assert_eq!(after_world.hb_in_epoch, 1, "per-epoch counter restarts");
        assert_eq!(after_world.dt_ms, Some(12_500));
        assert!(after_world.gap, "world-load stall must be flagged as a gap");
        assert_eq!(session.status().gaps, 1);
        assert_eq!(session.status().total_heartbeats, 3);
    }

    #[test]
    fn end_is_idempotent_and_due_follows_the_interval() {
        let mut session = EacSession::new(1_000);
        assert!(session.end(5).is_none());
        assert!(!session.due(0), "an epoch that never began is never due");

        session.begin(0, None);
        assert!(session.due(100));
        session.beat(100);
        assert!(!session.due(1_099));
        assert!(session.due(1_100));

        assert!(session.end(1_200).is_some());
        assert!(session.end(1_300).is_none());
        assert!(!session.due(9_999), "an ended epoch is never due");
    }

    #[test]
    fn interval_is_clamped_and_session_ids_are_non_zero() {
        let mut session = EacSession::new(1);
        assert_eq!(session.interval_ms(), MIN_INTERVAL_MS);
        session.set_interval_ms(999_999);
        assert_eq!(session.interval_ms(), MAX_INTERVAL_MS);
        assert_ne!(derive_session_id(0, 0), 0);
        assert_ne!(derive_session_id(1, 1), derive_session_id(2, 2));
    }
}
