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

mod par;

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
    SetSvmsDirect(bool),
    /// Send each slice as raw MIDI bytes (running status) in long messages instead of one
    /// short message at a time (KDMAPI synths that understand it, e.g. OmniMIDI).
    SetLongData(bool),
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

/// A one-slot channel where a new value replaces an unread one: for state the UI only needs
/// the latest of. It is drained only while the UI repaints (not when minimized), so a queue
/// here would grow without bound.
#[derive(Clone)]
pub struct Latest<T>(Sender<T>, Receiver<T>);

pub fn latest<T>() -> (Latest<T>, Receiver<T>) {
    let (tx, rx) = crossbeam_channel::bounded(1);
    (Latest(tx, rx.clone()), rx)
}

impl<T> Latest<T> {
    pub fn send(&self, v: T) -> Result<(), ()> {
        if let Err(crossbeam_channel::TrySendError::Full(v)) = self.0.try_send(v) {
            let _ = self.1.try_recv();
            let _ = self.0.try_send(v);
        }
        Ok(())
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
            self.gather(b);
        }
        Some(self.last)
    }

    /// Spread bucket `b` over the lower buckets, `last` becoming its smallest key.
    fn gather(&mut self, b: usize) {
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

    /// `min_key`, but only if it is below `limit`; otherwise None and `last` stays where
    /// it was, so pushes at or after the caller's own position remain valid.
    fn min_key_below(&mut self, limit: u32) -> Option<u32> {
        if self.buckets[0].is_empty() {
            let b = (1..33).find(|&b| !self.buckets[b].is_empty())?;
            // The least key bucket b can hold (it differs from `last` first at bit b - 1).
            if ((self.last >> (b - 1)) | 1) << (b - 1) >= limit
                || self.buckets[b].iter().all(|&(k, _)| k >= limit)
            {
                return None;
            }
            self.gather(b);
        }
        (self.last < limit).then_some(self.last)
    }

    /// The smallest key without moving anything.
    fn peek_min(&self) -> Option<u32> {
        if !self.buckets[0].is_empty() {
            return Some(self.last);
        }
        let b = (1..33).find(|&b| !self.buckets[b].is_empty())?;
        self.buckets[b].iter().map(|&(k, _)| k).min()
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

/// A SysEx message to send. In a batch of messages it stands as `sysex_marker(index)`,
/// which no short message can be (their status byte is below 0xF0).
#[derive(Clone, Copy, Debug, PartialEq)]
struct SysexRef {
    status: u8,
    at: usize,
    len: u32,
}

/// A batch as raw MIDI bytes, sent through `send` in pieces of about `STREAM_CHUNK` bytes,
/// each one complete on its own. Consecutive messages with the same status leave it out
/// (running status), and note-offs go out as note-ons of velocity 0 so they keep the run
/// going. A SysEx takes a long message to itself, so a malformed one can't swallow what
/// follows it.
fn stream_chunks(song: &SongData, msgs: &[u32], sysex: &[SysexRef], bytes: &mut Vec<u8>, mut send: impl FnMut(&[u8])) {
    const STREAM_CHUNK: usize = 4096;
    let mut running = 0u8;
    bytes.clear();
    for &m in msgs {
        let mut flush = false;
        if m as u8 == 0xF0 {
            let x = sysex[(m >> 8) as usize];
            if !bytes.is_empty() {
                send(bytes);
                bytes.clear();
            }
            if x.status == 0xF0 {
                bytes.push(0xF0);
            }
            bytes.extend_from_slice(song.midi.sysex(x.at, x.len));
            flush = true;
        } else {
            let (mut status, d1, mut d2) = (m as u8, (m >> 8) as u8, (m >> 16) as u8);
            if status & 0xF0 == 0x80 {
                (status, d2) = (status | 0x10, 0);
            }
            if status != running {
                bytes.push(status);
                running = status;
            }
            bytes.push(d1);
            if !matches!(status & 0xF0, 0xC0 | 0xD0) {
                bytes.push(d2);
            }
        }
        if flush || bytes.len() >= STREAM_CHUNK {
            send(bytes);
            bytes.clear();
            running = 0;
        }
    }
    if !bytes.is_empty() {
        send(bytes);
    }
}

/// The synth-clock time of each message of a slice (`out`), given the song time
/// `first_us + offs[i]` it is due at, the song time `now_us` and `speed`, and the synth's
/// clock `qpc` (ticks now) at `freq` ticks a second. Late messages go as of now, and no
/// time is before the previous one (`last`, which is updated): order is kept.
fn stamp_slice(first_us: u64, offs: &[u32], now_us: u64, speed: f64, qpc: u64, freq: u64, last: &mut u64, out: &mut Vec<u64>) {
    out.clear();
    let ticks_per_song_us = freq as f64 / 1e6 / speed;
    for &off in offs {
        let due = first_us + off as u64;
        let ahead = (due.saturating_sub(now_us) as f64 * ticks_per_song_us) as u64;
        *last = (qpc + ahead).max(*last);
        out.push(*last);
    }
}

fn sysex_marker(index: usize) -> u32 {
    0xF0 | (index as u32) << 8
}

/// Whether a SysEx payload resets the synth (GM on/off, GS or XG reset): what came
/// before it no longer matters.
fn is_reset(payload: &[u8]) -> bool {
    payload.windows(3).any(|w| w[0] == 0x7E && w[2] == 0x09)
        || payload.windows(4).any(|w| w == [0x40, 0x00, 0x7F, 0x00])
        || payload.windows(5).any(|w| w == [0x4C, 0x00, 0x00, 0x7E, 0x00])
}

/// Most SysEx messages replayed on a seek (the latest ones).
const CHASE_SYSEX: usize = 1024;

struct SeekResult {
    cursor: Cursor,
    /// Note-ons for notes that are sounding at the seek point.
    chase: Vec<u32>,
    /// Notes that started before the seek point.
    started: u64,
    /// SysEx, then program, controller, bend and pressure state at the seek point, to send
    /// before `chase` (SysEx as markers into `sysex`).
    controllers: Vec<u32>,
    sysex: Vec<SysexRef>,
}

/// What a controller-like record overwrites: the last of each kind is the state at a
/// seek point. Data entry and RPN/NRPN selects (CC 6, 38, 96-101) depend on one another
/// in order, so each of those is kept (per tick) instead of only the last.
fn controller_key(status: u8, d1: u8, tick: u32) -> u64 {
    let kind = match status & 0xF0 {
        0xB0 if matches!(d1, 6 | 38 | 96..=101) => d1 as u64 | (tick as u64) << 16,
        0xB0 | 0xA0 => d1 as u64,
        _ => 0,
    };
    status as u64 | kind << 8
}

/// Bank select goes before program change, everything else after.
fn controller_rank(msg: u32) -> u8 {
    match (msg as u8 & 0xF0, (msg >> 8) as u8) {
        (0xB0, 0 | 32) => 0,
        (0xC0, _) => 1,
        _ => 2,
    }
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
                let mut ctl: std::collections::HashMap<u64, (u32, u32, u32)> = Default::default();
                let mut sx = Vec::new();
                let mut seq = 0u32;
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
                            } else if let Record::Sysex { status, at, len } = rec {
                                seq += 1;
                                sx.push((t, seq, SysexRef { status, at, len }));
                            } else if let Record::Other { status, d1, d2 } = rec {
                                seq += 1;
                                ctl.insert(controller_key(status, d1, t), (t, seq, pack_short(status, d1, d2)));
                            }
                        }
                        _ => break,
                    }
                }
                (r, active, started, ctl, sx)
            })
            .collect();

        let mut res = SeekResult {
            cursor: Cursor {
                readers: Vec::with_capacity(midi.tracks.len()),
                due: RadixHeap::new(),
            },
            chase: Vec::new(),
            started: 0,
            controllers: Vec::new(),
            sysex: Vec::new(),
        };
        let mut sysex: Vec<(u32, usize, u32, SysexRef)> = Vec::new();
        // key -> (tick, track, order in track, message): the latest wins.
        let mut latest: std::collections::HashMap<u64, (u32, usize, u32, u32)> = Default::default();
        for (i, (r, active, started, ctl, sx)) in per_track.into_iter().enumerate() {
            if let Some(t) = r.peek_tick(midi.track(i)) {
                res.cursor.due.push(t, Due::Track(i as u32));
            }
            res.cursor.readers.push(r);
            res.started += started * midi.track_copies[i];
            sysex.extend(sx.into_iter().map(|(t, n, x)| (t, i, n, x)));
            for (key, (t, n, msg)) in ctl {
                let e = latest.entry(key).or_insert((t, i, n, msg));
                if (t, i, n) >= (e.0, e.1, e.2) {
                    *e = (t, i, n, msg);
                }
            }
            for (ch, key, vel, end) in active {
                res.chase.push(pack_short(0x90 | ch, key, vel));
                if let Some(e) = end {
                    res.cursor.due.push(e, Due::Off((ch, key)));
                }
            }
        }
        // SysEx from the last reset on; what the controllers did before it is undone.
        sysex.sort_unstable_by_key(|&(t, i, n, _)| (t, i, n));
        let reset = sysex.iter().rposition(|x| is_reset(midi.sysex(x.3.at, x.3.len)));
        let from = reset.unwrap_or(0).max(sysex.len().saturating_sub(CHASE_SYSEX));
        if let Some(r) = reset {
            let tick = sysex[r].0;
            latest.retain(|_, v| v.0 >= tick);
        }
        for (k, &(.., x)) in sysex[from..].iter().enumerate() {
            res.controllers.push(sysex_marker(k));
            res.sysex.push(x);
        }
        let mut ctl: Vec<_> = latest.into_values().collect();
        ctl.sort_unstable_by_key(|&(t, i, n, msg)| (controller_rank(msg), t, i, n));
        res.controllers.extend(ctl.into_iter().map(|(.., msg)| msg));
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
    /// The SysEx messages whose markers are in the batch being built (see `SysexRef`).
    sysex: Vec<SysexRef>,
    /// Scratch: what's due this tick, and its tracks.
    due_buf: Vec<Due>,
    track_buf: Vec<u32>,
    /// Notes' slots split over threads (see `par`); while set, note-offs and the skipper's
    /// per-slot state live there instead of in `cur.due` and `skipper`.
    par: Option<par::Parallel>,
    /// Note-offs at the start of the last `process_tick`'s messages (they go first).
    off_count: usize,
}

