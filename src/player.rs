use std::io::Write;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use rayon::prelude::*;

use crate::clock::PlaybackClock;
use crate::omnimidi::{pack_short, OmniMIDI};
use crate::output::{Output, OutputChoice, SvmsSetup};
use crate::parser::{Record, TrackReader};
use crate::song::SongData;

pub enum PlayerCommand {
    Play(Arc<SongData>),
    Stop,
    Pause,
    Resume,
    Seek(u64), // absolute time in microseconds
    SetOutput(OutputChoice),
    /// Max note-ons per second sent to the synth; 0 = no limit (skip only when lagging).
    SetSynthLimit(u32),
    /// Smart note skipping on/off. Off sends every note (for stress-testing a synth).
    SetSkipping(bool),
    /// Write the per-second stats to a CSV file (see `StatsLog`).
    SetStatsLog(bool),
    /// Submit through SVMS's native batch API instead of one `SendDirectData` per message.
    SetSvmsBatch(bool),
}

/// Player-thread and synth performance over the last second.
#[derive(Clone, Debug, Default)]
pub struct SchedStats {
    /// MIDI messages sent to the synth per second.
    pub msgs_per_sec: u32,
    /// Distinct ticks (event batches) processed per second.
    pub ticks_per_sec: u32,
    /// Fraction of wall time spent inside synth calls.
    pub synth_busy: f32,
    /// Average time per synth call, in nanoseconds.
    pub synth_ns_per_msg: f32,
    /// Longest time one tick's batch of synth calls took (µs). A synth that blocks
    /// (e.g. a full buffer) shows up here.
    pub worst_batch_us: u32,
    /// Fraction of wall time the player spent on its own work (decoding, queues).
    pub sched_busy: f32,
    /// Messages go to the synth through its batch entry point (one call per tick).
    pub batched: bool,
}

/// Accumulates `SchedStats` over one second.
struct StatsAcc {
    started: Instant,
    msgs: u64,
    ticks: u32,
    synth: Duration,
    worst_batch: Duration,
    sched: Duration,
}

impl StatsAcc {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            msgs: 0,
            ticks: 0,
            synth: Duration::ZERO,
            worst_batch: Duration::ZERO,
            sched: Duration::ZERO,
        }
    }

    fn take(&mut self) -> SchedStats {
        let wall = self.started.elapsed().as_secs_f64().max(1e-9);
        let stats = SchedStats {
            msgs_per_sec: (self.msgs as f64 / wall) as u32,
            ticks_per_sec: (self.ticks as f64 / wall) as u32,
            synth_busy: (self.synth.as_secs_f64() / wall) as f32,
            synth_ns_per_msg: (self.synth.as_nanos() as f64 / self.msgs.max(1) as f64) as f32,
            worst_batch_us: self.worst_batch.as_micros() as u32,
            sched_busy: (self.sched.as_secs_f64() / wall) as f32,
            batched: false, // filled in by the caller
        };
        *self = Self::new();
        stats
    }
}

#[derive(Clone, Debug)]
pub struct PlayerTelemetry {
    pub notes_played: u64,
    pub nps: u64,
    /// Note-ons per second not played: skipped to keep the synth in time, or copies of
    /// a repeated track (see `MidiFile::track_copies`).
    pub skipped_nps: u64,
    /// How far behind the clock event sending is.
    pub lag_us: u64,
    pub polyphony: u32,
    pub peak_polyphony: u32,
    pub midi_path: String,
    pub playback_time_us: u64,
    pub duration_us: u64,
    pub is_playing: bool,
    pub stats: SchedStats,
    /// CSV file the stats are being logged to.
    pub log_path: Option<String>,
    /// SVMS's own view of the last second, when the output is SVMS.
    pub svms: Option<crate::svms::Stats>,
}

impl Default for PlayerTelemetry {
    fn default() -> Self {
        Self {
            notes_played: 0,
            nps: 0,
            skipped_nps: 0,
            lag_us: 0,
            polyphony: 0,
            peak_polyphony: 0,
            midi_path: "(none)".to_string(),
            playback_time_us: 0,
            duration_us: 0,
            is_playing: false,
            stats: SchedStats::default(),
            log_path: None,
            svms: None,
        }
    }
}

/// Per-second stats written as CSV rows, one file per song (or per time logging is
/// switched on), in a `stats` folder next to the executable.
struct StatsLog {
    file: std::io::BufWriter<std::fs::File>,
    path: String,
    started: Instant,
}

impl StatsLog {
    fn create(song_path: &str) -> std::io::Result<Self> {
        let dir = std::env::current_exe()?
            .parent()
            .map(|d| d.join("stats"))
            .unwrap_or_else(|| "stats".into());
        std::fs::create_dir_all(&dir)?;
        let stem = std::path::Path::new(song_path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "song".into());
        let unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let path = dir.join(format!("{stem}_{unix}.csv"));
        let mut file = std::io::BufWriter::new(std::fs::File::create(&path)?);
        writeln!(
            file,
            "wall_s,song_s,speed,output,skipping,nps,skipped_nps,polyphony,msgs_per_s,\
             ticks_per_s,synth_ns_per_msg,synth_busy_pct,worst_batch_us,player_busy_pct,lag_ms,batched"
        )?;
        Ok(Self {
            file,
            path: path.to_string_lossy().into_owned(),
            started: Instant::now(),
        })
    }

