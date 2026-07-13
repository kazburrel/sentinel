#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionEdge {
    Detected,
    Stopped,
}

/// Tracks a PIR digital input and reports edges, not raw level, so callers
/// only react once per state change instead of on every poll.
pub struct MotionSensor {
    was_high: bool,
}

impl MotionSensor {
    pub fn new(initial_high: bool) -> Self {
        Self {
            was_high: initial_high,
        }
    }

    pub fn update(&mut self, is_high: bool) -> Option<MotionEdge> {
        let edge = if is_high && !self.was_high {
            Some(MotionEdge::Detected)
        } else if !is_high && self.was_high {
            Some(MotionEdge::Stopped)
        } else {
            None
        };
        self.was_high = is_high;
        edge
    }
}

/// Gates re-arming after an event ends: the area must stay quiet (PIR low)
/// for a full `quiet_duration_ms` before re-arming is allowed -- any PIR
/// high reading resets the quiet timer. Without this, a brief PIR gap
/// during one continuous real motion episode (the AM312's own ~2s
/// post-trigger blocking period, or someone briefly pausing) would let
/// `TAIL_DURATION` alone end the current event, and the very next high
/// reading would immediately start a whole new one -- fragmenting one
/// real episode into a burst of separate uploads/`analysis.json` files.
/// This closes that gap by requiring a real, longer quiet period before
/// treating the next motion as a new episode at all.
///
/// Takes plain millisecond timestamps rather than `embassy_time::Instant`
/// so this logic has zero hardware/executor dependency and is exercised
/// with plain integers below. These tests can't run via `cargo test` in
/// this crate specifically -- `firmware`'s target is pinned to
/// `xtensa-esp32s3-none-elf` (see `.cargo/config.toml`), and sibling
/// modules (`camera.rs`, `ws2812.rs`, ...) unconditionally depend on
/// `esp_hal`, which won't compile for a host target at all, so the whole
/// crate can't be built for `cargo test`'s default host target either.
/// Verified instead by copying this exact logic into a standalone
/// throwaway file and running it as a plain host binary (`rustc
/// rearm_gate_check.rs && ./rearm_gate_check`) before porting it here --
/// all four cases below passed there.
pub struct RearmGate {
    quiet_duration_ms: u64,
    quiet_since_ms: Option<u64>,
}

impl RearmGate {
    /// Starts the gate at `now_ms`. Callers only ever construct this right
    /// after an event's tail has expired, which guarantees PIR was already
    /// low at that instant -- so the gate always starts already counting
    /// down, never needing an initial "is PIR high" check of its own.
    pub fn start(quiet_duration_ms: u64, now_ms: u64) -> Self {
        Self {
            quiet_duration_ms,
            quiet_since_ms: Some(now_ms),
        }
    }

    /// Call on every poll with the current PIR level and tick. Returns
    /// `true` once the area has been continuously quiet for
    /// `quiet_duration_ms` since the most recent high reading (or since
    /// `start`, if there's been none) -- once `true`, the gate has served
    /// its purpose and the caller should re-arm a fresh `MotionSensor`.
    pub fn update(&mut self, pir_is_high: bool, now_ms: u64) -> bool {
        if pir_is_high {
            self.quiet_since_ms = None;
            false
        } else {
            let since = *self.quiet_since_ms.get_or_insert(now_ms);
            now_ms.saturating_sub(since) >= self.quiet_duration_ms
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_exactly_at_the_quiet_duration_boundary() {
        let mut gate = RearmGate::start(15_000, 0);
        assert!(!gate.update(false, 5_000));
        assert!(!gate.update(false, 14_999));
        assert!(gate.update(false, 15_000));
    }

    #[test]
    fn motion_during_cooldown_resets_the_quiet_timer() {
        let mut gate = RearmGate::start(15_000, 0);
        assert!(!gate.update(false, 14_000));
        assert!(!gate.update(true, 14_500)); // motion resets it
        assert!(!gate.update(false, 20_000)); // quiet timer restarts here
        assert!(!gate.update(false, 34_999));
        assert!(gate.update(false, 35_000)); // 15s after the 20_000 restart
    }

    #[test]
    fn sustained_toggling_never_opens_the_gate() {
        let mut gate = RearmGate::start(15_000, 0);
        let mut t = 0u64;
        for _ in 0..50 {
            t += 2_000;
            assert!(!gate.update(true, t), "toggling motion must never open the gate");
            t += 500;
            assert!(!gate.update(false, t));
        }
    }

    #[test]
    fn undisturbed_quiet_period_opens_after_exactly_quiet_duration_ms() {
        let mut gate = RearmGate::start(15_000, 1_000);
        for ms in (1_000..16_000).step_by(500) {
            let should_be_open = ms - 1_000 >= 15_000;
            assert_eq!(gate.update(false, ms), should_be_open, "at ms={ms}");
        }
    }
}
