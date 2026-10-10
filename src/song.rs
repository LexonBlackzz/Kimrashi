use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use crate::parser::{MidiFile, ParseError};

#[derive(Clone, Debug)]
pub struct TempoSegment {
    pub start_tick: u32,
    pub start_us: u64,
    pub end_tick: u32,
    pub end_us: u64,
    pub us_per_quarter: u32,
}

#[derive(Clone, Debug)]
pub struct TempoMap {
    segments: Vec<TempoSegment>,
    division: u16,
    duration_ticks: u32,
    duration_us: u64,
}

impl TempoMap {
    pub fn from_midi(midi: &MidiFile) -> Self {
        let mut tempo_by_tick: BTreeMap<u32, u32> = BTreeMap::new();
        tempo_by_tick.insert(0, 500_000);

        // A zero tempo would divide by zero in tick/time conversions.
        tempo_by_tick.extend(midi.tempos.iter().map(|&(tick, t)| (tick, t.max(1))));

        let division = midi.division;
        let max_tick = midi.end_tick;

        let mut segments = Vec::new();
        let mut last_tick = 0u32;
        let mut last_us = 0u64;
        let mut last_tempo = 500_000u32;

        for (&tick, &tempo) in tempo_by_tick.iter() {
            if tick > last_tick {
                let delta_ticks = tick - last_tick;
                let delta_us = tempo_ticks_to_us(delta_ticks, last_tempo, division);
                let end_us = last_us + delta_us;
                segments.push(TempoSegment {
                    start_tick: last_tick,
                    start_us: last_us,
                    end_tick: tick,
                    end_us,
                    us_per_quarter: last_tempo,
                });
                last_us = end_us;
                last_tick = tick;
            }
            last_tempo = tempo;
        }

        let effective_end_tick = max_tick.max(last_tick);
        if segments.is_empty() {
            segments.push(TempoSegment {
                start_tick: 0,
                start_us: 0,
                end_tick: effective_end_tick,
                end_us: tempo_ticks_to_us(effective_end_tick, last_tempo, division),
                us_per_quarter: last_tempo,
            });
            last_us = segments.last().map(|s| s.end_us).unwrap_or(0);
        } else if effective_end_tick > last_tick {
            let delta_ticks = effective_end_tick - last_tick;
            let delta_us = tempo_ticks_to_us(delta_ticks, last_tempo, division);
            let end_us = last_us + delta_us;
            segments.push(TempoSegment {
                start_tick: last_tick,
                start_us: last_us,
                end_tick: effective_end_tick,
                end_us,
                us_per_quarter: last_tempo,
            });
            last_us = end_us;
        } else if let Some(last) = segments.last_mut() {
            last.end_tick = effective_end_tick;
            last.end_us = last_us;
        }

        // Add a 3-second buffer to the end of the song to allow trailing notes and reverb to decay.
        const END_BUFFER_US: u64 = 3_000_000;
        let buffer_ticks = tempo_us_to_ticks(END_BUFFER_US, last_tempo, division);

        let final_duration_us = last_us + END_BUFFER_US;
        let final_duration_ticks = effective_end_tick + buffer_ticks;

        // Extend the last segment to make the time mapping aware of the new buffer duration.
        if let Some(last_seg) = segments.last_mut() {
            last_seg.end_tick = final_duration_ticks;
            last_seg.end_us = final_duration_us;
        } else {
            // This case handles empty or unusual MIDI files with no tempo map segments.
            segments.push(TempoSegment {
                start_tick: effective_end_tick,
                start_us: last_us,
                end_tick: final_duration_ticks,
                end_us: final_duration_us,
                us_per_quarter: last_tempo,
            });
        }

        Self {
            segments,
            division,
            duration_ticks: final_duration_ticks,
            duration_us: final_duration_us,
        }
    }

