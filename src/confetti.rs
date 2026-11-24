//! Confetti: when a note starts, its key throws a burst of paper pieces up into the notes; they
//! arc under gravity, tumble (their width swings as they flip) and shrink away. Moves with song
//! time, so exports are repeatable; a seek clears it.

use egui::Color32;

/// The adjustable look (see `Settings::confetti_*`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ConfettiLook {
    /// Pieces per note start.
    pub amount: u32,
    /// A piece's length, as a fraction of the note area's height.
    pub size: f32,
    /// Pull, in note-area heights per second squared.
    pub gravity: f32,
    /// Seconds a piece lives.
    pub life: f32,
    /// Random rainbow colours instead of the note's.
    pub rainbow: bool,
}

/// Most pieces at once: dense songs start notes on every key every frame.
pub const MAX: usize = 4000;

#[derive(Clone, Copy, Debug)]
struct Piece {
    /// Across the width (0..1) and up from the keyboard line (in note-area heights).
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    angle: f32,
    spin: f32,
    flip: f32,
    age: f32,
    color: [u8; 3],
}

pub struct Confetti {
    pieces: Vec<Piece>,
    was: [bool; 128],
    last_us: Option<u64>,
    rng: u64,
}

impl Default for Confetti {
    fn default() -> Self { Self { pieces: Vec::new(), was: [false; 128], last_us: None, rng: 0x5eed_c0ff_ee15_600d } }
}

