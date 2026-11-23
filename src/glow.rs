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
    /// How strongly the light colours the keys themselves (0..1).
    pub on_keys: f32,
    /// Glass keys: frosted keys, dark at rest, lit from inside by their light (`ui::glass_quads`).
    pub glass: bool,
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

    /// Key `p`'s light level and colour (0..1 per channel).
    pub fn key(&self, p: u8) -> (f32, [f32; 3]) { (self.level[p as usize & 127], self.color[p as usize & 127]) }

    pub fn is_dark(&self) -> bool { self.level.iter().all(|&l| l < 1e-3) }

    /// Tone-mapped light (0..1 per channel) for each of `width` columns; `columns[p]` is key p's
    /// (left, right) as fractions of the width.
    pub fn strip(&self, columns: &[(f32, f32); 128], width: usize, look: &GlowLook) -> Vec<[f32; 3]> {
        self.strip_with(columns, width, look.spread, 0.0)
    }

    /// The bloom over the keys: three times as wide as the halo above them.
    pub fn wide(&self, columns: &[(f32, f32); 128], width: usize, look: &GlowLook) -> Vec<[f32; 3]> {
        self.strip_with(columns, width, look.spread * 3.0, 0.0)
    }

    /// The hot spot where notes meet the keys: a narrow strip, its colour 60 % of the way to white.
    pub fn core(&self, columns: &[(f32, f32); 128], width: usize, _look: &GlowLook) -> Vec<[f32; 3]> {
        self.strip_with(columns, width, 0.35, 0.6)
    }

    fn strip_with(&self, columns: &[(f32, f32); 128], width: usize, spread: f32, whiten: f32) -> Vec<[f32; 3]> {
        let mut light = vec![[0.0f32; 3]; width];
        for p in 0..128 {
            let level = self.level[p];
            if level < 1e-3 { continue; }
            let (l, r) = columns[p];
            let (centre, sigma) = ((l + r) * 0.5 * width as f32, ((r - l) * width as f32 * spread).max(0.5));
            let color = self.color[p].map(|c| c + (1.0 - c) * whiten);
            let (x0, x1) = ((centre - 3.0 * sigma).max(0.0) as usize, ((centre + 3.0 * sigma) as usize + 1).min(width));
            for (x, px) in light.iter_mut().enumerate().take(x1).skip(x0) {
                let d = (x as f32 + 0.5 - centre) / sigma;
                let w = level * (-d * d).exp();
                for c in 0..3 { px[c] += w * color[c]; }
            }
        }
        // Tone map by the brightest channel so hot light keeps its hue ("fake HDR"), not per channel
        // (which drives every channel to 1 and washes colours out to pastel).
        for px in &mut light {
            let m = px[0].max(px[1]).max(px[2]);
            if m > 0.0 { let k = (1.0 - (-m).exp()) / m; for c in px.iter_mut() { *c *= k; } }
        }
        light
    }
}

/// How much of a column's light reaches `dy` pixels above the keyboard line.
pub fn falloff(dy: f32, reach_px: f32) -> f32 { (-dy.max(0.0) / reach_px.max(1.0)).exp() }

/// (halo, core) strength `dy` pixels above the keyboard line (negative: down into the keys, which
/// are `keys_px` tall). The halo rises `reach_px`; over the keys (where the `wide` strip stands in
/// for it) it stays strong all the way down. The core stays within a few pixels of the line.
pub fn weights(dy: f32, reach_px: f32, keys_px: f32) -> (f32, f32) {
    if dy >= 0.0 { (falloff(dy, reach_px), falloff(dy, (keys_px * 0.08).max(2.0))) }
    else { (falloff(-dy, keys_px * 3.0), falloff(-dy, keys_px * 0.25)) }
}