    fn row(&mut self, t: &PlayerTelemetry, speed: f64, output: &OutputChoice, skipping: bool) {
        let s = &t.stats;
        let output = match output {
            OutputChoice::Kdmapi => "kdmapi".to_owned(),
            OutputChoice::WinMM(id) => format!("winmm{id}"),
        };
        // Flushed per row so the file is usable while playing (and after a crash).
        let _ = writeln!(
            self.file,
            "{:.3},{:.3},{speed},{output},{skipping},{},{},{},{},{},{:.1},{:.2},{},{:.2},{:.3},{}",
            self.started.elapsed().as_secs_f64(),
            t.playback_time_us as f64 / 1e6,
            t.nps,
            t.skipped_nps,
            t.polyphony,
            s.msgs_per_sec,
            s.ticks_per_sec,
            s.synth_ns_per_msg,
            s.synth_busy * 100.0,
            s.worst_batch_us,
            s.sched_busy * 100.0,
            t.lag_us as f64 / 1e3,
            s.batched,
        )
        .and_then(|_| self.file.flush());
    }
}

/// Monotone priority queue on u32 keys (a radix heap): keys pushed are never below the
/// smallest key already taken, which holds for ticks during playback. Push is one Vec
/// append and the smallest key's items come out as a block, so it's O(1) amortized and
/// cache-friendly where a binary heap pays log n cache-missing steps per item.
///
/// Bucket 0 holds the items whose key equals `last`; bucket b > 0 holds keys whose
/// highest bit differing from `last` is bit b - 1.
struct RadixHeap<T> {
    last: u32,
    buckets: [Vec<(u32, T)>; 33],
}

impl<T: Copy> RadixHeap<T> {
    fn new() -> Self {
        Self {
            last: 0,
            buckets: std::array::from_fn(|_| Vec::new()),
        }
    }

    #[inline]
    fn bucket(&self, key: u32) -> usize {
        (32 - (key ^ self.last).leading_zeros()) as usize
    }

    #[inline]
    fn push(&mut self, key: u32, item: T) {
        debug_assert!(key >= self.last, "radix heap keys must not go backwards");
        let b = self.bucket(key);
        self.buckets[b].push((key, item));
    }

    /// The smallest key, if any (gathering its items into bucket 0).
    fn min_key(&mut self) -> Option<u32> {
        if self.buckets[0].is_empty() {
            let b = (1..33).find(|&b| !self.buckets[b].is_empty())?;
            let mut items = std::mem::take(&mut self.buckets[b]);
            self.last = items.iter().map(|&(k, _)| k).min().unwrap();
            // Every item now lands in a lower bucket, so bucket b stays empty.
            for &(k, item) in &items {
                let nb = self.bucket(k);
                self.buckets[nb].push((k, item));
            }
            items.clear();
            self.buckets[b] = items; // keep the allocation
        }
        Some(self.last)
    }

    /// Move the items with the smallest key into `out` (call `min_key` first).
    #[inline]
    fn take_min(&mut self, out: &mut Vec<T>) {
        out.extend(self.buckets[0].drain(..).map(|(_, item)| item));
    }
}

/// A pending note-off: (channel, key).
type PendingOff = (u8, u8);

/// Something due at a tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Due {
    /// This track has records at the tick.
    Track(u32),
    Off(PendingOff),
}

/// Playback position: a reader per track plus everything still due. Tracks and
/// note-offs share one queue so its position is always the tick being played, which
/// keeps every push at or after it (what a radix heap needs).
struct Cursor {
    readers: Vec<TrackReader>,
    due: RadixHeap<Due>,
}

struct SeekResult {
    cursor: Cursor,
    /// Note-ons for notes that are sounding at the seek point.
    chase: Vec<u32>,
    /// Notes that started before the seek point.
    started: u64,
}

impl Cursor {
    /// Position every track at `tick` (records before it are consumed), scanning
    /// tracks in parallel.
    fn seek(song: &SongData, tick: u32) -> SeekResult {
        let midi = &song.midi;
        let per_track: Vec<_> = (0..midi.tracks.len())
            .into_par_iter()
            .map(|i| {
                let data = midi.track(i);
                let mut r = TrackReader::default();
                let mut active = Vec::new();
                let mut started = 0u64;
                loop {
                    let mut n = r;
                    match n.next(data) {
                        Some((t, rec)) if t < tick => {
                            r = n;
                            if let Record::Note { ch, key, vel, dur } = rec {
                                started += 1;
                                let end = dur.map(|d| t + d);
                                if end.map_or(true, |e| e > tick) {
                                    active.push((ch, key, vel, end));
                                }
                            }
                        }
                        _ => break,
                    }
                }
                (r, active, started)
            })
            .collect();

        let mut res = SeekResult {
            cursor: Cursor {
                readers: Vec::with_capacity(midi.tracks.len()),
                due: RadixHeap::new(),
            },
            chase: Vec::new(),
            started: 0,
        };
        for (i, (r, active, started)) in per_track.into_iter().enumerate() {
            if let Some(t) = r.peek_tick(midi.track(i)) {
                res.cursor.due.push(t, Due::Track(i as u32));
            }
            res.cursor.readers.push(r);
            res.started += started * midi.track_copies[i];
            for (ch, key, vel, end) in active {
                res.chase.push(pack_short(0x90 | ch, key, vel));
                if let Some(e) = end {
                    res.cursor.due.push(e, Due::Off((ch, key)));
                }
            }
        }
        res
    }

    fn next_tick(&mut self) -> Option<u32> {
        self.due.min_key()
    }
}

const SLOTS: usize = 16 * 128;

/// What to do with a due note-off of a sent note.
#[derive(Debug, PartialEq)]
enum OffAction {
    Send,
    /// Skipped notes were merged into this one; keep it sounding until this tick.
    Postpone(u32),
    /// A merged note never ends, so neither does this one.
    Hold,
}

/// What sending costs, for `Skipper::regulate` to budget with.
#[derive(Clone, Copy)]
struct Costs {
    /// Time in the synth per message sent (ns).
    synth_ns: f32,
    /// Our own time per note decoded (ns).
    engine_ns: f32,
    /// Messages sent per note sent (its note-off, and events riding along).
    msgs_per_note: f32,
    /// Song time runs this much faster than the wall clock.
    speed: f64,
}

