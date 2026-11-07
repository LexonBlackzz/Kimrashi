//! A seek index for big files. Finding the notes at a position means reading every record
//! before it (a second or more in a black MIDI with billions of notes). The index keeps a
//! reader per track every `step` ticks, plus the notes that last longer than `step`, so a
//! seek starts at the last snapshot at least `step` before the target: every shorter note
//! that began earlier has finished by then, and the long ones are listed here.

use rayon::prelude::*;

use crate::parser::{Record, TrackReader};
use crate::song::SongData;

/// Files with fewer notes seek fast enough without it.
pub const MIN_NOTES: u64 = 20_000_000;
/// More long notes than this and listing them would cost more than it saves.
const MAX_LONG: usize = 8_000_000;
/// Most controller/SysEx records kept (a seek replays the ones before its snapshot).
const MAX_OTHER: usize = 2_000_000;
/// Memory for the snapshots.
const MAX_SNAPSHOT_BYTES: usize = 96 << 20;

/// A note that lasts longer than the index's `step`, or is never released.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LongNote {
    pub tick: u32,
    pub ch: u8,
    pub key: u8,
    pub vel: u8,
    pub dur: Option<u32>,
}

/// A track's records that are not notes, with their ticks.
pub type Others = Vec<(u32, Record)>;

pub struct SeekIndex {
    step: u32,
    /// `snapshots[k - 1]` has each track read up to (not including) the records at or after
    /// tick `k * step`.
    snapshots: Vec<Vec<TrackReader>>,
    /// Notes that began before each snapshot, counting a repeated track as many times as it
    /// stands for.
    started: Vec<u64>,
    /// Per track, in record order.
    long: Vec<Vec<LongNote>>,
    /// Per track; absent when there are too many to keep.
    other: Option<Vec<Others>>,
}

/// Where a seek can start from.
pub struct Resume<'a> {
    /// Tick of the snapshot: records from here on are read, earlier ones are not.
    pub before: u32,
    pub readers: &'a [TrackReader],
    pub started: u64,
    /// Per track, the long notes (those that began before `before` matter).
    pub long: &'a [Vec<LongNote>],
    pub other: Option<&'a [Others]>,
}

impl SeekIndex {
    pub fn build(song: &SongData) -> Option<Self> {
        if song.midi.note_count < MIN_NOTES { return None; }
        let step = song.tempo.time_to_tick(5_000_000).max(1);
        Self::build_with(song, step)
    }

    /// `step` ticks between snapshots, which is also how long a note may be before it is listed.
    pub fn build_with(song: &SongData, mut step: u32) -> Option<Self> {
        let midi = &song.midi;
        let tracks = midi.tracks.len();
        // Snapshots cost one reader per track each; space them out if that would be too much.
        let snapshots = |step: u32| (midi.end_tick / step) as usize;
        while snapshots(step) * tracks * std::mem::size_of::<TrackReader>() > MAX_SNAPSHOT_BYTES && step < u32::MAX / 2 {
            step *= 2;
        }
        let count = snapshots(step);
        if count < 2 { return None; }
        // Its own small pool: building runs in the background while the file plays.
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(std::thread::available_parallelism().map_or(2, |n| (n.get() / 4).clamp(2, 8)))
            .thread_name(|i| format!("seek-index-{i}"))
            .build().ok()?;
        let per_track: Vec<(Vec<TrackReader>, Vec<u32>, Vec<LongNote>, Others)> = pool.install(|| {
            (0..tracks).into_par_iter().map(|i| scan_track(song, i, step, count)).collect()
        });
        if per_track.iter().map(|t| t.2.len()).sum::<usize>() > MAX_LONG { return None; }
        let keep_other = per_track.iter().map(|t| t.3.len()).sum::<usize>() <= MAX_OTHER;
        let mut index = SeekIndex {
            step,
            snapshots: vec![Vec::with_capacity(tracks); count],
            started: vec![0; count],
            long: Vec::with_capacity(tracks),
            other: keep_other.then(|| Vec::with_capacity(tracks)),
        };
        for (i, (readers, started, long, other)) in per_track.into_iter().enumerate() {
            for k in 0..count {
                index.snapshots[k].push(readers[k]);
                index.started[k] += started[k] as u64 * midi.track_copies[i];
            }
            index.long.push(long);
            if let Some(all) = &mut index.other { all.push(other); }
        }
        Some(index)
    }