impl Engine {
    fn new(cur: Cursor, skipper: Skipper) -> Self {
        Self {
            par: None,
            off_count: 0,
            cur,
            skipper,
            notes_played: 0,
            notes_this_second: 0,
            repeats_skipped: 0,
            decoded: 0,
            sent: 0,
            polyphony: 0,
            peak_polyphony: 0,
            sysex: Vec::new(),
            due_buf: Vec::new(),
            track_buf: Vec::new(),
        }
    }

    /// The next tick with anything due, in either mode.
    fn next_tick(&mut self) -> Option<u32> {
        match &self.par {
            None => self.cur.next_tick(),
            Some(par) => par.next_tick(),
        }
    }

    /// Append the messages for `tick` (which plays at `target_us`, `us_per_tick` long)
    /// to `batch`: due note-offs first, so a key struck again on this tick isn't cut
    /// off, then every track's records at this tick.
    fn process_tick(&mut self, song: &SongData, tick: u32, target_us: u64, us_per_tick: f32, batch: &mut Vec<u32>) {
        let cur = &mut self.cur;
        let skipper = &mut self.skipper;
        let first = batch.len();

        // Note-offs due now go first, so a key struck again on this tick isn't cut off.
        if cur.due.min_key() != Some(tick) {
            self.off_count = 0;
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
        self.off_count = batch.len() - first;

        // Same order as the file within a tick (matters for e.g. a program change in
        // one track and a note in another on the same channel and tick).
        self.track_buf.sort_unstable();
        for &i in &self.track_buf {
            let data = song.midi.track(i as usize);
            let r = &mut cur.readers[i as usize];
            // A track standing for identical ones counts them; they are not played again.
            let copies = song.midi.track_copies[i as usize];
            macro_rules! note {
                ($ch:expr, $key:expr, $vel:expr, $dur:expr) => {{
                    let (ch, key, vel, dur): (u8, u8, u8, Option<u32>) = ($ch, $key, $vel, $dur);
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
                }};
            }
            let mut keys = [0u8; 16];
            while let Some(rec) = r.next_at(data, tick) {
                match rec {
                    Record::Note { ch, key, vel, dur } => {
                        note!(ch, key, vel, dur);
                        while let Some((ch, vel, dur)) = r.same_tick_run(data, &mut keys) {
                            for &key in &keys {
                                note!(ch, key, vel, dur);
                            }
                        }
                    }
                    Record::Other { status, d1, d2 } => batch.push(pack_short(status, d1, d2)),
                    Record::Sysex { status, at, len } => {
                        batch.push(sysex_marker(self.sysex.len()));
                        self.sysex.push(SysexRef { status, at, len });
                    }
                }
            }
            if let Some(t) = r.peek_tick(data) {
                cur.due.push(t, Due::Track(i));
            }
        }
        self.track_buf.clear();
    }
}

/// For the benchmark: what the engine does with every note of `song`, offline and as fast
/// as it goes (no synth, no clock).
///
/// Returns (notes decoded, notes counting repeated tracks, a hash of every message) so a
/// change to the engine can be checked to leave its output as it was. `ENGINE_INTERVAL_US`
/// runs it with note skipping at that interval, `ENGINE_HASH` computes the hash (it costs time).
pub fn bench_engine(song: &SongData) -> (u64, u64, u64, f64) {
    let secs = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<f64>().ok());
    let from = secs("ENGINE_FROM_S").map_or(0, |s| song.tempo.time_to_tick((s * 1e6) as u64));
    let mut eng = Engine::new(Cursor::seek(song, from).cursor, Skipper::new(0, true));
    if std::env::var_os("ENGINE_TRACKS").is_some() {
        let n = song.midi.tracks.len();
        let stop = secs("ENGINE_STOP_S").map_or(u32::MAX, |s| song.tempo.time_to_tick((s * 1e6) as u64));
        let counts: Vec<(usize, u64)> = (0..n).into_par_iter().map(|i| {
            let mut r = eng.cur.readers[i];
            let d = song.midi.track(i);
            let mut c = 0u64;
            while let Some(_) = r.next_before(d, stop) { c += 1; }
            (i, c)
        }).collect();
        eprintln!("{} tracks, copies {:?}", n, &song.midi.track_copies[..n.min(40)]);
        let mut sorted = counts.clone(); sorted.sort_by_key(|x| std::cmp::Reverse(x.1));
        eprintln!("top: {:?}", &sorted[..sorted.len().min(40)]);
        eprintln!("total {}", counts.iter().map(|x| x.1).sum::<u64>());
    }
    let started = Instant::now();
    eng.skipper.interval_us = std::env::var("ENGINE_INTERVAL_US").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let hash = std::env::var_os("ENGINE_HASH").is_some();
    let (mut batch, mut h) = (Vec::new(), 0xcbf29ce484222325u64);
    // `ENGINE_THREADS=n` runs the engine of `par` on n threads, in windows of
    // `ENGINE_WINDOW_TICKS` (or 6) ticks; its messages are the serial engine's, except that
    // the note-offs of a tick are by slot (the hash puts the serial ones in that order too).
    // `ENGINE_FROM_S` and `ENGINE_STOP_S` (seconds) run only part of the song.
    let threads: usize = std::env::var("ENGINE_THREADS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let stop = secs("ENGINE_STOP_S").map_or(u32::MAX, |s| song.tempo.time_to_tick((s * 1e6) as u64));
    let mut feed = |batch: &[u32]| {
        if hash { for &m in batch { h = (h ^ m as u64).wrapping_mul(0x100000001b3); } }
    };
    if threads > 0 {
        let span: u32 = std::env::var("ENGINE_WINDOW_TICKS").ok().and_then(|v| v.parse::<u32>().ok()).unwrap_or(6);
        eng.enter_parallel(threads);
        // Time follows the tick (1 tick = 1 ms), as below.
        let time = |tick: u32, _seg: &mut usize| (tick as u64 * 1000, 1000.0f32);
        let mut ends = Vec::new();
        while let Some(first) = eng.next_tick().filter(|&t| t < stop) {
            batch.clear();
            ends.clear();
            eng.process_window(song, first.saturating_add(span.max(1)), &time, &mut batch, &mut ends);
            feed(&batch);
        }
    } else {
        while let Some(tick) = eng.next_tick().filter(|&t| t < stop) {
            batch.clear();
            // Time follows the tick (1 tick = 1 ms) so a skip interval means something.
            eng.process_tick(song, tick, tick as u64 * 1000, 1000.0, &mut batch);
            if hash {
                batch[..eng.off_count].sort_unstable_by_key(|&m| Skipper::slot(m as u8, (m >> 8) as u8));
            }
            feed(&batch);
        }
    }
    let secs = started.elapsed().as_secs_f64();
    if let Some(par) = &eng.par {
        let t = &par.spent;
        eprintln!("stages (s): parse {:.2}, decide {:.2}, collect {:.2}, emit {:.2}; sent {}",
            t[0].as_secs_f64(), t[1].as_secs_f64(), t[2].as_secs_f64(), t[3].as_secs_f64(), eng.sent);
        let (c, p) = eng.busy();
        eprintln!("busy: chunks {:.2?}, partitions {:.2?}", c, p);
    }
    (eng.decoded, eng.notes_played, h, secs)
}

/// Messages due over a short stretch of song time, decoded ahead by the decoder thread
/// and sent to the synth by the player thread when their time comes.
#[derive(Default)]
struct Slice {
    /// Bumped by every seek; slices of an older epoch are stale and dropped.
    epoch: u32,
    /// Song time of its first tick.
    first_us: u64,
    /// The song has no more events after this (it carries none itself).
    end: bool,
    msgs: Vec<u32>,
    sysex: Vec<SysexRef>,
    /// With timestamps on: each message's time after `first_us` (song µs).
    offs: Vec<u32>,
    ticks: u32,
    /// What the decoder spent on it.
    own: Duration,
    /// Notes decoded and notes not played since the previous slice.
    notes: u64,
    skipped: u64,
    /// The engine's counters after it.
    notes_played: u64,
    polyphony: u32,
    peak_polyphony: u32,
}

/// What the decoder thread is told besides decoding on.
enum ToDecoder {
    /// Continue from `cursor` (see `Cursor::seek`); `chase` is what was sent to restore the
    /// sound, for the skipper and the polyphony.
    Seek { epoch: u32, cursor: Cursor, chase: Vec<u32>, started: u64, time_us: u64 },
    SynthLimit(u32),
    Skipping(bool),
}

/// What the sender tells the decoder, for the skipper's budget.
struct Feedback {
    /// How far behind the clock the last send was (wall µs).
    lag_us: std::sync::atomic::AtomicU64,
    /// Time in the synth per message (ns, f32 bits).
    synth_ns: std::sync::atomic::AtomicU32,
    /// How far ahead of the clock to decode (wall µs).
    ahead_us: std::sync::atomic::AtomicU64,
    /// Slices carry per-message times.
    timed: std::sync::atomic::AtomicBool,
    /// Messages decoded and not yet taken by the sender.
    queued: std::sync::atomic::AtomicU64,
}

/// Decoded song time is kept this far ahead of the clock (wall µs): enough that a slice
/// is ready when its time comes, little enough that the skipper sees what's happening
/// now rather than the future.
const AHEAD_US: u64 = 3_000;
/// With timestamps: slices go to the synth this far ahead of their time (wall µs). A
/// seek or stop waits this long for what was already handed over to play out.
const TIMED_LEAD_US: u64 = 4_000;
/// Decoded messages waiting for the sender at most, so that when it falls behind the
/// skipper hears about it soon.
const MAX_QUEUED_MSGS: u64 = 4 * MAX_SLICE_MSGS as u64;
/// Song time one slice may cover.
const SLICE_SPAN_US: u64 = 500;
/// Most messages in a slice, so a long backlog doesn't hold up the synth for long
/// before the first of them is heard.
const MAX_SLICE_MSGS: usize = 1 << 13;
/// Slices in flight at most.
const SLICES_IN_FLIGHT: usize = 32;

/// The decoding half of playback: turns the song into `Slice`s a little ahead of the
/// clock, so that decoding overlaps with the synth calls of the thread sending them.
struct Decoder {
    song: Arc<SongData>,
    eng: Engine,
    cmds: Receiver<ToDecoder>,
    out: Sender<Slice>,
    /// Emptied slices to reuse.
    spare: Receiver<Slice>,
    clock: Arc<PlaybackClock>,
    feedback: Arc<Feedback>,
    epoch: u32,
    costs: Costs,
    seg_idx: usize,
    /// The end of the song was reported (until the next seek).
    ended: bool,
    /// Threads the engine may use when decoding falls behind the clock (0 = never), and how
    /// many slices in a row it has been behind, or has had to wait for the clock.
    threads: usize,
    behind: u32,
    waited: u32,
    ends: Vec<(u32, u32)>,
    /// What the synth really charges per message (ns), learned from how late it renders:
    /// a synth can take events in a few ns each and still need microseconds to sound them.
    render_ns: f32,
    synth_seq: u64,
    /// `KIM_SYNTH_FEEDBACK=0` turns the learning off.
    learn_render: bool,
}

/// Threads for the engine on this machine: it takes half the cores, which a weak machine
/// can't spare. `KIM_ENGINE_THREADS` overrides (0 = never).
fn engine_threads() -> usize {
    if let Some(n) = std::env::var("KIM_ENGINE_THREADS").ok().and_then(|v| v.parse().ok()) {
        return n;
    }
    let cores = thread::available_parallelism().map_or(1, |n| n.get());
    if cores >= 6 { (cores / 2).clamp(2, 8) } else { 0 }
}

/// Slices in a row the decoder is behind the clock before the engine goes parallel, and
/// loops in a row it has to wait for the clock before it goes back.
const GO_PARALLEL_AFTER: u32 = 16;
const GO_SERIAL_AFTER: u32 = 400;

/// How `Decoder::deliver` went.
enum Delivery {
    Sent,
    /// A seek came in meanwhile: the slice is stale.
    Seeked,
    /// The player has gone.
    Gone,
}

impl Decoder {
    fn spawn(
        song: Arc<SongData>,
        eng: Engine,
        clock: Arc<PlaybackClock>,
        feedback: Arc<Feedback>,
    ) -> (Sender<ToDecoder>, Receiver<Slice>, Sender<Slice>) {
        let (cmd_tx, cmds) = crossbeam_channel::unbounded();
        let (out, slices) = crossbeam_channel::bounded(SLICES_IN_FLIGHT);
        let (spare_tx, spare) = crossbeam_channel::bounded(SLICES_IN_FLIGHT * 2);
        let d = Decoder { song, eng, cmds, out, spare, clock, feedback, epoch: 0, costs: Costs::default(), seg_idx: 0, ended: false, threads: engine_threads(), behind: 0, waited: 0, ends: Vec::new(), render_ns: 0.0, synth_seq: 0, learn_render: std::env::var("KIM_SYNTH_FEEDBACK").map_or(true, |v| v != "0") };
        thread::Builder::new().name("player-decode".into()).spawn(move || d.run()).expect("spawn decoder");
        (cmd_tx, slices, spare_tx)
    }