impl Default for Costs {
    fn default() -> Self {
        Self { synth_ns: 100.0, engine_ns: 20.0, msgs_per_note: 2.0, speed: 1.0 }
    }
}

/// Smart note skipping for synths that can't keep up.
///
/// Every few ms the skipper works out how many notes per second the thread can send
/// (its time, minus what decoding takes, over what the synth charges per note) and
/// compares that with the notes the song asks for, counted per channel+key. When the
/// song asks for more, every key gets the same minimum re-strike interval, the one at
/// which the keys together just fit: a key played less often than that is never
/// touched, a key hammered faster is thinned to that rate. A note-on that re-strikes
/// a key sooner than the interval is skipped:
/// - If the key is still sounding, the skipped note is merged into it: the sounding
///   note's note-off is postponed to the skipped note's end, so sustain isn't chopped.
/// - A note on a silent key is only skipped if it's also short (shorter than the
///   interval); longer notes are always played.
/// - Controllers, pitch bend etc. and note-offs of sent notes are never dropped, so
///   nothing gets stuck, and every key keeps sounding at a bounded rate.
///
/// The cost model can be off (a synth's price per message varies with its load), so
/// lag behind the clock scales the budget down, and on-time playback scales it back up.
struct Skipper {
    /// Minimum time between note-ons on one channel+key; 0 = send everything.
    interval_us: u64,
    /// Time of the last sent note-on per slot (u64::MAX = none yet).
    last_on_us: Box<[u64; SLOTS]>,
    /// Sent note-ons per slot not yet turned off.
    sounding: Box<[u32; SLOTS]>,
    /// Per slot: tick the sounding note must last until to cover merged notes (0 = none).
    hold_until: Box<[u32; SLOTS]>,
    /// Note-ons per slot since the last `regulate`, sent or not.
    demand: Box<[u32; SLOTS]>,
    /// Note-ons per second per slot, smoothed: up at once, down slowly.
    rate: Box<[f32; SLOTS]>,
    limit_nps: u32,
    /// Off: never skip (interval stays 0).
    enabled: bool,
    window_start: Instant,
    /// Scales the time budget: lower when behind the clock.
    gain: f32,
    /// Skipped since the telemetry last took it.
    skipped: u32,
}

impl Skipper {
    const NEVER: u32 = u32::MAX;
    /// How much of the thread's time to plan on using; the rest absorbs jitter.
    const UTILIZATION: f32 = 0.9;
    /// Seconds between budget updates.
    const STEP: f32 = 0.004;

    fn new(limit_nps: u32, enabled: bool) -> Self {
        Self {
            enabled,
            interval_us: 0,
            last_on_us: Box::new([u64::MAX; SLOTS]),
            sounding: Box::new([0; SLOTS]),
            hold_until: Box::new([0; SLOTS]),
            demand: Box::new([0; SLOTS]),
            rate: Box::new([0.0; SLOTS]),
            limit_nps,
            window_start: Instant::now(),
            gain: 1.0,
            skipped: 0,
        }
    }

    fn slot(ch: u8, key: u8) -> usize {
        (ch as usize & 0x0F) * 128 + (key as usize & 0x7F)
    }

    /// Decide whether to send a note-on at `time_us`. `end` is its end tick (None =
    /// never released) and `dur_us` its length.
    #[inline]
    fn note_on(&mut self, slot: usize, time_us: u64, end: Option<u32>, dur_us: u64) -> bool {
        self.demand[slot] += 1;
        let last = self.last_on_us[slot];
        let too_soon = last != u64::MAX && time_us.saturating_sub(last) < self.interval_us;
        if too_soon {
            if self.sounding[slot] > 0 {
                let end = end.unwrap_or(Self::NEVER);
                self.hold_until[slot] = self.hold_until[slot].max(end);
                self.skipped += 1;
                return false;
            }
            if end.is_some() && dur_us < self.interval_us {
                self.skipped += 1;
                return false;
            }
        }
        self.last_on_us[slot] = time_us;
        self.sounding[slot] += 1;
        true
    }

    /// A sent note's note-off is due at `tick`.
    #[inline]
    fn note_off(&mut self, slot: usize, tick: u32) -> OffAction {
        let hold = self.hold_until[slot];
        if self.sounding[slot] == 1 && hold > tick {
            self.hold_until[slot] = 0;
            if hold == Self::NEVER {
                return OffAction::Hold;
            }
            return OffAction::Postpone(hold);
        }
        self.sounding[slot] = self.sounding[slot].saturating_sub(1);
        if self.sounding[slot] == 0 {
            self.hold_until[slot] = 0;
        }
        OffAction::Send
    }

    /// Set the re-strike interval for the notes the song is asking for, given how far
    /// behind the clock sending is (`lag_us`) and what sending costs. Does nothing
    /// until `STEP` has passed since the last time.
    fn regulate(&mut self, lag_us: u64, now: Instant, costs: Costs) {
        if !self.enabled {
            self.interval_us = 0;
            return;
        }
        let elapsed = now.saturating_duration_since(self.window_start).as_secs_f32();
        if elapsed < Self::STEP {
            return;
        }
        self.window_start = now;
        let mut asked = 0.0;
        for (rate, n) in self.rate.iter_mut().zip(self.demand.iter_mut()) {
            let new = *n as f32 / elapsed;
            *rate = if new > *rate { new } else { *rate * 0.9 + new * 0.1 };
            *n = 0;
            asked += *rate;
        }
        self.gain = match lag_us {
            8_001.. => (self.gain * 0.85).max(0.05),
            2_001.. => self.gain * 0.98,
            0..1_000 => (self.gain * 1.01).min(1.3),
            _ => self.gain,
        };
        let budget_ns = 1e9 * Self::UTILIZATION * self.gain - asked * costs.engine_ns;
        let mut can_send = (budget_ns / (costs.msgs_per_note.max(1.0) * costs.synth_ns).max(1.0)).max(2_000.0);
        if self.limit_nps != 0 {
            can_send = can_send.min(self.limit_nps as f32);
        }
        // Once limiting, stay until the demand is clearly under, so it doesn't flap.
        let slack = if self.interval_us > 0 { 0.9 } else { 1.0 };
        if asked <= can_send * slack {
            self.interval_us = 0;
            return;
        }
        let per_key = fair_share(&self.rate[..], can_send);
        // Wall-clock rate to a song-time interval.
        let interval = 1e6 / per_key.max(1.0) as f64 * costs.speed;
        self.interval_us = (interval as u64).clamp(100, 500_000);
    }

