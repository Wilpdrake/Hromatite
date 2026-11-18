//! Epoch gate: a frame may only describe the session that is live **now**.
//!
//! The generator learned one 512-byte frame and kept rebuilding it. When VRChat
//! dropped the session and opened a new one (`BeginSession` 07:16:07, live run
//! 2026-11-18) it went on emitting 81 frames for the *old* `SessionId`
//! (`0x4466`), and the server answered `Authentication with the VRChat
//! Anti-Cheat system failed`. [`crate::eos`] resets its forge state on a
//! `BeginSession` it observes — but the payload is compiled into two images
//! (the resident `hromatite.dll` and the hot-reload feature image), each with
//! its own copy of those statics: an epoch reset in the image that owns the
//! hooks is invisible to the image whose thread owns the emitter.
//!
//! So the invariant cannot depend on a reset *event* reaching the emitter. It
//! is derived from evidence the emitter can always see for itself: a live
//! 512-byte frame carries the `SessionId` of the session the anti-cheat module
//! is really in. While that identity matches the template and keeps arriving,
//! emission is allowed; the first frame of another session, or a silence longer
//! than a few cadences, stops it.
//!
//! Everything here is a pure state machine (clock passed in), so the policy is
//! unit-testable on the host; [`crate::eos`] only feeds it.
//!
//! ```text
//! note_live(A) ─► arm(A) ─► verdict = Emit ──┐
//!                                  ▲        │ note_live(B)
//!                                  │        ▼
//!                        note_live(A)   Stop::ForeignSession (sticky)
//! ```
//!
//! A stop is *sticky*: the emitter must re-learn a template from the new
//! session before it can arm again, so a single stale frame cannot resume a
//! dead stream.

/// Smallest budget the model accepts, for callers that clamp configuration.
pub const MIN_STALE_MS: u64 = 500;
/// Default silence tolerated before emission is considered stale: four live
/// cadences (~2 s each), rounded up to a round number.
pub const DEFAULT_STALE_MS: u64 = 8_000;

/// Why emission was stopped. Every variant names both sides of the mismatch so
/// the log line is self-contained evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// A live frame of another session arrived while we were armed.
    ForeignSession { armed: u64, live: u64 },
    /// No live frame of the armed session within the budget.
    Stale { armed: u64, age_ms: u64 },
    /// `BeginSession` opened a new epoch after the arm.
    EpochReset {
        armed_generation: u64,
        now_generation: u64,
    },
}

impl Stop {
    /// Short reason token (`foreign`, `stale`, `reset`).
    pub fn reason(self) -> &'static str {
        match self {
            Self::ForeignSession { .. } => "foreign",
            Self::Stale { .. } => "stale",
            Self::EpochReset { .. } => "reset",
        }
    }

    /// One log line, no allocation beyond the format.
    pub fn line(self) -> String {
        match self {
            Self::ForeignSession { armed, live } => {
                format!("reason=foreign armed_sid=0x{armed:x} live_sid=0x{live:x}")
            }
            Self::Stale { armed, age_ms } => {
                format!("reason=stale armed_sid=0x{armed:x} age_ms={age_ms}")
            }
            Self::EpochReset {
                armed_generation,
                now_generation,
            } => format!(
                "reason=reset armed_generation={armed_generation} now_generation={now_generation}"
            ),
        }
    }
}

/// What the emitter may do at `now_ms`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Not armed (nothing was taken over in this epoch).
    Idle,
    /// Armed and confirmed by a live frame of the same session.
    Emit,
    /// Emission must stop; the state stays stopped until a new [`Gate::arm`].
    Stop(Stop),
}

impl Verdict {
    /// True for [`Verdict::Emit`].
    pub fn permits(self) -> bool {
        matches!(self, Self::Emit)
    }
}

/// A live 512-byte frame of the session the anti-cheat module is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Live {
    sid: u64,
    at_ms: u64,
}

/// The takeover this gate guards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Arm {
    sid: u64,
    at_ms: u64,
    generation: u64,
}

/// Epoch gate. Cheap enough to sit behind a `Mutex` in a hook handler.
#[derive(Clone, Debug)]
pub struct Gate {
    stale_ms: u64,
    generation: u64,
    arm: Option<Arm>,
    live: Option<Live>,
    stop: Option<Stop>,
}

impl Default for Gate {
    fn default() -> Self {
        Self::new(DEFAULT_STALE_MS)
    }
}