    /// Apply a command; true if it was a seek.
    fn apply(&mut self, cmd: ToDecoder) -> bool {
        match cmd {
            ToDecoder::Seek { epoch, cursor, chase, started, time_us } => {
                self.epoch = epoch;
                self.eng.par = None; // its tracks and slots are the old position's
                self.behind = 0;
                self.eng.cur = cursor;
                self.eng.notes_played = started;
                self.eng.polyphony = chase.len() as u32;
                self.eng.peak_polyphony = self.eng.peak_polyphony.max(self.eng.polyphony);
                self.eng.notes_this_second = 0;
                self.eng.skipper.reset(&chase, time_us);
                self.ended = false;
                true
            }
            ToDecoder::SynthLimit(limit) => {
                self.eng.skipper.limit_nps = limit;
                false
            }
            ToDecoder::Skipping(on) => {
                self.eng.skipper.enabled = on;
                false
            }
        }
    }

    /// Apply the commands waiting: Ok(whether any was a seek), or Err if the player is gone.
    fn commands(&mut self) -> Result<bool, ()> {
        let mut seeked = false;
        loop {
            match self.cmds.try_recv() {
                Ok(cmd) => seeked |= self.apply(cmd),
                Err(crossbeam_channel::TryRecvError::Empty) => return Ok(seeked),
                Err(crossbeam_channel::TryRecvError::Disconnected) => return Err(()),
            }
        }
    }

