//! Every saved preference in one typed struct, kept in `settings.json` next to the player.
//! A missing file, a missing field or a bad value falls back to the default, so adding a
//! field never breaks an old file.

use crate::SYNTH_LIMIT_OFF;
use crate::theme::{default_custom, NotePalette, UiTheme};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Debug)]
pub enum StatsMode {
    /// Just the note count.
    #[default]
    Count,
    /// One line: notes, NPS, polyphony, FPS.
    Compact,
    Detailed,
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Debug)]
pub enum ViewMode {
    /// Notes fall to (or rise from) a keyboard along the bottom.
    #[default]
    Vertical,
    /// The keyboard stands on its side at `keyboard_side`.
    Horizontal,
    /// No keyboard; a playhead in the middle, every track at its own speed.
    Parallax,
    /// The vertical view at an angle: the note area narrows toward the top.
    Perspective,
    /// Perspective with depth: notes are blocks, black keys stand up.
    Blocks,
    /// No time axis: a 128 x 128 grid of keys by velocity, a cell lit while a note sounds on it.
    Matrix,
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Debug)]
pub enum KeyboardSide {
    #[default]
    Left,
    Right,
}

#[derive(Clone, PartialEq, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct Settings {
    // Display
    pub view: ViewMode,
    pub keyboard_side: KeyboardSide,
    /// Horizontal: draw the keyboard (off: notes run to the edge).
    pub horizontal_keyboard: bool,
    /// Parallax: the farthest track's speed, as a fraction of the nearest's.
    pub parallax_far: f32,
    /// Perspective: the far (top) edge's width, as a fraction of the bottom.
    pub perspective_far: f32,
    /// Matrix: seconds a cell takes to fade out after its last note ends; 0 = off at once.
    pub matrix_fade: f32,
    /// Key glow (Vertical view): hit keys emit light (see `crate::glow`).
    pub glow: bool,
    pub glow_intensity: f32,
    pub glow_decay: f32,
    pub glow_spread: f32,
    pub glow_reach: f32,
    pub glow_on_keys: f32,
    pub glow_glass: bool,
    /// Rainbow bar along the top of the keys (part of the key glow), its height as a fraction of
    /// the keys' and its speed in turns per second.
    pub glow_bar: bool,
    pub glow_bar_height: f32,
    pub glow_bar_speed: f32,
    pub note_speed: f32,
    pub notes_rise: bool,
    pub piano_layout: bool,
    pub flat_notes: bool,
    /// Piano From Above's notes and keyboard (overrides `flat_notes`).
    pub pfa_style: bool,
    pub ui_theme: UiTheme,
    pub note_palette: NotePalette,
    /// Steps in the Rainbow palette.
    pub rainbow_size: usize,
    pub custom_colors: Vec<[u8; 3]>,
    /// The PNG the custom colors came from; Randomize draws from it.
    pub palette_image: String,
    pub collapse_overlaps: bool,
    pub max_instances: usize,
    pub vsync: bool,
    /// Max frames per second; 0 = unlimited.
    pub fps_limit: u32,
    pub stats: StatsMode,
    pub synth_stats: bool,
    // Audio
    /// Display name of the MIDI output; empty = the first one.
    pub output: String,
    pub skip_notes: bool,
    /// Max note-ons/s to the synth; `SYNTH_LIMIT_OFF` = no limit.
    pub synth_limit: f64,
    pub svms_batch: bool,
    /// SVMS only: hand batches straight to its event pages (needs a build that has the feed).
    pub svms_direct: bool,
    pub long_data: bool,
    // Loading
    pub velocity_levels: u8,
    pub verify_repeats: bool,
    /// FL projects: start playing the song's opening while the rest still loads.
    pub flp_quick_start: bool,
    pub loading_details: bool,
    // Updates
    pub check_updates: bool,
    pub last_update_check: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            view: ViewMode::Vertical,
            keyboard_side: KeyboardSide::Left,
            horizontal_keyboard: true,
            parallax_far: 0.3,
            perspective_far: 0.3,
            matrix_fade: 0.0,
            glow: false,
            glow_intensity: 1.0,
            glow_decay: 0.3,
            glow_spread: 2.0,
            glow_reach: 0.05,
            glow_on_keys: 0.8,
            glow_glass: false,
            glow_bar: false,
            glow_bar_height: 0.1,
            glow_bar_speed: 0.2,
            note_speed: 5.0,
            notes_rise: false,
            piano_layout: false,
            flat_notes: false,
            pfa_style: false,
            ui_theme: UiTheme::default(),
            note_palette: NotePalette::default(),
            rainbow_size: 7,
            custom_colors: default_custom(),
            palette_image: String::new(),
            collapse_overlaps: true,
            max_instances: 1_048_576,
            vsync: true,
            fps_limit: 0,
            stats: StatsMode::Count,
            synth_stats: false,
            output: String::new(),
            skip_notes: true,
            synth_limit: SYNTH_LIMIT_OFF,
            svms_batch: false,
            svms_direct: true,
            long_data: false,
            velocity_levels: 127,
            verify_repeats: false,
            flp_quick_start: false,
            loading_details: false,
            check_updates: true,
            last_update_check: 0,
        }
    }
}