/// The light at one point: `halo` and `core` (per column, from `strip` and `core`) weighted.
pub fn mix(halo: [f32; 3], core: [f32; 3], (h, c): (f32, f32)) -> [f32; 3] {
    std::array::from_fn(|i| (halo[i] * h + core[i] * c).min(1.0))
}

impl GlowLook {
    /// How strongly the light colours the keys (the bloom fades down them, so it leaves lit keys
    /// showing, glass or not).
    pub fn keys_tint(&self) -> f32 { self.on_keys }
}

/// Over the keys the light's colour is blended in rather than added (white keys can't get
/// brighter): as a premultiplied colour (0..1) and its opacity, for `c` from `Glow::strip`.
pub fn on_keys(c: [f32; 3], strength: f32) -> ([f32; 3], f32) {
    let k = strength.clamp(0.0, 1.0);
    (c.map(|v| v * k), c.iter().cloned().fold(0.0, f32::max) * k)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOOK: GlowLook = GlowLook { intensity: 1.0, decay: 0.3, spread: 2.0, reach: 0.25, on_keys: 0.8, glass: false };

    fn one_key(p: usize) -> [bool; 128] { let mut a = [false; 128]; a[p] = true; a }

    #[test]
    fn light_is_hottest_at_the_line_and_blooms_down_into_the_keys() {
        let (reach, keys) = (20.0, 40.0);
        assert_eq!(weights(0.0, reach, keys), (1.0, 1.0));
        let (halo_up, core_up) = weights(10.0, reach, keys);
        let (halo_down, core_down) = weights(-10.0, reach, keys);
        assert!(core_up < 0.2 && core_down < 0.5, "the core is tight: {core_up} {core_down}");
        assert!(halo_down > halo_up, "the halo reaches further into the keys than up: {halo_down} {halo_up}");
        let columns: [(f32, f32); 128] = std::array::from_fn(|p| (p as f32 / 128.0, (p + 1) as f32 / 128.0));
        let mut g = Glow::default();
        g.update(0, &one_key(64), &[Color32::RED; 128], &LOOK);
        let (halo, core) = (g.strip(&columns, 1280, &LOOK), g.core(&columns, 1280, &LOOK));
        assert!(halo[645][1] == 0.0 && core[645][1] > 0.1, "the core is whiter: {:?} {:?}", halo[645], core[645]);
        assert!(core[665][0] < halo[665][0], "and narrower");
    }

    #[test]
    fn the_keys_bloom_wide_and_deep() {
        let columns: [(f32, f32); 128] = std::array::from_fn(|p| (p as f32 / 128.0, (p + 1) as f32 / 128.0));
        let mut g = Glow::default();
        g.update(0, &one_key(64), &[Color32::WHITE; 128], &LOOK);
        let (halo, wide) = (g.strip(&columns, 1280, &LOOK), g.wide(&columns, 1280, &LOOK));
        assert!(halo[700][0] < 0.01 && wide[700][0] > 0.1, "four keys out the keys still glow: {} {}", halo[700][0], wide[700][0]);
        let (deep, _) = weights(-36.0, 20.0, 40.0);
        assert!(deep > 0.4, "most of the way down the keys: {deep}");
        assert_eq!(GlowLook { glass: true, ..LOOK }.keys_tint(), LOOK.on_keys, "glass keys take the full tint");
    }

    #[test]
    fn bright_light_keeps_its_colour() {
        let columns: [(f32, f32); 128] = std::array::from_fn(|p| (p as f32 / 128.0, (p + 1) as f32 / 128.0));
        let mut g = Glow::default();
        let hot = GlowLook { intensity: 3.0, ..LOOK };
        g.update(0, &one_key(64), &[Color32::from_rgb(255, 128, 51); 128], &hot);
        let [r, gr, b] = g.strip(&columns, 1280, &hot)[645];
        assert!(r < 1.0 && (gr / r - 0.5).abs() < 0.02 && (b / r - 0.2).abs() < 0.02, "orange stays orange, not pastel: {r} {gr} {b}");
    }

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
