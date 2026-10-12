mod omnimidi;
mod output;
mod archive;
mod bench;
mod benchmark;
mod render_bench;
mod clock;
mod gpu;
mod parser;
mod player;
mod song;
mod svms;
mod vsync;

use clock::PlaybackClock;
use crossbeam_channel::{unbounded, Receiver, Sender};
use eframe::{
    egui::{self, Align2, Color32, CornerRadius, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2},
};
use omnimidi::OmniMIDI;
use player::{Player, PlayerCommand, PlayerTelemetry};
use rayon::prelude::*;
use parser::{Record, TrackReader};
use song::SongData;
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::io::Write as _;
use std::time::{Duration, Instant};

#[cfg(target_os = "windows")]
use winapi::um::timeapi::{timeBeginPeriod, timeEndPeriod};

#[cfg(target_os = "windows")]
struct TimerResolutionGuard;

#[cfg(target_os = "windows")]
impl TimerResolutionGuard {
    fn new() -> Option<Self> {
        unsafe {
            if timeBeginPeriod(1) == 0 {
                Some(Self)
            } else {
                None
            }
        }
    }
}

#[cfg(target_os = "windows")]
impl Drop for TimerResolutionGuard {
    fn drop(&mut self) {
        unsafe {
            timeEndPeriod(1);
        }
    }
}

#[cfg(target_os = "windows")]
type TimerGuard = Option<TimerResolutionGuard>;

#[cfg(not(target_os = "windows"))]
type TimerGuard = Option<()>;

#[cfg(target_os = "windows")]
fn create_timer_guard() -> TimerGuard {
    TimerResolutionGuard::new()
}

#[cfg(not(target_os = "windows"))]
fn create_timer_guard() -> TimerGuard {
    None
}

const DEFAULT_NOTE_SPEED_SECONDS: f32 = 5.0;
const DEFAULT_PITCH_MIN: u8 = 21;
const DEFAULT_PITCH_MAX: u8 = 108;
const SYNTH_LIMIT_OFF: f64 = 20_000_000.0;
const NOTE_HOLD_MICROS: u64 = 1_000; // keep notes visible 1ms after Note Off to avoid abrupt drop

fn main() {
    let _timer = create_timer_guard();
    if bench::run_from_args() || render_bench::run_from_args() || benchmark::run_from_args() {
        return;
    }
    let omni_opt: Option<Arc<OmniMIDI>> = unsafe {
        match OmniMIDI::load::<&str>(None) {
            Ok(om) => Some(Arc::new(om)),
            Err(e) => {
                eprintln!("Failed to load OmniMIDI.dll: {}", e);
                None
            }
        }
    };

    let kdmapi = omni_opt.as_ref().is_some_and(|o| unsafe { o.available() });
    if omni_opt.is_some() && !kdmapi {
        eprintln!("KDMAPI not available.");
    }
    let saved = load_settings();
    let outputs = output::list_outputs(kdmapi);
    let output_idx = saved
        .get("output")
        .and_then(|name| outputs.iter().position(|(_, n)| n == name))
        .unwrap_or(0);
    let initial_output = outputs
        .get(output_idx)
        .map_or(output::OutputChoice::Kdmapi, |(choice, _)| choice.clone());
    let skipping = setting(&saved, "skip_notes").unwrap_or(true);
    let synth_limit = setting(&saved, "synth_limit").unwrap_or(SYNTH_LIMIT_OFF);

    let (cmd_tx, cmd_rx) = unbounded::<PlayerCommand>();
    let (telemetry_tx, telemetry_rx) = unbounded::<PlayerTelemetry>();
    let stop = Arc::new(AtomicBool::new(false));
    let clock = Arc::new(PlaybackClock::new());

    Player::new(
        omni_opt.clone(),
        initial_output,
        cmd_rx,
        telemetry_tx,
        stop.clone(),
        clock.clone(),
    );

    let _ = cmd_tx.send(PlayerCommand::SetSkipping(skipping));
    let svms_batch = setting(&saved, "svms_batch").unwrap_or(false);
    let _ = cmd_tx.send(PlayerCommand::SetSvmsBatch(svms_batch));
    if synth_limit < SYNTH_LIMIT_OFF {
        let _ = cmd_tx.send(PlayerCommand::SetSynthLimit(synth_limit as u32));
    }

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Kimrashi")
            .with_icon(egui::IconData {
                rgba: include_bytes!("../assets/icon64.rgba").to_vec(),
                width: 64,
                height: 64,
            })
            .with_inner_size([1280.0, 720.0])
            .with_min_inner_size([560.0, 360.0]),
        depth_buffer: gpu::DEPTH_BITS,
        // Present uncapped; VSync is done in-app (see vsync.rs) so it can be toggled live.
        wgpu_options: egui_wgpu::WgpuConfiguration {
            present_mode: eframe::wgpu::PresentMode::AutoNoVsync,
            wgpu_setup: egui_wgpu::WgpuSetupCreateNew {
                // egui asks for wgpu's conservative default limits (256 MB buffers);
                // take the adapter's real buffer limit so dense songs fit.
                device_descriptor: Arc::new(|adapter| {
                    let mut desc = (egui_wgpu::WgpuSetupCreateNew::default().device_descriptor)(adapter);
                    desc.required_limits.max_buffer_size = adapter.limits().max_buffer_size;
                    desc
                }),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        },
        ..Default::default()
    };

    // `kimrashi [file] [--from m:ss]`: open a file at startup, optionally seeking into it.
    let mut open_path = None;
    let mut open_at_us = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--from" => open_at_us = args.next().and_then(|t| bench::parse_time(&t).ok()),
            _ if !arg.starts_with("--") => open_path = Some(std::path::PathBuf::from(arg)),
            _ => {}
        }
    }

    let _ = eframe::run_native(
        "kimrashi",
        native_options,
        Box::new(move |_cc| {
            let timer_guard = create_timer_guard();
            let mut state = AppState {
                telemetry_rx,
                cmd_tx,
                stop,
                clock: clock.clone(),
                last_telemetry: PlayerTelemetry::default(),
                current_song: None,
                playback_time_us: 0,
                slider_override_us: None,
                slider_grabbed: false,
                note_speed_seconds: setting(&saved, "note_speed").unwrap_or(DEFAULT_NOTE_SPEED_SECONDS),
                outputs,
                output_idx,
                synth_limit,
                skipping,
                tempo: 1.0,
                log_stats: false,
                detailed_stats: setting(&saved, "detailed_stats").unwrap_or(false),
                synth_stats: setting(&saved, "synth_stats").unwrap_or(false),
                loading_details: setting(&saved, "loading_details").unwrap_or(false),
                verify_repeats: setting(&saved, "verify_repeats").unwrap_or(false),
                benchmark: None,
                svms_batch,
                loading: None,
                show_settings: false,
                vel_levels: setting(&saved, "velocity_levels").unwrap_or(127),
                vsync_enabled: setting(&saved, "vsync").unwrap_or(true),
                notes_upwards: setting(&saved, "notes_rise").unwrap_or(false),
                flat_notes: setting(&saved, "flat_notes").unwrap_or(false),
                visible: VisibleNotes::default(),
                instances: Arc::default(),
                refresh_rate_hz: vsync::VBlank::refresh_hz(),
                vblank: vsync::VBlank::new(),
                status_message: None,
                fps: 0.0,
                last_frame: None,
                timer_guard,
                color_by_channel: setting(&saved, "color_by_channel").unwrap_or(true),
                color_auto: setting(&saved, "color_auto").unwrap_or(true),
                piano_layout: setting(&saved, "piano_layout").unwrap_or(false),
                frame_times: FrameTimes::default(),
                frame_log: FrameLog::from_env(),
                open_at_us,
            };
            if let Some(path) = open_path {
                state.start_loading(path);
            }
            Ok(Box::new(state))
        }),
    );
}

/// Where a frame's CPU time went (see `FrameLog`).
#[derive(Default, Clone, Copy)]
struct FrameTimes {
    /// Decoding notes into the window, culling and sorting them.
    advance: AdvanceTimes,
    /// Turning the window into GPU instances.
    build: Duration,
    instances: usize,
    /// Notes decoded this frame.
    decoded: usize,
}

/// With `KIM_FRAMELOG=path` set, one line per second of average frame numbers.
struct FrameLog {
    file: std::fs::File,
    since: Instant,
    frames: u32,
    update: Duration,
    worst: Duration,
    /// Retain, decode, sort, cull, build.
    parts: [Duration; 5],
    instances: usize,
    decoded: usize,
}

impl FrameLog {
    fn from_env() -> Option<Self> {
        let mut file = std::fs::File::create(std::env::var_os("KIM_FRAMELOG")?).ok()?;
        let _ = writeln!(
            file,
            "fps,update_ms,worst_update_ms,retain_ms,decode_ms,sort_ms,cull_ms,build_ms,instances,new_per_frame"
        );
        Some(Self::new(file))
    }

    fn new(file: std::fs::File) -> Self {
        Self {
            file,
            since: Instant::now(),
            frames: 0,
            update: Duration::ZERO,
            worst: Duration::ZERO,
            parts: [Duration::ZERO; 5],
            instances: 0,
            decoded: 0,
        }
    }

    fn record(&mut self, update: Duration, t: &FrameTimes) {
        self.frames += 1;
        self.update += update;
        self.worst = self.worst.max(update);
        let a = t.advance;
        if update >= Duration::from_millis(30) {
            // A hitch: the second's averages hide it, so say where this frame went.
            let ms = |d: Duration| d.as_secs_f64() * 1e3;
            let _ = writeln!(
                self.file,
                "spike,{:.1},retain {:.1},decode {:.1},sort {:.1},cull {:.1},build {:.1},instances {},decoded {}",
                ms(update), ms(a.retain), ms(a.decode), ms(a.sort), ms(a.cull), ms(t.build), t.instances, t.decoded
            );
        }
        for (sum, d) in self.parts.iter_mut().zip([a.retain, a.decode, a.sort, a.cull, t.build]) {
            *sum += d;
        }
        self.instances = self.instances.max(t.instances);
        self.decoded += t.decoded;
        if self.since.elapsed() >= Duration::from_secs(1) { self.flush(); }
    }

    fn flush(&mut self) {
        if self.frames > 0 {
            let wall = self.since.elapsed();
            let n = self.frames.max(1) as f64;
            let ms = |d: Duration| d.as_secs_f64() * 1e3;
            let _ = writeln!(
                self.file,
                "{:.1},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{},{}",
                self.frames as f64 / wall.as_secs_f64(),
                ms(self.update) / n,
                ms(self.worst),
                ms(self.parts[0]) / n,
                ms(self.parts[1]) / n,
                ms(self.parts[2]) / n,
                ms(self.parts[3]) / n,
                ms(self.parts[4]) / n,
                self.instances,
                (self.decoded as f64 / n) as u64
            );
            let _ = self.file.flush();
            if let Ok(file) = self.file.try_clone() {
                *self = Self::new(file);
            }
        }
    }
}

struct VisualSong {
    data: Arc<SongData>,
    channel_colors: Vec<Color32>,
    track_colors: Vec<Color32>,
    pitch_min: u8,
    pitch_max: u8,
}

impl VisualSong {
    fn new(data: Arc<SongData>) -> Self {
        let (pitch_min, pitch_max) = match data.midi.pitch_range {
            Some((lo, hi)) => (lo.min(DEFAULT_PITCH_MIN), hi.max(DEFAULT_PITCH_MAX)),
            None => (DEFAULT_PITCH_MIN, DEFAULT_PITCH_MAX),
        };

        // Generate 16 colors, one for each MIDI channel
        let colors = generate_palette(16);
        let track_colors = generate_palette(data.midi.source_tracks.max(1));
        Self {
            data,
            channel_colors: colors,
            track_colors,
            pitch_min,
            pitch_max,
        }
    }

    fn color_for(&self, note: &NoteSegment, by_channel: bool) -> Color32 {
        if by_channel {
            if self.channel_colors.is_empty() {
                Color32::from_rgb(120, 180, 255)
            } else {
                let idx = (note.channel as usize) % self.channel_colors.len();
                self.channel_colors[idx]
            }
        } else if self.track_colors.is_empty() {
            Color32::from_rgb(120, 180, 255)
        } else {
            let idx = note.track as usize % self.track_colors.len();
            self.track_colors[idx]
        }
    }
}

/// A note decoded for drawing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NoteSegment {
    start_us: u64,
    end_us: u64,
    track: u16,
    channel: u8,
    pitch: u8,
}

impl NoteSegment {
    /// Decode only removes notes completely covered by a later note. Pixel culling
    /// happens when drawing, without extending spans across track/colour boundaries.
    fn absorb(&self, prev: &mut NoteSegment) -> bool {
        let covers = self.start_us <= prev.start_us && self.end_us >= prev.end_us;
        if covers {
            *prev = NoteSegment { start_us: prev.start_us.min(self.start_us), end_us: prev.end_us.max(self.end_us), ..*self };
        }
        covers
    }
}

