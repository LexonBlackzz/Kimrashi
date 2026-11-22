//! Parallax: no keyboard, a playhead in the middle, and every source track at its own speed.
//! Speeds snap to `STEPS` steps. Each step culls its own notes (see `VisibleNotes`), so the
//! culling shortcuts stay correct, and is drawn as one layer with its own speed.

use std::sync::Arc;
use crate::{build_note_instances, gpu, render_tracks_in, song::SongData, VisibleNotes, VisualSong};

/// Speed steps: tracks on the same step move together and are culled together.
pub const STEPS: usize = 64;

/// Speed step for each packed track, by its source track's rank (first = step 0 = nearest),
/// and how many steps there are (one per source track, up to `STEPS`).
// ponytail: ranks every source track that was packed, note-less ones included (usually just a
// conductor track); rank only tracks with notes if an empty step ever shows.
pub fn track_steps(song: &SongData) -> (Vec<u8>, usize) {
    let origins = &song.midi.track_origin;
    let mut distinct = origins.clone();
    distinct.sort_unstable();
    distinct.dedup();
    let n = distinct.len().max(1);
    let steps = n.min(STEPS);
    let step = |origin: u32| {
        let rank = distinct.binary_search(&origin).unwrap_or(0);
        if n == 1 { 0 } else { (rank * (steps - 1) + (n - 1) / 2) / (n - 1) }
    };
    (origins.iter().map(|&o| step(o) as u8).collect(), steps)
}

/// 1.0 for the nearest step, `far` for the farthest, linear between.
pub fn step_speed(step: usize, steps: usize, far: f32) -> f32 {
    if steps <= 1 { 1.0 } else { 1.0 - (1.0 - far) * step as f32 / (steps - 1) as f32 }
}

struct Layer {
    visible: VisibleNotes,
    instances: Arc<gpu::InstanceRing>,
    speed: f32,
}

/// Every speed step's notes, nearest first.
#[derive(Default)]
pub struct Parallax {
    /// What the layers were built for: the song, far speed and "Hide covered notes".
    built: Option<(Arc<SongData>, f32, bool)>,
    layers: Vec<Layer>,
}

impl Parallax {
    /// Drops every layer (stopping their decode workers).
    pub fn clear(&mut self) { *self = Self::default(); }