    fn fresh(&mut self) -> Slice {
        let mut s = self.spare.try_recv().unwrap_or_default();
        s.msgs.clear();
        s.sysex.clear();
        s.offs.clear();
        (s.end, s.ticks, s.own) = (false, 0, Duration::ZERO);
        s
    }

    /// Stamp a slice with the epoch and the engine's counters.
    fn stamp(&mut self, s: &mut Slice) {
        let eng = &mut self.eng;
        s.epoch = self.epoch;
        s.notes = std::mem::take(&mut eng.notes_this_second);
        s.skipped = std::mem::take(&mut eng.skipper.skipped) as u64 + std::mem::take(&mut eng.repeats_skipped);
        s.notes_played = eng.notes_played;
        s.polyphony = eng.polyphony;
        s.peak_polyphony = eng.peak_polyphony;
    }

    /// Hand a slice over, waiting for room while still taking commands.
    fn deliver(&mut self, mut slice: Slice) -> Delivery {
        loop {
            let msgs = slice.msgs.len() as u64;
            match self.out.send_timeout(slice, Duration::from_millis(1)) {
                Ok(()) => {
                    self.feedback.queued.fetch_add(msgs, Ordering::Relaxed);
                    return Delivery::Sent;
                }
                Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => return Delivery::Gone,
                Err(crossbeam_channel::SendTimeoutError::Timeout(s)) => {
                    slice = s;
                    match self.commands() {
                        Ok(true) => return Delivery::Seeked,
                        Ok(false) => {}
                        Err(()) => return Delivery::Gone,
                    }
                }
            }
        }
    }

    /// Once per report from the synth's monitor: late and getting later means each message
    /// costs more than we thought (up quickly); on time lets the price drift back down.
    fn learn_render_cost(&mut self) {
        let seq = self.clock.synth_late_seq.load(Ordering::Acquire);
        if !self.learn_render || seq == self.synth_seq {
            return;
        }
        self.synth_seq = seq;
        let sender = self.costs.synth_ns;
        let from = self.render_ns.max(sender);
        self.render_ns = match self.clock.synth_late_us.load(Ordering::Relaxed) {
            0 => self.render_ns * 0.99,
            1..=2_000 => self.render_ns, // late but catching up: leave the price alone
            2_001..=8_000 => from * 1.05,
            _ => from * 1.3,
        }
        .min(200_000.0);
    }

