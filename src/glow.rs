//! Key glow ("fake HDR"): hit keys emit light that spills sideways, rises into the note area and
//! decays after release. See docs/superpowers/specs/2026-11-23-key-glow-design.md.

use egui::Color32;

/// The adjustable look (see `Settings::glow_*`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GlowLook {
    /// Light of a sounding key (1 = a full-strength key before tone mapping).
    pub intensity: f32,
    /// Seconds for released light to fall to 1/e.
    pub decay: f32,
    /// Sideways spread, in widths of the emitting key.
    pub spread: f32,
    /// How far up the light reaches (1/e), as a fraction of the note area's height.
    pub reach: f32,
}

pub struct Glow {
    level: [f32; 128],
    color: [[f32; 3]; 128],
    last_us: Option<u64>,
}

impl Default for Glow {
    fn default() -> Self { Self { level: [0.0; 128], color: [[0.0; 3]; 128], last_us: None } }
}

impl Glow {
    /// Sounding keys are held at `intensity`, in their note's colour; released ones decay. Going
    /// back in time (a seek) starts over.
    pub fn update(&mut self, now_us: u64, active: &[bool; 128], colors: &[Color32; 128], look: &GlowLook) {
        let dt = match self.last_us {
            Some(last) if now_us >= last => (now_us - last) as f32 / 1e6,
            _ => { *self = Self::default(); 0.0 }
        };
        self.last_us = Some(now_us);
        let fade = (-dt / look.decay.max(1e-3)).exp();
        for p in 0..128 {
            if active[p] {
                let c = colors[p];
                let [r, g, b] = if c == Color32::TRANSPARENT { [255, 220, 140] } else { [c.r(), c.g(), c.b()] };
                self.level[p] = look.intensity;
                self.color[p] = [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0];
            } else {
                self.level[p] *= fade;
            }
        }
    }

    pub fn is_dark(&self) -> bool { self.level.iter().all(|&l| l < 1e-3) }

    /// Tone-mapped light (0..1 per channel) for each of `width` columns; `columns[p]` is key p's
    /// (left, right) as fractions of the width.
    pub fn strip(&self, columns: &[(f32, f32); 128], width: usize, look: &GlowLook) -> Vec<[f32; 3]> {
        let mut light = vec![[0.0f32; 3]; width];
        for p in 0..128 {
            let level = self.level[p];
            if level < 1e-3 { continue; }
            let (l, r) = columns[p];
            let (centre, sigma) = ((l + r) * 0.5 * width as f32, ((r - l) * width as f32 * look.spread).max(0.5));
            let (x0, x1) = ((centre - 3.0 * sigma).max(0.0) as usize, ((centre + 3.0 * sigma) as usize + 1).min(width));
            for (x, px) in light.iter_mut().enumerate().take(x1).skip(x0) {
                let d = (x as f32 + 0.5 - centre) / sigma;
                let w = level * (-d * d).exp();
                for c in 0..3 { px[c] += w * self.color[p][c]; }
            }
        }
        for px in &mut light { for c in px.iter_mut() { *c = 1.0 - (-*c).exp(); } }
        light
    }
}

/// How much of a column's light reaches `dy` pixels above the keyboard line.
pub fn falloff(dy: f32, reach_px: f32) -> f32 { (-dy.max(0.0) / reach_px.max(1.0)).exp() }

/// Over the keys, light is added at this strength.
pub const ON_KEYS: f32 = 0.5;

#[cfg(test)]
mod tests {
    use super::*;

    const LOOK: GlowLook = GlowLook { intensity: 1.0, decay: 0.3, spread: 2.0, reach: 0.25 };

    fn one_key(p: usize) -> [bool; 128] { let mut a = [false; 128]; a[p] = true; a }

    #[test]
    fn released_light_decays_and_seeks_clear_it() {
        let (mut g, red) = (Glow::default(), [Color32::RED; 128]);
        g.update(0, &one_key(60), &red, &LOOK);
        assert_eq!(g.level[60], 1.0);
        let half = (0.3 * std::f32::consts::LN_2 * 1e6) as u64;
        g.update(half, &[false; 128], &red, &LOOK);
        assert!((g.level[60] - 0.5).abs() < 0.01, "{}", g.level[60]);
        g.update(10, &[false; 128], &red, &LOOK);
        assert!(g.is_dark(), "a jump back starts over");
    }

    #[test]
    fn the_strip_peaks_at_the_key_and_tone_maps_below_one() {
        let columns: [(f32, f32); 128] = std::array::from_fn(|p| (p as f32 / 128.0, (p + 1) as f32 / 128.0));
        let mut g = Glow::default();
        g.update(0, &one_key(64), &[Color32::WHITE; 128], &LOOK);
        let s = g.strip(&columns, 1280, &LOOK);
        let (at, near, far) = (s[645][0], s[665][0], s[700][0]);
        assert!(at > near && near > far, "{at} {near} {far}");
        assert!(far < 0.01, "two spreads (40 px) out it has faded: {far}");
        let bright = GlowLook { intensity: 3.0, ..LOOK };
        g.update(0, &one_key(64), &[Color32::WHITE; 128], &bright);
        let hot = g.strip(&columns, 1280, &bright)[645][0];
        assert!(hot > at && hot < 1.0, "more light is brighter but never past white: {at} {hot}");
    }
}
