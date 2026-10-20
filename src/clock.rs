use std::sync::atomic::{fence, AtomicU64, Ordering::*};
use std::sync::Mutex;
use std::time::Instant;

/// The playback position, shared by the player, the decoder, the UI and the SVMS monitor.
/// Changes are rare and reads are constant, so readers take no lock: a sequence counter
/// (odd while a change is being written) lets them retry instead.
pub struct PlaybackClock {
    base: Instant,
    /// Odd while a writer is inside.
    seq: AtomicU64,
    /// Nanoseconds after `base` the clock was anchored at; `PAUSED` if it isn't running.
    anchor_ns: AtomicU64,
    offset_us: AtomicU64,
    /// Song microseconds per wall microsecond (playback speed), as f64 bits.
    speed: AtomicU64,
    /// Keeps writers apart.
    writer: Mutex<()>,
    /// How far behind its schedule the synth is rendering events (wall µs), as the SVMS
    /// monitor sees it; 0 if on time or unknown. Taking events instantly says nothing
    /// about rendering them, so the skipper learns the synth's real price per note from it.
    pub synth_late_us: AtomicU64,
    /// Counts reports of `synth_late_us`, so the skipper acts once per report.
    pub synth_late_seq: AtomicU64,
}

const PAUSED: u64 = u64::MAX;

#[derive(Clone, Copy)]
struct State {
    anchor_ns: u64,
    offset_us: u64,
    speed: f64,
}

impl PlaybackClock {
    pub fn new() -> Self {
        Self {
            base: Instant::now(),
            seq: AtomicU64::new(0),
            anchor_ns: AtomicU64::new(PAUSED),
            offset_us: AtomicU64::new(0),
            speed: AtomicU64::new(1f64.to_bits()),
            writer: Mutex::new(()),
            synth_late_us: AtomicU64::new(0),
            synth_late_seq: AtomicU64::new(0),
        }
    }

    fn ns(&self) -> u64 {
        self.base.elapsed().as_nanos() as u64
    }

    fn read(&self) -> State {
        loop {
            let before = self.seq.load(Acquire);
            if before & 1 == 0 {
                let s = State {
                    anchor_ns: self.anchor_ns.load(Relaxed),
                    offset_us: self.offset_us.load(Relaxed),
                    speed: f64::from_bits(self.speed.load(Relaxed)),
                };
                fence(Acquire);
                if self.seq.load(Relaxed) == before {
                    return s;
                }
            }
            std::hint::spin_loop();
        }
    }

    /// Replace the state with what `change` makes of the current one.
    fn write(&self, change: impl FnOnce(State) -> State) {
        let _guard = self.writer.lock().unwrap();
        let s = change(self.read());
        let n = self.seq.load(Relaxed);
        self.seq.store(n + 1, Relaxed);
        fence(Release);
        self.anchor_ns.store(s.anchor_ns, Relaxed);
        self.offset_us.store(s.offset_us, Relaxed);
        self.speed.store(s.speed.to_bits(), Relaxed);
        self.seq.store(n + 2, Release);
    }

    fn position(&self, s: State) -> u64 {
        if s.anchor_ns == PAUSED {
            return s.offset_us;
        }
        let elapsed = self.ns().saturating_sub(s.anchor_ns) as f64 / 1e3 * s.speed;
        s.offset_us.saturating_add(elapsed as u64)
    }

    /// Set the clock to a running state anchored to now at `current_us`.
    pub fn set_running(&self, current_us: u64) {
        let now = self.ns();
        self.write(|s| State { anchor_ns: now, offset_us: current_us, ..s });
    }

    /// Freeze the clock at `current_us` (used for pause/stop).
    pub fn set_paused(&self, current_us: u64) {
        self.write(|s| State { anchor_ns: PAUSED, offset_us: current_us, ..s });
    }

    /// Reset the clock back to zero and paused.
    pub fn reset(&self) {
        self.set_paused(0);
    }

    /// Change playback speed without jumping: re-anchor at the current position.
    pub fn set_speed(&self, speed: f64) {
        let now = self.ns();
        self.write(|s| {
            if s.anchor_ns == PAUSED {
                State { speed, ..s }
            } else {
                State { anchor_ns: now, offset_us: self.position(s), speed, ..s }
            }
        });
    }

    pub fn speed(&self) -> f64 {
        self.read().speed
    }

    /// Returns the current playback time in microseconds according to the shared clock.
    pub fn now_us(&self) -> u64 {
        self.position(self.read())
    }

    /// Whether it's paused, the position and the speed, in one consistent read.
    pub fn snapshot(&self) -> (bool, u64, f64) {
        let s = self.read();
        (s.anchor_ns == PAUSED, self.position(s), s.speed)
    }

    pub fn is_paused(&self) -> bool {
        self.read().anchor_ns == PAUSED
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn runs_pauses_and_changes_speed_without_jumping() {
        let c = PlaybackClock::new();
        assert_eq!(c.snapshot(), (true, 0, 1.0));
        c.set_running(5_000_000);
        std::thread::sleep(Duration::from_millis(20));
        let t = c.now_us();
        assert!((5_020_000..5_200_000).contains(&t), "{t}");
        c.set_speed(10.0);
        let u = c.now_us();
        assert!(u >= t && u - t < 100_000, "{t} {u}");
        std::thread::sleep(Duration::from_millis(20));
        assert!(c.now_us() - u >= 190_000);
        c.set_paused(42);
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(c.snapshot(), (true, 42, 10.0));
        c.reset();
        assert_eq!(c.snapshot(), (true, 0, 10.0));
    }

    #[test]
    fn readers_never_see_a_torn_state() {
        // Every write keeps offset == 2 * speed's integer part; a reader mixing two writes would break it.
        let c = std::sync::Arc::new(PlaybackClock::new());
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let (c, stop) = (c.clone(), stop.clone());
                std::thread::spawn(move || {
                    while !stop.load(Relaxed) {
                        let s = c.read();
                        assert_eq!(s.offset_us as f64, s.speed * 2.0);
                    }
                })
            })
            .collect();
        for i in 1..200_000u64 {
            c.write(|s| State { offset_us: i * 2, speed: i as f64, ..s });
        }
        stop.store(true, Relaxed);
        for r in readers {
            r.join().unwrap();
        }
    }
}