    /// The snapshot to start a seek to `tick` from: the last one at least `step` before it.
    pub fn resume(&self, tick: u32) -> Option<Resume<'_>> {
        let k = ((tick / self.step).checked_sub(1)? as usize).min(self.snapshots.len());
        if k == 0 { return None; }
        Some(Resume {
            before: k as u32 * self.step,
            readers: &self.snapshots[k - 1],
            started: self.started[k - 1],
            long: &self.long,
            other: self.other.as_deref(),
        })
    }

    pub fn summary(&self) -> String {
        let long: usize = self.long.iter().map(Vec::len).sum();
        let other: usize = self.other.as_ref().map_or(0, |o| o.iter().map(Vec::len).sum());
        let bytes = self.snapshots.iter().map(|s| s.len()).sum::<usize>() * std::mem::size_of::<TrackReader>()
            + long * std::mem::size_of::<LongNote>() + other * std::mem::size_of::<(u32, Record)>();
        format!("{} snapshots every {} ticks, {long} long notes, {} other records, {:.1} MB",
            self.snapshots.len(), self.step, if self.other.is_some() { other.to_string() } else { "too many".into() }, bytes as f64 / 1e6)
    }
}

/// One track read once: its reader at every snapshot, the notes started before each, its long
/// notes and its records that are not notes.
fn scan_track(song: &SongData, track: usize, step: u32, count: usize) -> (Vec<TrackReader>, Vec<u32>, Vec<LongNote>, Others) {
    let data = song.midi.track(track);
    let (mut r, mut started, mut run) = (TrackReader::default(), 0u32, [0u8; 16]);
    let (mut readers, mut counts, mut long, mut other) = (Vec::with_capacity(count), Vec::with_capacity(count), Vec::new(), Vec::new());
    for k in 1..=count {
        let limit = (k as u32).saturating_mul(step);
        while let Some((tick, rec)) = r.next_before(data, limit) {
            let Record::Note { ch, key, vel, dur } = rec else {
                if other.len() <= MAX_OTHER { other.push((tick, rec)); }
                continue;
            };
            started += 1;
            if dur.map_or(true, |d| d > step) {
                long.push(LongNote { tick, ch, key, vel, dur });
            } else if r.same_tick_run(data, &mut run).is_some() {
                started += 16; // plain notes behind a short one share its tick and length
            }
        }
        readers.push(r);
        counts.push(started);
    }
    // Past the last snapshot: only long notes still matter (no seek replays later records).
    while let Some((tick, rec)) = r.next(data) {
        if let Record::Note { ch, key, vel, dur } = rec {
            if dur.map_or(true, |d| d > step) { long.push(LongNote { tick, ch, key, vel, dur }); }
        }
    }
    (readers, counts, long, other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parser, player::Cursor, song::TempoMap};
    use std::sync::Arc;

    /// Four tracks of random notes (some never released, some very long), controllers, program
    /// and pitch bend changes, and SysEx including a GM reset.
    fn song() -> SongData {
        let mut seed = 12345u64;
        let mut rand = move |n: u64| { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 33) % n };
        let mut file = b"MThd\0\0\0\x06\0\x01\0\x04".to_vec();
        file.extend_from_slice(&480u16.to_be_bytes());
        for track in 0..4u8 {
            let mut events: Vec<(u64, u8, Vec<u8>)> = Vec::new(); // tick, order at a tick, bytes
            for _ in 0..300 {
                let (start, key) = (rand(6_000), 40 + rand(40) as u8);
                events.push((start, 1, vec![0x90 | track, key, 1 + rand(100) as u8]));
                let end = match rand(8) { 0 => None, 1 => Some(start + 1_000 + rand(4_000)), _ => Some(start + 1 + rand(300)) };
                if let Some(end) = end { events.push((end, 0, vec![0x80 | track, key, 0])); }
            }
            for _ in 0..40 {
                let tick = rand(6_000);
                events.push((tick, 1, match rand(3) {
                    0 => vec![0xB0 | track, [0, 7, 10, 64, 6, 101][rand(6) as usize], rand(128) as u8],
                    1 => vec![0xC0 | track, rand(128) as u8],
                    _ => vec![0xE0 | track, rand(128) as u8, rand(128) as u8],
                }));
            }
            events.push((rand(6_000), 1, vec![0xF0, 0x05, 0x7E, 0x7F, 0x09, 0x01, 0xF7]));
            events.push((rand(6_000), 1, vec![0xF0, 0x03, 0x41, 0x10, 0x42, 0xF7]));
            events.sort_by_key(|e| (e.0, e.1));
            let mut body = Vec::new();
            let mut last = 0;
            for (tick, _, bytes) in events {
                let mut delta = tick - last;
                last = tick;
                let mut vlv = vec![(delta & 127) as u8];
                delta >>= 7;
                while delta > 0 { vlv.push((delta & 127) as u8 | 128); delta >>= 7; }
                body.extend(vlv.iter().rev());
                body.extend(bytes);
            }
            body.extend_from_slice(&[0, 0xff, 0x2f, 0]);
            file.extend_from_slice(b"MTrk");
            file.extend_from_slice(&(body.len() as u32).to_be_bytes());
            file.extend(body);
        }
        let midi = Arc::new(parser::MidiFile::parse_stream(
            Box::new(parser::Plain(std::io::Cursor::new(file))), 127, &parser::Progress::default()).unwrap());
        let tempo = Arc::new(TempoMap::from_midi(&midi));
        SongData { path: String::new(), midi, tempo, load_time: Default::default(), seek: Default::default() }
    }

    #[test]
    fn seeking_from_a_snapshot_gives_what_reading_from_the_start_gives() {
        let data = song();
        let index = SeekIndex::build_with(&data, 400).unwrap();
        assert!(index.long.iter().any(|l| l.iter().any(|n| n.dur.is_none())), "no never-released note to test");
        assert!(index.long.iter().any(|l| l.iter().any(|n| n.dur.is_some())), "no long note to test");
        let enabled = crate::render_tracks(&data);
        let mut resumed = 0;
        for tick in (0..data.midi.end_tick + 800).step_by(37) {
            let Some(resume) = index.resume(tick) else { continue };
            resumed += 1;
            // What playback needs: notes sounding, controllers, positions.
            let plain = Cursor::seek_from(&data, tick, None);
            assert!(Cursor::seek_from(&data, tick, Some(&resume)).same_as(&plain), "audio seek to tick {tick}");
            // What the screen needs: the notes alive in a window from there.
            let now_us = data.tempo.tick_to_time_approx(tick, &mut 0);
            for reach in [now_us + 250_000, now_us + 2_000_000] {
                let mut readers = vec![parser::TrackReader::default(); data.midi.tracks.len()];
                let exact = crate::decode_tracks(&data, &mut readers, now_us, reach, Some(&enabled));
                let mut readers = resume.readers.to_vec();
                let fast = crate::decode_tracks_from(&data, &mut readers, now_us, reach, Some(&enabled), Some(&resume));
                assert_eq!(fast.notes, exact.notes, "decode at tick {tick}, reach {reach}");
            }
        }
        assert!(resumed > 100, "only {resumed} seeks used a snapshot");
    }
}
