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
//! * `dt > 2 * interval_ms` is the gap threshold (same rule as `session.py`).
//!
//! The cadence is **measured**, not assumed: live captures put the steady-state
//! heartbeat at a 2007 ms median (121 frames in `control_0009`, 114 of 118 in
//! 1900..2150 ms), so [`DEFAULT_INTERVAL_MS`] is that value. With a capture tap
//! attached the interval is re-learned from the wire on every frame; the
//! synthetic path (`beat`) still uses it as a timer.

/// Measured cadence of the real heartbeat (median of live captures).
pub const DEFAULT_INTERVAL_MS: u64 = 2_007;
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
    /// Frame size this beat describes: the model frame for a synthesized beat,
    /// the real captured size (512) for an observed one.
    pub frame_len: u32,
    /// True when the beat was read off the wire instead of synthesized.
    pub observed: bool,
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
    /// Heartbeats read off the wire (capture tap) rather than synthesized.
    pub observed: u64,
    pub interval_ms: u64,
    pub last_dt_ms: Option<u64>,
    pub gaps: u64,
    /// Size of the last frame this model described.
    pub frame_len: u32,
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
    observed: u64,
    last_hb_ms: Option<u64>,
    last_beat: Option<Beat>,
    last_frame_len: u32,
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
            observed: 0,
            last_hb_ms: None,
            last_beat: None,
            last_frame_len: 0,
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
            observed: self.observed,
            interval_ms: self.interval_ms,
            last_dt_ms: self.last_beat.and_then(|beat| beat.dt_ms),
            gaps: self.gaps,
            frame_len: self.last_frame_len,
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
        self.last_frame_len = crate::codec::FRAME_LEN as u32;
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
            frame_len: crate::codec::FRAME_LEN as u32,
            observed: false,
        };
        self.last_beat = Some(beat);
        if gap {
            self.gaps = self.gaps.saturating_add(1);
        }
        beat
    }

    /// Record one heartbeat that actually went out on the wire.
    ///
    /// The frame carries the truth the synthetic model had to guess: the real
    /// `SessionId`, the real `Sequence` and the transport's own inter-arrival
    /// time. A session id different from the current one opens a new epoch by
    /// itself, so a world switch is visible even when no `BeginSession` hook
    /// observation reached this image.
    ///
    /// `interval_ms` is re-learned from the capture on every frame whose `dt` is
    /// not a gap, which is what makes `eac.status` report the measured cadence
    /// (2007 ms) instead of a configured guess.
    pub fn record_observed(
        &mut self,
        now_ms: u64,
        session_id: u64,
        seq: u64,
        dt_ms: u64,
        frame_len: u32,
    ) -> Beat {
        if !self.running || (session_id != 0 && session_id != self.session_id) {
            self.epoch = self.epoch.saturating_add(1);
            self.session_id = session_id;
            self.running = true;
            self.seq = 0;
            self.hb_seq = 0;
            self.hb_in_epoch = 0;
            // The first frame of an epoch must not inherit the previous
            // session's timestamp as if it were a cadence sample.
            self.last_hb_ms = None;
        }

        let local_dt = self.last_hb_ms.map(|last| now_ms.saturating_sub(last));
        // The capture's `dt` is transport truth; the local clock is a fallback
        // for a record that lost it (first frame of a file, rotation).
        let dt = if dt_ms != 0 { Some(dt_ms) } else { local_dt };
        let gap = dt.is_some_and(|dt| dt > 2 * self.interval_ms);
        if let Some(measured) = dt.filter(|dt| {
            !gap && (MIN_INTERVAL_MS..=MAX_INTERVAL_MS).contains(dt)
        }) {
            self.interval_ms = measured;
        }

        self.seq = seq;
        self.hb_seq = self.hb_seq.saturating_add(1);
        self.hb_in_epoch = self.hb_in_epoch.saturating_add(1);
        self.total_heartbeats = self.total_heartbeats.saturating_add(1);
        self.observed = self.observed.saturating_add(1);
        self.last_hb_ms = Some(now_ms);
        self.last_frame_len = frame_len;

        let beat = Beat {
            epoch: self.epoch,
            session_id: self.session_id,
            seq,
            hb_in_epoch: self.hb_in_epoch,
            dt_ms: dt,
            gap,
            timestamp_ms: now_ms,
            nonce: 0,
            frame_len,
            observed: true,
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

    #[test]
    fn the_default_cadence_is_the_measured_one() {
        assert_eq!(EacSession::default().interval_ms(), 2_007);
        assert_eq!(DEFAULT_INTERVAL_MS, 2_007);
    }

    #[test]
    fn observed_frames_carry_the_wire_truth_and_teach_the_interval() {
        let mut session = EacSession::default();
        let first = session.record_observed(1_000, 0x158b, 1, 0, 512);
        assert_eq!(first.epoch, 1, "a frame opens an epoch without BeginSession");
        assert!(first.observed);
        assert_eq!(first.session_id, 0x158b);
        assert_eq!(first.seq, 1, "the wire sequence is used as-is");
        assert_eq!(first.frame_len, 512);
        assert_eq!(first.dt_ms, None, "the first frame has no predecessor");

        let second = session.record_observed(3_007, 0x158b, 2, 2_007, 512);
        assert_eq!(second.epoch, 1);
        assert_eq!(second.seq, 2);
        assert_eq!(second.dt_ms, Some(2_007));
        assert!(!second.gap);
        assert_eq!(session.interval_ms(), 2_007);

        // A world load: 26 s without a frame, then a fresh session id.
        let after_world = session.record_observed(29_007, 0x4d49, 1, 26_000, 512);
        assert_eq!(after_world.epoch, 2, "a new session id opens a new epoch");
        assert!(after_world.gap, "the stall must be reported as a gap");
        assert_eq!(after_world.seq, 1, "per-epoch sequence restarts");
        assert_eq!(
            session.interval_ms(),
            2_007,
            "a gap must never become the learned cadence"
        );

        let status = session.status();
        assert_eq!(status.observed, 3);
        assert_eq!(status.total_heartbeats, 3);
        assert_eq!(status.session_id, 0x4d49);
        assert_eq!(status.frame_len, 512);
        assert_eq!(status.gaps, 1);
    }

    #[test]
    fn a_synthesized_beat_reports_the_model_frame_and_an_observed_one_the_wire() {
        let mut session = EacSession::new(2_007);
        session.begin(0, Some(1));
        let model = session.beat(2_007);
        assert!(!model.observed);
        assert_eq!(model.frame_len as usize, crate::codec::FRAME_LEN);
        let wire = session.record_observed(4_014, 1, 2, 2_007, 512);
        assert!(wire.observed);
        assert_eq!(wire.frame_len as usize, crate::codec::WIRE_HEARTBEAT_LEN);
    }
}
