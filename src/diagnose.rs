//! "Diagnose MIDI": the numbers that say how hard a song is to load and play.

use crate::parser::Record;
use crate::song::SongData;

pub struct Report {
    pub notes: u64,
    pub tracks: usize,
    pub file_bytes: u64,
    /// Size on disk when the file came out of an archive.
    pub archive_bytes: Option<u64>,
    /// Memory the packed song takes once open.
    pub packed_bytes: u64,
    pub duration_us: u64,
    pub ppqn: u16,
    pub first_bpm: f64,
    pub min_bpm: f64,
    pub max_bpm: f64,
    pub tempo_changes: usize,
    /// Most note-ons in any 1 s window.
    pub max_nps: u64,
    /// Most notes sounding at once.
    pub max_polyphony: u64,
}

fn bpm(us_per_quarter: u32) -> f64 {
    60_000_000.0 / us_per_quarter.max(1) as f64
}

/// Reads every note once. Starts and ends are counted per millisecond of song time.
// ponytail: 1 ms resolution (16 B per ms of song, ~580 MB for 10 h) and a note released within
// the ms it starts counts as sounding; an exact sweep would need a time-ordered merge.
pub fn run(song: &SongData) -> Report {
    let (midi, tempo) = (&song.midi, &song.tempo);
    let ms = (tempo.duration_us() / 1000) as usize + 2;
    let (mut starts, mut ends) = (vec![0u64; ms], vec![0u64; ms]);
    let at = |us: u64| ((us / 1000) as usize).min(ms - 1);

    for i in 0..midi.tracks.len() {
        let (t, copies) = (midi.track(i), midi.track_copies[i]);
        let (mut r, mut on_idx, mut off_idx) = (crate::parser::TrackReader::default(), 0, 0);
        while let Some((tick, rec)) = r.next(t) {
            if let Record::Note { dur, .. } = rec {
                starts[at(tempo.tick_to_time_with_cache(tick, &mut on_idx))] += copies;
                if let Some(d) = dur {
                    ends[at(tempo.tick_to_time_with_cache(tick.saturating_add(d), &mut off_idx))] += copies;
                }
            }
        }
    }

    let (mut window, mut max_nps) = (0u64, 0u64);
    for i in 0..ms {
        window += starts[i];
        if i >= 1000 {
            window -= starts[i - 1000];
        }
        max_nps = max_nps.max(window);
    }
    let (mut sounding, mut max_polyphony) = (0u64, 0u64);
    for i in 0..ms {
        sounding += starts[i];
        max_polyphony = max_polyphony.max(sounding);
        sounding -= ends[i].min(sounding);
    }

    let bpms = || midi.tempos.iter().map(|&(_, t)| bpm(t));
    let first_bpm = midi.tempos.iter().find(|t| t.0 == 0).map_or(120.0, |&(_, t)| bpm(t));
    Report {
        notes: midi.note_count,
        tracks: midi.source_tracks,
        file_bytes: midi.file_bytes,
        archive_bytes: midi.source.as_ref().map(|s| s.1),
        packed_bytes: midi.packed_bytes(),
        duration_us: tempo.duration_us(),
        ppqn: midi.division,
        first_bpm,
        min_bpm: bpms().fold(first_bpm, f64::min),
        max_bpm: bpms().fold(first_bpm, f64::max),
        tempo_changes: midi.tempos.len(),
        max_nps,
        max_polyphony,
    }
}
