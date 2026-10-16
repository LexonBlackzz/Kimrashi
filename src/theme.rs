//! Look settings: egui chrome themes and note color palettes.

use crate::{color_from_hsv, generate_palette};
use egui::{Color32, Visuals};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Debug)]
pub enum UiTheme {
    #[default]
    Dark,
    Light,
    Midnight,
    Forest,
    Rose,
}

impl UiTheme {
    pub const ALL: [(Self, &'static str); 5] = [
        (Self::Dark, "Dark"), (Self::Light, "Light"), (Self::Midnight, "Midnight"), (Self::Forest, "Forest"), (Self::Rose, "Rose"),
    ];

    pub fn apply(self, ctx: &egui::Context) {
        let mut v = if self == Self::Light { Visuals::light() } else { Visuals::dark() };
        // (panel/window fill, text-box/extreme fill, accent)
        let tint = match self {
            Self::Midnight => Some(((18, 20, 34), (10, 11, 22), (90, 130, 255))),
            Self::Forest => Some(((20, 30, 24), (12, 20, 15), (90, 190, 120))),
            Self::Rose => Some(((34, 22, 28), (22, 13, 18), (240, 110, 150))),
            _ => None,
        };
        if let Some((panel, extreme, accent)) = tint {
            let c = |(r, g, b)| Color32::from_rgb(r, g, b);
            v.panel_fill = c(panel);
            v.window_fill = c(panel);
            v.extreme_bg_color = c(extreme);
            v.selection.bg_fill = c(accent).gamma_multiply(0.6);
            v.hyperlink_color = c(accent);
        }
        ctx.set_visuals(v);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Debug)]
pub enum NotePalette {
    /// Golden-ratio hue steps: neighbouring tracks always contrast.
    #[default]
    Golden,
    /// Evenly spaced hues around the wheel.
    Rainbow,
    /// A short hand-picked list, repeated.
    Fixed,
    /// Hue by pitch, violet (low) to red (high).
    Pitch,
    /// `Settings::custom_colors`, looped over every channel and track.
    Custom,
}

const RAINBOW: [Color32; 7] = [
    Color32::from_rgb(0xFF, 0x4D, 0x4D), // red
    Color32::from_rgb(0xFF, 0x9A, 0x3D), // orange
    Color32::from_rgb(0xFF, 0xE0, 0x3D), // yellow
    Color32::from_rgb(0x4D, 0xDB, 0x5C), // green
    Color32::from_rgb(0x3D, 0xA9, 0xFF), // blue
    Color32::from_rgb(0x6A, 0x5C, 0xFF), // indigo
    Color32::from_rgb(0xC0, 0x5C, 0xFF), // violet
];

impl NotePalette {
    pub const ALL: [(Self, &'static str); 5] = [
        (Self::Golden, "Golden"), (Self::Rainbow, "Rainbow"), (Self::Fixed, "Fixed list"), (Self::Pitch, "By pitch"), (Self::Custom, "Custom"),
    ];

    /// `count` colors, one per (track, channel) pair.
    pub fn colors(self, count: usize, custom: &[[u8; 3]]) -> Vec<Color32> {
        match self {
            // Pitch colors come from `pitch_color`; these only back the unused by-channel/track paths.
            Self::Golden | Self::Pitch => generate_palette(count),
            // The 7 rainbow colors, looped over however many channels or tracks there are.
            Self::Rainbow => looped(&RAINBOW, count),
            Self::Fixed => looped(&generate_palette(8), count),
            Self::Custom if custom.is_empty() => generate_palette(count),
            Self::Custom => looped(&custom.iter().map(|&[r, g, b]| Color32::from_rgb(r, g, b)).collect::<Vec<_>>(), count),
        }
    }
}

/// `count` colors cycling through `list`, each lap shifted one along so a run of
/// `list.len()` things doesn't repeat the same order forever.
fn looped(list: &[Color32], count: usize) -> Vec<Color32> {
    let n = list.len();
    (0..count).map(|i| list[(i + i / n) % n]).collect()
}

/// Violet at `lo` through red at `hi`.
pub fn pitch_color(pitch: u8, lo: u8, hi: u8) -> Color32 {
    let t = (pitch.saturating_sub(lo)) as f32 / (hi.saturating_sub(lo)).max(1) as f32;
    color_from_hsv(0.78 * (1.0 - t.min(1.0)), 0.65, 0.95)
}

pub const MAX_CUSTOM: usize = 256;

pub fn default_custom() -> Vec<[u8; 3]> {
    generate_palette(16).into_iter().map(|c| [c.r(), c.g(), c.b()]).collect()
}

/// `count` pleasant random colors: golden-ratio hue steps from a random start.
pub fn random_colors(count: usize) -> Vec<[u8; 3]> {
    use rand_core::{OsRng, RngCore};
    let unit = |r: u32| r as f32 / u32::MAX as f32;
    let (mut hue, s, v) = (unit(OsRng.next_u32()), 0.5 + 0.3 * unit(OsRng.next_u32()), 0.85 + 0.1 * unit(OsRng.next_u32()));
    (0..count).map(|_| {
        hue = (hue + 0.618_034).fract();
        let c = color_from_hsv(hue, s, v);
        [c.r(), c.g(), c.b()]
    }).collect()
}

/// `count` colors drawn at random from the image at `path` (no repeats until its colors run out).
pub fn random_from_png(path: &str, count: usize) -> Option<Vec<[u8; 3]>> {
    use rand_core::{OsRng, RngCore};
    let mut pool = colors_from_png(&std::fs::read(path).ok()?, 4096).ok()?;
    for i in (1..pool.len()).rev() {
        pool.swap(i, OsRng.next_u32() as usize % (i + 1));
    }
    Some(pool.into_iter().cycle().take(count).collect())
}

/// The distinct opaque colors of a PNG in reading order (so a strip of swatches gives its
/// swatches), at most `limit`.
pub fn colors_from_png(bytes: &[u8], limit: usize) -> Result<Vec<[u8; 3]>, String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("image too large")?];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let channels = info.color_type.samples();
    let mut out: Vec<[u8; 3]> = Vec::new();
    for px in buf[..info.buffer_size()].chunks_exact(channels) {
        let (rgb, alpha) = match channels {
            1 => ([px[0]; 3], 255),
            2 => ([px[0]; 3], px[1]),
            3 => ([px[0], px[1], px[2]], 255),
            _ => ([px[0], px[1], px[2]], px[3]),
        };
        if alpha > 0 && !out.contains(&rgb) {
            out.push(rgb);
            if out.len() == limit { break; }
        }
    }
    if out.is_empty() { Err("no opaque pixels".into()) } else { Ok(out) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palettes_fill_the_count_and_pitch_spans_hues() {
        let custom = random_colors(5);
        for (p, _) in NotePalette::ALL {
            for n in [1, 8, 16, 40] {
                assert_eq!(p.colors(n, &custom).len(), n);
            }
        }
        let r = NotePalette::Rainbow.colors(40, &custom);
        assert_eq!(r[7], r[1]); // each lap shifts one along
        assert_ne!(r[0], r[1]);
        let mut data = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut data, 3, 1);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header().unwrap().write_image_data(&[255, 0, 0, 0, 255, 0, 255, 0, 0]).unwrap();
        }
        assert_eq!(colors_from_png(&data, MAX_CUSTOM).unwrap(), vec![[255, 0, 0], [0, 255, 0]]);
        assert_ne!(pitch_color(21, 21, 108), pitch_color(108, 21, 108));
    }
}