/// Push `note`, or merge it into the last note pushed on its key (see `absorb`).
/// `last` holds, per key, the index in `out` of the last note pushed there; only
/// those from `base` on count (earlier ones belong to other tracks).
fn push_culled(out: &mut Vec<NoteSegment>, last: &mut [usize; 128], base: usize, note: NoteSegment) {
    let k = note.pitch as usize & 127;
    let prev = if last[k] >= base { out.get_mut(last[k]) } else { None };
    if !prev.is_some_and(|prev| note.absorb(prev)) {
        last[k] = out.len();
        out.push(note);
    }
}

/// What one worker decoded, over a run of tracks.
struct Decoded {
    notes: Vec<NoteSegment>,
    last: [usize; 128],
    /// Records looked at.
    scanned: usize,
}

impl Decoded {
    fn new(notes: Vec<NoteSegment>) -> Self {
        Self { notes, last: [usize::MAX; 128], scanned: 0 }
    }
}

/// A small max-end tree over runs of notes on one key. It finds the latest note
/// covering a row without walking the whole window, including past long notes.
#[derive(Default)]
struct KeyNotes {
    indices: Vec<usize>,
    starts: Vec<u64>,
    end_times: Vec<u64>,
    ends: Vec<u64>,
    tree: Vec<u64>,
    rows: Vec<RowNotes>,
    row_head: usize,
    row_time: u64,
    row_view: (u32, u64),
    added_start: u64,
}

struct RowNotes {
    lo: u64,
    hi: u64,
    expiry: u64,
    first: usize,
    second: usize,
    rest: Vec<usize>,
}

impl Default for RowNotes {
    fn default() -> Self {
        Self { lo: u64::MAX, hi: 0, expiry: 0, first: usize::MAX, second: usize::MAX, rest: Vec::new() }
    }
}

impl RowNotes {
    fn push(&mut self, i: usize) {
        if self.first == usize::MAX { self.first = i; }
        else if self.second == usize::MAX { self.second = i; }
        else { self.rest.push(i); }
    }
}

/// Track records one frame may decode before the rest waits for the next.
const DECODE_BUDGET: usize = 500_000;

const NOTE_BLOCK: usize = 64;

fn rendered_end(note: &NoteSegment) -> u64 {
    note.end_us.max(note.start_us.saturating_add(100)) // shader's minimum length
}

impl KeyNotes {
    fn push(&mut self, index: usize, note: &NoteSegment) {
        self.added_start = self.added_start.min(note.start_us);
        self.push_time(note);
        self.indices.push(index);
    }

    fn push_time(&mut self, note: &NoteSegment) {
        if self.end_times.len() % NOTE_BLOCK == 0 { self.ends.push(0); }
        let end = self.ends.last_mut().unwrap();
        *end = (*end).max(rendered_end(note));
        self.starts.push(note.start_us);
        self.end_times.push(rendered_end(note));
    }

    fn update_tree(&mut self, first: usize) {
        if self.indices.len() == first { return; }
        let leaves = self.ends.len().next_power_of_two();
        if self.tree.len() < leaves * 2 {
            self.tree = vec![0; leaves * 2];
            self.tree[leaves..leaves + self.ends.len()].copy_from_slice(&self.ends);
            for i in (1..leaves).rev() { self.tree[i] = self.tree[i * 2].max(self.tree[i * 2 + 1]); }
        } else {
            let leaves = self.tree.len() / 2;
            for block in first / NOTE_BLOCK..self.ends.len() {
                let mut i = leaves + block;
                self.tree[i] = self.ends[block];
                while i > 1 {
                    i /= 2;
                    self.tree[i] = self.tree[i * 2].max(self.tree[i * 2 + 1]);
                }
            }
        }
    }

    fn last_block(&self, node: usize, lo: usize, hi: usize, before: usize, time: u64) -> Option<usize> {
        if lo >= before || self.tree[node] < time { return None; }
        if hi - lo == 1 { return Some(lo); }
        let mid = (lo + hi) / 2;
        self.last_block(node * 2 + 1, mid, hi, before, time)
            .or_else(|| self.last_block(node * 2, lo, mid, before, time))
    }

    fn last_block_cert(&self, node: usize, lo: usize, hi: usize, before: usize, time: u64, missed: &mut u64) -> Option<usize> {
        if lo >= before { return None; }
        if self.tree[node] < time { *missed = (*missed).max(self.tree[node]); return None; }
        if hi - lo == 1 { return Some(lo); }
        let mid = (lo + hi) / 2;
        self.last_block_cert(node * 2 + 1, mid, hi, before, time, missed)
            .or_else(|| self.last_block_cert(node * 2, lo, mid, before, time, missed))
    }

    fn covering_cert(&self, mut before: usize, time: u64, missed: &mut u64) -> Option<usize> {
        while before > 0 {
            let block = (before - 1) / NOTE_BLOCK;
            for i in (block * NOTE_BLOCK..before).rev() {
                let end = self.end_times[i];
                if end >= time { return Some(i); }
                *missed = (*missed).max(end);
            }
            let block = self.last_block_cert(1, 0, self.tree.len() / 2, block, time, missed)?;
            before = ((block + 1) * NOTE_BLOCK).min(self.indices.len());
        }
        None
    }

    fn query_row(&self, window: &[NoteSegment], now: u64, window_end: u64, time: u64, guard: u64, row: &mut RowNotes) {
        let lo = time.saturating_sub(guard);
        let hi = time.saturating_add(guard);
        let mut before = self.starts.partition_point(|&start| start <= hi);
        row.lo = if before > 0 { self.starts[before - 1].saturating_sub(guard) } else { 0 };
        row.hi = window_end.saturating_sub(guard);
        if before < self.starts.len() { row.hi = row.hi.min(self.starts[before].saturating_sub(guard)); }
        row.expiry = u64::MAX;
        row.first = usize::MAX;
        row.second = usize::MAX;
        row.rest.clear();
        let mut missed = 0;
        while let Some(p) = self.covering_cert(before, lo, &mut missed) {
            let i = self.indices[p];
            let note = &window[i];
            let end = rendered_end(note);
            row.hi = row.hi.min(end.saturating_add(guard).saturating_add(1));
            if note.start_us < window_end && note.end_us + NOTE_HOLD_MICROS > now {
                row.push(i);
                row.expiry = row.expiry.min(note.end_us + NOTE_HOLD_MICROS);
                if note.start_us <= lo && end >= hi {
                    row.lo = row.lo.max(note.start_us.saturating_add(guard));
                    row.hi = row.hi.min(end.saturating_sub(guard).saturating_add(1));
                    break;
                }
                if note.start_us > lo { row.hi = row.hi.min(note.start_us.saturating_add(guard)); }
                if end < hi { row.lo = row.lo.max(end.saturating_sub(guard).saturating_add(1)); }
            }
            before = p;
        }
        if missed > 0 { row.lo = row.lo.max(missed.saturating_add(guard).saturating_add(1)); }
    }

    fn covering(&self, mut before: usize, time: u64) -> Option<usize> {
        while before > 0 {
            let block = (before - 1) / NOTE_BLOCK;
            for i in (block * NOTE_BLOCK..before).rev() {
                if self.end_times[i] >= time { return Some(i); }
            }
            let block = self.last_block(1, 0, self.tree.len() / 2, block, time)?;
            before = ((block + 1) * NOTE_BLOCK).min(self.indices.len());
        }
        None
    }
}

/// Notes on screen. Each note is decoded from the packed tracks once, when it enters
/// the window, and dropped once it has finished.
struct VisibleNotes {
    time_us: u64,
    /// Per track: position of the first record not yet decoded.
    readers: Vec<TrackReader>,
    window: Vec<NoteSegment>,
    /// Notes decoded last frame (after culling), and records looked at, to decide
    /// whether decoding is worth parallelizing.
    last_batch: usize,
    last_scanned: usize,
    /// Song time decoded up to, and how far one frame may extend it: a burst of millions
    /// of notes is taken over several frames (the far top of the screen fills in late)
    /// instead of stalling one, which would only bring in even more notes the next.
    frontier_us: u64,
    step_us: u64,
    /// Notes in `window` after the last sweep of finished ones, and frames since.
    alive: usize,
    frames_since_sweep: u32,
    keys: [KeyNotes; 128],
    indexed: usize,
    max_len_us: u64,
    total_len_us: u64,
    selected: Vec<usize>,
    selection: Vec<usize>,
    selected_bits: Vec<u64>,
    instance_style: Option<(bool, bool, bool)>,
    revision: u64,
    instance_revision: u64,
    instance_height_time: u32,
    instance_expiry: u64,
    /// Keyboard, row queries, bitset collection, instance packing.
    build_profile: [Duration; 4],
    row_counts: [usize; 2],
    /// Where the last `advance` spent its time (for `FrameLog`).
    timing: AdvanceTimes,
}

impl Default for VisibleNotes {
    fn default() -> Self {
        Self {
            time_us: 0, readers: Vec::new(), window: Vec::new(), last_batch: 0,
            last_scanned: 0, frontier_us: 0, step_us: u64::MAX, alive: 0, frames_since_sweep: 0,
            keys: std::array::from_fn(|_| KeyNotes::default()), indexed: 0, max_len_us: 0, total_len_us: 0,
            selected: Vec::new(), selection: Vec::new(), selected_bits: Vec::new(),
            instance_style: None, revision: 0, instance_revision: 0, instance_height_time: 0, instance_expiry: 0,
            build_profile: [Duration::ZERO; 4],
            row_counts: [0; 2],
            timing: AdvanceTimes::default(),
        }
    }
}

#[derive(Default, Clone, Copy)]
struct AdvanceTimes {
    retain: Duration,
    decode: Duration,
    sort: Duration,
    cull: Duration,
}

impl VisibleNotes {
    fn advance(&mut self, song: &SongData, now_us: u64, window_end: u64, _px_us: u64) {
        let tracks = &song.midi.tracks;
        // Pixel selection is view-dependent; zooming needs no destructive re-decode.
        if now_us < self.time_us || self.readers.len() != tracks.len() {
            *self = VisibleNotes {
                readers: vec![TrackReader::default(); tracks.len()],
                ..Default::default()
            };
        }
        let jumped = now_us - self.time_us > 1_000_000;
        self.time_us = now_us;
        for key in &mut self.keys { key.added_start = u64::MAX; }
        // Finished notes are skipped when drawing, and swept out now and then (a sweep
        // visits every note on screen).
        let t0 = Instant::now();
        self.frames_since_sweep += 1;
        if self.window.len() >= self.alive.max(4096) * 2 || self.frames_since_sweep >= 240 {
            self.window.retain(|n| n.end_us + NOTE_HOLD_MICROS > now_us);
            self.alive = self.window.len();
            self.frames_since_sweep = 0;
            self.keys = std::array::from_fn(|_| KeyNotes::default());
            self.indexed = 0;
            self.max_len_us = 0;
            self.total_len_us = 0;
            self.instance_style = None; // indices changed during compaction
        }
        self.timing.retain = t0.elapsed();
        let t0 = Instant::now();
        let tempo = &song.tempo;
        let now_tick = tempo.time_to_tick(now_us);
        // A fresh start or a seek takes everything at once; otherwise spend about
        // DECODE_BUDGET records a frame, but always reach notes about to sound.
        let reach = if jumped || self.frontier_us == 0 {
            window_end
        } else {
            window_end.min(self.frontier_us.saturating_add(self.step_us)).max((now_us + 100_000).min(window_end))
        };
        let end_tick = tempo.time_to_tick(reach);
        let decode = |track: usize, r: &mut TrackReader, acc: &mut Decoded| {
            let data = song.midi.track(track);
            if r.peek_tick(data).map_or(true, |t| t >= end_tick) {
                return; // nothing new in this track (most of the 60,000 in some files)
            }
            let mut tempo_cache = 0;
            let base = acc.notes.len();
            while let Some((tick, rec)) = r.next_before(data, end_tick) {
                acc.scanned += 1;
                let Record::Note { ch, key, dur, .. } = rec else { continue };
                // Cheap tick check first so fast-forwarding skips finished notes.
                if dur.is_some_and(|d| tick + d < now_tick) {
                    continue;
                }
                let end_us = match dur {
                    Some(d) => tempo.tick_to_time_approx(tick + d, &mut tempo_cache),
                    None => tempo.duration_us(),
                };
                if end_us + NOTE_HOLD_MICROS > now_us {
                    let note = NoteSegment {
                        start_us: tempo.tick_to_time_approx(tick, &mut tempo_cache),
                        end_us,
                        track: song.midi.track_origin[track] as u16,
                        channel: ch,
                        pitch: key,
                    };
                    push_culled(&mut acc.notes, &mut acc.last, base, note);
                }
            }
        };

        let first_new = self.window.len();
        // Seek / big skip / dense passage: decode all tracks in parallel.
        if jumped || self.last_scanned > 50_000 {
            // Each worker takes a run of tracks into one list (in track order).
            let parts: Vec<Decoded> = self
                .readers
                .par_iter_mut()
                .enumerate()
                .fold(|| Decoded::new(Vec::new()), |mut acc, (i, r)| {
                    decode(i, r, &mut acc);
                    acc
                })
                .collect();
            self.last_scanned = parts.iter().map(|p| p.scanned).sum();
            self.window.reserve(parts.iter().map(|p| p.notes.len()).sum());
            for p in parts {
                self.window.extend(p.notes);
            }
        } else {
            let mut acc = Decoded::new(std::mem::take(&mut self.window));
            for (i, r) in self.readers.iter_mut().enumerate() {
                decode(i, r, &mut acc);
            }
            self.last_scanned = acc.scanned;
            self.window = acc.notes;
        }
        self.timing.decode = t0.elapsed();
        let span = reach.saturating_sub(self.frontier_us.max(now_us)).max(1);
        self.step_us = if self.last_scanned > DECODE_BUDGET {
            // Over: the same span of song would cost this much too, so take less of it.
            ((span as u128 * DECODE_BUDGET as u128 / self.last_scanned as u128) as u64).max(2_000)
        } else {
            self.step_us.min(1 << 40).saturating_mul(3) / 2 + 1_000
        };
        self.frontier_us = reach;
        let t0 = Instant::now();
        // Draw order = start time, so later notes cover earlier ones. New notes all start
        // after the existing ones, so sorting just the batch keeps each key in order.
        // Stable: equal starts keep track order (higher track on top). Each track's run is
        // already sorted, so this is a cheap run merge.
        self.last_batch = self.window.len() - first_new;
        if self.last_batch > 50_000 {
            self.window[first_new..].par_sort_by_key(|n| n.start_us);
        } else {
            self.window[first_new..].sort_by_key(|n| n.start_us);
        }
        self.timing.sort = t0.elapsed();
        let t0 = Instant::now();

        // Cull across tracks too (tracks were culled individually while decoding),
        // compacting the batch in place.
        let mut last = [usize::MAX; 128];
        let mut w = first_new;
        for r in first_new..self.window.len() {
            let note = self.window[r];
            let k = note.pitch as usize & 127;
            if last[k] == usize::MAX || !note.absorb(&mut self.window[last[k]]) {
                self.window[w] = note;
                last[k] = w;
                w += 1;
            }
        }
        self.window.truncate(w);
        if w > first_new { self.revision = self.revision.wrapping_add(1); }
        // Index at once, however large: drawing a big unindexed window exactly costs
        // far more (tens of ms a frame, for dozens of frames) than building the index.
        self.index_notes(self.indexed);
        self.timing.cull = t0.elapsed();
    }

