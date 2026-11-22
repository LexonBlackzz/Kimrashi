//! The velocity matrix view's state: which (key, velocity) cells have a sounding note, read
//! straight from the packed tracks (the on-screen notes carry no velocity, and merge notes that
//! sound together on one key). See docs/superpowers/specs/2026-11-22-velocity-matrix-design.md.

use crate::parser::{Record, TrackReader};
use crate::song::SongData;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::{Arc, Weak};

/// One cell: how many notes sound on it, and the track (of the file) and channel of the newest.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Cell {
    pub count: u32,
    pub track: u16,
    pub ch: u8,
}

pub struct Matrix {
    /// The song read. Weak, but it pins the address, so a later song can't be mistaken for it.
    song: Weak<SongData>,
    readers: Vec<TrackReader>,
    /// Records are read through this tick (None: nothing read yet).
    done: Option<u32>,
    /// Sounding notes by end tick: (end, cell index).
    ending: BinaryHeap<Reverse<(u32, u16)>>,
    cells: Vec<Cell>,
}

impl Matrix {
    pub fn new() -> Self {
        Self { song: Weak::new(), readers: Vec::new(), done: None, ending: BinaryHeap::new(), cells: vec![Cell::default(); 128 * 128] }
    }

    pub fn cell(&self, key: u8, vel: u8) -> Cell { self.cells[cell_index(key, vel) as usize] }

    /// Every lit cell: (key, velocity, cell).
    pub fn lit(&self) -> impl Iterator<Item = (u8, u8, Cell)> + '_ {
        self.cells.iter().enumerate().filter(|(_, c)| c.count > 0).map(|(i, &c)| ((i / 128) as u8, (i % 128) as u8, c))
    }

    /// Frees the state (leaving the view).
    pub fn clear(&mut self) { if self.song.strong_count() > 0 || !self.readers.is_empty() { *self = Self::new(); } }

    /// Brings the grid to `tick`: a note sounds over `[tick, tick + dur)`. Going backwards replays
    /// from the start, or from the seek index's snapshot when that is ahead of what was read.
    pub fn advance(&mut self, song: &Arc<SongData>, tick: u32) {
        let tracks = song.midi.tracks.len();
        if !Weak::ptr_eq(&self.song, &Arc::downgrade(song)) || self.done.is_some_and(|d| tick < d) {
            self.reset(tracks);
            self.song = Arc::downgrade(song);
        }
        if let Some(resume) = song.seek.get().and_then(|index| index.resume(tick)) {
            if self.done.is_none_or(|d| resume.before > d + 1) {
                self.reset(tracks);
                self.readers.copy_from_slice(resume.readers);
                for (track, long) in resume.long.iter().enumerate() {
                    for n in long.iter().take_while(|n| n.tick < resume.before) {
                        self.start(song, track, tick, n.tick, n.ch, n.key, n.vel, n.dur);
                    }
                }
            }
        }
        for track in 0..tracks {
            let data = song.midi.track(track);
            let mut r = self.readers[track];
            while let Some((at, rec)) = r.next_before(data, tick.saturating_add(1)) {
                if let Record::Note { ch, key, vel, dur } = rec { self.start(song, track, tick, at, ch, key, vel, dur); }
            }
            self.readers[track] = r;
        }
        while let Some(&Reverse((end, i))) = self.ending.peek() {
            if end > tick { break; }
            self.ending.pop();
            self.cells[i as usize].count -= 1;
        }
        self.done = Some(tick);
    }

    /// A note at `at` (read while bringing the grid to `now`): lit unless it already ended.
    #[allow(clippy::too_many_arguments)]
    fn start(&mut self, song: &SongData, track: usize, now: u32, at: u32, ch: u8, key: u8, vel: u8, dur: Option<u32>) {
        let end = dur.map_or(u32::MAX, |d| at.saturating_add(d));
        if end <= now { return; }
        let i = cell_index(key, vel);
        let cell = &mut self.cells[i as usize];
        *cell = Cell { count: cell.count + 1, track: song.midi.track_origin[track] as u16, ch };
        self.ending.push(Reverse((end, i)));
    }

    fn reset(&mut self, tracks: usize) {
        self.readers = vec![TrackReader::default(); tracks];
        self.done = None;
        self.ending.clear();
        self.cells.fill(Cell::default());
    }
}

fn cell_index(key: u8, vel: u8) -> u16 { (key as u16 & 127) * 128 + (vel as u16 & 127) }