    pub fn tick_to_time_with_cache(&self, tick: u32, cached_idx: &mut usize) -> u64 {
        // Try to use the cached segment first
        if let Some(seg) = self.segments.get(*cached_idx) {
            // Check if the tick falls within the cached segment's range
            // (seg.end_tick can be equal to seg.start_tick for single-event segments, handle this)
            if (seg.end_tick > seg.start_tick && tick >= seg.start_tick && tick < seg.end_tick) ||
               (seg.end_tick == seg.start_tick && tick == seg.start_tick) {
                let delta_ticks = tick.saturating_sub(seg.start_tick);
                let delta_us = tempo_ticks_to_us(delta_ticks, seg.us_per_quarter, self.division);
                return seg.start_us.saturating_add(delta_us);
            }
        }

        // Cache miss or tick outside cached segment, perform binary search
        let idx = self.segments.partition_point(|seg| {
            if seg.end_tick == seg.start_tick {
                seg.start_tick < tick
            } else {
                seg.end_tick <= tick
            }
        });
        
        // `idx` is the segment containing `tick`; cache it for the next lookup.
        *cached_idx = idx;

        if let Some(seg) = self.segments.get(idx) {
            let delta_ticks = tick.saturating_sub(seg.start_tick);
            let delta_us = tempo_ticks_to_us(delta_ticks, seg.us_per_quarter, self.division);
            return seg.start_us.saturating_add(delta_us);
        }

        self.duration_us
    }

    pub fn time_to_tick(&self, time_us: u64) -> u32 {
        let idx = self.segments.partition_point(|seg| {
            if seg.end_us == seg.start_us {
                seg.start_us < time_us
            } else {
                seg.end_us <= time_us
            }
        });

        if let Some(seg) = self.segments.get(idx) {
            let delta_us = time_us.saturating_sub(seg.start_us);
            let ticks = tempo_us_to_ticks(delta_us, seg.us_per_quarter, self.division);
            return seg.start_tick.saturating_add(ticks);
        }

        self.duration_ticks
    }

    pub fn duration_us(&self) -> u64 {
        self.duration_us
    }

    /// Microseconds per tick in segment `idx` (as left by `tick_to_time_with_cache`).
    pub fn us_per_tick(&self, idx: usize) -> f32 {
        self.segments.get(idx).map_or(0.0, |s| s.us_per_quarter as f32 / self.division as f32)
    }
}

pub struct SongData {
    pub path: String,
    pub midi: Arc<MidiFile>,
    pub tempo: Arc<TempoMap>,
    /// Wall time of the whole load (open, parse, tempo map).
    pub load_time: std::time::Duration,
}

impl SongData {
    /// `vel_levels` and `progress`: see `MidiFile::open_and_parse`.
    pub fn load<P: AsRef<Path>>(
        path: P,
        vel_levels: u8,
        progress: &crate::parser::Progress,
    ) -> Result<Arc<Self>, ParseError> {
        let start = std::time::Instant::now();
        let path_ref = path.as_ref();
        let midi = Arc::new(MidiFile::open_and_parse(path_ref, vel_levels, progress)?);
        let tempo = Arc::new(TempoMap::from_midi(&midi));
        Ok(Arc::new(SongData {
            path: path_ref.to_string_lossy().into_owned(),
            midi,
            tempo,
            load_time: start.elapsed(),
        }))
    }
}

#[inline]
fn tempo_ticks_to_us(delta_ticks: u32, us_per_quarter: u32, division: u16) -> u64 {
    if delta_ticks == 0 {
        return 0;
    }
    // Tempo is 24-bit, so this fits in u64 (hot path: u128 division is much slower).
    delta_ticks as u64 * us_per_quarter as u64 / division as u64
}

#[inline]
fn tempo_us_to_ticks(delta_us: u64, us_per_quarter: u32, division: u16) -> u32 {
    if delta_us == 0 {
        return 0;
    }
    let num = delta_us as u128 * division as u128;
    let den = us_per_quarter as u128;
    (num / den) as u32
}