    fn index_notes(&mut self, first: usize) {
        self.index_until(first, self.window.len());
    }

    fn index_until(&mut self, first: usize, end: usize) {
        let counts: [usize; 128] = std::array::from_fn(|k| self.keys[k].indices.len());
        if first == 0 {
            self.max_len_us = 0;
            self.total_len_us = 0;
            let mut sizes = [0usize; 128];
            for note in &self.window[..end] { sizes[note.pitch as usize & 127] += 1; }
            for (key, size) in self.keys.iter_mut().zip(sizes) {
                key.indices.reserve(size);
            }
            if end > 50_000 {
                for (i, note) in self.window[..end].iter().enumerate() {
                    self.keys[note.pitch as usize & 127].indices.push(i);
                    self.max_len_us = self.max_len_us.max(note.end_us - note.start_us);
                    self.total_len_us = self.total_len_us.saturating_add(note.end_us - note.start_us);
                }
                let window = &self.window;
                self.keys.par_iter_mut().for_each(|key| {
                    let size = key.indices.len();
                    key.starts.reserve(size);
                    key.end_times.reserve(size);
                    key.ends.reserve(size.div_ceil(NOTE_BLOCK));
                    for p in 0..size { key.push_time(&window[key.indices[p]]); }
                    key.update_tree(0);
                });
                self.indexed = end;
                return;
            }
        }
        if end - first > 20_000 {
            // A dense passage: bucket the new notes by key, then each key takes its own.
            let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); 128];
            for (i, note) in self.window[..end].iter().enumerate().skip(first) {
                buckets[note.pitch as usize & 127].push(i);
                self.max_len_us = self.max_len_us.max(note.end_us - note.start_us);
                self.total_len_us = self.total_len_us.saturating_add(note.end_us - note.start_us);
            }
            let window = &self.window;
            self.keys.par_iter_mut().zip(buckets.par_iter()).zip(counts.par_iter()).for_each(|((key, new), &count)| {
                for &i in new { key.push(i, &window[i]); }
                key.update_tree(count);
            });
            self.indexed = end;
            return;
        }
        for (i, note) in self.window[..end].iter().enumerate().skip(first) {
            self.keys[note.pitch as usize & 127].push(i, note);
            self.max_len_us = self.max_len_us.max(note.end_us - note.start_us);
            self.total_len_us = self.total_len_us.saturating_add(note.end_us - note.start_us);
        }
        for (key, count) in self.keys.iter_mut().zip(counts) { key.update_tree(count); }
        self.indexed = end;
    }

    fn select_pixels(&mut self, now_us: u64, window_end: u64, height_time: f32, height_px: u32) {
        let t0 = Instant::now();
        self.selected.clear();
        if !self.dense(height_time, height_px) {
            self.selected.extend(self.window.iter().enumerate()
                .filter(|(_, n)| n.start_us < window_end && n.end_us + NOTE_HOLD_MICROS > now_us)
                .map(|(i, _)| i));
            return;
        }
        self.selected_bits.resize(self.window.len().div_ceil(64), 0);
        self.selected_bits.fill(0);
        let px_us = height_time as f64 * 1e6 / height_px.max(1) as f64;
        // Include uncertain edge samples as well: float rounding and the GPU's
        // subpixel raster grid must never hide an exposed colour/border pixel.
        let guard = 2 + (((height_time as f64 * 1e6) as u64).saturating_add(self.max_len_us) >> 21)
            + (px_us / 256.0).ceil() as u64;
        let height = height_px.max(1) as usize;
        self.row_counts = [0; 2];
        for key in &mut self.keys {
            if key.indices.is_empty() { continue; }
            let last = *key.indices.last().unwrap();
            if self.window[last].start_us <= now_us && self.window[last].end_us >= window_end.saturating_add(guard) {
                self.selected_bits[last / 64] |= 1 << (last % 64);
                key.rows.clear();
                continue; // top note covers the whole viewport, including its borders
            }
            let view = (height_time.to_bits(), guard);
            let mut rows = std::mem::take(&mut key.rows);
            if rows.len() != height || key.row_view != view || now_us < key.row_time {
                rows.clear();
                rows.resize_with(height, RowNotes::default);
                key.row_head = 0;
            } else {
                let shift = ((now_us - key.row_time) as f64 / px_us).round() as usize;
                if shift >= height {
                    for row in &mut rows { row.hi = 0; }
                    key.row_head = 0;
                } else {
                    key.row_head += shift;
                    if key.row_head >= height { key.row_head -= height; }
                }
            }
            let (first, last) = rows.split_at_mut(key.row_head);
            for (row, cached) in last.iter_mut().chain(first).enumerate() {
                let time = now_us.saturating_add(((row as f64 + 0.5) * px_us) as u64);
                cached.hi = cached.hi.min(key.added_start.saturating_sub(guard));
                if cached.lo <= time && time < cached.hi && now_us < cached.expiry {
                    self.row_counts[0] += 1;
                } else {
                    key.query_row(&self.window, now_us, window_end, time, guard, cached);
                    self.row_counts[1] += 1;
                }
                if cached.first != usize::MAX {
                    let i = cached.first;
                    self.selected_bits[i / 64] |= 1 << (i % 64);
                }
                if cached.second != usize::MAX {
                    let i = cached.second;
                    self.selected_bits[i / 64] |= 1 << (i % 64);
                }
                for &i in &cached.rest { self.selected_bits[i / 64] |= 1 << (i % 64); }
            }
            key.rows = rows;
            key.row_time = now_us;
            key.row_view = view;
            key.added_start = u64::MAX;
        }
        self.build_profile[1] = t0.elapsed();
        let t0 = Instant::now();
        // The bitset deduplicates row winners and emits them in the original draw
        // order without sorting or walking every NoteSegment again.
        for (block, &bits) in self.selected_bits.iter().enumerate() {
            let mut bits = bits;
            while bits != 0 {
                self.selected.push(block * 64 + bits.trailing_zeros() as usize);
                bits &= bits - 1;
            }
        }
        self.build_profile[2] = t0.elapsed();
    }

    fn dense(&self, height_time: f32, height_px: u32) -> bool {
        let key_count = self.keys.iter().filter(|key| !key.indices.is_empty()).count();
        // Query rows when notes compete for them, or long spans cause heavy
        // overdraw. An isolated long note must not slow a sparse window down.
        let overlap = ((height_time as f64 * 1e6) as u64).saturating_mul(key_count as u64)
            .saturating_mul(height_px.max(1) as u64) / 4;
        self.indexed == self.window.len() && self.window.len() > 4096 && (self.window.len() > height_px as usize * key_count * 2
            || self.total_len_us > overlap)
    }
}

/// A byte count with a unit (decimal: kB to PB).
fn si_bytes(b: u64) -> String {
    let (mut v, mut unit) = (b as f64, 0);
    while v >= 1000.0 && unit < 5 {
        v /= 1000.0;
        unit += 1;
    }
    format!("{v:.1} {}", ["B", "kB", "MB", "GB", "TB", "PB"][unit])
}

/// The load's counters (all relaxed atomics the worker bumps once per track): rates are
/// averages since the start, so nothing is sampled.
fn loading_details(ui: &mut egui::Ui, l: &Loading) {
    use std::sync::atomic::Ordering::Relaxed;
    let p = &l.progress;
    let live = p.live_notes.load(Relaxed);
    let (read, midi, notes) = (p.bytes.load(Relaxed), p.midi_bytes.load(Relaxed), p.notes.load(Relaxed) + live);
    let done = p.tracks_done.load(Relaxed);
    let total = p.tracks_total.load(Relaxed).max(done); // (a header can undercount)
    let secs_f = l.started.elapsed().as_secs_f64().max(0.001);
    let mb = |b: u64| format!("{:.1} MB", b as f64 / 1e6);
    let layers = p.layers.lock().map(|s| s.clone()).unwrap_or_default();
    egui::Grid::new("loading_details").num_columns(2).show(ui, |ui| {
        ui.label("Format");
        ui.label(if layers.is_empty() { "mid".to_owned() } else { format!("{layers} › mid") });
        ui.end_row();
        ui.label("Read from disk");
        ui.label(format!("{} of {} ({}/s)", mb(read.min(l.total)), mb(l.total), mb((read as f64 / secs_f) as u64)));
        ui.end_row();
        ui.label("MIDI parsed");
        ui.label(format!("{} ({}/s)", mb(midi), mb((midi as f64 / secs_f) as u64)));
        ui.end_row();
        let scanned = p.scan_done.load(Relaxed);
        if scanned > 0 {
            // The layer is read in full to find the repeated tracks; a track's share of it
            // times the track count is a guess at the whole.
            let unit = p.scan_unit.load(Relaxed);
            let guess = unit * p.tracks_total.load(Relaxed);
            // The speed over the last ~10 s (an average since the start would count the
            // time spent parsing, when nothing was scanned).
            let id = egui::Id::new("scan_samples");
            let mut samples: Vec<(f64, u64)> = ui.data_mut(|d| d.get_temp(id)).unwrap_or_default();
            if samples.last().is_none_or(|s| secs_f - s.0 >= 0.5) {
                samples.push((secs_f, scanned));
            }
            samples.retain(|s| s.0 <= secs_f && secs_f - s.0 <= 10.0);
            let rate = match samples.first() {
                Some(&(t, b)) if secs_f - t > 1.0 => scanned.saturating_sub(b) as f64 / (secs_f - t),
                _ => scanned as f64 / secs_f,
            };
            ui.data_mut(|d| d.insert_temp(id, samples));
            ui.label("Scanning repeats");
            ui.label(if guess > scanned {
                let left = (guess - scanned) as f64 / rate.max(1.0);
                format!("{} of ~{} ({}/s, ~{} left)", si_bytes(scanned), si_bytes(guess), si_bytes(rate as u64), secs(left))
            } else {
                format!("{} ({}/s)", si_bytes(scanned), si_bytes(rate as u64))
            });
            ui.end_row();
        }
        let trusted = p.trusted_tracks.load(Relaxed);
        if trusted > 0 {
            ui.label("Repeats");
            ui.label(format!("{trusted} tracks counted without reading (sizes add up)"));
            ui.end_row();
        }
        ui.label("Tracks");
        ui.label(if total == 0 { "waiting for the header…".to_owned() } else { format!("{done} of {total}") });
        ui.end_row();
        if live > 0 {
            ui.label("Current track");
            ui.label(format!("{} notes found so far", si(live as f64)));
            ui.end_row();
        }
        ui.label("Notes");
        ui.label(format!("{} ({}/s)", si(notes as f64), si(notes as f64 / secs_f)));
        ui.end_row();
    });
}

enum BenchMsg {
    Step(usize, String),
    Done(benchmark::Report),
}

/// A benchmark running on a worker thread, or its result.
struct BenchRun {
    rx: Receiver<BenchMsg>,
    step: usize,
    name: String,
    report: Option<benchmark::Report>,
    started: Instant,
}

/// A file being loaded on a worker thread.
struct Loading {
    name: String,
    rx: Receiver<Result<Arc<SongData>, String>>,
    /// Bytes parsed so far, out of `total`.
    progress: Arc<parser::Progress>,
    total: u64,
    started: Instant,
}

