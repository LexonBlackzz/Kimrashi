use std::sync::Mutex;
use std::time::Instant;

struct ClockState {
    anchor: Option<Instant>,
    offset_us: u64,
    /// Song microseconds per wall microsecond (playback speed).
    speed: f64,
}

pub struct PlaybackClock {
    state: Mutex<ClockState>,
}

impl PlaybackClock {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(ClockState {
                anchor: None,
                offset_us: 0,
                speed: 1.0,
            }),
        }
    }

    /// Set the clock to a running state anchored to `Instant::now()` at `current_us`.
    pub fn set_running(&self, current_us: u64) {
        let mut state = self.state.lock().unwrap();
        state.offset_us = current_us;
        state.anchor = Some(Instant::now());
    }

    /// Freeze the clock at `current_us` (used for pause/stop).
    pub fn set_paused(&self, current_us: u64) {
        let mut state = self.state.lock().unwrap();
        state.offset_us = current_us;
        state.anchor = None;
    }

    /// Reset the clock back to zero and paused.
    pub fn reset(&self) {
        let mut state = self.state.lock().unwrap();
        state.offset_us = 0;
        state.anchor = None;
    }

    /// Change playback speed without jumping: re-anchor at the current position.
    pub fn set_speed(&self, speed: f64) {
        let mut state = self.state.lock().unwrap();
        if let Some(anchor) = state.anchor {
            state.offset_us = Self::position(&state, anchor);
            state.anchor = Some(Instant::now());
        }
        state.speed = speed;
    }

    pub fn speed(&self) -> f64 {
        self.state.lock().unwrap().speed
    }

    /// Returns the current playback time in microseconds according to the shared clock.
    pub fn now_us(&self) -> u64 {
        let state = self.state.lock().unwrap();
        match state.anchor {
            Some(anchor) => Self::position(&state, anchor),
            None => state.offset_us,
        }
    }

    fn position(state: &ClockState, anchor: Instant) -> u64 {
        let elapsed = anchor.elapsed().as_secs_f64() * 1e6 * state.speed;
        state.offset_us.saturating_add(elapsed as u64)
    }

    pub fn is_paused(&self) -> bool {
        self.state.lock().unwrap().anchor.is_none()
    }
}