impl Gate {
    /// A disarmed gate that tolerates `stale_ms` of silence.
    pub const fn new(stale_ms: u64) -> Self {
        Self {
            stale_ms,
            generation: 0,
            arm: None,
            live: None,
            stop: None,
        }
    }

    /// Silence budget in milliseconds (used by status lines).
    pub fn stale_ms(&self) -> u64 {
        self.stale_ms
    }

    /// Change the silence budget; values below [`MIN_STALE_MS`] are raised.
    pub fn set_stale_ms(&mut self, stale_ms: u64) {
        self.stale_ms = stale_ms.max(MIN_STALE_MS);
    }

    /// Epoch counter, bumped by [`Self::reset`].
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Session the gate currently allows, if any.
    pub fn armed_sid(&self) -> Option<u64> {
        self.arm.map(|arm| arm.sid)
    }

    /// Session of the newest live frame, if any.
    pub fn live_sid(&self) -> Option<u64> {
        self.live.map(|live| live.sid)
    }

    /// The sticky stop reason, once emission was refused.
    pub fn stopped(&self) -> Option<Stop> {
        self.stop
    }

    /// Record a live 512-byte frame of session `sid`.
    ///
    /// The gate does not stop here: the verdict is computed where the emitter
    /// asks for it, so an observation and a decision can never interleave into
    /// "emit one frame of the wrong session anyway".
    pub fn note_live(&mut self, sid: u64, now_ms: u64) {
        self.live = Some(Live { sid, at_ms: now_ms });
    }

    /// Take over the session `sid`. Refused (`false`) unless a live frame of
    /// *that* session was seen within the budget: a takeover of a session the
    /// module is not in is exactly the bug this gate exists for.
    pub fn arm(&mut self, sid: u64, now_ms: u64) -> bool {
        if !self.fresh(sid, now_ms) {
            return false;
        }
        self.arm = Some(Arm {
            sid,
            at_ms: now_ms,
            generation: self.generation,
        });
        self.stop = None;
        true
    }

    /// Forget the takeover without bumping the epoch (a stopped emitter).
    pub fn disarm(&mut self) {
        self.arm = None;
    }

    /// `BeginSession`: a new epoch, nothing armed, nothing known.
    pub fn reset(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.arm = None;
        self.live = None;
        self.stop = None;
    }

    /// Decide for `now_ms`. Pure: repeated calls return the same verdict.
    pub fn verdict(&self, now_ms: u64) -> Verdict {
        if let Some(stop) = self.stop {
            return Verdict::Stop(stop);
        }
        let Some(arm) = self.arm else {
            return Verdict::Idle;
        };
        if arm.generation != self.generation {
            return Verdict::Stop(Stop::EpochReset {
                armed_generation: arm.generation,
                now_generation: self.generation,
            });
        }
        let Some(live) = self.live else {
            return Verdict::Stop(Stop::Stale {
                armed: arm.sid,
                age_ms: now_ms.saturating_sub(arm.at_ms),
            });
        };
        if live.sid != arm.sid {
            return Verdict::Stop(Stop::ForeignSession {
                armed: arm.sid,
                live: live.sid,
            });
        }
        let age = now_ms.saturating_sub(live.at_ms);
        if age > self.stale_ms {
            return Verdict::Stop(Stop::Stale {
                armed: arm.sid,
                age_ms: age,
            });
        }
        Verdict::Emit
    }

    /// Latch the stop a caller acted on, so status lines and the next tick see
    /// the same decision.
    pub fn latched(&mut self, verdict: Verdict) -> Verdict {
        if let Verdict::Stop(stop) = verdict
            && self.stop.is_none()
        {
            self.stop = Some(stop);
        }
        verdict
    }

    /// One line for `eac.hooks` / `eac.status`.
    pub fn line(&self) -> String {
        format!(
            "eac.epoch: generation={} armed={} live={} stale_ms={} stop={}",
            self.generation,
            self.armed_sid()
                .map(|sid| format!("0x{sid:x}"))
                .unwrap_or_else(|| "-".to_string()),
            self.live_sid()
                .map(|sid| format!("0x{sid:x}"))
                .unwrap_or_else(|| "-".to_string()),
            self.stale_ms,
            self.stop
                .map(|stop| stop.line())
                .unwrap_or_else(|| "none".to_string()),
        )
    }