struct AppState {
    telemetry_rx: Receiver<PlayerTelemetry>,
    cmd_tx: Sender<PlayerCommand>,
    stop: Arc<AtomicBool>,
    clock: Arc<PlaybackClock>,
    last_telemetry: PlayerTelemetry,
    current_song: Option<VisualSong>,
    playback_time_us: u64,
    slider_override_us: Option<u64>,
    slider_grabbed: bool,
    note_speed_seconds: f32,
    /// MIDI outputs (choice, display name) and the selected one.
    outputs: Vec<(output::OutputChoice, String)>,
    output_idx: usize,
    /// Max note-ons/s to the synth; `SYNTH_LIMIT_OFF` = no limit.
    synth_limit: f64,
    /// Smart note skipping (off = send every note, for synth stress tests).
    skipping: bool,
    /// Playback speed multiplier.
    tempo: f64,
    log_stats: bool,
    /// Full stats overlay (off = just the note count).
    detailed_stats: bool,
    /// Synth/player/SVMS lines in the detailed overlay.
    synth_stats: bool,
    /// Show the loading stats in a window while a file loads (they're always in the
    /// progress bar's tooltip).
    loading_details: bool,
    /// Read every repeat of a track instead of trusting a pattern that checks out.
    verify_repeats: bool,
    /// The benchmark window (see `benchmark`).
    benchmark: Option<BenchRun>,
    /// Submit through SVMS's native batch API (SVMS only).
    svms_batch: bool,
    loading: Option<Loading>,
    show_settings: bool,
    /// Velocity precision used when loading (127 = exact, 1 = single velocity).
    vel_levels: u8,
    vsync_enabled: bool,
    notes_upwards: bool,
    flat_notes: bool,
    visible: VisibleNotes,
    /// Reused across frames; the GPU callback holds a clone until it renders.
    instances: Arc<Vec<gpu::NoteInstance>>,
    /// Display refresh rate reported by the compositor (for the VSync label).
    refresh_rate_hz: Option<f32>,
    vblank: vsync::VBlank,
    status_message: Option<String>,
    fps: f32,
    last_frame: Option<Instant>,
    #[allow(dead_code)]
    timer_guard: TimerGuard,
    color_by_channel: bool,
    /// Pick by channel or by track from the song (see `colors_by_channel`).
    color_auto: bool,
    /// Notes laid out like the keys (see `key_columns`) rather than an even grid.
    piano_layout: bool,
    frame_times: FrameTimes,
    frame_log: Option<FrameLog>,
    /// Where to seek once the file given on the command line has loaded.
    open_at_us: Option<u64>,
}

impl eframe::App for AppState {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if self.vsync_enabled {
            let (rect, ppp) = ctx.input(|i| {
                let v = i.viewport();
                (v.outer_rect, v.native_pixels_per_point.unwrap_or(1.0))
            });
            let c = rect.map_or(Pos2::ZERO, |r| r.center());
            self.vblank.wait((c.x * ppp) as i32, (c.y * ppp) as i32);
        }

        let frame_start = Instant::now();
        if let Some(prev) = self.last_frame {
            let dt = (frame_start - prev).as_secs_f32();
            if dt > 0.0 {
                let instantaneous = 1.0 / dt;
                self.fps = 0.85 * self.fps + 0.15 * instantaneous;
            }
        }
        self.last_frame = Some(frame_start);

        self.poll_loading();

        while let Ok(mut t) = self.telemetry_rx.try_recv() {
            let duration_cap = t.duration_us.max(1);
            let mut new_time = if t.is_playing {
                self.clock.now_us()
            } else {
                t.playback_time_us
            };
            new_time = new_time.min(duration_cap);
            self.playback_time_us = new_time;
            t.playback_time_us = new_time;
            self.last_telemetry = t;
        }

        let duration_us = self
            .current_song
            .as_ref()
            .map(|s| s.data.tempo.duration_us())
            .unwrap_or(self.last_telemetry.duration_us)
            .max(1);

        let mut current_time_us = if let Some(override_us) = self.slider_override_us {
            override_us
        } else if self.last_telemetry.is_playing {
            self.clock.now_us()
        } else {
            self.playback_time_us
        };
        current_time_us = current_time_us.min(duration_us);

