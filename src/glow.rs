//! Key glow ("fake HDR"): hit keys emit light that spills sideways, rises into the note area and
//! decays after release. See docs/superpowers/specs/2026-11-23-key-glow-design.md.

use egui::Color32;

/// The adjustable look (see `Settings::glow_*`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GlowLook {
    /// The key glow itself (halo, light on the keys, glass keys); off leaves only the rainbow bar.
    pub glow: bool,
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
    /// Rainbow bar along the top of the keys, as a fraction of their height (0: off).
    pub bar_height: f32,
    /// How fast the rainbow moves, in turns per second (of song time).
    pub bar_speed: f32,
    /// Sapphire-style tail: each light also glows at 2, 4 and 8 times its width (and reach), each
    /// layer this much fainter than the last (0: a single soft glow).
    pub tail: f32,
    /// The rainbow bar's brightness at rest (0..1; hits flare it toward white).
    pub bar_rest: f32,
    /// One colour for the bar instead of the moving rainbow.
    pub bar_color: Option<[f32; 3]>,
}

/// Glow layers: the light itself plus three, each twice as wide (see `GlowLook::tail`).
const LAYERS: i32 = 4;

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
        self.strip_with(columns, width, look.spread, 0.0, look.tail)
    }

    /// The bloom over the keys: three times as wide as the halo above them (no tail: stacked on
    /// this width it would wash the whole keyboard).
    pub fn wide(&self, columns: &[(f32, f32); 128], width: usize, look: &GlowLook) -> Vec<[f32; 3]> {
        self.strip_with(columns, width, look.spread * 3.0, 0.0, 0.0)
    }

    /// The rainbow bar's colour (0..1) for each of `width` columns: a rainbow across the width,
    /// shifted by `phase` turns, dim at rest and flaring bright and whiter over lit keys.
    pub fn bar(&self, columns: &[(f32, f32); 128], width: usize, look: &GlowLook, phase: f32) -> Vec<[f32; 3]> {
        let mut lit = vec![0.0f32; width];
        for p in 0..128 {
            let level = self.level[p] / look.intensity.max(1e-3);
            if level < 1e-3 { continue; }
            let (l, r) = columns[p];
            let (centre, sigma) = ((l + r) * 0.5 * width as f32, ((r - l) * width as f32).max(0.5));
            let (x0, x1) = ((centre - 3.0 * sigma).max(0.0) as usize, ((centre + 3.0 * sigma) as usize + 1).min(width));
            for (x, v) in lit.iter_mut().enumerate().take(x1).skip(x0) {
                let d = (x as f32 + 0.5 - centre) / sigma;
                *v += level * (-d * d).exp();
            }
        }
        lit.iter().enumerate().map(|(x, &l)| {
            let f = 1.0 - (-2.0 * l).exp();
            let hue = ((x as f32 + 0.5) / width as f32 + phase).rem_euclid(1.0);
            let v = look.bar_rest + (1.0 - look.bar_rest) * f;
            let base = look.bar_color.unwrap_or_else(|| egui::ecolor::Hsva::new(hue, 1.0, 1.0, 1.0).to_srgb().map(|c| c as f32 / 255.0));
            base.map(|c| { let c = c * v; c + (1.0 - c) * 0.5 * f })
        }).collect()
    }

    /// The hot spot where notes meet the keys: a narrow strip, its colour 60 % of the way to white.
    pub fn core(&self, columns: &[(f32, f32); 128], width: usize, _look: &GlowLook) -> Vec<[f32; 3]> {
        self.strip_with(columns, width, 0.35, 0.6, 0.0)
    }

    fn strip_with(&self, columns: &[(f32, f32); 128], width: usize, spread: f32, whiten: f32, tail: f32) -> Vec<[f32; 3]> {
        let mut light = vec![[0.0f32; 3]; width];
        for p in 0..128 {
            let level = self.level[p];
            if level < 1e-3 { continue; }
            let (l, r) = columns[p];
            let (centre, sigma0) = ((l + r) * 0.5 * width as f32, ((r - l) * width as f32 * spread).max(0.5));
            let color = self.color[p].map(|c| c + (1.0 - c) * whiten);
            for k in 0..LAYERS {
                let (gain, sigma) = (tail.powi(k), sigma0 * 2f32.powi(k));
                if gain < 1e-3 { break; }
                let (x0, x1) = ((centre - 3.0 * sigma).max(0.0) as usize, ((centre + 3.0 * sigma) as usize + 1).min(width));
                for (x, px) in light.iter_mut().enumerate().take(x1).skip(x0) {
                    let d = (x as f32 + 0.5 - centre) / sigma;
                    let w = level * gain * (-d * d).exp();
                    for c in 0..3 { px[c] += w * color[c]; }
                }
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

/// Where the rainbow bar's hues stand at `now_us` of song time, in turns.
pub fn bar_phase(now_us: u64, look: &GlowLook) -> f32 { (now_us as f64 / 1e6 * look.bar_speed as f64).fract() as f32 }

/// How much of a column's light reaches `dy` pixels above the keyboard line.
pub fn falloff(dy: f32, reach_px: f32) -> f32 { (-dy.max(0.0) / reach_px.max(1.0)).exp() }

/// (halo, core) strength `dy` pixels above the keyboard line (negative: down into the keys, which
/// are `keys_px` tall). The halo rises `reach_px`; over the keys (where the `wide` strip stands in
/// for it) it stays strong all the way down. The core stays within a few pixels of the line.
/// `tail` stacks longer falloffs on the halo (see `stacked`).
pub fn weights(dy: f32, reach_px: f32, keys_px: f32, tail: f32) -> (f32, f32) {
    if dy >= 0.0 { (stacked(dy, reach_px, tail), falloff(dy, (keys_px * 0.08).max(2.0))) }
    else { (stacked(-dy, keys_px * 3.0, tail), falloff(-dy, keys_px * 0.25)) }
}

/// How much of the rainbow bar's light (it is `bar_px` tall) reaches `dy` pixels above it.
pub fn bar_glow(dy: f32, bar_px: f32, tail: f32) -> f32 { 0.7 * stacked(dy, bar_px * 2.0, tail) }

/// `falloff` plus layers reaching 2, 4 and 8 times as far, each `tail` times fainter; 1 at `dy` 0.
pub fn stacked(dy: f32, reach_px: f32, tail: f32) -> f32 {
    let (mut sum, mut total) = (0.0, 0.0);
    for k in 0..LAYERS {
        let gain = tail.powi(k);
        if gain < 1e-3 { break; }
        sum += gain * falloff(dy, reach_px * 2f32.powi(k));
        total += gain;
    }
    sum / total
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

    const LOOK: GlowLook = GlowLook { glow: true, intensity: 1.0, decay: 0.3, spread: 2.0, reach: 0.25, on_keys: 0.8, glass: false, bar_height: 0.1, bar_speed: 0.2, tail: 0.0, bar_rest: 0.35, bar_color: None };

    fn one_key(p: usize) -> [bool; 128] { let mut a = [false; 128]; a[p] = true; a }

    #[test]
    fn light_is_hottest_at_the_line_and_blooms_down_into_the_keys() {
        let (reach, keys) = (20.0, 40.0);
        assert_eq!(weights(0.0, reach, keys, 0.0), (1.0, 1.0));
        let (halo_up, core_up) = weights(10.0, reach, keys, 0.0);
        let (halo_down, core_down) = weights(-10.0, reach, keys, 0.0);
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
        let (deep, _) = weights(-36.0, 20.0, 40.0, 0.0);
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
    fn the_rainbow_bar_moves_and_flares_at_hit_keys() {
        let columns: [(f32, f32); 128] = std::array::from_fn(|p| (p as f32 / 128.0, (p + 1) as f32 / 128.0));
        let mut g = Glow::default();
        let rest = g.bar(&columns, 1280, &LOOK, 0.0);
        let luma = |c: [f32; 3]| c[0] + c[1] + c[2];
        assert!(rest[10][0] > rest[10][2] && rest[650][2] > rest[650][0], "red at the left, blue past the middle: {:?} {:?}", rest[10], rest[650]);
        assert!(rest.iter().all(|&c| luma(c) < 1.2), "dim at rest");
        let moved = g.bar(&columns, 1280, &LOOK, 0.25);
        for c in 0..3 { assert!((moved[100][c] - rest[420][c]).abs() < 0.02, "a quarter turn moves the rainbow a quarter of the way"); }
        g.update(0, &one_key(64), &[Color32::RED; 128], &LOOK);
        let lit = g.bar(&columns, 1280, &LOOK, 0.0);
        assert!(luma(lit[645]) > luma(rest[645]) + 0.8, "the hit key flares: {:?} {:?}", lit[645], rest[645]);
        assert!((luma(lit[100]) - luma(rest[100])).abs() < 1e-3, "far keys don't");
    }

    #[test]
    fn a_tail_stacks_wider_glows_like_sapphire() {
        let columns: [(f32, f32); 128] = std::array::from_fn(|p| (p as f32 / 128.0, (p + 1) as f32 / 128.0));
        let tailed = GlowLook { tail: 0.5, ..LOOK };
        let mut g = Glow::default();
        g.update(0, &one_key(64), &[Color32::RED; 128], &LOOK);
        let (plain, long) = (g.strip(&columns, 1280, &LOOK), g.strip(&columns, 1280, &tailed));
        assert!(plain[760][0] < 0.001 && long[760][0] > 0.05, "a soft tail far out: {} {}", plain[760][0], long[760][0]);
        assert!(long[760][1] == 0.0, "still red");
        let (up_plain, _) = weights(60.0, 20.0, 40.0, 0.0);
        let (up_long, _) = weights(60.0, 20.0, 40.0, 0.5);
        assert!(up_long > up_plain * 2.0, "and up: {up_plain} {up_long}");
        assert_eq!(weights(0.0, 20.0, 40.0, 0.5), (1.0, 1.0), "full strength at the line either way");
        assert!(stacked(30.0, 10.0, 0.5) > stacked(30.0, 10.0, 0.0));
    }

    #[test]
    fn the_bar_can_be_brighter_or_one_colour() {
        let columns: [(f32, f32); 128] = std::array::from_fn(|p| (p as f32 / 128.0, (p + 1) as f32 / 128.0));
        let g = Glow::default();
        let bright = g.bar(&columns, 1280, &GlowLook { bar_rest: 1.0, ..LOOK }, 0.0);
        assert!(bright[10][0] > 0.95, "fully lit at rest: {:?}", bright[10]);
        let cyan = g.bar(&columns, 1280, &GlowLook { bar_color: Some([0.0, 1.0, 1.0]), ..LOOK }, 0.0);
        assert!(cyan.iter().all(|c| c[0] == 0.0 && (c[1] - LOOK.bar_rest).abs() < 1e-3), "one colour, dim: {:?}", cyan[10]);
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