fn dir() -> Option<std::path::PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.to_path_buf())
}

impl Settings {
    /// The note shader's `flat_mode`: 0 shaded, 1 flat, 2 Piano From Above.
    /// The key glow's look, when it is on.
    pub fn glow_look(&self) -> Option<crate::glow::GlowLook> {
        self.glow.then_some(crate::glow::GlowLook { intensity: self.glow_intensity, decay: self.glow_decay,
            spread: self.glow_spread, reach: self.glow_reach, on_keys: self.glow_on_keys, glass: self.glow_glass,
            bar_height: if self.glow_bar { self.glow_bar_height } else { 0.0 }, bar_speed: self.glow_bar_speed })
    }

    pub fn note_mode(&self) -> u32 { if self.pfa_style { 2 } else { self.flat_notes as u32 } }

    pub fn load() -> Self {
        let Some(dir) = dir() else { return Self::default() };
        if let Ok(text) = std::fs::read_to_string(dir.join("settings.json")) {
            return Self::from_json(&text);
        }
        // ponytail: reads the pre-0.5 `settings.txt` once; drop this when nobody has one.
        std::fs::read_to_string(dir.join("settings.txt")).map_or_else(|_| Self::default(), |t| Self::from_legacy(&t))
    }

    pub fn save(&self) {
        if let (Some(dir), Ok(text)) = (dir(), serde_json::to_string_pretty(self)) {
            let _ = std::fs::write(dir.join("settings.json"), text);
        }
    }

    fn from_json(text: &str) -> Self {
        serde_json::from_str(text).unwrap_or_default()
    }

    /// `key=value` lines whose keys are this struct's field names (except `detailed_stats`).
    fn from_legacy(text: &str) -> Self {
        let mut map = serde_json::Map::new();
        for (k, v) in text.lines().filter_map(|l| l.split_once('=')) {
            let (k, v) = (k.trim(), v.trim());
            let value = if k == "output" { None } else { serde_json::from_str(v).ok() };
            map.insert(k.to_owned(), value.unwrap_or_else(|| v.into()));
        }
        if map.get("detailed_stats") == Some(&true.into()) {
            map.insert("stats".into(), "Detailed".into());
        }
        serde_json::from_value(map.into()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_fallbacks() {
        let s = Settings { note_speed: 2.5, stats: StatsMode::Compact, output: "Synth A".into(), ..Default::default() };
        assert_eq!(Settings::from_json(&serde_json::to_string(&s).unwrap()), s);
        assert_eq!(Settings::from_json("not json"), Settings::default());
        assert_eq!(Settings::from_json(r#"{"vsync":false}"#), Settings { vsync: false, ..Default::default() });
    }

    #[test]
    fn old_settings_files_get_the_vertical_view() {
        let s: Settings = serde_json::from_str(r#"{"note_speed": 2.0}"#).unwrap();
        assert_eq!((s.view, s.keyboard_side, s.parallax_far), (ViewMode::Vertical, KeyboardSide::Left, 0.3));
        let back: Settings = serde_json::from_str(&serde_json::to_string(&Settings { view: ViewMode::Parallax, ..s.clone() }).unwrap()).unwrap();
        assert_eq!(back.view, ViewMode::Parallax);
    }

    #[test]
    fn perspective_settings_default_and_round_trip() {
        let s = Settings::from_json(r#"{"view":"Perspective"}"#);
        assert_eq!((s.view, s.perspective_far), (ViewMode::Perspective, 0.3));
    }

    #[test]
    fn reads_the_old_text_file() {
        let s = Settings::from_legacy("note_speed=3.5\nnotes_rise=true\noutput=12\ndetailed_stats=true\nsynth_limit=50000\nunknown=1\n");
        assert_eq!((s.note_speed, s.notes_rise, s.synth_limit), (3.5, true, 50000.0));
        assert_eq!((s.output.as_str(), s.stats), ("12", StatsMode::Detailed));
    }
}