        // Consumed before the widgets run, so a focused button doesn't also react to it.
        if self.current_song.is_some()
            && !ctx.wants_keyboard_input()
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Space))
        {
            self.toggle_play(current_time_us);
        }

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(4.0);
            // Wraps onto a second line instead of clipping when the window is narrow.
            ui.horizontal_wrapped(|ui| {
                self.transport_controls(ui, current_time_us);
                ui.separator();
                wrap_for_slider(ui);
                ui.label("Note speed");
                ui.add(
                    egui::Slider::new(&mut self.note_speed_seconds, 0.01..=12.0)
                        .logarithmic(true)
                        .suffix(" s"),
                )
                .on_hover_text("Seconds a note takes to reach the keyboard");
                ui.separator();
                ui.toggle_value(&mut self.show_settings, "Settings");
            });
            let mut cancel = false;
            if let Some(l) = &self.loading {
                let done = l.progress.bytes.load(std::sync::atomic::Ordering::Relaxed) as f32;
                let frac = (done / l.total.max(1) as f32).min(1.0);
                ui.horizontal(|ui| {
                    ui.label(format!("Loading {}…", l.name));
                    ui.add(
                        egui::ProgressBar::new(frac)
                            .desired_width(220.0)
                            .text(format!("{:.0}% · {}", frac * 100.0, secs(l.started.elapsed().as_secs_f64()))),
                    )
                    .on_hover_ui(|ui| loading_details(ui, l));
                    cancel = ui.button("Cancel").clicked();
                });
                if self.loading_details {
                    egui::Window::new("Loading").collapsible(false).resizable(false).show(ctx, |ui| loading_details(ui, l));
                }
            }
            if cancel {
                self.cancel_loading();
            }
            match (&self.status_message, &self.current_song) {
                (Some(msg), _) => {
                    ui.colored_label(Color32::from_rgb(255, 140, 0), msg);
                }
                (None, Some(song)) => {
                    let name = std::path::Path::new(&song.data.path)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| song.data.path.clone());
                    ui.horizontal(|ui| {
                        ui.add(egui::Label::new(name).sense(Sense::hover()))
                            .on_hover_text(&song.data.path);
                        let (summary, details) = load_info(&song.data);
                        ui.add(egui::Label::new(egui::RichText::new(summary).weak()).truncate().sense(Sense::hover()))
                            .on_hover_text(details);
                    });
                }
                (None, None) => {
                    ui.weak("No file loaded");
                }
            }
            ui.add_space(2.0);
        });

        egui::TopBottomPanel::bottom("seekbar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.monospace(format_timestamp(current_time_us));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.monospace(format_timestamp(duration_us));
                    ui.spacing_mut().slider_width = ui.available_width();
                    let mut secs = current_time_us as f64 / 1_000_000.0;
                    let has_song = self.current_song.is_some();
                    let response = ui.add_enabled(
                        has_song,
                        egui::Slider::new(&mut secs, 0.0..=duration_us as f64 / 1_000_000.0)
                            .show_value(false),
                    );
                    if response.changed() {
                        self.slider_override_us = Some((secs.max(0.0) * 1_000_000.0) as u64);
                        self.slider_grabbed = true;
                    }
                    // Seek once the slider is let go (or right away for clicks and arrow keys).
                    if self.slider_grabbed && !response.is_pointer_button_down_on() {
                        if let Some(target) = self.slider_override_us.take() {
                            let target = target.min(duration_us);
                            let _ = self.cmd_tx.send(PlayerCommand::Seek(target));
                            self.playback_time_us = target;
                            if self.last_telemetry.is_playing {
                                self.clock.set_running(target);
                            } else {
                                self.clock.set_paused(target);
                            }
                        }
                        self.slider_grabbed = false;
                    }
                    if let Some(t) = self.slider_override_us {
                        current_time_us = t.min(duration_us);
                    }
                });
            });
            ui.add_space(4.0);
        });

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ctx, |ui| {
                draw_piano_roll(
                    ui,
                    frame,
                    self.current_song.as_ref(),
                    current_time_us,
                    duration_us,
                    &self.last_telemetry,
                    self.fps,
                    self.note_speed_seconds,
                    self.notes_upwards,
                    self.flat_notes,
                    self.colors_by_channel(),
                    self.piano_layout,
                    (self.detailed_stats, self.synth_stats),
                    &mut self.visible,
                    &mut self.instances,
                    &mut self.frame_times,
                );
            });

        self.settings_window(ctx);
        self.benchmark_window(ctx);

        if let Some(log) = &mut self.frame_log {
            log.record(frame_start.elapsed(), &self.frame_times);
        }

        // Always keep egui's animation loop active.
        ctx.request_repaint();
    }

    fn on_exit(&mut self) {
        self.save_settings();
        let _ = self.cmd_tx.send(PlayerCommand::Stop);
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl AppState {
    fn reset_visible_indices(&mut self) {
        self.visible = VisibleNotes::default();
    }

    /// What to color notes by: the setting, or for Auto, channels when the file has few
    /// tracks that use several channels between them (or one track), else tracks
    /// (a file of many tracks, or tracks that are its parts on one channel).
    fn colors_by_channel(&self) -> bool {
        match (&self.current_song, self.color_auto) {
            (Some(song), true) => {
                let m = &song.data.midi;
                m.note_tracks < 2 || (m.note_tracks <= 16 && m.channels_used.count_ones() >= 2)
            }
            _ => self.color_by_channel,
        }
    }

    fn toggle_play(&mut self, current_time_us: u64) {
        self.playback_time_us = current_time_us;
        if self.last_telemetry.is_playing {
            let _ = self.cmd_tx.send(PlayerCommand::Pause);
            self.last_telemetry.is_playing = false;
            self.clock.set_paused(current_time_us);
        } else {
            let _ = self.cmd_tx.send(PlayerCommand::Resume);
            self.last_telemetry.is_playing = true;
            self.clock.set_running(current_time_us);
        }
    }

    fn transport_controls(&mut self, ui: &mut egui::Ui, current_time_us: u64) {
        if ui.button("Open MIDI…").clicked() {
            self.open_file();
        }

        let has_song = self.current_song.is_some();
        let label = if self.last_telemetry.is_playing { "⏸ Pause" } else { "▶ Play" };
        if ui.add_enabled(has_song, egui::Button::new(label)).clicked() {
            self.toggle_play(current_time_us);
        }

        if ui
            .add_enabled(has_song, egui::Button::new("Unload"))
            .on_hover_text("Close the song and free its memory")
            .clicked()
        {
            self.slider_override_us = None;
            self.slider_grabbed = false;
            self.playback_time_us = 0;
            self.last_telemetry.is_playing = false;
            self.current_song = None;
            self.reset_visible_indices();
            self.clock.reset();
            let _ = self.cmd_tx.send(PlayerCommand::Stop);
        }

        wrap_for_slider(ui);
        ui.label("Tempo");
        let tempo = ui
            .add(
                egui::Slider::new(&mut self.tempo, 0.1..=20.0)
                    .logarithmic(true)
                    .custom_formatter(|v, _| format!("{v:.2}×")),
            )
            .on_hover_text("Playback speed. Faster = more notes per second for the synth. Double-click to reset.");
        if tempo.double_clicked() {
            self.tempo = 1.0;
        }
        if tempo.changed() || tempo.double_clicked() {
            self.clock.set_speed(self.tempo);
        }
    }

    fn start_benchmark(&mut self, ctx: &egui::Context) {
        let (tx, rx) = crossbeam_channel::unbounded();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let report = benchmark::run(benchmark::Scale::FULL, |i, name| {
                let _ = tx.send(BenchMsg::Step(i, name.to_owned()));
                ctx.request_repaint();
            });
            let _ = tx.send(BenchMsg::Done(report));
            ctx.request_repaint();
        });
        self.benchmark = Some(BenchRun { rx, step: 0, name: String::new(), report: None, started: Instant::now() });
    }

    fn benchmark_window(&mut self, ctx: &egui::Context) {
        let Some(b) = &mut self.benchmark else { return };
        while let Ok(m) = b.rx.try_recv() {
            match m {
                BenchMsg::Step(i, name) => (b.step, b.name) = (i, name),
                BenchMsg::Done(r) => b.report = Some(r),
            }
        }
        let mut open = true;
        egui::Window::new("Benchmark")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| match &b.report {
                None => {
                    ui.label(format!("Test {} of {}: {}", b.step + 1, benchmark::REFERENCE.len(), b.name));
                    ui.label(secs(b.started.elapsed().as_secs_f64()));
                    ui.add_space(6.0);
                    ui.weak("Hands off the mouse. Every program left open costs points.");
                    ctx.request_repaint_after(Duration::from_millis(250));
                }
                Some(r) => {
                    ui.vertical_centered(|ui| {
                        ui.label(egui::RichText::new(format!("{:.0}", r.score)).size(64.0).strong());
                        ui.label(format!("points · benchmark v{}", benchmark::VERSION));
                    });
                    ui.add_space(8.0);
                    egui::Grid::new("benchmark_results").num_columns(3).spacing([16.0, 4.0]).show(ui, |ui| {
                        for t in &r.tests {
                            ui.label(t.name);
                            ui.label(format!("{:.2} {}", t.rate, t.unit));
                            ui.label(format!("{:.0}", t.points));
                            ui.end_row();
                        }
                    });
                    ui.add_space(8.0);
                    ui.weak(format!("{} · {} threads · {:.0} GB RAM · {}", r.cpu, r.threads, r.ram_gb, secs(r.secs)));
                    ui.add_space(4.0);
                    ui.label(benchmark::nag(r));
                    if ui.button("Copy to share").clicked() {
                        ctx.copy_text(r.line());
                    }
                }
            });
        if !open {
            self.benchmark = None;
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        let mut start_benchmark = false;
        let mut open = self.show_settings;
        egui::Window::new("Settings")
            .open(&mut open)
            // Top right, clear of the stats overlay (still draggable).
            .pivot(egui::Align2::RIGHT_TOP)
            .default_pos(ctx.screen_rect().right_top() + Vec2::new(-12.0, 64.0))
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.heading("Display");
                egui::Grid::new("display").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                    ui.label("Notes");
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut self.notes_upwards, false, "Fall");
                        ui.selectable_value(&mut self.notes_upwards, true, "Rise");
                    });
                    ui.end_row();

                    ui.label("Layout");
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut self.piano_layout, false, "Even").on_hover_text(
                            "Every key the same width",
                        );
                        ui.selectable_value(&mut self.piano_layout, true, "Piano").on_hover_text(
                            "Like the keys: sharps narrower, drawn over the naturals",
                        );
                    });
                    ui.end_row();

                    ui.label("Shading");
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut self.flat_notes, false, "Shaded");
                        ui.selectable_value(&mut self.flat_notes, true, "Flat");
                    });
                    ui.end_row();

                    ui.label("Colors");
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut self.color_auto, true, "Auto").on_hover_text(
                            "By channel if a few tracks use several channels, else by track",
                        );
                        if ui.selectable_label(!self.color_auto && self.color_by_channel, "By channel").clicked() {
                            (self.color_auto, self.color_by_channel) = (false, true);
                        }
                        if ui.selectable_label(!self.color_auto && !self.color_by_channel, "By track").clicked() {
                            (self.color_auto, self.color_by_channel) = (false, false);
                        }
                    });
                    ui.end_row();

                    ui.label("VSync");
                    let vsync_label = match self.refresh_rate_hz {
                        Some(hz) => format!("Sync to display ({hz:.0} Hz)"),
                        None => "Sync to display".to_owned(),
                    };
                    ui.checkbox(&mut self.vsync_enabled, vsync_label);
                    ui.end_row();
                });

                ui.add_space(8.0);
                ui.heading("Audio");
                egui::Grid::new("audio").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                    ui.label("Output");
                    let selected =
                        self.outputs.get(self.output_idx).map_or("No MIDI output", |o| o.1.as_str());
                    let mut picked = None;
                    egui::ComboBox::from_id_salt("output")
                        .selected_text(selected)
                        .show_ui(ui, |ui| {
                            for (i, (_, name)) in self.outputs.iter().enumerate() {
                                if ui.selectable_label(i == self.output_idx, name).clicked() {
                                    picked = Some(i);
                                }
                            }
                        });
                    if let Some(i) = picked.filter(|&i| i != self.output_idx) {
                        self.output_idx = i;
                        let _ = self.cmd_tx.send(PlayerCommand::SetOutput(self.outputs[i].0.clone()));
                    }
                    ui.end_row();

                    ui.label("Skip notes");
                    if ui
                        .checkbox(&mut self.skipping, "When the synth falls behind")
                        .on_hover_text(
                            "Merge rapid re-strikes so playback stays in time when the synth \
                             can't keep up. Turn off to stress-test a synth: every note is sent, \
                             and Lag shows how far behind it falls.",
                        )
                        .changed()
                    {
                        let _ = self.cmd_tx.send(PlayerCommand::SetSkipping(self.skipping));
                    }
                    ui.end_row();

                    ui.label("Synth limit");
                    let response = ui
                        .add(
                            egui::Slider::new(&mut self.synth_limit, 1_000.0..=SYNTH_LIMIT_OFF)
                                .logarithmic(true)
                                .custom_formatter(|v, _| match v {
                                    v if v >= SYNTH_LIMIT_OFF => "off".to_owned(),
                                    v if v >= 1e6 => format!("{:.1}M notes/s", v / 1e6),
                                    v => format!("{:.0}k notes/s", v / 1e3),
                                }),
                        )
                        .on_hover_text(
                            "Max notes per second sent to the synth. Beyond it, rapid re-strikes \
                             of the same key are merged into the note already sounding.",
                        );
                    if response.changed() {
                        let limit =
                            if self.synth_limit >= SYNTH_LIMIT_OFF { 0 } else { self.synth_limit as u32 };
                        let _ = self.cmd_tx.send(PlayerCommand::SetSynthLimit(limit));
                    }
                    ui.end_row();

                    ui.label("SVMS submit");
                    if ui
                        .checkbox(&mut self.svms_batch, "Batch API")
                        .on_hover_text(
                            "SuperVirtualMIDISynth only: send each tick's messages in one \
                             send_short_batch call instead of one SendDirectData per message. \
                             Compare ns/msg with it on and off.",
                        )
                        .changed()
                    {
                        let _ = self.cmd_tx.send(PlayerCommand::SetSvmsBatch(self.svms_batch));
                    }
                    ui.end_row();

                    ui.label("Stats overlay");
                    ui.checkbox(&mut self.detailed_stats, "Detailed (otherwise note count only)");
                    ui.end_row();

                    ui.label("");
                    ui.add_enabled(
                        self.detailed_stats,
                        egui::Checkbox::new(&mut self.synth_stats, "Synth / player / SVMS timing"),
                    );
                    ui.end_row();

                    ui.label("Stats log");
                    if ui
                        .checkbox(&mut self.log_stats, "Write per-second stats to CSV")
                        .on_hover_text(
                            "msg/s, ns/msg, busy %, worst batch, lag, NPS, polyphony… one row per \
                             second, in a `stats` folder next to the player. With SVMS, every \
                             audio callback also goes to a matching _svms.csv.",
                        )
                        .changed()
                    {
                        let _ = self.cmd_tx.send(PlayerCommand::SetStatsLog(self.log_stats));
                    }
                    ui.end_row();
                    if let Some(path) = &self.last_telemetry.log_path {
                        ui.label("");
                        ui.weak(path);
                        ui.end_row();
                    }
                });

                ui.add_space(8.0);
                ui.heading("Loading");
                egui::Grid::new("loading").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                    ui.label("Velocity");
                    ui.add(egui::Slider::new(&mut self.vel_levels, 1..=127).custom_formatter(|v, _| {
                        match v as u8 {
                            127 => "exact".to_owned(),
                            1 => "single".to_owned(),
                            n => format!("{n} levels"),
                        }
                    }))
                    .on_hover_text(
                        "Velocity precision for files opened from now on. Fewer levels means \
                         more notes repeat their track's velocity, which packs them smaller in \
                         memory. Exact keeps every velocity; single plays every note at full \
                         velocity.",
                    );
                    ui.end_row();
                    ui.label("Repeats");
                    ui.checkbox(&mut self.verify_repeats, "Read every repeated track")
                        .on_hover_text(
                            "A file of one track repeated thousands of times (in xz) is loaded by reading \
                             a few copies and counting the rest, if the sizes add up to whole copies. \
                             Tick this to read every copy instead: exact, but a huge file can take hours.",
                        );
                    ui.end_row();
                    ui.label("Details");
                    ui.checkbox(&mut self.loading_details, "Show a stats window while loading")
                        .on_hover_text("Hovering the progress bar shows the same stats.");
                    ui.end_row();
                });
                ui.add_space(8.0);
                ui.heading("Benchmark");
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(self.benchmark.is_none(), |ui| {
                        if ui.button("Run (~1.5 min)").clicked() {
                            start_benchmark = true;
                        }
                    });
                })
                .response
                .on_hover_text(
                    "Times loading and playback code on made-up data and scores it. Close other \
                     programs first, and pause playback.",
                );
            });
        if start_benchmark {
            self.start_benchmark(ctx);
        }
        if self.show_settings && !open {
            self.save_settings();
        }
        self.show_settings = open;
    }

    /// Remember settings between runs (see `load_settings`).
    fn save_settings(&self) {
        let output = self.outputs.get(self.output_idx).map_or("", |o| o.1.as_str());
        let text = format!(
            "note_speed={}\nnotes_rise={}\nflat_notes={}\ncolor_by_channel={}\ncolor_auto={}\npiano_layout={}\nvsync={}\n\
             output={output}\nskip_notes={}\nsynth_limit={}\nvelocity_levels={}\nsvms_batch={}\ndetailed_stats={}\nsynth_stats={}\nloading_details={}\nverify_repeats={}\n",
            self.note_speed_seconds,
            self.notes_upwards,
            self.flat_notes,
            self.color_by_channel,
            self.color_auto,
            self.piano_layout,
            self.vsync_enabled,
            self.skipping,
            self.synth_limit,
            self.vel_levels,
            self.svms_batch,
            self.detailed_stats,
            self.synth_stats,
            self.loading_details,
            self.verify_repeats,
        );
        if let Some(path) = settings_path() {
            let _ = std::fs::write(path, text);
        }
    }

    fn open_file(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter(
                "MIDI file or archive",
                &["mid", "midi", "zip", "7z", "rar", "xz", "gz", "zst", "tar", "tgz", "txz"],
            )
            .add_filter("All files", &["*"])
            .pick_file()
        else {
            return;
        };
        self.start_loading(path);
    }

    /// Stop the load in progress, if any: its worker fails at its next read.
    fn cancel_loading(&mut self) {
        if let Some(l) = self.loading.take() {
            l.progress.cancel();
        }
    }

    /// Load `path` on a worker thread; `poll_loading` picks up the result.
    fn start_loading(&mut self, path: std::path::PathBuf) {
        let name = path
            .file_name()
            .map_or_else(|| path.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned());
        let total = std::fs::metadata(&path).map_or(1, |m| m.len());
        self.cancel_loading();
        let progress = Arc::new(parser::Progress::default());
        progress.verify_all.store(self.verify_repeats, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (vel_levels, p) = (self.vel_levels, progress.clone());
        std::thread::spawn(move || {
            let _ = tx.send(SongData::load(&path, vel_levels, &p).map_err(|e| e.to_string()));
        });
        self.status_message = None;
        self.loading = Some(Loading {
            name,
            rx,
            progress,
            total,
            started: Instant::now(),
        });
    }

    fn poll_loading(&mut self) {
        let Some(l) = &self.loading else { return };
        let result = match l.rx.try_recv() {
            Ok(result) => result,
            Err(crossbeam_channel::TryRecvError::Empty) => return,
            Err(crossbeam_channel::TryRecvError::Disconnected) => Err("loader thread died".into()),
        };
        self.loading = None;
        match result {
            Ok(song) => {
                let duration = song.tempo.duration_us();
                // Reset the clock before Play so it can't override the player's start.
                self.clock.set_paused(0);
                let _ = self.cmd_tx.send(PlayerCommand::Play(song.clone()));
                if let Some(us) = self.open_at_us.take() {
                    let _ = self.cmd_tx.send(PlayerCommand::Seek(us));
                }

                self.last_telemetry.midi_path = song.path.clone();
                self.last_telemetry.duration_us = duration;
                self.last_telemetry.playback_time_us = 0;
                self.last_telemetry.is_playing = true;
                self.current_song = Some(VisualSong::new(song));
                self.playback_time_us = 0;
                self.slider_override_us = None;
                self.slider_grabbed = false;
                self.status_message = None;
                self.reset_visible_indices();
            }
            Err(e) => {
                self.status_message = Some(format!("Failed to load MIDI: {}", e));
            }
        }
    }
}


