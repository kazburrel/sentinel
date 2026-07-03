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