/// Where cell (key, velocity) sits in `area`: keys across, velocity 127 at the top.
pub fn cell_rect(area: egui::Rect, key: u8, vel: u8) -> egui::Rect {
    let (w, h) = (area.width() / 128.0, area.height() / 128.0);
    egui::Rect::from_min_size(egui::pos2(area.left() + key as f32 * w, area.bottom() - (vel as f32 + 1.0) * h), egui::vec2(w, h))
}

/// A cell's colour: what the roll would give its newest note.
pub fn color(song: &crate::VisualSong, key: u8, c: Cell) -> egui::Color32 {
    song.color_for(&crate::NoteSegment { source: 0, start_us: 0, end_us: 0, track: c.track, channel: c.ch, pitch: key })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    /// A song from per-track notes (tick, key, velocity, length in ticks), 480 ticks per beat.
    fn song(tracks: &[Vec<(u32, u8, u8, u32)>]) -> Arc<SongData> {
        let mut file = b"MThd\0\0\0\x06\0\x01".to_vec();
        file.extend_from_slice(&(tracks.len() as u16).to_be_bytes());
        file.extend_from_slice(&480u16.to_be_bytes());
        for notes in tracks {
            let mut events: Vec<(u32, [u8; 3])> = Vec::new();
            for &(tick, key, vel, len) in notes {
                events.push((tick, [0x90, key, vel]));
                events.push((tick + len, [0x80, key, 0]));
            }
            events.sort_by_key(|e| (e.0, e.1[0] == 0x90)); // offs before ons at a tick
            let (mut body, mut last) = (Vec::new(), 0);
            for (tick, msg) in events {
                let mut d = tick - last;
                let mut bytes = vec![(d & 0x7F) as u8];
                while { d >>= 7; d > 0 } { bytes.insert(0, (d & 0x7F) as u8 | 0x80); }
                body.extend(bytes);
                body.extend(msg);
                last = tick;
            }
            body.extend([0, 0xFF, 0x2F, 0]);
            file.extend_from_slice(b"MTrk");
            file.extend_from_slice(&(body.len() as u32).to_be_bytes());
            file.extend(body);
        }
        let midi = parser::MidiFile::parse_stream(Box::new(parser::Plain(std::io::Cursor::new(file))), 127,
            &parser::Progress::default()).unwrap();
        SongData::from_midi(String::new(), midi, Default::default())
    }

    fn lit(m: &Matrix) -> Vec<(u8, u8, u32)> {
        (0..128u8).flat_map(|k| (0..128u8).map(move |v| (k, v)))
            .filter_map(|(k, v)| { let c = m.cell(k, v); (c.count > 0).then_some((k, v, c.count)) }).collect()
    }

    #[test]
    fn notes_on_one_key_light_each_velocity() {
        let s = song(&[vec![(0, 60, 40, 480)], vec![(240, 60, 90, 720)]]);
        let mut m = Matrix::new();
        m.advance(&s, 300);
        assert_eq!(lit(&m), [(60, 40, 1), (60, 90, 1)]);
        assert_eq!(m.cell(60, 90).track, 1, "the cell knows its note's track");
        m.advance(&s, 480);
        assert_eq!(lit(&m), [(60, 90, 1)], "a note ends at its end tick");
        m.advance(&s, 960);
        assert_eq!(lit(&m), []);
    }

    #[test]
    fn jumps_give_the_same_grid_as_walking() {
        // Short and long notes on a few tracks; a seek index with a small step, as big files get.
        let tracks: Vec<Vec<_>> = (0..4u32).map(|t| (0..200u32).map(|i| {
            let len = if i % 17 == 0 { 3000 } else { 10 + (i * 7 + t) % 90 };
            (i * 37 + t * 5, 40 + ((i + t) % 30) as u8, 20 + ((i * 3 + t) % 80) as u8, len)
        }).collect()).collect();
        let s = song(&tracks);
        let _ = s.seek.set(crate::seek::SeekIndex::build_with(&s, 200).expect("index"));
        let mut walked = Matrix::new();
        let mut jumped = Matrix::new();
        for tick in (0..8000).step_by(50) {
            walked.advance(&s, tick);
            if tick % 1150 == 0 || tick == 7950 {
                let mut fresh = Matrix::new();
                fresh.advance(&s, tick);
                assert_eq!(lit(&fresh), lit(&walked), "fresh at {tick}");
            }
        }
        for tick in [7000, 900, 6100, 6150, 300, 7950] {
            jumped.advance(&s, tick);
            let mut fresh = Matrix::new();
            fresh.advance(&s, tick);
            assert_eq!(lit(&jumped), lit(&fresh), "jump to {tick}");
        }
    }
}