fn draw_piano_roll(
    ui: &mut egui::Ui,
    frame: &eframe::Frame,
    song: Option<&VisualSong>,
    now_us: u64,
    duration_us: u64,
    telemetry: &PlayerTelemetry,
    fps: f32,
    note_speed_seconds: f32,
    notes_upwards: bool,
    flat_notes: bool,
    color_by_channel: bool,
    piano_layout: bool,
    (detailed_stats, synth_stats): (bool, bool),
    visible: &mut VisibleNotes,
    instances: &mut Arc<Vec<gpu::NoteInstance>>,
    times: &mut FrameTimes,
) {
    let (response, painter) = ui.allocate_painter(ui.available_size(), Sense::hover());
    let rect = response.rect;

    let keyboard_height = (rect.height() * 0.12).clamp(36.0, 120.0);
    let note_area = Rect::from_min_max(
        Pos2::new(rect.left(), rect.top()),
        Pos2::new(rect.right(), rect.bottom() - keyboard_height),
    );
    let keyboard_rect = Rect::from_min_max(
        Pos2::new(rect.left(), rect.bottom() - keyboard_height),
        Pos2::new(rect.right(), rect.bottom()),
    );

    painter.rect_filled(rect, CornerRadius::ZERO, Color32::from_rgb(10, 10, 12));
    painter.rect_filled(note_area, CornerRadius::ZERO, Color32::from_rgb(20, 20, 24));
    painter.rect_filled(
        keyboard_rect,
        CornerRadius::ZERO,
        Color32::from_rgb(235, 235, 240),
    );

    let speed = note_speed_seconds.clamp(0.01, 15.0);
    let future_us = (speed * 1_000_000.0).max(1.0) as u64;
    let window_end = now_us
        .saturating_add(future_us)
        .min(duration_us.max(now_us));

    let (pitch_min, pitch_max) = if let Some(song) = song {
        (song.pitch_min, song.pitch_max)
    } else {
        (DEFAULT_PITCH_MIN, DEFAULT_PITCH_MAX)
    };

    let pitch_span_i = (pitch_max as i32 - pitch_min as i32 + 1).max(1);

    let mut active_keys = [false; 128];
    let mut active_colors = [Color32::TRANSPARENT; 128];
    let mut gpu_key_positions = [gpu::KeyPosition {
        left: 0.0,
        right: 0.0,
        _pad0: 0.0,
        _pad1: 0.0,
    }; 256];

    let columns = key_columns(piano_layout, pitch_min, pitch_max);
    if pitch_span_i > 0 {
        for pitch in pitch_min..=pitch_max {
            let (left, right) = columns[pitch as usize];
            gpu_key_positions[pitch as usize] = gpu::KeyPosition {
                left,
                right,
                _pad0: 0.0,
                _pad1: 0.0,
            };
        }
    }

    painter.line_segment(
        [
            Pos2::new(note_area.left(), note_area.bottom()),
            Pos2::new(note_area.right(), note_area.bottom()),
        ],
        Stroke::new(1.0_f32, Color32::from_rgb(200, 200, 210)),
    );

    if let Some(song) = song {
        let px_us = future_us as f32 / (note_area.height() * ui.ctx().pixels_per_point()).max(1.0);
        visible.advance(&song.data, now_us, window_end, px_us as u64);
        times.advance = visible.timing;
        times.decoded = visible.last_batch;
        let t0 = Instant::now();

        (active_keys, active_colors) = build_note_instances(
            song, visible, now_us, window_end, speed,
            ((note_area.bottom() * ui.ctx().pixels_per_point()).round()
                - (note_area.top() * ui.ctx().pixels_per_point()).round()).max(1.0) as u32,
            color_by_channel, piano_layout, instances,
        );

        times.build = t0.elapsed();
        times.instances = instances.len();
        if let Some(state) = frame.wgpu_render_state() {
            if !instances.is_empty() {
                painter.add(gpu::NotesCallback::new(
                    note_area,
                    state.target_format,
                    instances.clone(),
                    gpu_key_positions,
                    speed,
                    now_us,
                    if notes_upwards { 1.0 } else { -1.0 },
                    flat_notes,
                ));
            }
        }
    } else {
        painter.text(
            note_area.center(),
            Align2::CENTER_CENTER,
            "Load a MIDI file to start playback",
            egui::FontId::proportional(18.0),
            Color32::from_rgb(190, 190, 200),
        );
    }

    draw_keyboard(
        &painter,
        keyboard_rect,
        pitch_min,
        pitch_max,
        piano_layout.then_some(&columns),
        &active_keys,
        &active_colors,
    );

    let mut stats_text = if detailed_stats {
        format!(
            "Time: {}/{}
Notes: {}
NPS: {}
Skipped: {}/s
Poly: {} (max {})
FPS: {:>4.1}",
            format_timestamp(now_us.min(duration_us)),
            format_timestamp(duration_us),
            telemetry.notes_played,
            telemetry.nps,
            telemetry.skipped_nps,
            telemetry.polyphony,
            telemetry.peak_polyphony,
            fps,
        )
    } else {
        format!("Notes: {}", telemetry.notes_played)
    };
    if detailed_stats && synth_stats {
        stats_text += &format!(
            "

Synth:  {} msg/s, {:.0} ns/msg ({})
        busy {:.0}%, worst batch {} us
             Player: busy {:.0}%, {} ticks/s
Lag:    {:.1} ms",
            si(telemetry.stats.msgs_per_sec as f64),
            telemetry.stats.synth_ns_per_msg,
            if telemetry.stats.batched { "batch call" } else { "per message" },
            telemetry.stats.synth_busy * 100.0,
            telemetry.stats.worst_batch_us,
            telemetry.stats.sched_busy * 100.0,
            si(telemetry.stats.ticks_per_sec as f64),
            telemetry.lag_us as f64 / 1000.0,
        );
        if let Some(s) = &telemetry.svms {
            stats_text += &svms_text(s);
        }
    }
    let font_id = egui::FontId::monospace(14.0);
    let (stats_galley, stats_size) = ui.ctx().fonts(|fonts| {
        let galley = fonts.layout_no_wrap(
            stats_text.clone(),
            font_id.clone(),
            Color32::from_rgb(225, 225, 230),
        );
        (galley.clone(), galley.size())
    });
    let stats_bg = Rect::from_min_size(
        note_area.left_top() + Vec2::new(6.0, 6.0),
        stats_size + Vec2::new(12.0, 12.0),
    );
    painter.rect_filled(
        stats_bg,
        CornerRadius::same(6),
        Color32::from_rgba_unmultiplied(12, 12, 16, 210),
    );
    painter.galley(
        stats_bg.min + Vec2::new(6.0, 6.0),
        stats_galley,
        Color32::from_rgb(50, 50, 70),
    );
}

/// Each key's column across the notes area, as (left, right) fractions of its width.
/// Even: one equal column per semitone. Piano: as in Piano From Above, white keys all
/// the same width, sharps narrower and centred on the line between their neighbours
/// (C#, F# nudged left and D#, A# right, as on a real keyboard), so a sharp's column
/// overlaps the naturals beside it.
fn key_columns(piano: bool, pitch_min: u8, pitch_max: u8) -> [(f32, f32); 128] {
    const SHARP: f32 = 0.65; // a sharp's width as a fraction of a white key's
    let mut out = [(0.0, 0.0); 128];
    if pitch_min > pitch_max {
        return out;
    }
    if !piano {
        let span = (pitch_max - pitch_min + 1) as f32;
        for p in pitch_min..=pitch_max {
            let i = (p - pitch_min) as f32;
            out[p as usize] = (i / span, (i + 1.0) / span);
        }
        return out;
    }
    let whites_below = |p: u8| (0..p).filter(|&q| !is_black_key(q)).count() as f32;
    let first_sharp = is_black_key(pitch_min);
    // Room for half a sharp past each end if that end is a sharp.
    let buffer = (first_sharp as u8 + is_black_key(pitch_max) as u8) as f32 * SHARP / 2.0;
    let whites = whites_below(pitch_max + 1) - whites_below(pitch_min);
    let white_cx = 1.0 / (whites + buffer);
    for p in pitch_min..=pitch_max {
        let index = whites_below(p) - whites_below(pitch_min);
        let (offset, width) = if is_black_key(p) {
            let nudge = match p % 12 {
                1 | 6 => -SHARP / 5.0,
                3 | 10 => SHARP / 5.0,
                _ => 0.0,
            };
            ((first_sharp as u8 as f32 - 1.0) * SHARP / 2.0 + nudge, SHARP)
        } else {
            (first_sharp as u8 as f32 * SHARP / 2.0, 1.0)
        };
        let left = white_cx * (index + offset);
        out[p as usize] = (left, left + white_cx * width);
    }
    out
}

fn draw_keyboard(
    painter: &egui::Painter,
    rect: Rect,
    pitch_min: u8,
    pitch_max: u8,
    // Where the notes' columns are, when they follow the keys (see `key_columns`);
    // otherwise the notes are on an even grid and the keys are laid out on their own.
    columns: Option<&[(f32, f32); 128]>,
    active_keys: &[bool; 128],
    active_colors: &[Color32; 128],
) {
    if pitch_min > pitch_max {
        return;
    }

    let white_idle = Color32::from_rgb(245, 245, 250);
    let white_active = Color32::from_rgb(255, 220, 140);
    let black_idle = Color32::from_rgb(25, 25, 30);
    let black_active = Color32::from_rgb(255, 160, 100);

    // --- Strategy: Calculate layout based on white keys.
    // We ignore the passed `_key_width` as it's based on a semitone-grid
    // for the piano roll, not a realistic keyboard layout.

    // White-key index per pitch (None for black keys and out-of-range pitches).
    let mut white_key_positions = [None::<u32>; 128];
    let mut white_key_count = 0;

    // 1. Find all white keys in range and count them.
    for pitch in pitch_min..=pitch_max {
        if !is_black_key(pitch) {
            white_key_positions[pitch as usize] = Some(white_key_count);
            white_key_count += 1;
        }
    }

    if white_key_count == 0 {
        return; // No white keys in range, nothing to draw.
    }

    let white_key_width = rect.width() / white_key_count as f32;
    let black_key_width = white_key_width * 0.7; // 70% of a white key's width
    let black_key_height = rect.height() * 0.65; // 65% of the keyboard height, for overlap

    // --- Draw white keys first (bottom layer)
    for (idx, index) in white_key_positions
        .iter()
        .enumerate()
        .filter_map(|(p, i)| Some((p, (*i)?)))
    {
        let (left, right) = match columns {
            Some(c) => (rect.left() + c[idx].0 * rect.width(), rect.left() + c[idx].1 * rect.width()),
            None => {
                let left = rect.left() + (index as f32 * white_key_width);
                (left, left + white_key_width)
            }
        };
        let key_rect =
            Rect::from_min_max(Pos2::new(left, rect.top()), Pos2::new(right, rect.bottom()));

        let color = if active_keys[idx] {
            let channel_color = active_colors[idx]; // This is now a channel color
            if channel_color == Color32::TRANSPARENT {
                white_active
            } else {
                channel_color
            }
        } else {
            white_idle
        };

        painter.rect_filled(key_rect, CornerRadius::same(1), color);
        painter.rect_stroke(
            key_rect,
            CornerRadius::same(2),
            Stroke::new(1.0_f32, Color32::from_rgb(150, 150, 160)),
            StrokeKind::Inside,
        );
    }

    // --- Draw black keys on top layer
    for pitch in pitch_min..=pitch_max {
        if is_black_key(pitch) {
            // Find the white key *before* this black key.
            let prev_white_key_pitch = pitch.saturating_sub(1);

            // Check if the previous white key is in our visible range (when the notes
            // follow the keys, a black key at the edge is drawn too).
            if columns.is_some() || white_key_positions[prev_white_key_pitch as usize].is_some() {
                let (left, right) = match columns {
                    Some(c) => (rect.left() + c[pitch as usize].0 * rect.width(), rect.left() + c[pitch as usize].1 * rect.width()),
                    None => {
                        // Center the black key on the dividing line between the two white keys
                        let prev_index = white_key_positions[prev_white_key_pitch as usize].unwrap_or(0);
                        let center_x = rect.left() + (prev_index as f32 + 1.0) * white_key_width;
                        let left = center_x - (black_key_width / 2.0);
                        (left, left + black_key_width)
                    }
                };

                let key_rect = Rect::from_min_max(
                    Pos2::new(left, rect.top()),
                    Pos2::new(right, rect.top() + black_key_height),
                );

                let idx = pitch as usize;
                let color = if active_keys[idx] {
                    let channel_color = active_colors[idx]; // This is now a channel color
                    if channel_color == Color32::TRANSPARENT {
                        black_active
                    } else {
                        channel_color
                    }
                } else {
                    black_idle
                };

                painter.rect_filled(key_rect, CornerRadius::same(1), color);
                // Add a border to black keys
                painter.rect_stroke(
                    key_rect,
                    CornerRadius::same(1),
                    Stroke::new(1.0_f32, Color32::from_rgb(80, 80, 80)),
                    StrokeKind::Inside,
                );
            }
        }
    }
}

fn is_black_key(pitch: u8) -> bool {
    matches!(pitch % 12, 1 | 3 | 6 | 8 | 10)
}

/// `settings.txt` next to the player: `key=value` lines.
fn settings_path() -> Option<std::path::PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join("settings.txt"))
}

fn load_settings() -> HashMap<String, String> {
    let text = settings_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect()
}

fn setting<T: std::str::FromStr>(saved: &HashMap<String, String>, key: &str) -> Option<T> {
    saved.get(key)?.parse().ok()
}

/// In a wrapping row, start a new line if a label + slider won't fit (egui doesn't
/// wrap a slider as one unit, so it would get clipped at the window edge).
fn wrap_for_slider(ui: &mut egui::Ui) {
    let needed = ui.spacing().slider_width + 180.0;
    if ui.available_size_before_wrap().x < needed {
        ui.end_row();
    }
}

