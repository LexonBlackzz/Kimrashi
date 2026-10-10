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
    pub notes_played: u32,
    pub nps: u32,
    /// Note-ons per second skipped to keep the synth in time.
    pub skipped_nps: u32,
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

/// A pending note-off: (channel, key, virtual). Virtual offs belong to skipped notes;
/// they only keep the polyphony count right and are never sent.
type PendingOff = (u8, u8, bool);

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
            res.started += started;
            for (ch, key, vel, end) in active {
                res.chase.push(pack_short(0x90 | ch, key, vel));
                if let Some(e) = end {
                    res.cursor.due.push(e, Due::Off((ch, key, false)));
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

/// Smart note skipping for synths that can't keep up.
///
/// While playback is on time, nothing is skipped. When sending falls behind the clock
/// (or exceeds the user's synth limit), a minimum re-strike interval per channel+key
/// kicks in and adapts: a note-on that re-strikes a key sooner than that is skipped.
/// - If the key is still sounding, the skipped note is merged into it: the sounding
///   note's note-off is postponed to the skipped note's end, so sustain isn't chopped.
/// - A note on a silent key is only skipped if it's also short (shorter than the
///   interval); longer notes are always played.
/// - Controllers, pitch bend etc. and note-offs of sent notes are never dropped, so
///   nothing gets stuck, and every key keeps sounding at a bounded rate.
struct Skipper {
    /// Minimum time between note-ons on one channel+key; 0 = send everything.
    interval_us: u64,
    /// Time of the last sent note-on per slot (u64::MAX = none yet).
    last_on_us: Box<[u64; SLOTS]>,
    /// Sent note-ons per slot not yet turned off.
    sounding: Box<[u32; SLOTS]>,
    /// Per slot: tick the sounding note must last until to cover merged notes (0 = none).
    hold_until: Box<[u32; SLOTS]>,
    limit_nps: u32,
    /// Off: never skip (interval stays 0).
    enabled: bool,
    window_start: Instant,
    sent_in_window: u32,
    skipped: u32,
}

impl Skipper {
    const NEVER: u32 = u32::MAX;

    fn new(limit_nps: u32, enabled: bool) -> Self {
        Self {
            enabled,
            interval_us: 0,
            last_on_us: Box::new([u64::MAX; SLOTS]),
            sounding: Box::new([0; SLOTS]),
            hold_until: Box::new([0; SLOTS]),
            limit_nps,
            window_start: Instant::now(),
            sent_in_window: 0,
            skipped: 0,
        }
    }

    fn slot(ch: u8, key: u8) -> usize {
        (ch as usize & 0x0F) * 128 + (key as usize & 0x7F)
    }

    /// Decide whether to send a note-on at `time_us`. `end` is its end tick (None =
    /// never released) and `dur_us` its length.
    fn note_on(&mut self, slot: usize, time_us: u64, end: Option<u32>, dur_us: u64) -> bool {
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
        self.sent_in_window += 1;
        true
    }

    /// A sent note's note-off is due at `tick`.
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

    /// Adapt the re-strike interval to how far behind sending is (`lag_us`) and to the
    /// note rate limit. Grows fast under pressure (at least a quarter of the lag, so a
    /// sudden bomb is caught within a few ms), decays slowly once comfortably on time.
    fn regulate(&mut self, lag_us: u64) {
        if !self.enabled {
            self.interval_us = 0;
            return;
        }
        let elapsed = self.window_start.elapsed();
        if elapsed < Duration::from_millis(5) {
            return;
        }
        let rate = self.sent_in_window as f64 / elapsed.as_secs_f64();
        let limit = self.limit_nps as f64;
        let over_limit = self.limit_nps != 0 && rate > limit;
        if lag_us > 20_000 || over_limit {
            self.interval_us = (self.interval_us * 2).max(lag_us / 4).clamp(250, 500_000);
        } else if lag_us < 5_000 && (self.limit_nps == 0 || rate < limit * 0.8) {
            self.interval_us = self.interval_us * 3 / 4;
            if self.interval_us < 250 {
                self.interval_us = 0;
            }
        }
        self.window_start = Instant::now();
        self.sent_in_window = 0;
    }

    /// Forget all per-key state (after a seek or output change); `chase` are the
    /// note-ons sent to restore the sound at `time_us`.
    fn reset(&mut self, chase: &[u32], time_us: u64) {
        *self = Self::new(self.limit_nps, self.enabled);
        for &msg in chase {
            let slot = Self::slot(msg as u8, (msg >> 8) as u8);
            self.sounding[slot] += 1;
            self.last_on_us[slot] = time_us;
        }
    }
}

/// The playback thread's hot path: turns ticks into MIDI messages. Free of clocks,
/// channels and synth calls, so it can be benchmarked on its own.
struct Engine {
    cur: Cursor,
    skipper: Skipper,
    notes_played: u32,
    notes_this_second: u32,
    polyphony: u32,
    peak_polyphony: u32,
    end_tempo_idx: usize,
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
            polyphony: 0,
            peak_polyphony: 0,
            end_tempo_idx: 0,
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

    /// Append the messages for `tick` (which plays at `target_us`) to `batch`: due
    /// note-offs first, so a key struck again on this tick isn't cut off, then every
    /// track's records at this tick.
    fn process_tick(&mut self, song: &SongData, tick: u32, target_us: u64, batch: &mut Vec<u32>) {
        let cur = &mut self.cur;
        let skipper = &mut self.skipper;

        // Note-offs due now go first, so a key struck again on this tick isn't cut off.
        if cur.due.min_key() != Some(tick) {
            return;
        }
        cur.due.take_min(&mut self.due_buf);
        for &due in &self.due_buf {
            let (ch, key, virt) = match due {
                Due::Off(off) => off,
                Due::Track(i) => {
                    self.track_buf.push(i);
                    continue;
                }
            };
            if virt {
                self.polyphony = self.polyphony.saturating_sub(1);
                continue;
            }
            match skipper.note_off(Skipper::slot(ch, key), tick) {
                OffAction::Send => {
                    self.polyphony = self.polyphony.saturating_sub(1);
                    batch.push(pack_short(0x80 | ch, key, 0));
                }
                OffAction::Postpone(t2) => cur.due.push(t2, Due::Off((ch, key, false))),
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
            loop {
                let mut n = *r;
                let rec = match n.next(data) {
                    Some((t, rec)) if t == tick => rec,
                    _ => break,
                };
                *r = n;
                match rec {
                    Record::Note { ch, key, vel, dur } => {
                        self.notes_played = self.notes_played.saturating_add(1);
                        self.notes_this_second = self.notes_this_second.saturating_add(1);
                        self.polyphony += 1;
                        self.peak_polyphony = self.peak_polyphony.max(self.polyphony);
                        let end = dur.map(|d| tick + d);
                        let dur_us = match end {
                            Some(e) if skipper.interval_us > 0 => song
                                .tempo
                                .tick_to_time_with_cache(e, &mut self.end_tempo_idx)
                                - target_us,
                            _ => 0,
                        };
                        let send = skipper.note_on(Skipper::slot(ch, key), target_us, end, dur_us);
                        if let Some(e) = end {
                            // A zero-length note's off lands on this tick: it's taken on
                            // the next call, after this note-on, as in the file.
                            cur.due.push(e, Due::Off((ch, key, !send)));
                        }
                        if send {
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

        let tempo = &song.tempo;
        let duration_us = tempo.duration_us();
        let mut eng = Engine::new(Cursor::seek(&song, 0).cursor, Skipper::new(self.synth_limit, self.skipping));
        // One tick's messages, sent to the synth in one timed call (or loop).
        let mut batch: Vec<u32> = Vec::new();
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
        let mut current_tempo_segment_idx: usize = 0;

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

            if self.clock.is_paused() {
                thread::sleep(Duration::from_millis(5));
                continue;
            }

            if let Some(target_us) = pending_seek.take() {
                let target_tick = tempo.time_to_tick(target_us.min(duration_us));
                let seek = Cursor::seek(&song, target_tick);
                eng.cur = seek.cursor;
                eng.notes_played = seek.started as u32;
                eng.polyphony = seek.chase.len() as u32;
                eng.peak_polyphony = eng.peak_polyphony.max(eng.polyphony);
                eng.notes_this_second = 0;
                abs_time_us =
                    tempo.tick_to_time_with_cache(target_tick, &mut current_tempo_segment_idx);
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
                if self.clock.now_us() >= duration_us {
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

            let target_us = tempo.tick_to_time_with_cache(tick, &mut current_tempo_segment_idx);

            // Wait for the event's time. Sleep in short slices (commands stay responsive),
            // then spin the last stretch for accuracy.
            let now_us = self.clock.now_us();
            let speed = self.clock.speed();
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
            let tick_start = Instant::now();
            abs_time_us = target_us;
            live.lag_us = ((now_us - target_us) as f64 / speed) as u64; // wall time
            eng.skipper.regulate(live.lag_us);
            batch.clear();
            eng.process_tick(&song, tick, target_us, &mut batch);

            // Send this tick's messages, timing the synth separately from our own work.
            let send_start = Instant::now();
            self.out.send_batch(&batch);
            let send_time = send_start.elapsed();
            acc.sched += send_start - tick_start;
            acc.synth += send_time;
            acc.worst_batch = acc.worst_batch.max(send_time);
            acc.msgs += batch.len() as u64;
            acc.ticks += 1;

            // --- Telemetry and Stats ---
            if last_second.elapsed() >= Duration::from_secs(1) {
                live.nps = std::mem::take(&mut eng.notes_this_second);
                live.skipped_nps = std::mem::take(&mut eng.skipper.skipped);
                live.stats = acc.take();
                live.stats.batched = self.out.batched();
                live.playback_time_us = self.clock.now_us();
                eng.sync(&mut live);
                if let Some(log) = &mut log {
                    log.row(&live, speed, &self.choice, self.skipping);
                }
                last_second = Instant::now();
            }

            if last_ui.elapsed() >= UI_UPDATE {
                eng.sync(&mut live);
                live.svms = self.out.svms_stats();
                live.playback_time_us = self.clock.now_us();
                live.is_playing = !self.clock.is_paused();
                self.telemetry_tx.send(live.clone()).ok();
                last_ui = Instant::now();
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
        assert_eq!(offs, [(192, Due::Off((0, 60, false))), (384, Due::Off((1, 48, false)))]);
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
}