    fn run(mut self) {
        let tempo = self.song.tempo.clone();
        loop {
            if self.commands().is_err() {
                return;
            }
            let Some(tick) = self.eng.next_tick() else {
                if !self.ended {
                    self.ended = true;
                    let mut slice = self.fresh();
                    self.stamp(&mut slice);
                    slice.end = true;
                    if let Delivery::Gone = self.deliver(slice) {
                        return;
                    }
                }
                match self.cmds.recv_timeout(Duration::from_millis(5)) {
                    Ok(cmd) => {
                        self.apply(cmd);
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                }
                continue;
            };
            let first_us = tempo.tick_to_time_approx(tick, &mut self.seg_idx);

            // Keep to the clock: no further ahead than `AHEAD_US`.
            let (_, now_us, speed) = self.clock.snapshot();
            let ahead = (self.feedback.ahead_us.load(Ordering::Relaxed) as f64 * speed) as u64;
            if self.feedback.queued.load(Ordering::Relaxed) > MAX_QUEUED_MSGS {
                thread::sleep(Duration::from_micros(50));
                continue;
            }
            if first_us > now_us.saturating_add(ahead) {
                self.behind = 0;
                self.waited += 1;
                if self.waited >= GO_SERIAL_AFTER && self.eng.par.is_some() {
                    self.eng.leave_parallel();
                }
                let wait = ((first_us - now_us - ahead) as f64 / speed) as u64;
                thread::sleep(Duration::from_micros(wait.clamp(200, 2_000)));
                continue;
            }
            // Behind the clock: the engine alone is not keeping up, so give it threads.
            self.waited = 0;
            self.behind = if first_us <= now_us { self.behind + 1 } else { 0 };
            if self.behind >= GO_PARALLEL_AFTER && self.threads > 0 && self.eng.par.is_none() {
                self.eng.enter_parallel(self.threads);
            }

            let start = Instant::now();
            let mut slice = self.fresh();
            self.costs.speed = speed;
            self.costs.synth_ns = f32::from_bits(self.feedback.synth_ns.load(Ordering::Relaxed));
            let lag_us = self.feedback.lag_us.load(Ordering::Relaxed);
            self.learn_render_cost();
            self.costs.synth_ns = self.costs.synth_ns.max(self.render_ns);
            self.eng.regulate(lag_us, start, self.costs);
            let (decoded0, sent0) = (self.eng.decoded, self.eng.sent);
            let timed = self.feedback.timed.load(Ordering::Relaxed);
            // Song time moves faster than wall time when sped up, and a slice is one send.
            let span = (SLICE_SPAN_US as f64 * speed.max(1.0)) as u64;
            slice.first_us = first_us;
            let (mut tick, mut tick_us) = (tick, first_us);
            if self.eng.par.is_some() {
                let limit_us = (first_us + span).min(now_us.saturating_add(ahead));
                let end_tick = tempo.time_to_tick(limit_us).max(tick + 1);
                let time = |t: u32, seg: &mut usize| (tempo.tick_to_time_approx(t, seg), tempo.us_per_tick(*seg));
                self.ends.clear();
                self.eng.process_window(&self.song, end_tick, &time, &mut slice.msgs, &mut self.ends);
                slice.ticks += self.ends.len() as u32;
                if timed {
                    for &(t, end) in &self.ends {
                        let off = (tempo.tick_to_time_approx(t, &mut self.seg_idx) - first_us).min(u32::MAX as u64) as u32;
                        slice.offs.resize(end as usize, off);
                    }
                }
            } else {
            loop {
                self.eng.process_tick(&self.song, tick, tick_us, tempo.us_per_tick(self.seg_idx), &mut slice.msgs);
                slice.ticks += 1;
                if timed {
                    slice.offs.resize(slice.msgs.len(), (tick_us - first_us).min(u32::MAX as u64) as u32);
                }
                if slice.msgs.len() >= MAX_SLICE_MSGS {
                    break;
                }
                let Some(next) = self.eng.cur.next_tick() else { break };
                let next_us = tempo.tick_to_time_approx(next, &mut self.seg_idx);
                if next_us - first_us >= span || next_us > now_us.saturating_add(ahead) {
                    break;
                }
                (tick, tick_us) = (next, next_us);
            }
            }
            std::mem::swap(&mut self.eng.sysex, &mut slice.sysex);
            self.eng.sysex.clear();
            slice.own = start.elapsed();

            // What things cost, for the skipper's budget (only from slices big enough to
            // measure).
            let (decoded, sent) = ((self.eng.decoded - decoded0) as f32, (self.eng.sent - sent0) as f32);
            let ewma = |cost: &mut f32, sample: f32| *cost += 0.2 * (sample - *cost);
            if decoded >= 32.0 {
                ewma(&mut self.costs.engine_ns, slice.own.as_nanos() as f32 / decoded);
            }
            if sent >= 32.0 {
                ewma(&mut self.costs.msgs_per_note, slice.msgs.len() as f32 / sent);
            }

            self.stamp(&mut slice);
            if let Delivery::Gone = self.deliver(slice) {
                return;
            }
        }
    }
}

pub struct Player {
    omni: Option<Arc<OmniMIDI>>,
    choice: OutputChoice,
    out: Output,
    synth_limit: u32,
    skipping: bool,
    log_stats: bool,
    /// See `PlayerCommand::SetLongData`, and the scratch its bytes are built in.
    long_data: bool,
    stream_buf: Vec<u8>,
    /// Timestamped submission (SVMS): wanted, the latest time handed to the synth (its
    /// clock's ticks), and a scratch for the times of one slice.
    horizon: std::cell::Cell<u64>,
    stamps: Vec<u64>,
    cmd_rx: Receiver<PlayerCommand>,
    telemetry_tx: Latest<PlayerTelemetry>,
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
        telemetry_tx: Latest<PlayerTelemetry>,
        stop: Arc<AtomicBool>,
        clock: Arc<PlaybackClock>,
    ) {
        // The playback thread waits for each slice's time and sends it to the synth; a
        // decoder thread (see `Decoder`) keeps slices coming a few ms ahead, so seeks and
        // stops still take effect at once.
        thread::spawn(move || {
            let mut player = Self {
                omni,
                choice,
                out: Output::Closed,
                synth_limit: 0,
                skipping: true,
                log_stats: false,
                long_data: false,
                stream_buf: Vec::new(),
                horizon: Default::default(),
                stamps: Vec::new(),
                cmd_rx,
                telemetry_tx,
                stop,
                svms: SvmsSetup { clock: clock.clone(), log: Default::default(), batch: false, direct: true },
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
                    Ok(PlayerCommand::SetLongData(on)) => {
                        self.long_data = on;
                        continue;
                    }
                    Ok(PlayerCommand::SetSvmsBatch(on)) => {
                        self.svms.batch = on;
                        continue;
                    }
                    Ok(PlayerCommand::SetSvmsDirect(on)) => {
                        self.svms.direct = on;
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

    /// Plays a song: this thread waits for each decoded slice's time and sends it to the
    /// synth; a `Decoder` thread keeps slices coming a little ahead of the clock. Seeks
    /// are scanned here, then handed to the decoder (slices from before it are dropped,
    /// by epoch), so seeks and stops still take effect at once.
    fn play_song(&mut self, song: Arc<SongData>) -> Option<Arc<SongData>> {
        if !self.out.is_open() {
            self.out = Output::open(&self.choice, self.omni.as_ref(), &self.svms);
        }

        const UI_UPDATE: Duration = Duration::from_millis(33);

        let tempo = &song.tempo;
        let duration_us = tempo.duration_us();
        let feedback = Arc::new(Feedback {
            lag_us: Default::default(),
            synth_ns: std::sync::atomic::AtomicU32::new(Costs::default().synth_ns.to_bits()),
            ahead_us: std::sync::atomic::AtomicU64::new(AHEAD_US),
            timed: Default::default(),
            queued: Default::default(),
        });
        let mut timed_on = false;
        let eng = Engine::new(Cursor::seek(&song, 0).cursor, Skipper::new(self.synth_limit, self.skipping));
        // Dropping `to_decoder` (when this returns) ends the decoder thread.
        let (to_decoder, slices, spare) = Decoder::spawn(song.clone(), eng, self.clock.clone(), feedback.clone());
        let mut epoch = 0u32;
        // The slice being waited for, and whether the decoder has run out of events.
        let mut waiting: Option<Slice> = None;
        let mut exhausted = false;
        let mut synth_ns = Costs::default().synth_ns;
        let mut acc = StatsAcc::new();
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
        // Notes decoded and not played, since the last second was reported.
        let (mut notes_this_second, mut skipped_this_second) = (0u64, 0u64);

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
                        let _ = to_decoder.send(ToDecoder::SynthLimit(limit));
                    }
                    PlayerCommand::SetSkipping(on) => {
                        self.skipping = on;
                        let _ = to_decoder.send(ToDecoder::Skipping(on));
                    }
                    PlayerCommand::SetStatsLog(on) => {
                        self.log_stats = on;
                        log = if on { log.or_else(|| open_log(true)) } else { None };
                        live.log_path = log.as_ref().map(|l| l.path.clone());
                        *self.svms.log.lock().unwrap() = svms_log(&log);
                    }
                    PlayerCommand::SetLongData(on) => self.long_data = on,
                    PlayerCommand::SetSvmsBatch(on) => {
                        self.svms.batch = on;
                        self.out.set_svms_batch(on);
                    }
                    PlayerCommand::SetSvmsDirect(on) => {
                        self.svms.direct = on;
                        self.out.set_svms_direct(on);
                    }
                }
            }

            let (paused, now_us, speed) = self.clock.snapshot();
            if paused {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            let want_timed = self.out.can_timed();
            if want_timed != timed_on {
                timed_on = want_timed;
                feedback.timed.store(timed_on, Ordering::Relaxed);
                feedback.ahead_us.store(AHEAD_US + if timed_on { TIMED_LEAD_US } else { 0 }, Ordering::Relaxed);
            }

            if let Some(target_us) = pending_seek.take() {
                let target_tick = tempo.time_to_tick(target_us.min(duration_us));
                let seek = Cursor::seek(&song, target_tick);
                abs_time_us = tempo.tick_to_time_with_cache(target_tick, &mut seg_idx);
                // Everything decoded so far is from before the seek.
                epoch = epoch.wrapping_add(1);
                let _ = to_decoder.send(ToDecoder::Seek {
                    epoch,
                    cursor: seek.cursor,
                    chase: seek.chase.clone(),
                    started: seek.started,
                    time_us: abs_time_us,
                });
                while let Ok(old) = slices.try_recv() {
                    feedback.queued.fetch_sub(old.msgs.len() as u64, Ordering::Relaxed);
                    let _ = spare.try_send(old);
                }
                waiting = None;
                exhausted = false;
                self.clock.set_running(abs_time_us);
                live.notes_played = seek.started;
                live.polyphony = seek.chase.len() as u32;
                live.peak_polyphony = live.peak_polyphony.max(live.polyphony);
                notes_this_second = 0;
                live.nps = 0;
                live.playback_time_us = abs_time_us;
                live.is_playing = true;
                self.telemetry_tx.send(live.clone()).ok();

                self.all_notes_off();
                self.send_events(&song, &seek.controllers, &seek.sysex);
                self.out.send_batch(&seek.chase);
                continue;
            }

            let slice = match waiting.take() {
                Some(slice) => slice,
                None => {
                    // The song ran out: it is over once its time has.
                    if exhausted {
                        if now_us >= duration_us {
                            // Finished: pause at the end but stay on this song so seeks still work.
                            self.all_notes_off();
                            abs_time_us = duration_us;
                            self.clock.set_paused(abs_time_us);
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
                    }
                    match slices.recv_timeout(Duration::from_micros(500)) {
                        Ok(slice) => {
                            feedback.queued.fetch_sub(slice.msgs.len() as u64, Ordering::Relaxed);
                            slice
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                            self.all_notes_off();
                            return None;
                        }
                    }
                }
            };
            if slice.epoch != epoch {
                let _ = spare.try_send(slice);
                continue;
            }
            if slice.end {
                exhausted = true;
                let _ = spare.try_send(slice);
                continue;
            }

            // Wait for its time. Sleep in short slices (commands stay responsive), then
            // spin the last stretch for accuracy. With timestamps it goes out a few ms
            // early instead (the synth knows when), so nothing needs spinning; a slice
            // with SysEx still goes out on time, since SysEx can't be timestamped.
            let timed = timed_on && slice.sysex.is_empty() && slice.offs.len() == slice.msgs.len();
            let lead = if timed { (TIMED_LEAD_US as f64 * speed) as u64 } else { 0 };
            // Song time runs `speed`x faster than wall time.
            let wait_us = (slice.first_us.saturating_sub(now_us).saturating_sub(lead) as f64 / speed) as u64;
            if wait_us > 1_500 {
                thread::sleep(Duration::from_micros((wait_us - 1_000).min(5_000)));
                waiting = Some(slice);
                continue;
            }
            if wait_us > 0 && !timed {
                std::hint::spin_loop();
                waiting = Some(slice);
                continue;
            }

            // Everything due by now goes out in order; behind the clock, slices follow
            // each other without a pause (no decoding in between: that's another thread).
            live.lag_us = (now_us.saturating_sub(slice.first_us) as f64 / speed) as u64; // wall time
            feedback.lag_us.store(live.lag_us, Ordering::Relaxed);
            abs_time_us = slice.first_us;
            acc.ticks += slice.ticks;
            acc.sched += slice.own;
            notes_this_second += slice.notes;
            skipped_this_second += slice.skipped;
            (live.notes_played, live.polyphony, live.peak_polyphony) =
                (slice.notes_played, slice.polyphony, slice.peak_polyphony);

            // Send, timing the synth separately from the decoding.
            let send_start = Instant::now();
            if timed {
                self.send_timed(&slice, speed);
            } else {
                self.send_events(&song, &slice.msgs, &slice.sysex);
            }
            let done = Instant::now();
            let send_time = done - send_start;
            acc.synth += send_time;
            acc.worst_batch = acc.worst_batch.max(send_time);
            acc.msgs += slice.msgs.len() as u64;

            // What the synth costs, for the skipper's budget (only from slices big
            // enough to measure).
            if slice.msgs.len() >= 32 {
                synth_ns += 0.2 * (send_time.as_nanos() as f32 / slice.msgs.len() as f32 - synth_ns);
                feedback.synth_ns.store(synth_ns.to_bits(), Ordering::Relaxed);
            }
            let _ = spare.try_send(slice);

            // --- Telemetry and Stats ---
            if done - last_second >= Duration::from_secs(1) {
                live.nps = std::mem::take(&mut notes_this_second);
                live.skipped_nps = std::mem::take(&mut skipped_this_second);
                live.stats = acc.take();
                live.stats.batched = self.out.batched();
                live.playback_time_us = self.clock.now_us();
                if let Some(log) = &mut log {
                    log.row(&live, speed, &self.choice, self.skipping);
                }
                last_second = done;
            }

            if done - last_ui >= UI_UPDATE {
                live.svms = self.out.svms_stats();
                live.playback_time_us = self.clock.now_us();
                live.is_playing = !self.clock.is_paused();
                self.telemetry_tx.send(live.clone()).ok();
                last_ui = done;
            }
        }
    }

    /// Send a batch that may hold SysEx markers: the messages between them go out as
    /// batches, each SysEx on its own, in order.
    fn send_events(&mut self, song: &SongData, msgs: &[u32], sysex: &[SysexRef]) {
        if self.long_data && self.out.can_long() {
            // All of it through long messages, even a few events: they don't go through
            // the synth's event buffer, so mixing the paths could reorder them.
            let Self { out, stream_buf, .. } = self;
            return stream_chunks(song, msgs, sysex, stream_buf, |b| out.send_long(b));
        }
        if sysex.is_empty() {
            return self.out.send_batch(msgs);
        }
        let (mut from, mut bytes) = (0, Vec::new());
        for (i, &m) in msgs.iter().enumerate() {
            if m as u8 != 0xF0 {
                continue;
            }
            self.out.send_batch(&msgs[from..i]);
            from = i + 1;
            let x = sysex[(m >> 8) as usize];
            bytes.clear();
            if x.status == 0xF0 {
                bytes.push(0xF0);
            }
            bytes.extend_from_slice(song.midi.sysex(x.at, x.len));
            self.out.send_long(&bytes);
        }
        self.out.send_batch(&msgs[from..]);
    }

    /// Hand a slice to the synth with the time each message is due, in the synth's clock.
    fn send_timed(&mut self, slice: &Slice, speed: f64) {
        let Some((qpc, freq)) = self.out.synth_clock() else { return };
        let mut horizon = self.horizon.get();
        stamp_slice(slice.first_us, &slice.offs, self.clock.now_us(), speed, qpc, freq, &mut horizon, &mut self.stamps);
        self.horizon.set(horizon);
        self.out.send_batch_at(&slice.msgs, &self.stamps);
    }

    /// Wait for what was handed to the synth with a time in the future to play out, so
    /// that what is sent next (silencing, a seek's chase) comes after it.
    fn wait_for_horizon(&self) {
        let horizon = self.horizon.replace(0);
        if let Some((now, freq)) = self.out.synth_clock() {
            if horizon > now {
                thread::sleep(Duration::from_micros((horizon - now) * 1_000_000 / freq.max(1) + 200));
            }
        }
    }

    fn all_notes_off(&self) {
        self.wait_for_horizon();
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
    fn seek_chases_controller_state() {
        // ch 0: bank, program, volume twice (last wins), bend; RPN 0 data; then a note at 200.
        let trk = [
            0x00, 0xB0, 0, 5, 0x00, 0xC0, 9, 0x00, 0xB0, 7, 100, 0x01, 0xB0, 7, 50,
            0x00, 0xE0, 1, 2, 0x00, 0xB0, 101, 0, 0x00, 0xB0, 100, 0, 0x00, 0xB0, 6, 12,
            0x81, 0x48, 0x90, 60, 100, 0x10, 0x80, 60, 0,
        ];
        let mut f = b"MThd".to_vec();
        f.extend_from_slice(&[0, 0, 0, 6, 0, 0, 0, 1, 0, 96]);
        f.extend_from_slice(b"MTrk");
        f.extend_from_slice(&(trk.len() as u32).to_be_bytes());
        f.extend_from_slice(&trk);
        let path = std::env::temp_dir().join("kimrashi_seek_ctl_test.mid");
        std::fs::write(&path, f).unwrap();
        let song = SongData::load(&path, 127, &Default::default()).unwrap();
        let s = Cursor::seek(&song, 100);
        let m = |st, a, b| pack_short(st, a, b);
        // bank, program, then the rest by tick; volume 100 was overwritten by 50.
        assert_eq!(
            s.controllers,
            [m(0xB0, 0, 5), m(0xC0, 9, 0), m(0xB0, 7, 50), m(0xE0, 1, 2), m(0xB0, 101, 0), m(0xB0, 100, 0), m(0xB0, 6, 12)]
        );
        assert!(s.chase.is_empty());
        assert!(Cursor::seek(&song, 0).controllers.is_empty());
    }

    #[test]
    fn sysex_is_played_in_order_and_chased_from_the_last_reset() {
        let trk = [
            0x00, 0xB0, 7, 90, // tick 0: volume (undone by the reset)
            0x0A, 0xF0, 5, 0x7E, 0x7F, 0x09, 0x01, 0xF7, // tick 10: GM on
            0x0A, 0xF0, 3, 0x41, 0x10, 0xF7, // tick 20
            0x00, 0xB0, 10, 64, // tick 20: pan
            0x0A, 0x90, 60, 100, 0x0A, 0x80, 60, 0, // tick 30..40
        ];
        let mut f = b"MThd".to_vec();
        f.extend_from_slice(&[0, 0, 0, 6, 0, 0, 0, 1, 0, 96]);
        f.extend_from_slice(b"MTrk");
        f.extend_from_slice(&(trk.len() as u32).to_be_bytes());
        f.extend_from_slice(&trk);
        let path = std::env::temp_dir().join(format!("kimrashi_sysex_test_{}.mid", std::process::id()));
        std::fs::write(&path, f).unwrap();
        let song = SongData::load(&path, 127, &Default::default()).unwrap();
        let _ = std::fs::remove_file(&path);

        let mut eng = Engine::new(Cursor::seek(&song, 0).cursor, Skipper::new(0, true));
        let mut batch = vec![];
        while let Some(tick) = eng.cur.next_tick() {
            eng.process_tick(&song, tick, 0, 0.0, &mut batch);
        }
        let m = &song.midi;
        let bytes: Vec<_> = eng.sysex.iter().map(|x| (x.status, m.sysex(x.at, x.len).to_vec())).collect();
        assert_eq!(bytes, [(0xF0, vec![0x7E, 0x7F, 0x09, 0x01, 0xF7]), (0xF0, vec![0x41, 0x10, 0xF7])]);
        assert_eq!(
            batch,
            [pack_short(0xB0, 7, 90), sysex_marker(0), sysex_marker(1), pack_short(0xB0, 10, 64), pack_short(0x90, 60, 100), pack_short(0x80, 60, 0)]
        );

        // As a byte stream: running status, the note-off as a velocity-0 note-on, and each
        // SysEx in a message of its own (the chunk before it ends there).
        let mut chunks: Vec<Vec<u8>> = vec![];
        stream_chunks(&song, &batch, &eng.sysex, &mut vec![], |b| chunks.push(b.to_vec()));
        assert_eq!(
            chunks,
            [
                vec![0xB0, 7, 90],
                vec![0xF0, 0x7E, 0x7F, 0x09, 0x01, 0xF7],
                vec![0xF0, 0x41, 0x10, 0xF7],
                vec![0xB0, 10, 64, 0x90, 60, 100, 60, 0],
            ]
        );

        // Seeking to tick 25: the volume came before the reset, so only the reset, the
        // later SysEx and the pan are sent.
        let s = Cursor::seek(&song, 25);
        assert_eq!(s.controllers, [sysex_marker(0), sysex_marker(1), pack_short(0xB0, 10, 64)]);
        assert_eq!(s.sysex.len(), 2);
        assert_eq!(m.sysex(s.sysex[0].at, s.sysex[0].len), [0x7E, 0x7F, 0x09, 0x01, 0xF7]);
        // At tick 15 only the reset is behind it; at tick 5 only the volume.
        assert_eq!(Cursor::seek(&song, 15).controllers, [sysex_marker(0)]);
        assert_eq!(Cursor::seek(&song, 5).controllers, [pack_short(0xB0, 7, 90)]);
    }

    /// How fast the playback thread(s) get through a song with no synth to wait for: plays
    /// generated black MIDI unpaced on a closed output and prints what telemetry says.
    /// `cargo test --release player_loop_throughput -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn player_loop_throughput() {
        use crate::clock::PlaybackClock;
        let path = std::env::temp_dir().join(format!("kimrashi_loop_{}.mid", std::process::id()));
        std::fs::write(&path, crate::benchmark::midi_file(16, 2_000_000)).unwrap();
        let song = SongData::load(&path, 127, &Default::default()).unwrap();
        let _ = std::fs::remove_file(&path);
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let (tel_tx, tel_rx) = latest();
        let stop = Arc::new(AtomicBool::new(false));
        let clock = Arc::new(PlaybackClock::new());
        clock.set_speed(std::env::var("SPEED").ok().and_then(|s| s.parse().ok()).unwrap_or(1e9));
        Player::new(None, OutputChoice::Kdmapi, cmd_rx, tel_tx, stop.clone(), clock);
        let total = song.midi.note_count;
        crate::output::SIM_SYNTH_NS.store(std::env::var("SYNTH_NS").ok().and_then(|s| s.parse().ok()).unwrap_or(0), Ordering::Relaxed);
        cmd_tx.send(PlayerCommand::SetSkipping(false)).unwrap();
        cmd_tx.send(PlayerCommand::Play(song)).unwrap();
        // Unpaced (a huge speed): the song plays as fast as the player can get through it.
        let start = Instant::now();
        let (mut played, mut worst_lag) = (0, 0);
        while start.elapsed() < Duration::from_secs(60) {
            if let Ok(t) = tel_rx.recv_timeout(Duration::from_millis(100)) {
                played = t.notes_played;
                worst_lag = worst_lag.max(t.lag_us);
                if !t.is_playing && played > 0 {
                    break;
                }
            }
        }
        let secs = start.elapsed().as_secs_f64();
        stop.store(true, Ordering::SeqCst);
        println!("{played} of {total} notes in {secs:.2} s: {:.1} M notes/s, worst lag {worst_lag} us", played as f64 / secs / 1e6);
    }

    #[test]
    fn the_player_sends_what_the_engine_makes_and_plays_it_again_after_a_seek() {
        use crate::clock::PlaybackClock;
        let (song, _) = load_random_song("pipeline");
        let mut eng = Engine::new(Cursor::seek(&song, 0).cursor, Skipper::new(0, true));
        let mut want = vec![];
        while let Some(tick) = eng.cur.next_tick() {
            eng.process_tick(&song, tick, 0, 0.0, &mut want);
        }
        assert!(want.len() > 500);

        *crate::output::CAPTURE.lock().unwrap() = Some(vec![]);
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let (tel_tx, tel_rx) = latest();
        let stop = Arc::new(AtomicBool::new(false));
        let clock = Arc::new(PlaybackClock::new());
        clock.set_speed(1e6); // the song is over in an instant
        Player::new(None, OutputChoice::Kdmapi, cmd_rx, tel_tx, stop.clone(), clock.clone());
        let finished = || {
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(20) {
                if let Ok(t) = tel_rx.recv_timeout(Duration::from_millis(50)) {
                    if !t.is_playing && t.playback_time_us > 0 {
                        return;
                    }
                }
            }
            panic!("the song did not finish");
        };
        let take = || std::mem::take(crate::output::CAPTURE.lock().unwrap().as_mut().unwrap());
        // Everything a pass sends, minus the all-notes-off (2 messages x 16 channels) at its end.
        let check = |got: Vec<u32>, lead: usize| {
            assert_eq!(got.len(), lead + want.len() + 32);
            assert!(got[lead..got.len() - 32] == want[..], "the messages differ");
        };
        cmd_tx.send(PlayerCommand::SetSkipping(false)).unwrap();
        cmd_tx.send(PlayerCommand::Play(song)).unwrap();
        finished();
        check(take(), 0);

        // Like the UI's seek: the clock runs from there, and the player is told.
        clock.set_running(0);
        cmd_tx.send(PlayerCommand::Seek(0)).unwrap();
        finished();
        check(take(), 32); // the seek's own all-notes-off first
        stop.store(true, Ordering::SeqCst);
        *crate::output::CAPTURE.lock().unwrap() = None;
    }

    /// Read a byte stream back into messages the way a synth would (running status, SysEx
    /// as markers' worth of bytes), with note-offs as the note-ons of velocity 0 they were sent as.
    fn read_stream(bytes: &[u8]) -> Vec<u32> {
        let (mut out, mut i, mut running) = (vec![], 0, 0u8);
        while i < bytes.len() {
            if bytes[i] & 0x80 != 0 {
                running = bytes[i];
                i += 1;
            }
            assert!(running != 0, "data without a status");
            let n = if matches!(running & 0xF0, 0xC0 | 0xD0) { 1 } else { 2 };
            let d = &bytes[i..i + n];
            assert!(d.iter().all(|&b| b < 0x80), "a status byte inside a message");
            out.push(pack_short(running, d[0], *d.get(1).unwrap_or(&0)));
            i += n;
        }
        out
    }

    #[test]
    fn byte_streams_read_back_as_the_messages_they_were_made_from() {
        let (song, _) = load_random_song("stream");
        let mut eng = Engine::new(Cursor::seek(&song, 0).cursor, Skipper::new(0, true));
        let (mut batch, mut sent) = (vec![], 0);
        while let Some(tick) = eng.cur.next_tick() {
            eng.process_tick(&song, tick, 0, 0.0, &mut batch);
        }
        let batch = batch.repeat(20); // enough for several chunks
        let mut chunks = vec![];
        stream_chunks(&song, &batch, &[], &mut vec![], |b| {
            assert!(b.len() < 4096 + 3);
            chunks.push(b.to_vec());
        });
        assert!(chunks.len() > 1);
        let got: Vec<u32> = chunks.iter().flat_map(|c| read_stream(c)).collect();
        let want: Vec<u32> = batch
            .iter()
            .map(|&m| if m & 0xF0 == 0x80 { pack_short(m as u8 | 0x10, (m >> 8) as u8, 0) } else { m })
            .collect();
        assert_eq!(got, want);
        // and it is shorter than the 4 bytes a message
        sent += chunks.iter().map(|c| c.len()).sum::<usize>();
        assert!(sent < batch.len() * 3, "{sent} bytes for {} messages", batch.len());
    }

    #[test]
    fn timestamps_follow_the_song_clock_and_never_go_backwards() {
        let (mut last, mut out) = (0, vec![]);
        // A 10 MHz clock at 2x speed: a song microsecond is 5 ticks. Due 2000, 2500 and
        // 2500 song us ahead of now.
        stamp_slice(1_000_000, &[1_000, 1_500, 1_500], 999_000, 2.0, 5_000, 10_000_000, &mut last, &mut out);
        assert_eq!(out, [15_000, 17_500, 17_500]);
        // A later slice that is already late goes as of now, but not before what was sent.
        stamp_slice(1_000_200, &[0], 1_001_000, 2.0, 5_100, 10_000_000, &mut last, &mut out);
        assert_eq!((&out[..], last), (&[17_500][..], 17_500));
        // On time and past the horizon: its own time.
        stamp_slice(1_010_000, &[0], 1_001_000, 2.0, 5_100, 10_000_000, &mut last, &mut out);
        assert_eq!(out, [5_100 + 45_000]);
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

    /// A song to compare engines on: `tracks` tracks of notes on few keys (so keys are
    /// struck again while sounding), some of them zero-length, with controllers and SysEx
    /// among them.
    fn dense_song(name: &str, tracks: usize) -> Arc<SongData> {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut rnd = move |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        let mut file = b"MThd".to_vec();
        file.extend_from_slice(&[0, 0, 0, 6, 0, 1, 0, tracks as u8, 0, 96]);
        for _ in 0..tracks {
            let mut events: Vec<(u32, Vec<u8>)> = vec![];
            let mut tick = 0u32;
            for _ in 0..600 {
                tick += rnd(4) as u32;
                let (ch, key) = (rnd(3) as u8, 60 + rnd(6) as u8);
                match rnd(12) {
                    0 => events.push((tick, vec![0xB0 | ch, rnd(128) as u8, rnd(128) as u8])),
                    1 => events.push((tick, vec![0xC0 | ch, rnd(128) as u8])),
                    2 => events.push((tick, vec![0xF0, 3, 0x41, rnd(128) as u8, 0xF7])),
                    _ => {
                        let len = [0, 0, 1, 2, 5, 9, 20, 40][rnd(8) as usize];
                        events.push((tick, vec![0x90 | ch, key, 1 + rnd(127) as u8]));
                        events.push((tick + len, vec![0x80 | ch, key, 0]));
                    }
                }
            }
            events.sort_by_key(|e| e.0); // stable: a note-on stays before its own off
            let (mut trk, mut last) = (vec![], 0);
            for (t, data) in &events {
                let mut delta = vec![((t - last) & 0x7F) as u8];
                let mut d = (t - last) >> 7;
                while d > 0 {
                    delta.push((d & 0x7F) as u8 | 0x80);
                    d >>= 7;
                }
                trk.extend(delta.iter().rev());
                trk.extend_from_slice(data);
                last = *t;
            }
            file.extend_from_slice(b"MTrk");
            file.extend_from_slice(&(trk.len() as u32).to_be_bytes());
            file.extend_from_slice(&trk);
        }
        let path = std::env::temp_dir().join(format!("kimrashi_{name}_{}.mid", std::process::id()));
        std::fs::write(&path, file).unwrap();
        let song = SongData::load(&path, 127, &Default::default()).unwrap();
        let _ = std::fs::remove_file(&path);
        song
    }

    /// What the engine sends for the whole song, 100 us a tick, with a skip interval; the
    /// parallel engine in windows of `span` ticks (0 = the serial one) and switching to the
    /// other every `toggle` windows (0 = never). Note-offs of a tick are by slot, as the
    /// parallel engine has them.
    fn engine_stream(song: &SongData, interval: u64, threads: usize, span: u32, toggle: u32, eager: bool) -> (Vec<u32>, Vec<SysexRef>, u64, u32) {
        let mut eng = Engine::new(Cursor::seek(song, 0).cursor, Skipper::new(0, true));
        eng.skipper.interval_us = interval;
        let (mut all, mut batch, mut ends) = (vec![], vec![], vec![]);
        let time = |tick: u32, _: &mut usize| (tick as u64 * 100, 100.0f32);
        if span > 0 && toggle == 0 {
            eng.enter_parallel(threads);
            eng.par.as_mut().unwrap().eager = eager;
        }
        let mut step = 0;
        while let Some(first) = eng.next_tick() {
            step += 1;
            if toggle > 0 && step % toggle == 0 {
                if eng.par.is_some() {
                    eng.leave_parallel()
                } else {
                    eng.enter_parallel(threads);
                    eng.par.as_mut().unwrap().eager = eager;
                }
            }
            batch.clear();
            if eng.par.is_some() {
                ends.clear();
                eng.process_window(song, first + span.max(1), &time, &mut batch, &mut ends);
                assert!(ends.windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 <= w[1].1));
                assert_eq!(ends.last().map_or(0, |e| e.1) as usize, batch.len());
            } else {
                eng.process_tick(song, first, first as u64 * 100, 100.0, &mut batch);
                batch[..eng.off_count].sort_unstable_by_key(|&m| Skipper::slot(m as u8, (m >> 8) as u8));
            }
            all.extend_from_slice(&batch);
        }
        eng.leave_parallel();
        (all, eng.sysex, eng.notes_played, eng.polyphony)
    }

    #[test]
    fn the_parallel_engine_sends_what_the_serial_one_does() {
        let song = dense_song("par", 7);
        for interval in [0, 600, 2_000] {
            let want = engine_stream(&song, interval, 1, 0, 0, false);
            assert!(want.0.len() > 2_000 && !want.1.is_empty(), "{}", want.0.len());
            if interval > 0 {
                assert!(want.0.len() < engine_stream(&song, 0, 1, 0, 0, false).0.len(), "nothing was skipped");
            }
            for (threads, span, toggle) in [(1, 1, 0), (3, 1, 0), (3, 4, 0), (4, 25, 0), (8, 1000, 0), (3, 3, 5), (2, 20, 2)] {
                for eager in [false, true] {
                    let got = engine_stream(&song, interval, threads, span, toggle, eager);
                    assert!(got == want, "interval {interval}, {threads} threads, windows of {span} ticks, toggling every {toggle}, eager {eager}");
                }
            }
        }
    }

    /// Many notes at one tick with the same velocity and length (the plain two-byte records
    /// `same_tick_run` takes 16 at a time), broken up now and then by other notes and controllers.
    #[test]
    fn bulk_reads_of_plain_notes_match_one_by_one() {
        use crate::parser::{MidiFile, Plain, Progress};
        let mut x = 0x1234_5678_9ABC_DEF1u64;
        let mut rnd = move |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        let mut events: Vec<(u32, Vec<u8>)> = vec![];
        let mut tick = 0;
        for _ in 0..400 {
            tick += rnd(5) as u32 * 3;
            let (ch, vel, len) = (rnd(2) as u8, 90 + rnd(3) as u8, 5);
            for k in 0..rnd(70) as u8 {
                let (vel, len) = if rnd(40) == 0 { (1 + rnd(127) as u8, rnd(30) as u32) } else { (vel, len) };
                events.push((tick, vec![0x90 | ch, 20 + k, vel]));
                events.push((tick + len, vec![0x80 | ch, 20 + k, 0]));
            }
            if rnd(6) == 0 {
                events.push((tick, vec![0xB0, rnd(128) as u8, rnd(128) as u8]));
            }
        }
        events.sort_by_key(|e| e.0);
        let (mut trk, mut last) = (vec![], 0);
        for (t, data) in &events {
            let mut delta = vec![((t - last) & 0x7F) as u8];
            let mut d = (t - last) >> 7;
            while d > 0 {
                delta.push((d & 0x7F) as u8 | 0x80);
                d >>= 7;
            }
            trk.extend(delta.iter().rev());
            trk.extend_from_slice(data);
            last = *t;
        }
        let mut file = b"MThd".to_vec();
        file.extend_from_slice(&[0, 0, 0, 6, 0, 0, 0, 1, 0, 96]);
        file.extend_from_slice(b"MTrk");
        file.extend_from_slice(&(trk.len() as u32).to_be_bytes());
        file.extend_from_slice(&trk);
        let m = MidiFile::parse_stream(Box::new(Plain(std::io::Cursor::new(file))), 127, &Progress::default()).unwrap();

        let t = m.track(0);
        let (mut one, mut r) = (vec![], TrackReader::default());
        while let Some(x) = r.next(t) {
            one.push(x);
        }
        let (mut bulk, mut runs, mut r, mut keys) = (vec![], 0, TrackReader::default(), [0u8; 16]);
        while let Some((tick, rec)) = r.next(t) {
            bulk.push((tick, rec));
            if matches!(rec, Record::Note { .. }) {
                while let Some((ch, vel, dur)) = r.same_tick_run(t, &mut keys) {
                    runs += 1;
                    bulk.extend(keys.iter().map(|&key| (tick, Record::Note { ch, key, vel, dur })));
                }
            }
        }
        assert!(runs > 100, "only {runs} bulk reads");
        assert!(one == bulk, "the records differ");
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