/// One-line load summary plus a per-phase breakdown (for the hover).
fn load_info(song: &SongData) -> (String, String) {
    let m = &song.midi;
    let notes = m.note_count as f64;
    let parse = m.parse_time.as_secs_f64().max(1e-9);
    let total = song.load_time.as_secs_f64();
    let packed = m.packed_bytes() as f64;
    let summary = format!(
        "{} notes · loaded in {} ({} notes/s) · {} packed ({:.2} B/note)",
        si(notes),
        secs(total),
        si(notes / parse),
        bytes(packed),
        packed / notes.max(1.0),
    );
    let file = match &m.source {
        Some((format, size)) => format!(
            "{} (unpacked from a {} {format})",
            bytes(m.file_bytes as f64),
            bytes(*size as f64)
        ),
        None => bytes(m.file_bytes as f64),
    };
    let parse_label = if m.source.is_some() { "Decompress, parse & pack" } else { "Parse & pack" };
    let st = &m.pack_stats;
    let pct = |n: u64| 100.0 * n as f64 / st.notes.max(1) as f64;
    let details = format!(
        "Tracks: {}{}\nFile: {file}\n\nTotal load: {}\n  {parse_label}: {} ({} notes/s, {}/s)\n  \
         Open, index & tempo map: {}\n\nPacked: {} ({:.2} B/note, {:.2}:1 vs file)\n  \
         repeats stored once: {} before, {:.1}x smaller ({} distinct chunks)\n\n\
         Note encoding (a note is 2 bytes unless it needs extras):\n  \
         2-byte notes: {:.1}%\n  new velocity: {:.1}% (+1 byte)\n  \
         new duration: {:.1}% (+{:.2} bytes avg)\n  long delta: {:.1}% (+1 byte or more)\n  \
         status byte: {:.1}% (+1 byte)\n  other events: {}",
        m.source_tracks,
        if m.tracks.len() == m.source_tracks {
            String::new()
        } else {
            format!(" (packed as {}: big ones are split by channel)", m.tracks.len())
        },
        secs(total),
        secs(parse),
        si(notes / parse),
        bytes(m.file_bytes as f64 / parse),
        secs((total - parse).max(0.0)),
        bytes(packed),
        packed / notes.max(1.0),
        packed / (m.file_bytes as f64).max(1.0),
        bytes(m.logical_bytes as f64),
        m.logical_bytes as f64 / packed.max(1.0),
        si(m.chunks.len() as f64),
        pct(st.two_byte),
        pct(st.new_velocity),
        pct(st.new_duration),
        st.duration_bytes as f64 / st.new_duration.max(1) as f64,
        pct(st.long_delta),
        pct(st.status_byte),
        bytes(st.other_bytes as f64),
    );
    (summary, details)
}

/// 1234567 -> "1.23M"
fn si(v: f64) -> String {
    match v {
        v if v >= 1e12 => format!("{:.2}T", v / 1e12),
        v if v >= 1e9 => format!("{:.2}B", v / 1e9),
        v if v >= 1e6 => format!("{:.2}M", v / 1e6),
        v if v >= 1e3 => format!("{:.1}K", v / 1e3),
        v => format!("{v:.0}"),
    }
}

fn bytes(v: f64) -> String {
    match v {
        v if v >= 1e12 => format!("{:.2} TB", v / 1e12),
        v if v >= 1e9 => format!("{:.2} GB", v / 1e9),
        v if v >= 1e6 => format!("{:.1} MB", v / 1e6),
        v if v >= 1e3 => format!("{:.1} KB", v / 1e3),
        v => format!("{v:.0} B"),
    }
}

fn secs(s: f64) -> String {
    if s >= 1.0 {
        format!("{s:.2} s")
    } else {
        format!("{:.1} ms", s * 1e3)
    }
}

/// The SVMS section of the stats overlay: the synth's own view of the last second.
fn svms_text(s: &svms::Stats) -> String {
    let lost = if s.lost > 0 { format!(", {} records lost", s.lost) } else { String::new() };
    format!(
        "\n\nSVMS:   load {:.0}% avg, {:.0}% max @ {}\n        {} over budget, p99 {:.0}%, \
         {} frames @ {} Hz\n        sched {:.1}% · render {:.1}% · post {:.1}% · path {}\n\
         \x20       events {}/s, late {}/s (max {:.1} ms)\n        skipped {:.0} ms\n\
         \x20       voices {}/{} (peak {}, releasing {})\n        note-ons {}/s, steals {}/s\n\
         \x20       backlog {} sched, {} ingress, {} compiled\n        submit blocked {:.1} ms/s{lost}",
        s.load_avg,
        s.load_max,
        format_timestamp(s.worst_at_us),
        s.over_budget,
        s.p99,
        s.buffer_frames,
        s.sample_rate,
        s.schedule_pct,
        s.render_pct,
        s.post_pct,
        svms::paths(s.paths),
        si(s.events_per_s),
        si(s.late_per_s),
        s.max_late_ms,
        s.skipped_ms,
        si(s.active_voices as f64),
        si(s.max_voices as f64),
        si(s.peak_voices as f64),
        si(s.releasing_voices as f64),
        si(s.note_ons_per_s),
        si(s.steals_per_s),
        si(s.scheduled_backlog as f64),
        si(s.ingress_backlog as f64),
        si(s.compiled_backlog as f64),
        s.blocked_ms_per_s,
    )
}

fn format_timestamp(us: u64) -> String {
    let total_ms = us / 1_000;
    let millis = total_ms % 1_000;
    let total_seconds = total_ms / 1_000;
    let seconds = total_seconds % 60;
    let minutes = (total_seconds / 60) % 60;
    let hours = total_seconds / 3_600;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
    } else {
        format!("{minutes:02}:{seconds:02}.{millis:03}")
    }
}

fn generate_palette(count: usize) -> Vec<Color32> {
    const BASE_COLORS: &[Color32] = &[
        Color32::from_rgb(0xFF, 0x8C, 0x42),
        Color32::from_rgb(0x3B, 0x9C, 0xFF),
        Color32::from_rgb(0x69, 0xDB, 0x7C),
        Color32::from_rgb(0xE0, 0x6C, 0x75),
        Color32::from_rgb(0xB0, 0x7B, 0xE0),
        Color32::from_rgb(0xFF, 0xC5, 0x3D),
        Color32::from_rgb(0x4D, 0x96, 0xB5),
        Color32::from_rgb(0xFF, 0x6F, 0x91),
    ];

    if count == 0 {
        return Vec::new();
    }
    // ***CHANGED***: Force HSV generator for 16 colors, otherwise use base
    if count <= BASE_COLORS.len() && count != 16 {
        return BASE_COLORS[..count].to_vec();
    }

    let mut colors = Vec::with_capacity(count);
    let mut hue = 0.0f32;
    let step = 0.618_033_988_75; // golden ratio conjugate
    for _ in 0..count {
        hue = (hue + step) % 1.0;
        colors.push(color_from_hsv(hue, 0.6, 0.95));
    }
    colors
}

/// Shared by drawing and the renderer benchmark, so both measure the same work.
fn build_note_instances(
    song: &VisualSong,
    visible: &mut VisibleNotes,
    now_us: u64,
    window_end: u64,
    height_time: f32,
    height_px: u32,
    color_by_channel: bool,
    piano_layout: bool,
    instances: &mut Arc<Vec<gpu::NoteInstance>>,
) -> ([bool; 128], [Color32; 128]) {
    let mut active_keys = [false; 128];
    let mut active_colors = [Color32::TRANSPARENT; 128];
    let dense = visible.dense(height_time, height_px);
    let style = (color_by_channel, piano_layout, dense);
    if !dense {
        // Sparse windows are cheaper to visit directly than to query every row.
        let write = visible.instance_style != Some(style) || visible.instance_revision != visible.revision
            || visible.instance_height_time != height_time.to_bits() || now_us >= visible.instance_expiry;
        if write {
            let buf = Arc::make_mut(instances);
            buf.clear();
            let mut expiry = u64::MAX;
            let passes = if piano_layout { 2 } else { 1 };
            for note in (0..passes).flat_map(|pass| visible.window.iter().rev()
                .filter(move |n| !piano_layout || is_black_key(n.pitch) == (pass == 0)))
                .filter(|n| n.start_us < window_end && n.end_us + NOTE_HOLD_MICROS > now_us) {
                let color = song.color_for(note, color_by_channel);
                if note.start_us <= now_us && note.end_us > now_us && !active_keys[note.pitch as usize] {
                    active_keys[note.pitch as usize] = true;
                    active_colors[note.pitch as usize] = color;
                }
                expiry = expiry.min(note.end_us + NOTE_HOLD_MICROS);
                buf.push(gpu::NoteInstance {
                    start_us: [note.start_us as u32, (note.start_us >> 32) as u32],
                    len: ((note.end_us - note.start_us) as f32 / 1e6).max(0.000_1),
                    key_color: note.pitch as u32 | ((color.r() as u32) << 24)
                        | ((color.g() as u32) << 16) | ((color.b() as u32) << 8),
                });
            }
            visible.instance_expiry = expiry;
        } else {
            for note in visible.window.iter().rev() {
                if note.start_us <= now_us && note.end_us > now_us && !active_keys[note.pitch as usize] {
                    active_keys[note.pitch as usize] = true;
                    active_colors[note.pitch as usize] = song.color_for(note, color_by_channel);
                }
            }
        }
        visible.instance_style = Some(style);
        visible.instance_revision = visible.revision;
        visible.instance_height_time = height_time.to_bits();
        return (active_keys, active_colors);
    }
    // Keyboard state includes sounding notes too short to cover a raster sample.
    let t0 = Instant::now();
    for (pitch, key) in visible.keys.iter().enumerate() {
        let mut before = key.starts.partition_point(|&start| start <= now_us);
        while let Some(p) = key.covering(before, now_us) {
            let note = &visible.window[key.indices[p]];
            if note.end_us > now_us {
                active_keys[pitch] = true;
                active_colors[pitch] = song.color_for(note, color_by_channel);
                break;
            }
            before = p;
        }
    }
    visible.build_profile[0] = t0.elapsed();
    visible.select_pixels(now_us, window_end, height_time, height_px);
    let t0 = Instant::now();
    if visible.instance_style != Some(style) || visible.selected != visible.selection {
        let buf = Arc::make_mut(instances);
        buf.clear();
        // Newest first, with sharps in front of naturals in the piano layout.
        let passes = if piano_layout { 2 } else { 1 };
        let window = &visible.window;
        let selected = &visible.selected;
        for &i in (0..passes).flat_map(|pass| {
            selected.iter().rev().filter(move |&&i| {
                !piano_layout || is_black_key(window[i].pitch) == (pass == 0)
            })
        }) {
            let note = &visible.window[i];
            let color = song.color_for(note, color_by_channel);
            let color_u32 = ((color.r() as u32) << 16) | ((color.g() as u32) << 8) | color.b() as u32;
            buf.push(gpu::NoteInstance {
                start_us: [note.start_us as u32, (note.start_us >> 32) as u32],
                len: ((note.end_us - note.start_us) as f32 / 1_000_000.0).max(0.000_1),
                key_color: note.pitch as u32 | (color_u32 << 8),
            });
        }
        std::mem::swap(&mut visible.selection, &mut visible.selected);
        visible.instance_style = Some(style);
    }
    visible.build_profile[3] = t0.elapsed();
    (active_keys, active_colors)
}