    /// Forget all per-key state (after a seek or output change); `chase` are the
    /// note-ons sent to restore the sound at `time_us`.
    fn reset(&mut self, chase: &[u32], time_us: u64) {
        let gain = self.gain;
        *self = Self::new(self.limit_nps, self.enabled);
        self.gain = gain;
        for &msg in chase {
            let slot = Self::slot(msg as u8, (msg >> 8) as u8);
            self.sounding[slot] += 1;
            self.last_on_us[slot] = time_us;
        }
    }
}

/// The rate `c` at which `sum(min(rate, c)) == budget`: each key gets what it asks for
/// up to `c`. (If everything fits, the largest rate.)
fn fair_share(rates: &[f32], budget: f32) -> f32 {
    let (mut lo, mut hi) = (0.0f32, rates.iter().copied().fold(0.0, f32::max));
    for _ in 0..24 {
        let mid = (lo + hi) / 2.0;
        let sum: f32 = rates.iter().map(|&r| r.min(mid)).sum();
        if sum > budget {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    lo
}

/// The playback thread's hot path: turns ticks into MIDI messages. Free of clocks,
/// channels and synth calls, so it can be benchmarked on its own.
struct Engine {
    cur: Cursor,
    skipper: Skipper,
    notes_played: u64,
    notes_this_second: u64,
    /// Notes left out because their track repeats an earlier one, since last taken.
    repeats_skipped: u64,
    /// Notes decoded and notes sent, ever (the cost model takes differences).
    decoded: u64,
    sent: u64,
    polyphony: u32,
    peak_polyphony: u32,
    /// Scratch: what's due this tick, and its tracks.
    due_buf: Vec<Due>,
    track_buf: Vec<u32>,
}

impl Engine {
    fn new(cur: Cursor, skipper: Skipper) -> Self {
        Self {
            cur,
            skipper,
            notes_played: 0,
            notes_this_second: 0,
            repeats_skipped: 0,
            decoded: 0,
            sent: 0,
            polyphony: 0,
            peak_polyphony: 0,
            due_buf: Vec::new(),
            track_buf: Vec::new(),
        }
    }

    /// Copy the note counters into telemetry.
    fn sync(&self, live: &mut PlayerTelemetry) {
        live.notes_played = self.notes_played;
        live.polyphony = self.polyphony;
        live.peak_polyphony = self.peak_polyphony;
    }

    /// Append the messages for `tick` (which plays at `target_us`, `us_per_tick` long)
    /// to `batch`: due note-offs first, so a key struck again on this tick isn't cut
    /// off, then every track's records at this tick.
    fn process_tick(&mut self, song: &SongData, tick: u32, target_us: u64, us_per_tick: f32, batch: &mut Vec<u32>) {
        let cur = &mut self.cur;
        let skipper = &mut self.skipper;

        // Note-offs due now go first, so a key struck again on this tick isn't cut off.
        if cur.due.min_key() != Some(tick) {
            return;
        }
        cur.due.take_min(&mut self.due_buf);
        for &due in &self.due_buf {
            let (ch, key) = match due {
                Due::Off(off) => off,
                Due::Track(i) => {
                    self.track_buf.push(i);
                    continue;
                }
            };
            match skipper.note_off(Skipper::slot(ch, key), tick) {
                OffAction::Send => {
                    self.polyphony = self.polyphony.saturating_sub(1);
                    batch.push(pack_short(0x80 | ch, key, 0));
                }
                OffAction::Postpone(t2) => cur.due.push(t2, Due::Off((ch, key))),
                OffAction::Hold => {}
            }
        }
        self.due_buf.clear();

        // Same order as the file within a tick (matters for e.g. a program change in
        // one track and a note in another on the same channel and tick).
        self.track_buf.sort_unstable();
        for &i in &self.track_buf {
            let data = song.midi.track(i as usize);
            let r = &mut cur.readers[i as usize];
            // A track standing for identical ones counts them; they are not played again.
            let copies = song.midi.track_copies[i as usize];
            while let Some(rec) = r.next_at(data, tick) {
                match rec {
                    Record::Note { ch, key, vel, dur } => {
                        self.notes_played += copies;
                        self.repeats_skipped += copies - 1;
                        self.decoded += 1;
                        self.notes_this_second += copies;
                        let end = dur.map(|d| tick + d);
                        let dur_us = match dur {
                            Some(d) if skipper.interval_us > 0 => (d as f32 * us_per_tick) as u64,
                            _ => 0,
                        };
                        // A skipped note leaves nothing behind: no note-off, no polyphony.
                        if skipper.note_on(Skipper::slot(ch, key), target_us, end, dur_us) {
                            self.sent += 1;
                            self.polyphony += 1;
                            self.peak_polyphony = self.peak_polyphony.max(self.polyphony);
                            if let Some(e) = end {
                                // A zero-length note's off lands on this tick: it's taken on
                                // the next call, after this note-on, as in the file.
                                cur.due.push(e, Due::Off((ch, key)));
                            }
                            batch.push(pack_short(0x90 | ch, key, vel));
                        }
                    }
                    Record::Other { status, d1, d2 } => batch.push(pack_short(status, d1, d2)),
                }
            }
            if let Some(t) = r.peek_tick(data) {
                cur.due.push(t, Due::Track(i));
            }
        }
        self.track_buf.clear();
    }
}

pub struct Player {
    omni: Option<Arc<OmniMIDI>>,
    choice: OutputChoice,
    out: Output,
    synth_limit: u32,
    skipping: bool,
    log_stats: bool,
    cmd_rx: Receiver<PlayerCommand>,
    telemetry_tx: Sender<PlayerTelemetry>,
    stop: Arc<AtomicBool>,
    clock: Arc<PlaybackClock>,
    /// Passed to SVMS outputs: the clock, where its callback log goes (follows
    /// `log_stats`), and whether to submit through its batch API.
    svms: SvmsSetup,
}

impl Player {
    pub fn new(
        omni: Option<Arc<OmniMIDI>>,
        choice: OutputChoice,
        cmd_rx: Receiver<PlayerCommand>,
        telemetry_tx: Sender<PlayerTelemetry>,
        stop: Arc<AtomicBool>,
        clock: Arc<PlaybackClock>,
    ) {
        // Single playback thread: it waits for each event's time and sends straight to the
        // synth, so seeks and stops take effect immediately (nothing is queued ahead).
        thread::spawn(move || {
            let mut player = Self {
                omni,
                choice,
                out: Output::Closed,
                synth_limit: 0,
                skipping: true,
                log_stats: false,
                cmd_rx,
                telemetry_tx,
                stop,
                svms: SvmsSetup { clock: clock.clone(), log: Default::default(), batch: false },
                clock,
            };
            player.run_loop();
        });
    }

    fn run_loop(&mut self) {
        let mut queued_song: Option<Arc<SongData>> = None;

        while !self.stop.load(Ordering::SeqCst) {
            let song = match queued_song.take() {
                Some(song) => song,
                None => match self.cmd_rx.recv() {
                    Ok(PlayerCommand::Play(song)) => song,
                    Ok(PlayerCommand::Stop) => {
                        self.all_notes_off();
                        self.out = Output::Closed;
                        self.clock.reset();
                        self.telemetry_tx
                            .send(PlayerTelemetry {
                                midi_path: "(none)".to_string(),
                                ..PlayerTelemetry::default()
                            })
                            .ok();
                        continue;
                    }
                    Ok(PlayerCommand::SetOutput(choice)) => {
                        self.choice = choice;
                        continue;
                    }
                    Ok(PlayerCommand::SetSynthLimit(limit)) => {
                        self.synth_limit = limit;
                        continue;
                    }
                    Ok(PlayerCommand::SetSkipping(on)) => {
                        self.skipping = on;
                        continue;
                    }
                    Ok(PlayerCommand::SetStatsLog(on)) => {
                        self.log_stats = on;
                        continue;
                    }
                    Ok(PlayerCommand::SetSvmsBatch(on)) => {
                        self.svms.batch = on;
                        continue;
                    }
                    // No song loaded yet; nothing to pause, resume or seek.
                    Ok(PlayerCommand::Pause | PlayerCommand::Resume | PlayerCommand::Seek(_)) => {
                        continue
                    }
                    Err(_) => break,
                },
            };

            // Close the output after each song so it gets re-opened for the next one.
            queued_song = self.play_song(song);
            self.out = Output::Closed;
        }

        self.all_notes_off();
        self.out = Output::Closed;
    }

    fn play_song(&mut self, song: Arc<SongData>) -> Option<Arc<SongData>> {
        if !self.out.is_open() {
            self.out = Output::open(&self.choice, self.omni.as_ref(), &self.svms);
        }

        const UI_UPDATE: Duration = Duration::from_millis(33);
        /// Most messages sent in one call, so a long backlog doesn't hold up the synth
        /// for long before the first of them is heard.
        const MAX_SLICE_MSGS: usize = 1 << 15;

        let tempo = &song.tempo;
        let duration_us = tempo.duration_us();
        let mut eng = Engine::new(Cursor::seek(&song, 0).cursor, Skipper::new(self.synth_limit, self.skipping));
        // One tick's messages, sent to the synth in one timed call (or loop).
        let mut batch: Vec<u32> = Vec::new();
        let mut acc = StatsAcc::new();
        let mut costs = Costs::default();
        let open_log = |on: bool| on.then(|| StatsLog::create(&song.path)).and_then(|r| {
            r.map_err(|e| eprintln!("Could not create stats log: {e}")).ok()
        });
        let mut log = open_log(self.log_stats);
        let svms_log = |log: &Option<StatsLog>| log.as_ref().map(|l| l.path.replace(".csv", "_svms.csv").into());
        *self.svms.log.lock().unwrap() = svms_log(&log);

        let mut live = PlayerTelemetry {
            midi_path: song.path.clone(),
            duration_us,
            is_playing: true,
            log_path: log.as_ref().map(|l| l.path.clone()),
            ..PlayerTelemetry::default()
        };
        let mut abs_time_us: u64 = 0;
        let mut seg_idx: usize = 0;

        let mut last_second = Instant::now();
        let mut last_ui = Instant::now();
        let mut pending_seek: Option<u64> = None;

        self.clock.set_running(abs_time_us);
        self.telemetry_tx.send(live.clone()).ok();

        loop {
            if self.stop.load(Ordering::SeqCst) {
                self.all_notes_off();
                self.clock.set_paused(abs_time_us);
                return None;
            }

            // --- Command Handling ---
            while let Ok(cmd) = self.cmd_rx.try_recv() {
                match cmd {
                    PlayerCommand::Seek(pos_us) => {
                        pending_seek = Some(pos_us);
                    }
                    PlayerCommand::Stop => {
                        self.all_notes_off();
                        self.clock.reset();
                        return None;
                    }
                    PlayerCommand::Pause => {
                        if !self.clock.is_paused() {
                            abs_time_us = self.clock.now_us();
                            self.clock.set_paused(abs_time_us);
                        }
                    }
                    PlayerCommand::Resume => {
                        if self.clock.is_paused() {
                            abs_time_us = self.clock.now_us();
                            self.clock.set_running(abs_time_us);
                        }
                    }
                    PlayerCommand::Play(next_song) => {
                        self.all_notes_off();
                        self.clock.set_paused(0);
                        return Some(next_song);
                    }
                    PlayerCommand::SetOutput(choice) => {
                        // Switch synths mid-song; re-chase so held notes keep sounding.
                        self.all_notes_off();
                        self.out = Output::Closed;
                        self.choice = choice;
                        self.out = Output::open(&self.choice, self.omni.as_ref(), &self.svms);
                        pending_seek = Some(self.clock.now_us());
                    }
                    PlayerCommand::SetSynthLimit(limit) => {
                        self.synth_limit = limit;
                        eng.skipper.limit_nps = limit;
                    }
                    PlayerCommand::SetSkipping(on) => {
                        self.skipping = on;
                        eng.skipper.enabled = on;
                    }
                    PlayerCommand::SetStatsLog(on) => {
                        self.log_stats = on;
                        log = if on { log.or_else(|| open_log(true)) } else { None };
                        live.log_path = log.as_ref().map(|l| l.path.clone());
                        *self.svms.log.lock().unwrap() = svms_log(&log);
                    }
                    PlayerCommand::SetSvmsBatch(on) => {
                        self.svms.batch = on;
                        self.out.set_svms_batch(on);
                    }
                }
            }

            let (paused, now_us, speed) = self.clock.snapshot();
            if paused {
                thread::sleep(Duration::from_millis(5));
                continue;
            }

            if let Some(target_us) = pending_seek.take() {
                let target_tick = tempo.time_to_tick(target_us.min(duration_us));
                let seek = Cursor::seek(&song, target_tick);
                eng.cur = seek.cursor;
                eng.notes_played = seek.started;
                eng.polyphony = seek.chase.len() as u32;
                eng.peak_polyphony = eng.peak_polyphony.max(eng.polyphony);
                eng.notes_this_second = 0;
                abs_time_us =
                    tempo.tick_to_time_with_cache(target_tick, &mut seg_idx);
                eng.skipper.reset(&seek.chase, abs_time_us);
                self.clock.set_running(abs_time_us);
                eng.sync(&mut live);
                live.nps = 0;
                live.playback_time_us = abs_time_us;
                live.is_playing = true;
                self.telemetry_tx.send(live.clone()).ok();

                self.all_notes_off();
                self.out.send_batch(&seek.chase);
                continue;
            }

            let Some(tick) = eng.cur.next_tick() else {
                if now_us >= duration_us {
                    // Finished: pause at the end but stay on this song so seeks still work.
                    self.all_notes_off();
                    abs_time_us = duration_us;
                    self.clock.set_paused(abs_time_us);
                    eng.sync(&mut live);
                    live.playback_time_us = abs_time_us;
                    live.is_playing = false;
                    live.nps = 0;
                    live.polyphony = 0;
                    live.stats = SchedStats::default();
                    self.telemetry_tx.send(live.clone()).ok();
                } else {
                    thread::sleep(Duration::from_millis(5));
                }
                continue;
            };

            let target_us = tempo.tick_to_time_with_cache(tick, &mut seg_idx);

            // Wait for the event's time. Sleep in short slices (commands stay responsive),
            // then spin the last stretch for accuracy.
            if target_us > now_us {
                // Song time runs `speed`x faster than wall time.
                let wait_us = ((target_us - now_us) as f64 / speed) as u64;
                if wait_us > 1_500 {
                    thread::sleep(Duration::from_micros((wait_us - 1_000).min(5_000)));
                } else {
                    std::hint::spin_loop();
                }
                continue;
            }

            // Everything due by now goes out in one call, in order. (When behind, that
            // is a lot of ticks: one clock read, one synth call and one stats update
            // instead of one each per tick.)
            let slice_start = Instant::now();
            live.lag_us = ((now_us - target_us) as f64 / speed) as u64; // wall time
            costs.speed = speed;
            eng.skipper.regulate(live.lag_us, slice_start, costs);
            batch.clear();
            let (decoded0, sent0) = (eng.decoded, eng.sent);
            let (mut tick, mut target_us) = (tick, target_us);
            loop {
                abs_time_us = target_us;
                eng.process_tick(&song, tick, target_us, tempo.us_per_tick(seg_idx), &mut batch);
                acc.ticks += 1;
                if batch.len() >= MAX_SLICE_MSGS {
                    break;
                }
                let Some(next) = eng.cur.next_tick() else { break };
                let next_us = tempo.tick_to_time_with_cache(next, &mut seg_idx);
                if next_us > now_us {
                    break;
                }
                (tick, target_us) = (next, next_us);
            }

            // Send, timing the synth separately from our own work.
            let send_start = Instant::now();
            self.out.send_batch(&batch);
            let done = Instant::now();
            let (send_time, own_time) = (done - send_start, send_start - slice_start);
            acc.sched += own_time;
            acc.synth += send_time;
            acc.worst_batch = acc.worst_batch.max(send_time);
            acc.msgs += batch.len() as u64;

            // What things cost, for the skipper's budget (only from slices big enough
            // to measure).
            let (decoded, sent) = ((eng.decoded - decoded0) as f32, (eng.sent - sent0) as f32);
            let ewma = |cost: &mut f32, sample: f32| *cost += 0.2 * (sample - *cost);
            if batch.len() >= 32 {
                ewma(&mut costs.synth_ns, send_time.as_nanos() as f32 / batch.len() as f32);
            }
            if decoded >= 32.0 {
                ewma(&mut costs.engine_ns, own_time.as_nanos() as f32 / decoded);
            }
            if sent >= 32.0 {
                ewma(&mut costs.msgs_per_note, batch.len() as f32 / sent);
            }

            // --- Telemetry and Stats ---
            if done - last_second >= Duration::from_secs(1) {
                live.nps = std::mem::take(&mut eng.notes_this_second);
                live.skipped_nps =
                    std::mem::take(&mut eng.skipper.skipped) as u64 + std::mem::take(&mut eng.repeats_skipped);
                live.stats = acc.take();
                live.stats.batched = self.out.batched();
                live.playback_time_us = self.clock.now_us();
                eng.sync(&mut live);
                if let Some(log) = &mut log {
                    log.row(&live, speed, &self.choice, self.skipping);
                }
                last_second = done;
            }

            if done - last_ui >= UI_UPDATE {
                eng.sync(&mut live);
                live.svms = self.out.svms_stats();
                live.playback_time_us = self.clock.now_us();
                live.is_playing = !self.clock.is_paused();
                self.telemetry_tx.send(live.clone()).ok();
                last_ui = done;
            }
        }
    }

    fn all_notes_off(&self) {
        let msgs: Vec<u32> = (0..16)
            .flat_map(|ch| {
                [
                    pack_short(0xB0 | ch, 123, 0), // All notes off
                    pack_short(0xB0 | ch, 121, 0), // Reset all controllers
                ]
            })
            .collect();
        self.out.send_batch(&msgs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seek_chases_sounding_notes() {
        // Bass note held 0..384 on ch 1; C4 on ch 0 every quarter (96 ticks), 96 long.
        let mut trk = vec![0x00, 0x91, 48, 80];
        for _ in 0..4 {
            trk.extend_from_slice(&[0x00, 0x90, 60, 100, 0x60, 0x80, 60, 0]);
        }
        trk.extend_from_slice(&[0x00, 0x81, 48, 0]);
        let mut f = b"MThd".to_vec();
        f.extend_from_slice(&[0, 0, 0, 6, 0, 0, 0, 1, 0, 96]);
        f.extend_from_slice(b"MTrk");
        f.extend_from_slice(&(trk.len() as u32).to_be_bytes());
        f.extend_from_slice(&trk);
        let path = std::env::temp_dir().join("kimrashi_seek_test.mid");
        std::fs::write(&path, f).unwrap();
        let song = SongData::load(&path, 127, &Default::default()).unwrap();

        let s = Cursor::seek(&song, 120);
        assert_eq!(s.started, 3);
        assert_eq!(s.chase, [pack_short(0x91, 48, 80), pack_short(0x90, 60, 100)]);
        let mut cursor = s.cursor;
        assert_eq!(cursor.next_tick(), Some(192));
        let mut offs: Vec<_> = cursor.due.buckets.iter().flatten().copied().collect();
        offs.retain(|(_, d)| matches!(d, Due::Off(_)));
        offs.sort();
        assert_eq!(offs, [(192, Due::Off((0, 60))), (384, Due::Off((1, 48)))]);
    }

    #[test]
    fn skipper_merges_restrikes_without_stuck_or_cut_notes() {
        let mut s = Skipper::new(0, true);
        let slot = Skipper::slot(0, 60);
        s.interval_us = 1_000; // under pressure

        assert!(s.note_on(slot, 0, Some(10), 100)); // sent
        assert!(!s.note_on(slot, 500, Some(40), 3_000)); // re-strike while sounding: merged
        assert!(!s.note_on(slot, 900, Some(30), 2_000)); // merged too; hold stays at 40
        // The sent note's off at tick 10 is postponed to cover the merged notes...
        assert_eq!(s.note_off(slot, 10), OffAction::Postpone(40));
        // ...then sent at 40: exactly one off per sent on, nothing stuck.
        assert_eq!(s.note_off(slot, 40), OffAction::Send);
        assert_eq!(s.sounding[slot], 0);

        // Silent key, struck too soon: short blips are skipped, long notes still play.
        assert!(!s.note_on(slot, 950, Some(50), 100));
        assert!(s.note_on(slot, 960, Some(900), 50_000));
        // Outside the interval: always sent.
        assert!(s.note_on(slot, 5_000, Some(950), 100));
        assert_eq!(s.skipped, 3);

        // No pressure: everything is sent, overlapping voices included.
        s.interval_us = 0;
        assert!(s.note_on(slot, 5_001, Some(960), 100));
        // A merged never-ending note keeps its key held forever (like the original).
        let mut s = Skipper::new(0, true);
        s.interval_us = 1_000;
        assert!(s.note_on(slot, 0, Some(10), 100));
        assert!(!s.note_on(slot, 10, None, 0));
        assert_eq!(s.note_off(slot, 10), OffAction::Hold);
    }
    /// What a tick should send: its note-offs (sorted), then its messages in track order.
    type Tick = (Vec<u32>, Vec<u32>);

    /// Write a song of three tracks of notes on different keys (and some controllers) to
    /// `path`, and return what each tick should send.
    fn random_song(path: &std::path::Path) -> std::collections::BTreeMap<u32, Tick> {
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let mut rnd = move |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        let mut want: std::collections::BTreeMap<u32, Tick> = Default::default();
        let mut file = b"MThd".to_vec();
        file.extend_from_slice(&[0, 0, 0, 6, 0, 1, 0, 3, 0, 96]);
        for _ in 0..3 {
            // (tick, 0 = note-off, which goes first in a tick, and the message)
            let mut events: Vec<(u32, u8, u32)> = vec![];
            let mut busy_until = [0u32; 16 * 128];
            let mut tick = 0u32;
            for _ in 0..300 {
                tick += rnd(6) as u32;
                let (ch, key) = (rnd(3) as u8, 60 + rnd(12) as u8);
                if rnd(5) == 0 {
                    let (d1, d2) = (rnd(128) as u8, rnd(128) as u8);
                    events.push((tick, 1, pack_short(0xB0 | ch, d1, d2)));
                } else if busy_until[Skipper::slot(ch, key)] <= tick {
                    let (vel, len) = (1 + rnd(127) as u8, 1 + rnd(20) as u32);
                    busy_until[Skipper::slot(ch, key)] = tick + len;
                    events.push((tick, 1, pack_short(0x90 | ch, key, vel)));
                    events.push((tick + len, 0, pack_short(0x80 | ch, key, 0)));
                }
            }
            events.sort_by_key(|e| (e.0, e.1)); // stable: generation order within a kind
            let (mut trk, mut last) = (vec![], 0);
            for &(t, kind, msg) in &events {
                // delta as a variable-length quantity
                let mut delta = vec![((t - last) & 0x7F) as u8];
                let mut d = (t - last) >> 7;
                while d > 0 {
                    delta.push((d & 0x7F) as u8 | 0x80);
                    d >>= 7;
                }
                trk.extend(delta.iter().rev());
                let data = [msg as u8, (msg >> 8) as u8, (msg >> 16) as u8];
                trk.extend_from_slice(if msg as u8 & 0xF0 == 0xC0 { &data[..2] } else { &data[..] });
                last = t;
                let e = want.entry(t).or_default();
                if kind == 0 {
                    e.0.push(msg)
                } else {
                    e.1.push(msg)
                }
            }
            file.extend_from_slice(b"MTrk");
            file.extend_from_slice(&(trk.len() as u32).to_be_bytes());
            file.extend_from_slice(&trk);
        }
        std::fs::write(path, file).unwrap();
        for (offs, _) in want.values_mut() {
            offs.sort();
        }
        want
    }

    fn load_random_song(name: &str) -> (Arc<SongData>, std::collections::BTreeMap<u32, Tick>) {
        let path = std::env::temp_dir().join(format!("kimrashi_{name}_{}.mid", std::process::id()));
        let want = random_song(&path);
        let song = SongData::load(&path, 127, &Default::default()).unwrap();
        let _ = std::fs::remove_file(&path);
        (song, want)
    }

    #[test]
    fn engine_sends_what_the_file_says_when_nothing_is_skipped() {
        let (song, want) = load_random_song("engine");
        let mut eng = Engine::new(Cursor::seek(&song, 0).cursor, Skipper::new(0, true));
        let mut got: std::collections::BTreeMap<u32, Tick> = Default::default();
        while let Some(tick) = eng.cur.next_tick() {
            let mut batch = vec![];
            eng.process_tick(&song, tick, 0, 0.0, &mut batch);
            let n_offs = batch.iter().take_while(|&&m| m & 0xF0 == 0x80).count();
            let (offs, rest) = batch.split_at(n_offs);
            let e = got.entry(tick).or_default();
            e.0.extend(offs);
            e.0.sort();
            e.1.extend_from_slice(rest);
        }
        assert_eq!(got, want);
        assert_eq!(eng.polyphony, 0);
    }

    #[test]
    fn engine_skipping_leaves_no_stuck_notes() {
        let (song, want) = load_random_song("engine_skip");
        let mut eng = Engine::new(Cursor::seek(&song, 0).cursor, Skipper::new(0, true));
        eng.skipper.interval_us = 600; // 100 us per tick: a key can sound every 6th tick
        let (mut ons, mut offs) = ([0i32; 16 * 128], [0i32; 16 * 128]);
        let mut sent = 0;
        while let Some(tick) = eng.cur.next_tick() {
            let mut batch = vec![];
            eng.process_tick(&song, tick, tick as u64 * 100, 100.0, &mut batch);
            for m in batch {
                let slot = Skipper::slot(m as u8, (m >> 8) as u8);
                match m & 0xF0 {
                    0x90 => {
                        ons[slot] += 1;
                        sent += 1;
                    }
                    0x80 => offs[slot] += 1,
                    _ => {}
                }
            }
        }
        assert_eq!(ons, offs);
        let asked = want.values().flat_map(|t| &t.1).filter(|&&m| m & 0xF0 == 0x90).count();
        assert!(sent > 0 && sent < asked, "{sent} of {asked}");
        assert_eq!(eng.polyphony, 0);
    }

    #[test]
    fn budget_goes_to_the_keys_that_ask_for_more_than_their_share() {
        // Many keys at 1000 notes/s and one sparse key at 10/s; a slow synth (1 us per
        // message, 2 messages per note) can take 450K notes/s: 450 per key.
        let mut rates = vec![1000.0f32; 1000];
        rates.push(10.0);
        let share = fair_share(&rates, 450_000.0);
        assert!((share - 450.0).abs() < 1.0, "{share}");
        // Everything fits: nobody is capped (the share is at least the largest rate).
        assert!(fair_share(&rates, 2_000_000.0) >= 999.0);

        let costs = Costs { synth_ns: 1000.0, engine_ns: 0.0, msgs_per_note: 2.0, speed: 1.0 };
        let ask = |s: &mut Skipper| {
            for slot in 0..1000 {
                for _ in 0..5 {
                    s.note_on(slot, 0, Some(1), 0); // 5 in 5 ms = 1000/s
                }
            }
        };
        let mut s = Skipper::new(0, true);
        ask(&mut s);
        s.regulate(0, s.window_start + Duration::from_millis(5), costs);
        // 450 notes/s per key is one every 2.2 ms
        assert!((2_100..2_350).contains(&s.interval_us), "{}", s.interval_us);

        // A fast synth takes it all: no skipping.
        let mut s = Skipper::new(0, true);
        ask(&mut s);
        let fast = Costs { synth_ns: 50.0, ..costs };
        s.regulate(0, s.window_start + Duration::from_millis(5), fast);
        assert_eq!(s.interval_us, 0);

        // Falling behind the clock shrinks the budget even if the model says all is well.
        for _ in 0..20 {
            ask(&mut s);
            s.regulate(50_000, s.window_start + Duration::from_millis(5), fast);
        }
        assert!(s.interval_us > 0 && s.gain < 0.2, "{} {}", s.interval_us, s.gain);
    }
}