    /// Advances and builds every layer for this frame, nearest first, and returns what to draw.
    /// `span_s`: the note speed (seconds from the edge to the playhead for the nearest layer);
    /// `time_px`: pixels along the time axis; `max_instances`: the cap over all layers (0 = none).
    #[allow(clippy::too_many_arguments)]
    pub fn frame(&mut self, song: &VisualSong, now_us: u64, duration_us: u64, span_s: f32, far: f32,
        time_px: u32, flat_mode: u32, collapse: bool, max_instances: usize) -> Vec<gpu::LayerDraw>
    {
        let current = self.built.as_ref().is_some_and(|(s, f, c)| Arc::ptr_eq(s, &song.data) && *f == far && *c == collapse);
        if !current {
            let (steps, count) = track_steps(&song.data);
            self.layers = (0..count).map(|k| {
                let members: Vec<bool> = steps.iter().map(|&s| s as usize == k).collect();
                let mut visible = VisibleNotes::default();
                visible.set_collapse(collapse);
                visible.set_max_instances(0); // the cap is applied across layers below
                visible.set_tracks(Some(render_tracks_in(&song.data, &members).into()));
                Layer { visible, instances: Arc::new(gpu::InstanceRing::default()), speed: step_speed(k, count, far) }
            }).collect();
            self.built = Some((song.data.clone(), far, collapse));
        }
        let count = self.layers.len();
        let mut used = 0;
        let mut out = Vec::new();
        for (k, layer) in self.layers.iter_mut().enumerate() {
            if max_instances > 0 && used >= max_instances { break; } // far layers go first
            // The layer shows [now - half, now + half]; its time origin is the left edge. Near the
            // song's start the edge is before 0: the origin stays at 0 and `time_shift` moves the notes.
            let half_us = (span_s.max(0.01) / layer.speed * 1e6) as u64;
            let origin = now_us.saturating_sub(half_us);
            let time_shift = (half_us - (now_us - origin)) as f32 / 1e6;
            let window_end = now_us.saturating_add(half_us).min(duration_us.max(now_us));
            let height_time = 2.0 * half_us as f32 / 1e6;
            // ponytail: near the start, pixel culling maps rows from the clamped origin, up to
            // `time_shift` off; it only picks which overlapping notes survive for those seconds.
            layer.visible.advance(&song.data, origin, window_end, 2 * half_us / time_px.max(1) as u64);
            build_note_instances(song, &mut layer.visible, origin, window_end, height_time, time_px, false, &mut layer.instances);
            if max_instances > 0 && used + layer.instances.len() > max_instances {
                Arc::make_mut(&mut layer.instances).cap(max_instances - used, origin, false);
            }
            used += layer.instances.len();
            let depth = 1.0 / count as f32;
            out.push(gpu::LayerDraw { instances: layer.instances.clone(), view: gpu::NoteView {
                height_time, now_us: origin, time_shift, direction_sign: -1.0, flat_mode, horizontal: true, playhead: 0.5,
                brightness: 1.0 - 0.3 * k as f32 / (count - 1).max(1) as f32, depth: [k as f32 * depth, depth],
            } });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    fn song_from_bodies(bodies: &[Vec<u8>]) -> VisualSong {
        let mut file = b"MThd\0\0\0\x06\0\x01".to_vec();
        file.extend_from_slice(&(bodies.len() as u16).to_be_bytes());
        file.extend_from_slice(&480u16.to_be_bytes());
        for body in bodies {
            file.extend_from_slice(b"MTrk");
            file.extend_from_slice(&(body.len() as u32).to_be_bytes());
            file.extend_from_slice(body);
        }
        let midi = parser::MidiFile::parse_stream(Box::new(parser::Plain(std::io::Cursor::new(file))), 127,
            &parser::Progress::default()).unwrap();
        VisualSong::new(SongData::from_midi(String::new(), midi, Default::default()))
    }

    /// Each track holds `keys[track]` from 0 s to 4 s (120 BPM), on channel `track % 16`.
    fn song(keys: &[&[u8]]) -> VisualSong {
        song_from_bodies(&keys.iter().enumerate().map(|(t, keys)| {
            let ch = t as u8 % 16;
            let mut body = Vec::new();
            for &k in *keys { body.extend_from_slice(&[0, 0x90 | ch, k, 100]); }
            for (i, &k) in keys.iter().enumerate() {
                body.extend_from_slice(if i == 0 { &[0x9e, 0][..] } else { &[0][..] });
                body.extend_from_slice(&[0x80 | ch, k, 0]);
            }
            body.extend_from_slice(&[0, 0xff, 0x2f, 0]);
            body
        }).collect::<Vec<_>>())
    }

    #[test]
    fn steps_spread_tracks_near_to_far() {
        let (steps, count) = track_steps(&song(&[&[60], &[61], &[62]]).data);
        assert_eq!((steps, count), (vec![0, 1, 2], 3));
        assert_eq!((step_speed(0, 3, 0.3), step_speed(2, 3, 0.3)), (1.0, 0.3));
        assert_eq!(track_steps(&song(&[&[60]]).data).1, 1);
        assert_eq!(step_speed(0, 1, 0.3), 1.0);
        let keys: Vec<[u8; 1]> = (0..200).map(|t| [30 + (t % 60) as u8]).collect();
        let (steps, count) = track_steps(&song(&keys.iter().map(|k| &k[..]).collect::<Vec<_>>()).data);
        assert_eq!((count, steps[0], steps[199]), (STEPS, 0, 63));
        assert!(steps.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn identical_tracks_on_different_steps_both_draw() {
        // Tracks 0 and 1 are identical; track 2 starts a tick later.
        let bodies: Vec<_> = [0, 0, 1].iter().map(|&d| vec![d, 0x90, 60, 100, 0x9e, 0, 0x80, 60, 0, 0, 0xff, 0x2f, 0]).collect();
        let song = song_from_bodies(&bodies);
        assert_eq!(crate::render_tracks(&song.data), [false, true, true]);
        assert_eq!(crate::render_tracks_in(&song.data, &[true, false, true]), [true, false, true]);
        assert_eq!(crate::render_tracks_in(&song.data, &[true, false, false]), [true, false, false]);
        assert_eq!(crate::render_tracks_in(&song.data, &[true, true, false]), [false, true, false]);
    }

    #[test]
    fn a_masked_layer_only_decodes_its_tracks() {
        let song = song(&[&[60], &[61], &[62]]);
        let mut visible = VisibleNotes::default();
        visible.set_tracks(Some(Arc::from([false, true, false])));
        visible.advance(&song.data, 0, 2_000_000, 1000);
        let tracks: Vec<_> = visible.window.iter().map(|n| n.track).collect();
        assert!(!tracks.is_empty() && tracks.iter().all(|&t| t == 1), "{tracks:?}");
    }

    #[test]
    fn layer_windows_scale_with_speed_and_survive_song_start() {
        let song = song(&[&[60], &[61], &[62]]);
        let mut parallax = Parallax::default();
        let layers = parallax.frame(&song, 2_000_000, 4_000_000, 1.0, 0.5, 600, 1, true, 1 << 20);
        assert_eq!(layers.len(), 3);
        assert_eq!((layers[0].view.height_time, layers[0].view.now_us), (2.0, 1_000_000));
        assert_eq!((layers[2].view.height_time, layers[2].view.now_us, layers[2].view.time_shift), (4.0, 0, 0.0));
        assert!(layers[0].view.depth[0] < layers[2].view.depth[0] && layers[0].view.brightness > layers[2].view.brightness);
        let layers = Parallax::default().frame(&song, 500_000, 4_000_000, 1.0, 0.5, 600, 1, true, 1 << 20);
        assert_eq!((layers[2].view.now_us, layers[2].view.time_shift), (0, 1.5));
    }

    #[test]
    fn cap_drops_far_layers_first() {
        let song = song(&[&[60, 64, 67], &[61], &[62]]);
        let layers = Parallax::default().frame(&song, 1_000_000, 4_000_000, 1.0, 0.5, 600, 1, true, 2);
        assert_eq!(layers.len(), 1);
        assert_eq!((layers[0].view.depth[0], layers[0].instances.len()), (0.0, 2));
    }
}