    /// True when a live frame of `sid` is no older than the budget.
    fn fresh(&self, sid: u64, now_ms: u64) -> bool {
        self.live.is_some_and(|live| {
            live.sid == sid && now_ms.saturating_sub(live.at_ms) <= self.stale_ms
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unarmed_gate_never_emits() {
        let mut gate = Gate::new(1_000);
        assert_eq!(gate.verdict(0), Verdict::Idle);
        gate.note_live(0xA, 10);
        assert_eq!(gate.verdict(20), Verdict::Idle, "no takeover was asked for");
    }

    #[test]
    fn emission_needs_a_live_frame_of_the_armed_session() {
        let mut gate = Gate::new(1_000);
        gate.note_live(0xA, 100);
        assert!(gate.arm(0xA, 110), "armed on fresh evidence");
        assert_eq!(gate.verdict(120), Verdict::Emit);
        assert_eq!(gate.verdict(1_100), Verdict::Emit, "exactly one budget");
        assert_eq!(
            gate.verdict(1_101),
            Verdict::Stop(Stop::Stale {
                armed: 0xA,
                age_ms: 1_001
            })
        );
    }

    #[test]
    fn a_frame_of_another_session_stops_the_emitter_for_good() {
        let mut gate = Gate::new(10_000);
        gate.note_live(0x4466, 1_000);
        assert!(gate.arm(0x4466, 1_000));
        assert_eq!(gate.verdict(3_000), Verdict::Emit);

        // The live module moved to a new session: its next heartbeat says so.
        gate.note_live(0x7a1b, 5_000);
        let stop = Stop::ForeignSession {
            armed: 0x4466,
            live: 0x7a1b,
        };
        assert_eq!(gate.verdict(5_000), Verdict::Stop(stop));
        assert_eq!(
            gate.verdict(5_200),
            Verdict::Stop(stop),
            "a stop must not heal because the new session is live"
        );
        assert_eq!(gate.latched(Verdict::Stop(stop)), Verdict::Stop(stop));
        assert_eq!(gate.stopped(), Some(stop));
        assert!(gate.line().contains("live_sid=0x7a1b"));

        // Only a fresh arm of the *new* session resumes emission.
        assert!(gate.arm(0x7a1b, 5_200));
        assert_eq!(gate.verdict(5_200), Verdict::Emit);
        assert_eq!(gate.stopped(), None);
    }

    #[test]
    fn a_begin_session_reset_cannot_resume_the_old_sid() {
        let mut gate = Gate::new(10_000);
        gate.note_live(0x4466, 1_000);
        assert!(gate.arm(0x4466, 1_000));
        assert_eq!(gate.generation(), 0);

        gate.reset();
        assert_eq!(gate.generation(), 1);
        assert_eq!(gate.verdict(2_000), Verdict::Idle, "nothing armed");

        // The old identity is gone with the epoch, so re-arming it is refused
        // until a live frame of it shows up again.
        assert!(!gate.arm(0x4466, 2_000));
        gate.note_live(0x4466, 2_100);
        assert!(gate.arm(0x4466, 2_100));
    }

    #[test]
    fn the_arm_generation_is_checked_even_without_an_explicit_reset() {
        let mut gate = Gate::new(10_000);
        gate.note_live(0x11, 1_000);
        assert!(gate.arm(0x11, 1_000));
        // Simulate an arm that outlived its epoch (a copied static, a reused
        // struct): the generation no longer matches.
        gate.generation = 3;
        assert_eq!(
            gate.verdict(1_000),
            Verdict::Stop(Stop::EpochReset {
                armed_generation: 0,
                now_generation: 3
            })
        );
    }

    #[test]
    fn arming_without_fresh_evidence_is_refused() {
        let mut gate = Gate::new(1_000);
        assert!(!gate.arm(0xAA, 0), "nothing learned yet");
        gate.note_live(0xAA, 0);
        assert!(!gate.arm(0xAA, 2_000), "the learned frame is too old");
        assert!(!gate.arm(0xBB, 500), "another session is not armed either");
        gate.note_live(0xAA, 2_500);
        assert!(gate.arm(0xAA, 2_600));
    }

    #[test]
    fn the_budget_is_raised_to_the_model_minimum() {
        let mut gate = Gate::new(0);
        gate.set_stale_ms(1);
        assert_eq!(gate.stale_ms(), MIN_STALE_MS);
        gate.set_stale_ms(DEFAULT_STALE_MS);
        assert_eq!(gate.stale_ms(), DEFAULT_STALE_MS);
    }
}
