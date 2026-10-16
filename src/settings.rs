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

#[derive(Clone, PartialEq, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct Settings {
    // Display
    pub note_speed: f32,
    pub notes_rise: bool,
    pub piano_layout: bool,
    pub flat_notes: bool,
    pub ui_theme: UiTheme,
    pub note_palette: NotePalette,
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
    pub long_data: bool,
    // Loading
    pub velocity_levels: u8,
    pub verify_repeats: bool,
    pub loading_details: bool,
    // Updates
    pub check_updates: bool,
    pub last_update_check: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            note_speed: 5.0,
            notes_rise: false,
            piano_layout: false,
            flat_notes: false,
            ui_theme: UiTheme::default(),
            note_palette: NotePalette::default(),
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
            long_data: false,
            velocity_levels: 127,
            verify_repeats: false,
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
    fn reads_the_old_text_file() {
        let s = Settings::from_legacy("note_speed=3.5\nnotes_rise=true\noutput=12\ndetailed_stats=true\nsynth_limit=50000\nunknown=1\n");
        assert_eq!((s.note_speed, s.notes_rise, s.synth_limit), (3.5, true, 50000.0));
        assert_eq!((s.output.as_str(), s.stats), ("12", StatsMode::Detailed));
    }
}