fn color_from_hsv(h: f32, s: f32, v: f32) -> Color32 {
    let i = (h * 6.0).floor() as i32;
    let f = h * 6.0 - i as f32;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);

    let (r, g, b) = match i.rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };

    Color32::from_rgb((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn piano_columns_follow_the_keys() {
        let c = key_columns(true, 21, 108); // a full 88-key piano: 52 white keys
        let white = 1.0 / 52.0;
        assert_eq!(c[21], (0.0, white)); // A0
        assert!((c[108].1 - 1.0).abs() < 1e-5); // C8 ends at the right edge
        // white keys tile
        let whites: Vec<_> = (21..=108u8).filter(|&p| !is_black_key(p)).collect();
        for w in whites.windows(2) {
            assert!((c[w[0] as usize].1 - c[w[1] as usize].0).abs() < 1e-5);
        }
        // C#4 sits over the line between C4 and D4, nudged left; D#4 nudged right; G#4 centred
        let line = |lower: u8| c[lower as usize].1;
        let centre = |p: u8| (c[p as usize].0 + c[p as usize].1) / 2.0;
        assert!(centre(61) < line(60) && line(60) - centre(61) < white / 2.0);
        assert!(centre(63) > line(62));
        assert!((centre(68) - line(67)).abs() < 1e-5);
        // a sharp is 0.65 of a white key wide and overlaps both neighbours
        assert!(((c[61].1 - c[61].0) / white - 0.65).abs() < 1e-4);
        assert!(c[61].0 < c[60].1 && c[61].1 > c[62].0);
        // a range that starts and ends on sharps gets room for them (A#, nudged right,
        // pokes out a little at the far end, as it does in Piano From Above)
        let c = key_columns(true, 22, 106);
        assert!(c[22].0 >= 0.0 && c[106].1 <= 1.01);
        // even: a column per semitone
        let c = key_columns(false, 60, 71);
        assert_eq!(c[60], (0.0, 1.0 / 12.0));
        assert!((c[71].1 - 1.0).abs() < 1e-6);
    }

    #[test]
    fn exact_culling_only_drops_what_is_hidden() {
        let n = |start_us, end_us| NoteSegment { start_us, end_us, track: 0, channel: 0, pitch: 60 };
        let mut prev = n(100, 500);
        assert!(!n(101, 500).absorb(&mut prev)); // starts a µs later: prev shows above it
        assert!(!n(100, 499).absorb(&mut prev)); // ends sooner: prev shows below it
        assert!(n(100, 500).absorb(&mut prev)); // same span: hidden
        assert!(n(100, 800).absorb(&mut prev)); // same start, longer: hidden
        assert_eq!((prev.start_us, prev.end_us), (100, 800));
    }

    #[test]
    fn culls_only_fully_hidden_notes() {
        let n = |start_us, end_us, pitch| NoteSegment { start_us, end_us, track: 0, channel: 0, pitch };
        let mut out = vec![];
        let mut last = [usize::MAX; 128];
        push_culled(&mut out, &mut last, 0, n(0, 500, 60));
        push_culled(&mut out, &mut last, 0, n(10, 500, 60)); // visible sliver stays
        push_culled(&mut out, &mut last, 0, n(20, 300, 60));
        push_culled(&mut out, &mut last, 0, n(30, 900, 61));
        push_culled(&mut out, &mut last, 0, n(150, 900, 60));
        push_culled(&mut out, &mut last, 0, n(240, 1000, 60));
        let got: Vec<_> = out.iter().map(|n| (n.start_us, n.end_us, n.pitch)).collect();
        assert_eq!(got, [(0, 500, 60), (10, 500, 60), (20, 300, 60), (30, 900, 61), (150, 900, 60), (240, 1000, 60)]);
    }

    fn test_song() -> VisualSong {
        let mut file = b"MThd\0\0\0\x06\0\x01\0\x03\x01\xe0".to_vec();
        for track in 0..3 {
            let body = [0, 0x90 | track, 60, 100, 0x9e, 0, 0x80 | track, 60, 0, 0, 0xff, 0x2f, 0];
            file.extend_from_slice(b"MTrk");
            file.extend_from_slice(&(body.len() as u32).to_be_bytes());
            file.extend_from_slice(&body);
        }
        let midi = Arc::new(parser::MidiFile::parse_stream(
            Box::new(parser::Plain(std::io::Cursor::new(file))), 127, &parser::Progress::default(),
        ).unwrap());
        let tempo = Arc::new(song::TempoMap::from_midi(&midi));
        VisualSong::new(Arc::new(SongData { path: String::new(), midi, tempo, load_time: Duration::ZERO }))
    }

    fn indexed(mut notes: Vec<NoteSegment>, force_pixels: bool) -> VisibleNotes {
        if force_pixels {
            // Outside this test's view; enough notes to exercise the dense path.
            notes.extend((0..4500).map(|_| NoteSegment {
                start_us: 9_000_000, end_us: 10_000_000, track: 0, channel: 0, pitch: 59,
            }));
        }
        notes.sort_by_key(|n| (n.start_us, n.track));
        let mut visible = VisibleNotes { window: notes, ..Default::default() };
        visible.index_notes(0);
        visible
    }

    /// Simple painter's algorithm at pixel centres, including the shader's clipped
    /// geometry, minimum length and border shading. Input is back to front.
    fn raster(
        song: &VisualSong, notes: &[NoteSegment], now: u64, speed: f32,
        height: u32, by_channel: bool, piano: bool, flat: bool, upwards: bool,
    ) -> Vec<[u8; 3]> {
        let width = 48;
        let columns = key_columns(piano, 59, 62);
        let mut pixels = vec![[0; 3]; width * height as usize];
        let passes = if piano { 2 } else { 1 };
        for note in (0..passes).flat_map(|pass| notes.iter().filter(move |n| {
            !piano || is_black_key(n.pitch) == (pass == 1)
        })) {
            if note.start_us >= now + (speed as f64 * 1e6) as u64 || note.end_us + NOTE_HOLD_MICROS <= now { continue; }
            let start = (note.start_us as i64 - now as i64) as f32 / 1e6;
            let len = ((note.end_us - note.start_us) as f32 / 1e6).max(0.000_1);
            let clip = |time: f32| (1.0 - (time / speed).clamp(0.0, 2.0) * 2.0).clamp(-1.0, 1.0);
            let c0 = clip(start);
            let c1 = clip(start + len);
            let y0 = (1.0 - c0) * 0.5 * height as f32;
            let y1 = (1.0 - c1) * 0.5 * height as f32;
            let (left, right) = columns[note.pitch as usize];
            let x0 = left * width as f32;
            let x1 = right * width as f32;
            let color = song.color_for(note, by_channel);
            for y in 0..height as usize {
                let cy = y as f32 + 0.5;
                if cy < (y0 * 256.0).round() / 256.0 || cy >= (y1 * 256.0).round() / 256.0 { continue; }
                let v = (cy - y0) / (y1 - y0);
                for x in 0..width {
                    let cx = x as f32 + 0.5;
                    if cx < x0 || cx >= x1 { continue; }
                    let u = (cx - x0) / (x1 - x0);
                    let hx = 1.0 / (x1 - x0).max(1.0);
                    let hy = 1.0 / (y1 - y0).max(1.0);
                    let border = u < hx || u > 1.0 - hx || v < hy || v > 1.0 - hy;
                    let dark = border || (c0 - c1).abs() * (height as f32) < 7.0;
                    let shade = |c: u8| if flat { c } else {
                        let c = c as f32 / 255.0;
                        (c * c * if dark { 0.4 } else { 1.0 } * 255.0).round() as u8
                    };
                    let y = if upwards { height as usize - 1 - y } else { y };
                    pixels[y * width + x] = [shade(color.r()), shade(color.g()), shade(color.b())];
                }
            }
        }
        pixels
    }

    fn assert_raster_equal(song: &VisualSong, visible: &mut VisibleNotes, now: u64, speed: f32, height: u32) {
        visible.select_pixels(now, now + (speed as f64 * 1e6) as u64, speed, height);
        let selected: Vec<_> = visible.selected.iter().map(|&i| visible.window[i]).collect();
        for by_channel in [false, true] {
            for piano in [false, true] {
                for flat in [false, true] {
                    for upwards in [false, true] {
                        assert_eq!(
                            raster(song, &selected, now, speed, height, by_channel, piano, flat, upwards),
                            raster(song, &visible.window, now, speed, height, by_channel, piano, flat, upwards),
                            "now={now}, speed={speed}, height={height}, channel={by_channel}, piano={piano}, flat={flat}, up={upwards}",
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn pixel_culling_preserves_three_track_layers_and_colours() {
        let song = test_song();
        let n = |start_us, end_us, track, pitch| NoteSegment { start_us, end_us, track, channel: track as u8, pitch };
        let notes = vec![
            n(0, 100_000, 0, 60), n(20, 100_000, 1, 60), n(20, 40_000, 2, 60),
            n(50_500, 99_000, 0, 60), n(1000, 90_000, 1, 61), n(1000, 20_000, 2, 61),
        ];
        let mut visible = indexed(notes, true);
        for now in [0, 1, 17, 1539] {
            for height in [16, 64, 1024] { assert_raster_equal(&song, &mut visible, now, 0.1, height); }
        }
        visible.select_pixels(0, 100_000, 0.1, 64);
        let surviving: Vec<_> = visible.selected.iter().map(|&i| visible.window[i]).filter(|n| n.pitch == 60).collect();
        assert_eq!(surviving.iter().map(|n| (n.start_us, n.end_us, n.track)).collect::<Vec<_>>(),
            [(20, 100_000, 1), (20, 40_000, 2), (50_500, 99_000, 0)]);
        let mut instances = Arc::new(Vec::new());
        build_note_instances(&song, &mut visible, 0, 100_000, 0.1, 64, false, false, &mut instances);
        for instance in instances.iter().filter(|n| n.key_color & 255 == 60) {
            let start = instance.start_us[0] as u64;
            let note = surviving.iter().find(|n| n.start_us == start &&
                ((n.end_us - n.start_us) as f32 / 1e6).max(0.000_1) == instance.len &&
                ((song.color_for(n, false).r() as u32) << 24 |
                 (song.color_for(n, false).g() as u32) << 16 |
                 (song.color_for(n, false).b() as u32) << 8 | 60) == instance.key_color);
            assert!(note.is_some(), "surviving instance must keep its original colour and span");
        }
    }

    #[test]
    fn pixel_culling_matches_the_painter_on_dense_notes_and_close_zoom() {
        let song = test_song();
        let mut seed = 7u64;
        let notes = (0..7000).map(|i| {
            seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17;
            let start_us = seed % 1_000_000;
            NoteSegment {
                start_us, end_us: start_us + (seed >> 20) % 100_000,
                track: i % 3, channel: (i % 3) as u8, pitch: 60 + (i % 3) as u8,
            }
        }).collect();
        let mut visible = indexed(notes, false);
        for (now, speed) in [(0, 1.0), (333_333, 0.2), (777_777, 0.01)] {
            assert_raster_equal(&song, &mut visible, now, speed, 48);
        }
    }

    #[test]
    fn scrolling_row_cache_matches_the_painter_frame_after_frame() {
        let song = test_song();
        let mut seed = 99u64;
        let notes = (0..7000).map(|i| {
            seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17;
            let start_us = seed % 3_000_000;
            NoteSegment {
                start_us, end_us: start_us + (seed >> 20) % 300_000,
                track: i % 3, channel: (i % 3) as u8, pitch: 60 + (i % 2) as u8,
            }
        }).collect();
        let mut visible = indexed(notes, false);
        // Same zoom, small steps (the cache shifts), then a jump past the whole viewport.
        for now in (0..40).map(|f| f * 16_667).chain([2_000_000, 2_016_667]) {
            assert_raster_equal(&song, &mut visible, now, 1.0, 48);
        }
    }

    #[test]
    fn zoom_restores_subpixel_notes_and_unchanged_instances_are_reused() {
        let song = test_song();
        let mut visible = indexed((0..5000).map(|i| NoteSegment {
            start_us: i * 100, end_us: i * 100 + 100, track: (i % 3) as u16,
            channel: (i % 3) as u8, pitch: 60,
        }).collect(), false);
        let mut instances = Arc::new(Vec::new());
        build_note_instances(&song, &mut visible, 0, 500_000, 0.5, 100, false, true, &mut instances);
        let coarse = instances.clone();
        build_note_instances(&song, &mut visible, 0, 500_000, 0.5, 100, false, true, &mut instances);
        assert!(Arc::ptr_eq(&coarse, &instances));
        build_note_instances(&song, &mut visible, 0, 500_000, 0.5, 10_000, false, true, &mut instances);
        assert!(instances.len() > coarse.len() * 10);
        assert_raster_equal(&song, &mut visible, 123_456, 0.01, 48);
    }

    #[test]
    fn seek_rewind_parallel_decode_and_sweep_keep_the_same_notes() {
        let song = test_song();
        let mut stepped = VisibleNotes::default();
        stepped.advance(&song.data, 0, 1_000_000, 1000);
        stepped.advance(&song.data, 2_000_000, 5_000_000, 3000); // jumped parallel path
        let mut fresh = VisibleNotes::default();
        fresh.advance(&song.data, 2_000_000, 5_000_000, 3000);
        assert_eq!(stepped.window, fresh.window);
        let mut a = Arc::new(Vec::new());
        let mut b = Arc::new(Vec::new());
        let build = |visible: &mut VisibleNotes, buf: &mut Arc<Vec<gpu::NoteInstance>>, now| {
            build_note_instances(&song, visible, now, now + 3_000_000, 3.0, 900, false, true, buf);
        };
        build(&mut stepped, &mut a, 2_000_000);
        stepped.frames_since_sweep = 239;
        stepped.advance(&song.data, 2_000_000, 5_000_000, 3000);
        build(&mut stepped, &mut b, 2_000_000);
        assert_eq!(a, b);
        stepped.advance(&song.data, 0, 3_000_000, 10); // rewind, different zoom
        fresh = VisibleNotes::default();
        fresh.advance(&song.data, 0, 3_000_000, 10);
        build(&mut stepped, &mut a, 0);
        build(&mut fresh, &mut b, 0);
        assert_eq!(a, b);
    }

    #[test]
    fn sparse_cache_expires_notes_and_updates_colour_mode() {
        let song = test_song();
        let mut visible = indexed(vec![
            NoteSegment { start_us: 0, end_us: 2000, track: 1, channel: 2, pitch: 60 },
            NoteSegment { start_us: 5000, end_us: 10_000, track: 1, channel: 2, pitch: 60 },
        ], false);
        let mut instances = Arc::new(Vec::new());
        build_note_instances(&song, &mut visible, 0, 10_000, 0.01, 100, false, false, &mut instances);
        assert_eq!(instances.len(), 2);
        build_note_instances(&song, &mut visible, 4000, 14_000, 0.01, 100, false, false, &mut instances);
        assert_eq!(instances.len(), 1);
        let track_colour = instances[0].key_color;
        let (active, colours) = build_note_instances(&song, &mut visible, 6000, 16_000, 0.01, 100, true, false, &mut instances);
        assert_ne!(track_colour, instances[0].key_color);
        assert!(active[60]);
        assert_eq!(colours[60], song.color_for(&visible.window[1], true));
    }
}
