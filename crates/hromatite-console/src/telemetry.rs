//! Conservative telemetry scheduling over the existing append-only channel.

use std::time::{Duration, Instant};

const POSITION_INTERVAL: Duration = Duration::from_secs(1);
const SUMMARY_INTERVAL: Duration = Duration::from_secs(5);
const STALE_AFTER: u64 = 10;

pub struct AutoRefresh {
    enabled: bool,
    suspended: bool,
    next_position: Instant,
    next_summary: Instant,
}

impl AutoRefresh {
    pub fn new(now: Instant) -> Self {
        Self {
            enabled: false,
            suspended: false,
            next_position: now,
            next_summary: now,
        }
    }

    pub fn set_enabled(&mut self, enabled: bool, now: Instant) {
        if self.enabled != enabled {
            self.next_position = now;
            self.next_summary = now;
        }
        self.enabled = enabled;
        self.suspended = false;
    }

    pub fn suspended(&self) -> bool {
        self.suspended
    }

    pub fn poll(&mut self, now: Instant, log_age_secs: Option<u64>) -> Vec<&'static str> {
        if !self.enabled {
            return Vec::new();
        }
        if log_age_secs.is_none_or(|age| age > STALE_AFTER) {
            self.suspended = true;
            return Vec::new();
        }
        if self.suspended {
            self.next_position = now;
            self.next_summary = now;
        }
        self.suspended = false;

        let mut commands = Vec::with_capacity(4);
        if now >= self.next_position {
            commands.push("pos");
            self.next_position = now + POSITION_INTERVAL;
        }
        if now >= self.next_summary {
            commands.extend(["who", "late", "gct"]);
            self.next_summary = now + SUMMARY_INTERVAL;
        }
        commands
    }
}

#[cfg(test)]
mod tests {
    use super::AutoRefresh;
    use std::time::{Duration, Instant};

    #[test]
    fn auto_refresh_is_rate_limited_and_stale_logs_suspend_it() {
        let start = Instant::now();
        let mut auto = AutoRefresh::new(start);
        assert!(auto.poll(start, Some(0)).is_empty());

        auto.set_enabled(true, start);
        assert_eq!(auto.poll(start, Some(0)), ["pos", "who", "late", "gct"]);
        assert!(
            auto.poll(start + Duration::from_millis(100), Some(0))
                .is_empty()
        );
        assert_eq!(auto.poll(start + Duration::from_secs(1), Some(1)), ["pos"]);

        assert!(
            auto.poll(start + Duration::from_secs(2), Some(11))
                .is_empty()
        );
        assert!(auto.suspended());
        assert!(
            auto.poll(start + Duration::from_secs(3), Some(12))
                .is_empty()
        );

        let resumed = auto.poll(start + Duration::from_secs(4), Some(0));
        assert_eq!(resumed, ["pos", "who", "late", "gct"]);
        assert!(!auto.suspended());
        assert!(
            auto.poll(start + Duration::from_millis(4500), Some(1))
                .is_empty()
        );
    }
}