impl Confetti {
    fn random(&mut self) -> f32 {
        // splitmix64
        self.rng = self.rng.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Moves the pieces on to `now_us` and throws a burst from each key whose note just started.
    /// `columns[p]` is key p's (left, right) as fractions of the width. Going back in time (a
    /// seek) starts over.
    pub fn update(&mut self, now_us: u64, active: &[bool; 128], colors: &[Color32; 128], columns: &[(f32, f32); 128], look: &ConfettiLook) {
        let dt = match self.last_us {
            Some(last) if now_us >= last => (now_us - last) as f32 / 1e6,
            _ => { let rng = self.rng; *self = Self { rng, ..Self::default() }; 0.0 }
        };
        self.last_us = Some(now_us);
        for q in &mut self.pieces {
            q.vy -= look.gravity * dt;
            q.x += q.vx * dt;
            q.y += q.vy * dt;
            q.angle += q.spin * dt;
            q.age += dt;
        }
        self.pieces.retain(|q| q.age < look.life && (q.y >= 0.0 || q.vy > 0.0));
        for p in 0..128 {
            let started = active[p] && !self.was[p];
            self.was[p] = active[p];
            if !started { continue; }
            let (l, r) = columns[p];
            let c = colors[p];
            let note = if c == Color32::TRANSPARENT { [255, 220, 140] } else { [c.r(), c.g(), c.b()] };
            for _ in 0..look.amount {
                if self.pieces.len() >= MAX { break; }
                let color = if look.rainbow {
                    let [r, g, b] = egui::ecolor::Hsva::new(self.random(), 0.9, 1.0, 1.0).to_srgb();
                    [r, g, b]
                } else { note };
                let piece = Piece {
                    x: l + (r - l) * self.random(),
                    y: 0.0,
                    vx: (self.random() - 0.5) * 0.3,
                    vy: 0.5 + 0.7 * self.random(),
                    angle: self.random() * std::f32::consts::TAU,
                    spin: (self.random() - 0.5) * 12.0,
                    flip: 4.0 + 8.0 * self.random(),
                    age: 0.0,
                    color,
                };
                self.pieces.push(piece);
            }
        }
    }

    pub fn len(&self) -> usize { self.pieces.len() }

    /// The pieces as filled quads over the note area, `left`/`width` across and up from `line`,
    /// `height` the note area's height in pixels. They tumble (width swings with the flip) and
    /// shrink away over the last third of their life.
    pub fn quads(&self, left: f32, width: f32, line: f32, height: f32, look: &ConfettiLook) -> Vec<crate::pfa::Quad> {
        self.pieces.iter().map(|q| {
            let shrink = ((look.life - q.age) / (look.life / 3.0)).clamp(0.0, 1.0);
            let len = look.size * height * shrink;
            let (hw, hh) = (len * 0.5 * (q.age * q.flip + q.angle).cos().abs().max(0.15), len * 0.3);
            let (cx, cy) = (left + q.x * width, line - q.y * height);
            let (s, c) = q.angle.sin_cos();
            let corner = |dx: f32, dy: f32| [cx + dx * c - dy * s, cy + dx * s + dy * c];
            crate::pfa::Quad { p: [corner(-hw, -hh), corner(hw, -hh), corner(hw, hh), corner(-hw, hh)], c: [q.color; 4] }
        }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOOK: ConfettiLook = ConfettiLook { amount: 6, size: 0.02, gravity: 1.5, life: 1.5, rainbow: false };

    fn columns() -> [(f32, f32); 128] { std::array::from_fn(|p| (p as f32 / 128.0, (p + 1) as f32 / 128.0)) }
    fn one_key(p: usize) -> [bool; 128] { let mut a = [false; 128]; a[p] = true; a }

    #[test]
    fn a_note_start_throws_a_burst_from_its_key_and_holding_throws_no_more() {
        let mut c = Confetti::default();
        c.update(0, &one_key(64), &[Color32::RED; 128], &columns(), &LOOK);
        assert_eq!(c.len(), 6);
        assert!(c.pieces.iter().all(|q| (0.5..64.0 / 128.0 + 1.0 / 128.0).contains(&q.x) && q.y == 0.0 && q.vy > 0.0 && q.color == [255, 0, 0]));
        c.update(10_000, &one_key(64), &[Color32::RED; 128], &columns(), &LOOK);
        assert_eq!(c.len(), 6, "held: no new burst");
        c.update(20_000, &[false; 128], &[Color32::RED; 128], &columns(), &LOOK);
        c.update(30_000, &one_key(64), &[Color32::RED; 128], &columns(), &LOOK);
        assert_eq!(c.len(), 12, "struck again: another burst");
    }

    #[test]
    fn pieces_rise_fall_and_die_and_a_seek_clears_them() {
        let mut c = Confetti::default();
        let (none, red) = ([false; 128], [Color32::RED; 128]);
        c.update(0, &one_key(64), &red, &columns(), &LOOK);
        c.update(100_000, &none, &red, &columns(), &LOOK);
        assert!(c.pieces.iter().all(|q| q.y > 0.0), "they rise");
        let top = c.pieces.iter().map(|q| q.vy).fold(f32::MIN, f32::max);
        c.update(300_000, &none, &red, &columns(), &LOOK);
        assert!(c.pieces.iter().map(|q| q.vy).fold(f32::MIN, f32::max) < top, "gravity slows them");
        c.update(2_000_000, &none, &red, &columns(), &LOOK);
        assert_eq!(c.len(), 0, "gone after their life (or back below the line)");
        c.update(0, &one_key(64), &red, &columns(), &LOOK);
        c.update(0, &none, &red, &columns(), &LOOK);
        c.update(0, &one_key(64), &red, &columns(), &LOOK);
        assert_eq!(c.len(), 12);
        c.update(0, &one_key(64), &red, &columns(), &LOOK);
        c.update(1, &none, &red, &columns(), &LOOK);
        c.update(0, &none, &red, &columns(), &LOOK);
        assert_eq!(c.len(), 0, "a jump back clears");
    }

    #[test]
    fn dense_songs_stay_under_the_cap_and_rainbow_varies_colour() {
        let mut c = Confetti::default();
        let look = ConfettiLook { amount: 40, rainbow: true, ..LOOK };
        for f in 0..20u64 {
            let active: [bool; 128] = std::array::from_fn(|_| f % 2 == 0);
            c.update(f * 1000, &active, &[Color32::RED; 128], &columns(), &look);
        }
        assert_eq!(c.len(), MAX);
        assert!(c.pieces.iter().any(|q| q.color != c.pieces[0].color), "rainbow pieces differ");
        let quads = c.quads(0.0, 1280.0, 600.0, 600.0, &look);
        assert_eq!(quads.len(), MAX);
    }
}
