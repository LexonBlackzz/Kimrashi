mod omnimidi;
mod output;
mod archive;
mod bench;
mod benchmark;
mod render_bench;
mod export;
mod export_ui;
mod clock;
mod gpu;
mod parser;
mod player;
mod settings;
mod song;
mod theme;
mod ui;
mod svms;
mod sig;
mod update;
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
use settings::{Settings, StatsMode};
use song::SongData;
use ui::{draw_piano_roll, format_timestamp, si, secs, wrap_for_slider, load_info, loading_details, key_columns, is_black_key};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
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

const DEFAULT_PITCH_MIN: u8 = 21;
const DEFAULT_PITCH_MAX: u8 = 108;
/// Arrow-key seek step.
const SEEK_STEP_US: u64 = 5_000_000;
const SYNTH_LIMIT_OFF: f64 = 20_000_000.0;
const NOTE_HOLD_MICROS: u64 = 1_000; // keep notes visible 1ms after Note Off to avoid abrupt drop

fn main() {
    let _timer = create_timer_guard();
    if bench::run_from_args() || export::run_from_args() || render_bench::run_from_args() || benchmark::run_from_args() || update::run_from_args() {
        return;
    }
    update::cleanup_old();
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
    let cfg = Settings::load();
    let outputs = output::list_outputs(kdmapi);
    let output_idx = outputs.iter().position(|(_, n)| *n == cfg.output).unwrap_or(0);
    let initial_output = outputs
        .get(output_idx)
        .map_or(output::OutputChoice::Kdmapi, |(choice, _)| choice.clone());

    let (cmd_tx, cmd_rx) = unbounded::<PlayerCommand>();
    let (telemetry_tx, telemetry_rx) = player::latest::<PlayerTelemetry>();
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

    let _ = cmd_tx.send(PlayerCommand::SetSkipping(cfg.skip_notes));
    let _ = cmd_tx.send(PlayerCommand::SetSvmsBatch(cfg.svms_batch));
    let _ = cmd_tx.send(PlayerCommand::SetLongData(cfg.long_data));
    if cfg.synth_limit < SYNTH_LIMIT_OFF {
        let _ = cmd_tx.send(PlayerCommand::SetSynthLimit(cfg.synth_limit as u32));
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
        // Real swap-chain VSync (fixed at startup). Waiting for the blank by hand and then
        // presenting uncapped tears wherever the scanout happens to be when the frame lands.
        wgpu_options: egui_wgpu::WgpuConfiguration {
            present_mode: if cfg.vsync {
                eframe::wgpu::PresentMode::Fifo
            } else {
                eframe::wgpu::PresentMode::AutoNoVsync
            },
            // Let the CPU run only one frame ahead of the display so Fifo really paces it.
            desired_maximum_frame_latency: Some(1),
            wgpu_setup: egui_wgpu::WgpuSetupCreateNew {
                // egui asks for wgpu's conservative default limits (256 MB buffers);
                // take the adapter's real buffer limit so dense songs fit.
                device_descriptor: Arc::new(|adapter| {
                    let mut desc = (egui_wgpu::WgpuSetupCreateNew::default().device_descriptor)(adapter);
                    desc.required_limits.max_buffer_size = adapter.limits().max_buffer_size;
                    desc.required_limits.max_storage_buffer_binding_size = adapter.limits().max_storage_buffer_binding_size;
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
                applied_look: None,
                playback_time_us: 0,
                slider_override_us: None,
                slider_grabbed: false,
                outputs,
                output_idx,
                tempo: 1.0,
                log_stats: false,
                benchmark: None,
                loading: None,
                show_settings: false,
                settings_tab: SettingsTab::Display,
                settings_search: String::new(),
                visible: VisibleNotes::default(),
                instances: Arc::default(),
                refresh_rate_hz: vsync::refresh_hz(),
                status_message: None,
                fps: 0.0,
                last_frame: None,
                timer_guard,
                updater: {
                    let mut updater = update::Updater::new(cfg.check_updates, cfg.last_update_check);
                    updater.start_if_due();
                    updater
                },
                cfg,
                frame_times: FrameTimes::default(),
                frame_log: FrameLog::from_env(),
                open_at_us,
                export: Default::default(),
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
    /// One color per (track, channel) pair: index = channel rank + track * channels used.
    colors: Vec<Color32>,
    /// Each channel's place among the channels the file uses.
    channel_rank: [u8; 16],
    channels: usize,
    palette: theme::NotePalette,
    pitch_min: u8,
    pitch_max: u8,
}

impl VisualSong {
    fn new(data: Arc<SongData>) -> Self {
        let (pitch_min, pitch_max) = match data.midi.pitch_range {
            Some((lo, hi)) => (lo.min(DEFAULT_PITCH_MIN), hi.max(DEFAULT_PITCH_MAX)),
            None => (DEFAULT_PITCH_MIN, DEFAULT_PITCH_MAX),
        };
        let used = data.midi.channels_used;
        let channel_rank = std::array::from_fn(|c| (used & ((1u16 << c) - 1)).count_ones() as u8);
        let channels = used.count_ones().max(1) as usize;
        let mut song = Self { data, colors: Vec::new(), channel_rank, channels, palette: theme::NotePalette::default(), pitch_min, pitch_max };
        song.set_palette(theme::NotePalette::default(), &theme::default_custom());
        song
    }

    fn set_palette(&mut self, palette: theme::NotePalette, custom: &[[u8; 3]]) {
        // ponytail: capped; past 65k pairs the colors just repeat.
        let pairs = (self.data.midi.source_tracks.max(1) * self.channels).min(65_536);
        self.colors = palette.colors(pairs, custom);
        self.palette = palette;
    }

    /// Every track and every channel its own color, whichever the file has more of.
    fn color_for(&self, note: &NoteSegment) -> Color32 {
        if self.palette == theme::NotePalette::Pitch {
            return theme::pitch_color(note.pitch, self.pitch_min, self.pitch_max);
        }
        let i = self.channel_rank[note.channel() as usize % 16] as usize + note.track as usize * self.channels;
        self.colors[i % self.colors.len()]
    }
}

/// A note decoded for drawing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NoteSegment {
    start_us: u64,
    end_us: u64,
    source: u32,
    track: u16,
    channel: u8,
    pitch: u8,
}

/// Border suppression, kept in the spare high bits of `channel` (a segment is a piece of
/// a note when a newer note cut it or a newer note's end exposed it again).
const NO_START_BORDER: u8 = 0x10;
const NO_END_BORDER: u8 = 0x20;

impl NoteSegment {
    fn channel(&self) -> u8 { self.channel & 15 }
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
    updates: Vec<NoteSegment>,
}

impl Decoded {
    fn new(notes: Vec<NoteSegment>) -> Self {
        Self { notes, last: [usize::MAX; 128], scanned: 0, updates: Vec::new() }
    }
}

const NOTE_PAGE: usize = 2_048;

#[derive(Debug)]
struct NotePage {
    notes: Vec<NoteSegment>,
    first: usize,
    expiry: u64,
    pitches: [u64; 2],
}

#[derive(Default, Debug)]
struct NoteWindow {
    pages: Vec<Option<NotePage>>,
    /// Every page before this one has expired: per-frame work starts here, not at ID 0.
    first_live: usize,
    len: usize,
    live: usize,
    /// (expiry, page) entries; a page's real expiry may have grown since, so it is rechecked.
    due: std::collections::BinaryHeap<std::cmp::Reverse<(u64, usize)>>,
    /// Index ranges of expired pages (the note data is dropped on expiry).
    retired: std::collections::VecDeque<(usize, usize)>,
}

impl NoteWindow {
    fn len(&self) -> usize { self.len }
    fn get(&self, i: usize) -> Option<&NoteSegment> {
        let page = self.pages.get(i / NOTE_PAGE)?.as_ref()?;
        let offset = i % NOTE_PAGE;
        (offset >= page.first).then(|| page.notes.get(offset)).flatten()
    }
    fn entries_range(&self, first: usize, end: usize) -> impl DoubleEndedIterator<Item = (usize, &NoteSegment)> {
        let stop = end.div_ceil(NOTE_PAGE);
        let base = (first / NOTE_PAGE).max(self.first_live).min(stop);
        self.pages[base..stop].iter().enumerate()
            .filter_map(move |(p, page)| page.as_ref().map(|page| ((base + p) * NOTE_PAGE, page)))
            .flat_map(move |(base, page)| {
                let lo = first.saturating_sub(base).max(page.first).min(page.notes.len());
                let hi = end.saturating_sub(base).min(page.notes.len()).max(lo);
                page.notes[lo..hi].iter().enumerate().map(move |(i, note)| (base + lo + i, note))
            })
    }
    fn entries(&self) -> impl DoubleEndedIterator<Item = (usize, &NoteSegment)> { self.entries_range(0, self.len) }
    fn iter(&self) -> impl DoubleEndedIterator<Item = &NoteSegment> { self.entries().map(|(_, note)| note) }
    fn extend(&mut self, notes: Vec<NoteSegment>) {
        let mut notes = notes.into_iter();
        while notes.len() > 0 {
            let p = self.len / NOTE_PAGE;
            let offset = self.len % NOTE_PAGE;
            if p == self.pages.len() { self.pages.push(None); }
            self.first_live = self.first_live.min(p);
            let page = self.pages[p].get_or_insert_with(|| NotePage {
                notes: vec![NoteSegment { source: 0, start_us: 0, end_us: 0, track: 0, channel: 0, pitch: 0 }; offset],
                first: offset, expiry: 0, pitches: [0; 2],
            });
            let count = notes.len().min(NOTE_PAGE - offset);
            page.notes.reserve(count);
            for note in notes.by_ref().take(count) {
                page.expiry = page.expiry.max(note.end_us + NOTE_HOLD_MICROS);
                page.pitches[note.pitch as usize / 64] |= 1 << (note.pitch % 64);
                page.notes.push(note);
            }
            self.due.push(std::cmp::Reverse((page.expiry, p)));
            self.len += count;
            self.live += count;
        }
    }
    fn replace(&mut self, i: usize, note: NoteSegment) {
        let page = self.pages[i / NOTE_PAGE].as_mut().unwrap();
        page.notes[i % NOTE_PAGE] = note;
        // No heap entry needed: the one pushed by `extend` is never later than this, and a
        // due entry re-checks the page's real expiry.
        page.expiry = page.expiry.max(note.end_us + NOTE_HOLD_MICROS);
    }
    fn expire(&mut self, now: u64) -> [u64; 2] {
        let mut pitches = [0; 2];
        // Only pages that are due are visited; scanning every page grew with the song.
        // A page is ~50 KB to free; a sustained note ending can expire tens of thousands at
        // once, so release a bounded number per frame (dead notes lingering a few frames
        // are harmless: row queries use times, not page liveness).
        let mut budget = 256;
        while let Some(&std::cmp::Reverse((due, p))) = self.due.peek() {
            if due > now || budget == 0 { break; }
            self.due.pop();
            let Some(page) = self.pages[p].as_ref() else { continue };
            if page.expiry > now { self.due.push(std::cmp::Reverse((page.expiry, p))); continue; }
            budget -= 1;
            let page = self.pages[p].take().unwrap();
            self.live -= page.notes.len() - page.first;
            for k in 0..2 { pitches[k] |= page.pitches[k]; }
            self.retired.push_back((p * NOTE_PAGE + page.first, p * NOTE_PAGE + page.notes.len()));
        }
        while self.pages.get(self.first_live).is_some_and(|page| page.is_none()) { self.first_live += 1; }
        pitches
    }
    /// First note ID that can still be live (a multiple of 64).
    fn live_base(&self) -> usize { self.first_live * NOTE_PAGE }
    fn retired_range(&mut self) -> Option<(usize, usize)> { self.retired.pop_front() }
}

impl From<Vec<NoteSegment>> for NoteWindow {
    fn from(notes: Vec<NoteSegment>) -> Self { let mut window = Self::default(); window.extend(notes); window }
}
impl std::ops::Index<usize> for NoteWindow {
    type Output = NoteSegment;
    fn index(&self, i: usize) -> &NoteSegment { self.get(i).unwrap() }
}
impl PartialEq for NoteWindow {
    fn eq(&self, other: &Self) -> bool { self.entries().eq(other.entries()) }
}

#[derive(Default)]
struct KeyCompact {
    indices: Vec<usize>,
    starts: Vec<u64>,
    ends: Vec<u64>,
    cursor: usize,
}

/// A small max-end tree over runs of notes on one key. It finds the latest note
/// covering a row without walking the whole window, including past long notes.
#[derive(Default)]
struct KeyNotes {
    indices: Vec<usize>,
    compact: Option<KeyCompact>,
    prune: bool,
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

/// Finished, globally ordered notes appended and indexed in one frame.
const NOTE_BATCH: usize = 300_000;
const DECODE_AHEAD_US: u64 = 10_000_000;
/// A seek decodes only this far past the window itself; the worker fills in the rest,
/// at most `WORKER_CHUNK_US` of song at a time (it all sits in memory until sent).
const SYNC_AHEAD_US: u64 = 250_000;
const WORKER_CHUNK_US: u64 = 50_000;

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

    fn query_row(&self, window: &NoteWindow, now: u64, window_end: u64, time: u64, guard: u64, row: &mut RowNotes) {
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
            let Some(note) = window.get(i) else { before = p; continue; };
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

fn render_tracks(song: &SongData) -> Vec<bool> {
    let mut latest = HashMap::new();
    for (i, refs) in song.midi.tracks.iter().enumerate() { latest.insert(refs.as_slice(), i); }
    let mut enabled = vec![false; song.midi.tracks.len()];
    for &i in latest.values() { enabled[i] = true; }
    // Identical packed tracks have identical spans on every key. The last track
    // covers every earlier copy, including equal-start track colours.
    enabled
}

fn decode_notes(song: &SongData, readers: &mut [TrackReader], now_us: u64, reach: u64) -> Decoded {
    decode_tracks(song, readers, now_us, reach, None)
}

/// Each enabled track's notes in `[now_us, reach)`, in track order.
fn decode_parts(song: &SongData, readers: &mut [TrackReader], now_us: u64, reach: u64, enabled: Option<&[bool]>) -> Vec<Decoded> {
    let tempo = &song.tempo;
    let now_tick = tempo.time_to_tick(now_us);
    let end_tick = tempo.time_to_tick(reach);
    let decode = |track: usize, r: &mut TrackReader, acc: &mut Decoded| {
        if enabled.is_some_and(|mask| !mask[track]) { return; }
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
                let note = NoteSegment { source: track as u32,
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
    // Indexed collection preserves track order.
    readers.par_iter_mut().enumerate().map(|(i, r)| {
        let mut acc = Decoded::new(Vec::new());
        decode(i, r, &mut acc);
        acc
    }).collect()
}

/// Notes per key (ascending start, covered ones absorbed) and records scanned. Groups of
/// tracks are bucketed by key in parallel, then each key is joined in group order, so a
/// key sees tracks in order.
fn decode_keys(song: &SongData, readers: &mut [TrackReader], now_us: u64, reach: u64, enabled: Option<&[bool]>) -> (Vec<Vec<NoteSegment>>, usize) {
    let parts = decode_parts(song, readers, now_us, reach, enabled);
    let scanned = parts.iter().map(|p| p.scanned).sum();
    let group = parts.len().div_ceil(64).max(1);
    let groups: Vec<Vec<Vec<NoteSegment>>> = parts.par_chunks(group).map(|tracks| {
        let mut keys: Vec<Vec<NoteSegment>> = vec![Vec::new(); 128];
        for t in tracks { for &n in &t.notes { keys[n.pitch as usize & 127].push(n); } }
        keys
    }).collect();
    drop(parts); // the per-key copies replace them; don't hold both
    let keys = (0..128usize).into_par_iter().map(|k| {
        let mut v = Vec::with_capacity(groups.iter().map(|g| g[k].len()).sum());
        for g in &groups { v.extend_from_slice(&g[k]); }
        v.sort_by_key(|n| n.start_us); // stable: ties keep track order
        let (mut w, mut last) = (0, usize::MAX);
        for r in 0..v.len() {
            let note = v[r];
            if last == usize::MAX || !note.absorb(&mut v[last]) {
                v[w] = note;
                last = w;
                w += 1;
            }
        }
        v.truncate(w);
        v
    }).collect();
    (keys, scanned)
}

/// Every enabled track's notes in `[now_us, reach)`, sorted by start.
fn decode_tracks(song: &SongData, readers: &mut [TrackReader], now_us: u64, reach: u64, enabled: Option<&[bool]>) -> Decoded {
    let parts = decode_parts(song, readers, now_us, reach, enabled);
    let mut out = Decoded::new(Vec::new());
    out.scanned = parts.iter().map(|p| p.scanned).sum();
    out.notes.reserve(parts.iter().map(|p| p.notes.len()).sum());
    for part in parts { out.notes.extend(part.notes); }
    out.notes.par_sort_by_key(|n| n.start_us);
    let mut last = [usize::MAX; 128];
    let mut w = 0;
    for r in 0..out.notes.len() {
        let note = out.notes[r];
        let k = note.pitch as usize & 127;
        if last[k] == usize::MAX || !note.absorb(&mut out.notes[last[k]]) {
            out.notes[w] = note;
            last[k] = w;
            w += 1;
        }
    }
    out.notes.truncate(w);
    out
}

type NoteId = (u64, u32, u8, u8);

fn note_id(note: &NoteSegment) -> NoteId {
    (note.start_us, note.source, note.channel(), note.pitch)
}

/// A note sounding on a key. `Ord` is what is drawn on top: later start, then later track.
#[derive(Clone, Copy)]
struct Layer {
    note: NoteSegment,
    seq: u32,
}

impl Layer {
    fn rank(&self) -> (u64, u32, u32) { (self.note.start_us, self.note.source, self.seq) }
}
impl PartialEq for Layer { fn eq(&self, other: &Self) -> bool { self.rank() == other.rank() } }
impl Eq for Layer {}
impl PartialOrd for Layer { fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(other)) } }
impl Ord for Layer { fn cmp(&self, other: &Self) -> std::cmp::Ordering { self.rank().cmp(&other.rank()) } }

/// The part of a key's timeline currently on top, begun at `start`.
struct OpenSegment {
    start: u64,
    top: Layer,
    /// Already sent (with a provisional end), so later changes are updates.
    emitted: bool,
}

/// One key's sounding notes and visible segment, carried from chunk to chunk.
#[derive(Default)]
struct FlattenKey {
    active: std::collections::BinaryHeap<Layer>,
    open: Option<OpenSegment>,
}

impl OpenSegment {
    fn segment(&self, end: u64, key: u8) -> NoteSegment {
        let note = self.top.note;
        let mut flags = 0;
        if self.start != note.start_us { flags |= NO_START_BORDER; }
        if end != note.end_us { flags |= NO_END_BORDER; }
        // `source` is the track while decoding; a flattened segment has no use for it (it
        // is unique by start on its key) and carries the whole note's length instead, so
        // the shader can still darken the pieces of a note that is thin.
        let whole = (note.end_us - note.start_us).min(u32::MAX as u64) as u32;
        NoteSegment { start_us: self.start, end_us: end, channel: note.channel() | flags, pitch: key, source: whole, ..note }
    }
}

impl FlattenKey {
    /// Sweep this key through every event before `horizon`. `starts` are the notes that
    /// begin in the chunk, in drawing order. Whatever is on top of the key at any moment
    /// is a segment; the rest is hidden and never leaves the worker. A segment still open
    /// at `horizon` is sent ending there and extended by later chunks.
    fn sweep(&mut self, key: u8, starts: &[NoteSegment], seq: u32, horizon: u64, now: u64,
             notes: &mut Vec<NoteSegment>, updates: &mut Vec<NoteSegment>) {
        let mut i = 0;
        loop {
            let ends = self.open.as_ref().map(|open| open.top.note.end_us);
            let begins = starts.get(i).map(|n| n.start_us);
            let t = match (ends, begins) {
                (Some(e), Some(b)) => e.min(b),
                (Some(t), None) | (None, Some(t)) => t,
                (None, None) => break,
            };
            if t >= horizon { break; }
            while let Some(n) = starts.get(i).filter(|n| n.start_us == t) {
                // A note of no length is still drawn, at the shader's minimum length.
                self.active.push(Layer { note: NoteSegment { end_us: rendered_end(n), ..*n }, seq: seq + i as u32 });
                i += 1;
            }
            while self.active.peek().is_some_and(|l| l.note.end_us <= t) { self.active.pop(); }
            if let Some(open) = self.open.take() {
                if t > open.start {
                    let segment = open.segment(t, key);
                    if open.emitted { updates.push(segment); }
                    else if t + NOTE_HOLD_MICROS > now { notes.push(segment); }
                }
            }
            self.open = self.active.peek().map(|&top| OpenSegment { start: t, top, emitted: false });
        }
        if let Some(open) = &mut self.open {
            let segment = open.segment(horizon, key);
            if open.emitted { updates.push(segment); }
            else if horizon + NOTE_HOLD_MICROS > now { notes.push(segment); open.emitted = true; }
        }
    }
}

/// Turn a chunk of decoded notes (sorted by start) into the visible segments of every key.
fn flatten_chunk(keys: &mut [FlattenKey], decoded: Decoded, horizon: u64, now: u64) -> Decoded {
    let mut buckets: Vec<Vec<NoteSegment>> = vec![Vec::new(); keys.len()];
    for note in decoded.notes { buckets[note.pitch as usize & 127].push(note); }
    flatten_buckets(keys, buckets, decoded.scanned, horizon, now)
}

/// `flatten_chunk` for notes already split per key, each ascending in start.
fn flatten_buckets(keys: &mut [FlattenKey], buckets: Vec<Vec<NoteSegment>>, scanned: usize, horizon: u64, now: u64) -> Decoded {
    let first = buckets.iter().map(Vec::len).sum();
    let parts: Vec<_> = keys.par_iter_mut().zip(buckets.par_iter()).enumerate().map(|(key, (state, starts))| {
        let (mut notes, mut updates) = (Vec::new(), Vec::new());
        state.sweep(key as u8, starts, 0, horizon, now, &mut notes, &mut updates);
        (notes, updates)
    }).collect();
    let mut out = Decoded::new(Vec::with_capacity(first));
    out.scanned = scanned;
    for (notes, updates) in parts { out.notes.extend(notes); out.updates.extend(updates); }
    out.notes.par_sort_by_key(|n| n.start_us);
    out
}

struct NoteBatch {
    generation: u64,
    notes: Vec<NoteSegment>,
    updates: Vec<NoteSegment>,
    decoded_end: u64,
    through: u64,
}

struct DecodeWorker {
    now: Arc<AtomicU64>,
    target: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    receiver: std::sync::mpsc::Receiver<NoteBatch>,
    pending: Option<NoteBatch>,
    song: Arc<SongData>,
    thread: std::thread::Thread,
}

impl Drop for DecodeWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.unpark();
        // Dropping the receiver cancels a blocked send; never join in a frame.
    }
}

impl DecodeWorker {
    fn new(song: Arc<SongData>, mut readers: Vec<TrackReader>, through: u64, generation: u64, mut flatten: Option<Vec<FlattenKey>>, prepared: Vec<NoteSegment>) -> Self {
        let now = Arc::new(AtomicU64::new(0));
        let target = Arc::new(AtomicU64::new(through));
        let stop = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = std::sync::mpsc::sync_channel(8);
        let (worker_now, worker_target, worker_stop, worker_song) =
            (now.clone(), target.clone(), stop.clone(), song.clone());
        let enabled = render_tracks(&song);
        let handle = std::thread::Builder::new().name("note-decode".into()).spawn(move || {
            let mut through = through;
            let mut prepared = prepared.into_iter().peekable();
            loop {
                let notes = prepared.by_ref().take(NOTE_BATCH).collect();
                let watermark = prepared.peek().map_or(through, |n| n.start_us);
                if sender.send(NoteBatch { generation, notes, updates: Vec::new(), decoded_end: through, through: watermark }).is_err() { return; }
                if prepared.peek().is_none() { break; }
                if worker_stop.load(Ordering::Acquire) { return; }
            }
            let decode_pool = rayon::ThreadPoolBuilder::new()
                .num_threads(std::thread::available_parallelism().map_or(1, |n| n.get().min(4)))
                .thread_name(|i| format!("note-decode-{i}"))
                .build().expect("note decode pool");
            while !worker_stop.load(Ordering::Acquire) {
                let reach = worker_target.load(Ordering::Acquire).min(through.saturating_add(WORKER_CHUNK_US));
                if reach <= through {
                    std::thread::park_timeout(Duration::from_millis(2));
                    continue;
                }
                let decoded = decode_pool.install(|| {
                    let now = worker_now.load(Ordering::Acquire);
                    match &mut flatten {
                        Some(keys) => {
                            let (buckets, scanned) = decode_keys(&worker_song, &mut readers, now, reach, Some(&enabled));
                            flatten_buckets(keys, buckets, scanned, reach, now)
                        }
                        None => decode_tracks(&worker_song, &mut readers, now, reach, Some(&enabled)),
                    }
                });
                if worker_stop.load(Ordering::Acquire) { break; }
                let mut updates = decoded.updates.into_iter().peekable();
                while updates.peek().is_some() {
                    let batch = updates.by_ref().take(NOTE_BATCH).collect();
                    if sender.send(NoteBatch { generation, notes: Vec::new(), updates: batch, decoded_end: reach, through }).is_err() { return; }
                }
                let mut notes = decoded.notes.into_iter().peekable();
                loop {
                    let batch: Vec<_> = notes.by_ref().take(NOTE_BATCH).collect();
                    let watermark = notes.peek().map_or(reach, |n| n.start_us);
                    if sender.send(NoteBatch { generation, notes: batch, updates: Vec::new(), decoded_end: reach, through: watermark }).is_err() { return; }
                    if notes.peek().is_none() { break; }
                    if worker_stop.load(Ordering::Acquire) { return; }
                }
                through = reach;
            }
        }).expect("note decode worker");
        Self { now, target, stop, receiver, pending: None, song, thread: handle.thread().clone() }
    }
}

#[derive(Clone, Copy)]
struct InstanceSlot {
    id: u32,
    version: u32,
}

impl InstanceSlot {
    const EMPTY: Self = Self { id: u32::MAX, version: 0 };
}

/// Slots by note ID, in page-sized chunks allocated on first write: most notes are never
/// drawn, and a flat Vec's doubling cost 50-100 ms at tens of millions of IDs.
#[derive(Default)]
struct SlotStore(Vec<Option<Box<[InstanceSlot; NOTE_PAGE]>>>);

impl SlotStore {
    fn get(&self, i: usize) -> Option<InstanceSlot> { self.0.get(i / NOTE_PAGE)?.as_ref().map(|c| c[i % NOTE_PAGE]) }
    fn free_chunk(&mut self, i: usize) { if let Some(c) = self.0.get_mut(i / NOTE_PAGE) { *c = None; } }
}
impl std::ops::Index<usize> for SlotStore {
    type Output = InstanceSlot;
    fn index(&self, i: usize) -> &InstanceSlot {
        self.0.get(i / NOTE_PAGE).and_then(|c| c.as_ref()).map_or(&InstanceSlot::EMPTY, |c| &c[i % NOTE_PAGE])
    }
}
impl std::ops::IndexMut<usize> for SlotStore {
    fn index_mut(&mut self, i: usize) -> &mut InstanceSlot {
        if self.0.len() <= i / NOTE_PAGE { self.0.resize_with(i / NOTE_PAGE + 1, || None); }
        &mut self.0[i / NOTE_PAGE].get_or_insert_with(|| vec![InstanceSlot::EMPTY; NOTE_PAGE].into_boxed_slice().try_into().ok().unwrap())[i % NOTE_PAGE]
    }
}

/// Notes on screen. Each note is decoded from the packed tracks once, when it enters
/// the window, and dropped once it has finished.
struct VisibleNotes {
    time_us: u64,
    collapse_overlaps: bool,
    max_instances: usize,
    note_indices: HashMap<NoteId, usize>,
    worker: Option<DecodeWorker>,
    generation: u64,
    through: u64,
    window: NoteWindow,
    /// Notes decoded last frame (after culling), and records looked at, to decide
    /// whether decoding is worth parallelizing.
    last_batch: usize,
    last_scanned: usize,
    /// Notes in `window` after the last sweep of finished ones, and frames since.
    alive: usize,
    frames_since_sweep: u32,
    keys: [KeyNotes; 128],
    indexed: usize,
    compact_from: usize,
    max_len_us: u64,
    total_len_us: u64,
    selected: Vec<usize>,
    selection: Vec<usize>,
    selected_bits: Vec<u64>,
    /// One bit per word of `selected_bits`, so a frame visits only words that were set.
    selected_summary: Vec<u64>,
    instance_dirty: bool,
    instance_ring_id: u64,
    instance_slots: SlotStore,
    retired_slots: Vec<InstanceSlot>,
    instance_style: Option<(bool, bool)>,
    revision: u64,
    instance_revision: u64,
    instance_height_time: u32,
    instance_expiry: u64,
    /// Keyboard, row queries, bitset collection, instance packing.
    build_profile: [Duration; 6],
    row_counts: [usize; 2],
    /// Where the last `advance` spent its time (for `FrameLog`).
    timing: AdvanceTimes,
}

impl Default for VisibleNotes {
    fn default() -> Self {
        Self {
            time_us: 0, collapse_overlaps: true, max_instances: 1_048_576, note_indices: HashMap::new(), worker: None, generation: 0, through: 0, window: NoteWindow::default(), last_batch: 0,
            last_scanned: 0, alive: 0, frames_since_sweep: 0,
            keys: std::array::from_fn(|_| KeyNotes::default()), indexed: 0, compact_from: 0, max_len_us: 0, total_len_us: 0,
            selected: Vec::new(), selection: Vec::new(), selected_bits: Vec::new(), selected_summary: Vec::new(),
            instance_dirty: false, instance_ring_id: 0, instance_slots: SlotStore::default(), retired_slots: Vec::new(), instance_style: None, revision: 0, instance_revision: 0, instance_height_time: 0, instance_expiry: 0,
            build_profile: [Duration::ZERO; 6],
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
    /// Hand the slot back; applied to the ring at the start of the next build.
    fn release_slot(&mut self, slot: InstanceSlot) {
        if slot.id != u32::MAX { self.retired_slots.push(slot); }
    }

    fn cache_instance(&mut self, i: usize, ring: &mut gpu::InstanceRing, note: gpu::NoteInstance, expiry: u64) {
        let id = ring.push(note, expiry);
        self.instance_slots[i] = InstanceSlot { id, version: ring.version(id) };
    }

    /// Length of the note a window entry belongs to (see `OpenSegment::segment`), or 0 when not flattened.
    fn whole_us(&self, note: &NoteSegment) -> u64 {
        if self.collapse_overlaps { note.source as u64 } else { 0 } // 0: measure the clamped quad
    }

    fn set_collapse(&mut self, enabled: bool) {
        if self.collapse_overlaps != enabled {
            *self = Self { collapse_overlaps: enabled, max_instances: self.max_instances, generation: self.generation.wrapping_add(1), ..Default::default() };
        }
    }

    fn set_max_instances(&mut self, max: usize) {
        if self.max_instances != max {
            *self = Self { max_instances: max, collapse_overlaps: self.collapse_overlaps,
                generation: self.generation.wrapping_add(1), ..Default::default() };
        }
    }

    fn advance(&mut self, song: &Arc<SongData>, now_us: u64, window_end: u64, _px_us: u64) {
        let reset = now_us < self.time_us || now_us.saturating_sub(self.time_us) > 1_000_000
            || self.worker.as_ref().is_some_and(|w| !Arc::ptr_eq(&w.song, song));
        if reset {
            let generation = self.generation.wrapping_add(1);
            *self = Self { generation, collapse_overlaps: self.collapse_overlaps,
                max_instances: self.max_instances, ..Default::default() };
        }
        self.time_us = now_us;
        for key in &mut self.keys { key.added_start = u64::MAX; }
        self.timing.retain = Duration::ZERO;
        self.frames_since_sweep = self.frames_since_sweep.wrapping_add(1);
        let t0 = Instant::now();
        self.timing.decode = Duration::ZERO;
        self.timing.sort = Duration::ZERO;
        self.last_batch = 0;
        if self.worker.is_none() {
            let mut readers = vec![TrackReader::default(); song.midi.tracks.len()];
            let enabled = render_tracks(song);
            let mut flatten = self.collapse_overlaps.then(|| (0..128).map(|_| FlattenKey::default()).collect::<Vec<_>>());
            let reach = window_end.saturating_add(SYNC_AHEAD_US);
            let mut decoded = decode_tracks(song, &mut readers, now_us, reach, Some(&enabled));
            if let Some(keys) = &mut flatten { decoded = flatten_chunk(keys, decoded, reach, now_us); }
            let cutoff = song.tempo.tick_to_time_approx(song.tempo.time_to_tick(window_end), &mut 0);
            let first_future = decoded.notes.partition_point(|n| n.start_us < cutoff);
            let prepared = decoded.notes.split_off(first_future);
            let mut sizes = [0usize; 128];
            for note in decoded.notes.iter().chain(&prepared) { sizes[note.pitch as usize] += 1; }
            for (key, size) in self.keys.iter_mut().zip(sizes) {
                let size = size.saturating_mul(2);
                key.indices.reserve(size.saturating_sub(key.indices.len()));
                key.starts.reserve(size.saturating_sub(key.starts.len()));
                key.end_times.reserve(size.saturating_sub(key.end_times.len()));
            }
            let expected = (decoded.notes.len() + prepared.len()).saturating_mul(2);
            if self.collapse_overlaps {
                let open = decoded.notes.iter().chain(&prepared).filter(|n| n.end_us.saturating_add(1) >= reach).count();
                self.note_indices.reserve(open.saturating_mul(2));
            }
            self.selected_bits.reserve(expected.div_ceil(64).saturating_sub(self.selected_bits.len()));

            self.last_scanned = decoded.scanned;
            self.last_batch = decoded.notes.len();
            if self.collapse_overlaps {
                for (i, n) in decoded.notes.iter().enumerate() {
                    if n.end_us.saturating_add(1) >= reach { self.note_indices.insert(note_id(n), self.window.len() + i); }
                }
            }
            self.window.extend(decoded.notes);
            self.through = window_end;
            self.worker = Some(DecodeWorker::new(song.clone(), readers, reach, self.generation, flatten, prepared));
        }
        let mut worker = self.worker.take().unwrap();
        worker.now.store(now_us, Ordering::Release);
        worker.target.store(window_end.saturating_add(DECODE_AHEAD_US), Ordering::Release);
        worker.thread.unpark();
        // Keep taking batches until the window is covered (bounded by NOTE_BATCH below):
        // stopping after one left whole batches of in-window notes for the next frame.
        while self.last_batch == 0 || self.through < window_end {
            let urgent = self.through < now_us.saturating_add(100_000).min(window_end);
            let batch = worker.pending.take().or_else(|| {
                if urgent { worker.receiver.recv().ok() } else { worker.receiver.try_recv().ok() }
            });
            let Some(mut batch) = batch else { break };
            if batch.generation != self.generation { continue; }
            if !urgent && batch.notes.first().is_some_and(|n| n.start_us >= window_end) {
                worker.pending = Some(batch);
                break;
            }
            let cutoff = song.tempo.tick_to_time_approx(song.tempo.time_to_tick(window_end), &mut 0);
            let entered = batch.notes.partition_point(|n| n.start_us < cutoff);
            if entered < batch.notes.len() {
                let tail = batch.notes.split_off(entered);
                let through = tail[0].start_us;
                worker.pending = Some(NoteBatch { generation: batch.generation,
                    notes: tail, updates: Vec::new(), decoded_end: batch.decoded_end, through: batch.through });
                batch.through = through;
                if batch.notes.is_empty() && batch.updates.is_empty() { break; }
            }
            if !urgent && self.last_batch + batch.notes.len() + batch.updates.len() > NOTE_BATCH {
                worker.pending = Some(batch);
                break;
            }
            self.last_batch += batch.notes.len() + batch.updates.len();
            for note in batch.updates {
                if let Some(&i) = self.note_indices.get(&note_id(&note)) {
                    let Some(old) = self.window.get(i).copied() else { continue; };
                    let key = &mut self.keys[note.pitch as usize];
                    // Updates are rare; a binary search over the key's ascending indices finds it.
                    let p = key.indices.binary_search(&i).ok();
                    if let Some(p) = p {
                        key.end_times[p] = rendered_end(&note);
                        let block = p / NOTE_BLOCK;
                        key.ends[block] = key.end_times[block * NOTE_BLOCK..((block + 1) * NOTE_BLOCK).min(key.end_times.len())]
                            .iter().copied().max().unwrap_or(0);
                        let mut node = key.tree.len() / 2 + block;
                        key.tree[node] = key.ends[block];
                        while node > 1 {
                            node /= 2;
                            key.tree[node] = key.tree[node * 2].max(key.tree[node * 2 + 1]);
                        }
                    }
                    // An extension only changes rows from the old end on; using its start
                    // threw away every cached row after it (~40 ms once a second).
                    key.added_start = key.added_start.min(rendered_end(&old));
                    self.total_len_us = self.total_len_us.saturating_add(note.end_us.saturating_sub(old.end_us));
                    self.max_len_us = self.max_len_us.max(note.end_us - note.start_us);
                    self.window.replace(i, note);
                    if let Some(compact) = &mut key.compact {
                        if let Ok(p) = compact.indices.binary_search(&i) { compact.ends[p] = rendered_end(&note); }
                    }
                    if let Some(slot) = self.instance_slots.get(i).filter(|s| s.id != u32::MAX) {
                        self.release_slot(slot);
                        self.instance_slots[i] = InstanceSlot::EMPTY;
                    }
                    self.instance_dirty = true;
                    self.instance_expiry = 0;
                }
            }
            if self.collapse_overlaps {
                for (i, n) in batch.notes.iter().enumerate() {
                    if n.end_us.saturating_add(1) >= batch.decoded_end { self.note_indices.insert(note_id(n), self.window.len() + i); }
                }
            }
            self.window.extend(batch.notes);
            self.through = batch.through;
        }
        self.worker = Some(worker);
        self.timing.decode = t0.elapsed();
        let t0 = Instant::now();
        if self.last_batch > 0 { self.revision = self.revision.wrapping_add(1); }
        let retain = Instant::now();
        let pitches = self.window.expire(now_us);
        for (k, key) in self.keys.iter_mut().enumerate() {
            if pitches[k / 64] & (1 << (k % 64)) != 0 { key.prune = true; }
        }
        // An expired page's instance slots expire in the ring at the same moment (end +
        // hold), so nothing is released per note: visiting every slot of every page made the
        // frame a dense passage ends in (millions of notes at once) cost ~70 ms.
        while let Some((_, end)) = self.window.retired_range() {
            if end % NOTE_PAGE == 0 { self.instance_slots.free_chunk(end - 1); } // page fully retired
        }
        // The open-note map is looked up only by id; drop entries of expired pages in bulk.
        if self.frames_since_sweep % 60 == 0 {
            let window = &self.window;
            self.note_indices.retain(|_, &mut i| window.get(i).is_some());
        }
        self.alive = self.window.live;
        self.timing.retain = retain.elapsed();
        // Fully index the received batch so the exact pixel selector stays cheap.
        self.index_notes(self.indexed);
        // A pass walks every entry, so a fixed budget lets dead entries pile up to several
        // times the live ones when notes enter fast; scale with the index instead.
        let entries: usize = self.keys.iter().map(|k| k.indices.len()).sum();
        // Only while stale entries exceed half the live ones.
        if entries > self.window.live + self.window.live / 2 {
            self.compact_keys((entries / 128).clamp(50_000, 250_000));
        }
        self.timing.cull = t0.elapsed();
    }

    fn compact_keys(&mut self, mut budget: usize) {
        let window = &mut self.window;
        // Resume where the last frame ran out: restarting at key 0 starved the high keys,
        // since a finished key is re-armed by the next expiring page.
        for n in 0..128 {
            let k = (self.compact_from + n) % 128;
            if budget == 0 { self.compact_from = k; break; }
            let key = &mut self.keys[k];
            if key.compact.is_none() && key.prune {
                key.prune = false;
                key.compact = Some(KeyCompact {
                    indices: Vec::with_capacity(key.indices.len()), starts: Vec::with_capacity(key.starts.len()),
                    ends: Vec::with_capacity(key.end_times.len()), cursor: 0,
                });
            }
            let Some(compact) = &mut key.compact else { continue };
            let end = key.indices.len().min(compact.cursor + budget);
            budget -= end - compact.cursor;
            for p in compact.cursor..end {
                let i = key.indices[p];
                if let Some(note) = window.get(i).copied() {
                    compact.indices.push(i);
                    compact.starts.push(note.start_us);
                    compact.ends.push(rendered_end(&note));
                }
            }
            compact.cursor = end;
            if end == key.indices.len() {
                let compact = key.compact.take().unwrap();
                key.indices = compact.indices;
                key.starts = compact.starts;
                key.end_times = compact.ends;
                // Compaction reserved the old length; most of it is now dead.
                key.indices.shrink_to(key.indices.len() + key.indices.len() / 4);
                key.starts.shrink_to(key.starts.len() + key.starts.len() / 4);
                key.end_times.shrink_to(key.end_times.len() + key.end_times.len() / 4);
                key.ends.clear();
                key.ends.extend(key.end_times.chunks(NOTE_BLOCK).map(|block| *block.iter().max().unwrap()));
                key.tree.clear();
                key.update_tree(0);
            }
        }
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
            for (_, note) in self.window.entries_range(0, end) { sizes[note.pitch as usize & 127] += 1; }
            for (key, size) in self.keys.iter_mut().zip(sizes) {
                key.indices.reserve(size);
            }
            if end > 50_000 {
                for (i, note) in self.window.entries_range(0, end) {
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
            for (i, note) in self.window.entries_range(first, end) {
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
        for (i, note) in self.window.entries_range(first, end) {
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
            self.selected.extend(self.window.entries()
                .filter(|(_, n)| n.start_us < window_end && n.end_us + NOTE_HOLD_MICROS > now_us)
                .map(|(i, _)| i));
            return;
        }
        // The bitmap stays zeroed between frames (extraction clears what it reads) and a
        // summary bit per word means a frame visits only touched words, not every ID
        // ever entered, which grew with the length of the song.
        let base = self.window.live_base();
        let mut bits = std::mem::take(&mut self.selected_bits);
        let mut summary = std::mem::take(&mut self.selected_summary);
        bits.resize(self.window.len().div_ceil(64) - base / 64, 0);
        summary.resize(bits.len().div_ceil(64), 0);
        // An ID below `base` would be an expired note; never index out of range for it.
        let mark = |bits: &mut Vec<u64>, summary: &mut Vec<u64>, i: usize| if i >= base {
            let word = (i - base) / 64;
            bits[word] |= 1 << (i % 64);
            summary[word / 64] |= 1 << (word % 64);
        };
        let px_us = height_time as f64 * 1e6 / height_px.max(1) as f64;
        // Include uncertain edge samples as well: float rounding and the GPU's
        // subpixel raster grid must never hide an exposed colour/border pixel.
        // Rounded up to a multiple of 16: rows are cached per guard, and a guard that crept
        // up by one as max_len_us grew used to throw away every cached row (~40 ms).
        let guard = (2 + (((height_time as f64 * 1e6) as u64).saturating_add(self.max_len_us) >> 21)
            + (px_us / 256.0).ceil() as u64).next_multiple_of(16);
        let height = height_px.max(1) as usize;
        let times: Vec<_> = (0..height).map(|row|
            now_us.saturating_add(((row as f64 + 0.5) * px_us) as u64)).collect();
        self.row_counts = [0; 2];
        for key in &mut self.keys {
            if key.indices.is_empty() { continue; }
            let last = *key.indices.last().unwrap();
            if self.window.get(last).is_some_and(|n| n.start_us <= now_us && n.end_us >= window_end.saturating_add(guard)) {
                mark(&mut bits, &mut summary, last);
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
                let time = times[row];
                cached.hi = cached.hi.min(key.added_start.saturating_sub(guard));
                if cached.lo <= time && time < cached.hi && now_us < cached.expiry {
                    self.row_counts[0] += 1;
                } else {
                    key.query_row(&self.window, now_us, window_end, time, guard, cached);
                    self.row_counts[1] += 1;
                }
                if cached.first != usize::MAX {
                    let i = cached.first;
                    mark(&mut bits, &mut summary, i);
                }
                if cached.second != usize::MAX {
                    let i = cached.second;
                    mark(&mut bits, &mut summary, i);
                }
                for &i in &cached.rest { mark(&mut bits, &mut summary, i); }
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
        for (group, summary_word) in summary.iter_mut().enumerate() {
            while *summary_word != 0 {
                let block = group * 64 + summary_word.trailing_zeros() as usize;
                *summary_word &= *summary_word - 1;
                let mut word = std::mem::take(&mut bits[block]);
                while word != 0 {
                    self.selected.push(base + block * 64 + word.trailing_zeros() as usize);
                    word &= word - 1;
                }
            }
        }
        self.selected_bits = bits;
        self.selected_summary = summary;
        self.build_profile[2] = t0.elapsed();
    }

    fn dense(&self, height_time: f32, height_px: u32) -> bool {
        let key_count = self.keys.iter().filter(|key| !key.indices.is_empty()).count();
        // Query rows when notes compete for them, or long spans cause heavy
        // overdraw. An isolated long note must not slow a sparse window down.
        let overlap = ((height_time as f64 * 1e6) as u64).saturating_mul(key_count as u64)
            .saturating_mul(height_px.max(1) as u64) / 4;
        self.indexed == self.window.len() && self.window.live > 4096 && (self.window.live > height_px as usize * key_count * 2
            || self.total_len_us > overlap)
    }
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
    /// The look last pushed to egui and the song; `None` forces a re-apply.
    applied_look: Option<(theme::UiTheme, theme::NotePalette, Vec<[u8; 3]>)>,
    playback_time_us: u64,
    slider_override_us: Option<u64>,
    slider_grabbed: bool,
    /// Everything saved between runs (see `settings`).
    cfg: Settings,
    /// MIDI outputs (choice, display name) and the selected one.
    outputs: Vec<(output::OutputChoice, String)>,
    output_idx: usize,
    /// Playback speed multiplier.
    tempo: f64,
    log_stats: bool,
    /// The benchmark window (see `benchmark`).
    benchmark: Option<BenchRun>,
    loading: Option<Loading>,
    show_settings: bool,
    settings_tab: SettingsTab,
    settings_search: String,
    visible: VisibleNotes,
    /// Reused across frames; the GPU callback holds a clone until it renders.
    instances: Arc<gpu::InstanceRing>,
    /// Display refresh rate reported by the compositor (for the VSync label).
    refresh_rate_hz: Option<f32>,
    /// The last load error, shown until dismissed or a new load starts.
    status_message: Option<String>,
    fps: f32,
    last_frame: Option<Instant>,
    #[allow(dead_code)]
    timer_guard: TimerGuard,
    updater: update::Updater,
    frame_times: FrameTimes,
    frame_log: Option<FrameLog>,
    /// Where to seek once the file given on the command line has loaded.
    open_at_us: Option<u64>,
    export: export_ui::ExportUi,
}

#[derive(Clone, Copy, PartialEq)]
enum SettingsTab {
    Display,
    Audio,
    Loading,
    Performance,
    Benchmark,
}

impl eframe::App for AppState {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        match self.updater.poll() {
            update::Outcome::Restart => {
                self.save_settings();
                update::relaunch();
            }
            update::Outcome::Checked => self.save_settings(),
            update::Outcome::Nothing => {}
        }
        self.updater.window(ctx);
        if self.cfg.fps_limit > 0 {
            if let Some(prev) = self.last_frame {
                let target = prev + std::time::Duration::from_secs_f64(1.0 / self.cfg.fps_limit as f64);
                if let Some(left) = target.checked_duration_since(Instant::now()) {
                    // Sleep is coarse; spin the last stretch.
                    std::thread::sleep(left.saturating_sub(std::time::Duration::from_micros(1500)));
                    while Instant::now() < target {
                        std::hint::spin_loop();
                    }
                }
            }
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

        if !ctx.wants_keyboard_input() {
            let (open, cancel, back, fwd) = ctx.input_mut(|i| {
                (
                    i.consume_key(egui::Modifiers::COMMAND, egui::Key::O),
                    i.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
                    i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft),
                    i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight),
                )
            });
            if open {
                self.open_file();
            }
            if cancel {
                self.cancel_loading();
            }
            if self.current_song.is_some() && (back || fwd) {
                let target = if fwd { current_time_us.saturating_add(SEEK_STEP_US) } else { current_time_us.saturating_sub(SEEK_STEP_US) };
                self.seek(target.min(duration_us));
            }
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
                    egui::Slider::new(&mut self.cfg.note_speed, 0.01..=12.0)
                        .logarithmic(true)
                        .suffix(" s"),
                )
                .on_hover_text("Seconds a note takes to reach the keyboard");
                ui.separator();
                ui.toggle_value(&mut self.show_settings, "Settings").on_hover_text("Ctrl+,");
                self.updater.toolbar(ui);
            });
            let (mut cancel, mut dismiss_error) = (false, false);
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
                    cancel = ui.button("Cancel").on_hover_text("Esc").clicked();
                });
                if self.cfg.loading_details {
                    egui::Window::new("Loading").collapsible(false).resizable(false).show(ctx, |ui| loading_details(ui, l));
                }
            }
            if cancel {
                self.cancel_loading();
            }
            if dismiss_error {
                self.status_message = None;
            }
            match (&self.status_message, &self.current_song) {
                (Some(msg), _) => {
                    ui.horizontal(|ui| {
                        ui.colored_label(Color32::from_rgb(255, 110, 100), format!("⚠ {msg}"));
                        if ui.small_button("Dismiss").clicked() {
                            dismiss_error = true;
                        }
                    });
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
                    )
                    .on_hover_text("Click or drag to seek · ←/→ ±5 s");
                    if response.changed() {
                        self.slider_override_us = Some((secs.max(0.0) * 1_000_000.0) as u64);
                        self.slider_grabbed = true;
                    }
                    // Seek once the slider is let go (or right away for clicks and arrow keys).
                    if self.slider_grabbed && !response.is_pointer_button_down_on() {
                        if let Some(target) = self.slider_override_us.take() {
                            let target = target.min(duration_us);
                            self.seek(target);
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

        let look = (self.cfg.ui_theme, self.cfg.note_palette, self.cfg.custom_colors.clone());
        if self.applied_look.as_ref() != Some(&look) {
            if self.applied_look.as_ref().map(|l| l.0) != Some(look.0) {
                look.0.apply(ctx);
            }
            if let Some(song) = &mut self.current_song {
                song.set_palette(look.1, &look.2);
                self.visible.instance_ring_id = u64::MAX; // drops the cached note colors
            }
            self.applied_look = Some(look);
        }
        self.visible.set_collapse(self.cfg.collapse_overlaps);
        self.visible.set_max_instances(self.cfg.max_instances);
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
                    self.cfg.note_speed,
                    self.cfg.notes_rise,
                    self.cfg.flat_notes,
                    self.cfg.piano_layout,
                    (self.cfg.stats, self.cfg.synth_stats),
                    &mut self.visible,
                    &mut self.instances,
                    &mut self.frame_times,
                );
            });

        self.settings_window(ctx);
        self.export.window(ctx);
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

    /// Jump to `target_us`, keeping the clock running or paused as it was.
    fn seek(&mut self, target_us: u64) {
        let _ = self.cmd_tx.send(PlayerCommand::Seek(target_us));
        self.playback_time_us = target_us;
        if self.last_telemetry.is_playing {
            self.clock.set_running(target_us);
        } else {
            self.clock.set_paused(target_us);
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

    /// Remember settings between runs.
    fn save_settings(&self) {
        let mut cfg = self.cfg.clone();
        cfg.output = self.outputs.get(self.output_idx).map_or_else(String::new, |o| o.1.clone());
        cfg.check_updates = self.updater.enabled;
        cfg.last_update_check = self.updater.last_check;
        cfg.save();
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
        progress.verify_all.store(self.cfg.verify_repeats, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (vel_levels, p) = (self.cfg.velocity_levels, progress.clone());
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
        let name = self.loading.take().map(|l| l.name).unwrap_or_default();
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
                self.applied_look = None;
                self.playback_time_us = 0;
                self.slider_override_us = None;
                self.slider_grabbed = false;
                self.status_message = None;
                self.reset_visible_indices();
            }
            Err(e) => {
                self.status_message = Some(format!("Couldn't open {name}: {e}"));
            }
        }
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
    piano_layout: bool,
    instances: &mut Arc<gpu::InstanceRing>,
) -> ([bool; 128], [Color32; 128]) {
    if visible.instance_ring_id != instances.id() {
        *instances = Arc::new(gpu::InstanceRing::default());
        visible.instance_ring_id = instances.id();
        visible.retired_slots.clear();
        visible.instance_style = None;
        visible.selection.clear();
    }
    let t_retire = Instant::now();
    visible.build_profile = [Duration::ZERO; 6];
    if !visible.retired_slots.is_empty() {
        let ring = Arc::make_mut(instances);
        for slot in visible.retired_slots.drain(..) { ring.release(slot.id, slot.version); }
    }
    visible.build_profile[4] = t_retire.elapsed();
    let mut active_keys = [false; 128];
    let mut active_colors = [Color32::TRANSPARENT; 128];
    let dense = visible.dense(height_time, height_px);
    let style = (piano_layout, dense);
    if !dense {
        // Sparse windows are cheaper to visit directly than to query every row.
        let write = visible.instance_dirty || visible.instance_style != Some(style) || visible.instance_revision != visible.revision
            || visible.instance_height_time != height_time.to_bits() || now_us >= visible.instance_expiry;
        if write {
            let buf = Arc::make_mut(instances);
            buf.begin(now_us);
                let mut expiry = u64::MAX;
            let passes = if piano_layout { 2 } else { 1 };
            let indices: Vec<_> = (0..passes).flat_map(|pass| visible.window.entries().rev()
                .filter(move |(_, n)| !piano_layout || is_black_key(n.pitch) == (pass == 0)))
                .filter(|(_, n)| n.start_us < window_end && n.end_us + NOTE_HOLD_MICROS > now_us)
                .map(|(i, _)| i).collect();
            for i in indices {
                let note = visible.window[i];
                let color = song.color_for(&note);
                if note.start_us <= now_us && note.end_us > now_us && !active_keys[note.pitch as usize] {
                    active_keys[note.pitch as usize] = true;
                    active_colors[note.pitch as usize] = color;
                }
                expiry = expiry.min(note.end_us + NOTE_HOLD_MICROS);
                let slot = visible.instance_slots[i];
                if slot.id != u32::MAX && buf.version(slot.id) == slot.version { buf.push_slot(slot.id); continue; }
                visible.release_slot(slot);
                visible.cache_instance(i, buf, gpu::NoteInstance::new(&note, color, visible.whole_us(&note)), note.end_us + NOTE_HOLD_MICROS);
            }
            visible.instance_expiry = expiry;
        } else {
            for note in visible.window.iter().rev() {
                if note.start_us <= now_us && note.end_us > now_us && !active_keys[note.pitch as usize] {
                    active_keys[note.pitch as usize] = true;
                    active_colors[note.pitch as usize] = song.color_for(note);
                }
            }
        }
        visible.instance_style = Some(style);
        visible.instance_revision = visible.revision;
        visible.instance_height_time = height_time.to_bits();
        if instances.len() > visible.max_instances && visible.max_instances > 0 {
            Arc::make_mut(instances).cap(visible.max_instances, now_us, piano_layout);
        }
        visible.instance_dirty = false;
        return (active_keys, active_colors);
    }
    // Keyboard state includes sounding notes too short to cover a raster sample.
    let t0 = Instant::now();
    for (pitch, key) in visible.keys.iter().enumerate() {
        let mut before = key.starts.partition_point(|&start| start <= now_us);
        while let Some(p) = key.covering(before, now_us) {
            let Some(note) = visible.window.get(key.indices[p]) else { before = p; continue; };
            if note.end_us > now_us {
                active_keys[pitch] = true;
                active_colors[pitch] = song.color_for(note);
                break;
            }
            before = p;
        }
    }
    visible.build_profile[0] = t0.elapsed();
    visible.select_pixels(now_us, window_end, height_time, height_px);
    let t0 = Instant::now();
    if visible.instance_dirty || visible.instance_style != Some(style) || visible.selected != visible.selection {
        let buf = Arc::make_mut(instances);
        buf.begin(now_us);
        // Newest first; in the piano layout one pass routes naturals behind the sharps.
        let selected = std::mem::take(&mut visible.selected);
        for &i in selected.iter().rev() {
            let slot = visible.instance_slots[i];
            if piano_layout { buf.route_naturals(!is_black_key(visible.window[i].pitch)); }
            if slot.id != u32::MAX && buf.version(slot.id) == slot.version {
                buf.push_slot(slot.id);
                continue;
            }
            visible.release_slot(slot);
            let note = visible.window[i];
            let color = song.color_for(&note);
            visible.cache_instance(i, buf, gpu::NoteInstance::new(&note, color, visible.whole_us(&note)), note.end_us + NOTE_HOLD_MICROS);
        }
        buf.finish();
        visible.selected = selected;
        std::mem::swap(&mut visible.selection, &mut visible.selected);
        visible.instance_style = Some(style);
    }
    visible.instance_dirty = false;
    visible.build_profile[3] = t0.elapsed();
    let t0 = Instant::now();
    if instances.len() > visible.max_instances && visible.max_instances > 0 {
        Arc::make_mut(instances).cap(visible.max_instances, now_us, piano_layout);
    }
    visible.build_profile[5] = t0.elapsed();
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
        let n = |start_us, end_us| NoteSegment { source: 0, start_us, end_us, track: 0, channel: 0, pitch: 60 };
        let mut prev = n(100, 500);
        assert!(!n(101, 500).absorb(&mut prev)); // starts a µs later: prev shows above it
        assert!(!n(100, 499).absorb(&mut prev)); // ends sooner: prev shows below it
        assert!(n(100, 500).absorb(&mut prev)); // same span: hidden
        assert!(n(100, 800).absorb(&mut prev)); // same start, longer: hidden
        assert_eq!((prev.start_us, prev.end_us), (100, 800));
    }

    #[test]
    fn culls_only_fully_hidden_notes() {
        let n = |start_us, end_us, pitch| NoteSegment { source: 0, start_us, end_us, track: 0, channel: 0, pitch };
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
            notes.extend((0..4500).map(|_| NoteSegment { source: 0,
                start_us: 9_000_000, end_us: 10_000_000, track: 0, channel: 0, pitch: 59,
            }));
        }
        notes.sort_by_key(|n| (n.start_us, n.track));
        let mut visible = VisibleNotes { window: notes.into(), ..Default::default() };
        visible.index_notes(0);
        visible
    }

    /// Simple painter's algorithm at pixel centres, including the shader's clipped
    /// geometry, minimum length and border shading. Input is back to front.
    fn raster(
        song: &VisualSong, notes: &[NoteSegment], now: u64, speed: f32,
        height: u32, piano: bool, flat: bool, upwards: bool,
    ) -> Vec<[u8; 3]> {
        raster_with(song, notes, now, speed, height, piano, flat, upwards, false)
    }

    /// `flattened`: segments carry their whole note's length in `source` (thin notes darken
    /// by that, not by the visible piece).
    fn raster_with(
        song: &VisualSong, notes: &[NoteSegment], now: u64, speed: f32,
        height: u32, piano: bool, flat: bool, upwards: bool, flattened: bool,
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
            let color = song.color_for(note);
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
                    let (start_border, end_border) = (note.channel & NO_START_BORDER == 0, note.channel & NO_END_BORDER == 0);
                    let border = u < hx || u > 1.0 - hx || (start_border && v < hy) || (end_border && v > 1.0 - hy);
                    let measured = if flattened { note.source as f32 / 1e6 / speed * height as f32 * 2.0 } else { (c0 - c1).abs() * height as f32 };
                    let dark = border || measured < 7.0;
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
                            raster(song, &selected, now, speed, height, piano, flat, upwards),
                            raster(song, &visible.window.iter().copied().collect::<Vec<_>>(), now, speed, height, piano, flat, upwards),
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
        let n = |start_us, end_us, track, pitch| NoteSegment { source: 0, start_us, end_us, track, channel: track as u8, pitch };
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
        let mut instances = Arc::new(gpu::InstanceRing::default());
        build_note_instances(&song, &mut visible, 0, 100_000, 0.1, 64, false, &mut instances);
        for instance in instances.iter().filter(|n| n.key_color & 255 == 60) {
            let start = instance.start_us[0] as u64;
            let note = surviving.iter().find(|n| n.start_us == start &&
                ((n.end_us - n.start_us) as f32 / 1e6).max(0.000_1) == instance.len &&
                ((song.color_for(n).r() as u32) << 24 |
                 (song.color_for(n).g() as u32) << 16 |
                 (song.color_for(n).b() as u32) << 8 | 60) == instance.key_color);
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
            NoteSegment { source: 0,
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
            NoteSegment { source: 0,
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
        let mut visible = indexed((0..5000).map(|i| NoteSegment { source: 0,
            start_us: i * 100, end_us: i * 100 + 100, track: (i % 3) as u16,
            channel: (i % 3) as u8, pitch: 60,
        }).collect(), false);
        let mut instances = Arc::new(gpu::InstanceRing::default());
        build_note_instances(&song, &mut visible, 0, 500_000, 0.5, 100, true, &mut instances);
        let coarse = instances.clone();
        build_note_instances(&song, &mut visible, 0, 500_000, 0.5, 100, true, &mut instances);
        assert!(Arc::ptr_eq(&coarse, &instances));
        build_note_instances(&song, &mut visible, 0, 500_000, 0.5, 10_000, true, &mut instances);
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
        let mut a = Arc::new(gpu::InstanceRing::default());
        let mut b = Arc::new(gpu::InstanceRing::default());
        let build = |visible: &mut VisibleNotes, buf: &mut Arc<gpu::InstanceRing>, now| {
            build_note_instances(&song, visible, now, now + 3_000_000, 3.0, 900, true, buf);
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
    fn worker_reset_discards_pending_batches_and_same_track_count_songs() {
        let song = test_song();
        let mut visible = VisibleNotes::default();
        visible.advance(&song.data, 2_000_000, 3_000_000, 1000);
        let generation = visible.generation;
        visible.worker.as_mut().unwrap().pending = Some(NoteBatch {
            generation, decoded_end: 9_000_000, updates: Vec::new(), notes: vec![NoteSegment { source: 0, start_us: 2_500_000, end_us: 9_000_000,
                track: 99, channel: 0, pitch: 60 }], through: 9_000_000,
        });
        visible.advance(&song.data, 0, 1_000_000, 1000);
        assert_ne!(visible.generation, generation);
        assert!(visible.window.iter().all(|n| n.track != 99));
        let generation = visible.generation;
        visible.advance(&test_song().data, 0, 1_000_000, 1000);
        assert_ne!(visible.generation, generation);
    }

    #[test]
    fn identical_packed_tracks_only_drop_fully_hidden_earlier_copies() {
        let mut file = b"MThd\0\0\0\x06\0\x01\0\x03\x01\xe0".to_vec();
        for delta in [0, 0, 1] {
            let body = [delta, 0x90, 60, 100, 0x9e, 0, 0x80, 60, 0, 0, 0xff, 0x2f, 0];
            file.extend_from_slice(b"MTrk");
            file.extend_from_slice(&(body.len() as u32).to_be_bytes());
            file.extend(body);
        }
        let midi = Arc::new(parser::MidiFile::parse_stream(Box::new(parser::Plain(std::io::Cursor::new(file))),
            127, &parser::Progress::default()).unwrap());
        let tempo = Arc::new(song::TempoMap::from_midi(&midi));
        let data = SongData { path: String::new(), midi, tempo, load_time: Duration::ZERO };
        let enabled = render_tracks(&data);
        assert_eq!(enabled, [false, true, true]);
        let readers = vec![TrackReader::default(); data.midi.tracks.len()];
        let exact = decode_notes(&data, &mut readers.clone(), 0, 20_000_000);
        let culled = decode_tracks(&data, &mut readers.clone(), 0, 20_000_000, Some(&enabled));
        assert_eq!(exact.notes, culled.notes);
    }

    fn flatten_all(notes: &[NoteSegment], horizon: u64) -> Vec<NoteSegment> {
        let mut keys: Vec<_> = (0..128).map(|_| FlattenKey::default()).collect();
        let mut sorted = notes.to_vec();
        sorted.sort_by_key(|n| n.start_us); // stable: ties keep track order
        flatten_chunk(&mut keys, Decoded::new(sorted), horizon, 0).notes
    }

    #[test]
    fn flatten_hides_covered_notes_and_exposes_the_older_tail() {
        let n = |start_us, end_us, source| NoteSegment { source, start_us, end_us, track: source as u16, channel: 0, pitch: 60 };
        // A 0..100, B 20..50 on top, C 30..40 on top of B, D 10..20 fully hidden by B? (no: D ends before B).
        let notes = [n(0, 100_000, 0), n(20_000, 50_000, 1), n(30_000, 40_000, 2), n(60_000, 70_000, 3), n(65_000, 66_000, 4)];
        let flat = flatten_all(&notes, 1_000_000);
        let view: Vec<_> = flat.iter().map(|s| (s.start_us, s.end_us, s.track as u32, s.channel)).collect();
        // A until B starts (cut: no end border), B until C (cut), C whole, B resumes (no start border),
        // A resumes (no start border), D over A, E over D inside it, D resumes, A resumes.
        assert_eq!(view, [
            (0, 20_000, 0, NO_END_BORDER), (20_000, 30_000, 1, NO_END_BORDER), (30_000, 40_000, 2, 0),
            (40_000, 50_000, 1, NO_START_BORDER), (50_000, 60_000, 0, NO_START_BORDER | NO_END_BORDER),
            (60_000, 65_000, 3, NO_END_BORDER), (65_000, 66_000, 4, 0), (66_000, 70_000, 3, NO_START_BORDER),
            (70_000, 100_000, 0, NO_START_BORDER),
        ]);
    }

    #[test]
    fn flatten_drops_a_note_covered_from_its_start() {
        let n = |start_us, end_us, source| NoteSegment { source, start_us, end_us, track: source as u16, channel: 0, pitch: 60 };
        // The later track wins an equal start; the earlier note is never visible.
        let flat = flatten_all(&[n(0, 10_000, 0), n(0, 20_000, 1)], 1_000_000);
        assert_eq!(flat.iter().map(|s| (s.start_us, s.end_us, s.track)).collect::<Vec<_>>(), [(0, 20_000, 1)]);
    }

    #[test]
    fn flatten_extends_an_open_segment_across_chunks() {
        let n = |start_us, end_us| NoteSegment { source: 0, start_us, end_us, track: 0, channel: 0, pitch: 60 };
        let mut keys: Vec<_> = (0..128).map(|_| FlattenKey::default()).collect();
        let first = flatten_chunk(&mut keys, Decoded::new(vec![n(10, 300)]), 100, 0);
        assert_eq!(first.notes.len(), 1);
        assert_eq!((first.notes[0].start_us, first.notes[0].end_us), (10, 100));
        assert_eq!(first.notes[0].channel, NO_END_BORDER);
        let second = flatten_chunk(&mut keys, Decoded::new(Vec::new()), 200, 0);
        assert!(second.notes.is_empty());
        assert_eq!(second.updates.len(), 1);
        assert_eq!(note_id(&second.updates[0]), note_id(&first.notes[0]));
        assert_eq!(second.updates[0].end_us, 200);
        // The note's real end closes the segment with an end border.
        let third = flatten_chunk(&mut keys, Decoded::new(Vec::new()), 400, 0);
        assert_eq!(third.updates.len(), 1);
        assert_eq!((third.updates[0].end_us, third.updates[0].channel), (300, 0));
        assert!(flatten_chunk(&mut keys, Decoded::new(Vec::new()), 500, 0).updates.is_empty());
    }

    #[test]
    fn flatten_matches_the_unflattened_painter() {
        let song = test_song();
        let mut seed = 7u64;
        let mut rand = move |m: u64| { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; seed % m };
        // Overlapping notes on four keys and three tracks. Every note is taller than the
        // 7 px "thin note" threshold at the heights below: pieces of a thin note do not darken.
        let notes: Vec<_> = (0..400).map(|_| {
            let start = rand(90_000);
            let track = rand(3);
            NoteSegment { source: track as u32, start_us: start, end_us: start + 20_000 + rand(40_000),
                track: track as u16, channel: track as u8, pitch: 59 + rand(4) as u8 }
        }).collect();
        let mut sorted = notes.clone();
        sorted.sort_by_key(|n| (n.start_us, n.source));
        let flat = flatten_all(&sorted, 1_000_000);
        for key in 59..63 {
            let mut on_key: Vec<_> = flat.iter().filter(|s| s.pitch == key).collect();
            on_key.sort_by_key(|s| s.start_us);
            assert!(on_key.windows(2).all(|w| w[0].end_us <= w[1].start_us), "segments overlap on key {key}");
        }
        for height in [64, 300] {
            for now in [0, 5_000, 23_456] {
                for by_channel in [false, true] {
                    for piano in [false, true] {
                        let a = raster_with(&song, &flat, now, 0.1, height, piano, false, false, true);
                        let b = raster(&song, &sorted, now, 0.1, height, piano, false, false);
                        let differing: Vec<_> = a.iter().zip(&b).enumerate().filter(|(_, (x, y))| x != y).map(|(i, _)| (i / 48, i % 48)).take(5).collect();
                        assert!(differing.is_empty(), "height={height} now={now} channel={by_channel} piano={piano}: first differing (row, col) {differing:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn flatten_matches_the_painter_for_stacked_restrikes() {
        let song = test_song();
        // Ten-second notes restruck every 8.8 ms on one key: the newest one is cut after a few rows.
        let notes: Vec<_> = (0..400u64).map(|i| NoteSegment { source: 0, start_us: i * 8_800, end_us: i * 8_800 + 10_000_000,
            track: 0, channel: 0, pitch: 60 }).collect();
        let flat = flatten_all(&notes, 100_000_000);
        for height in [64, 300, 900] {
            for now in [3_500_000, 3_502_000, 3_505_000] {
                let a = raster_with(&song, &flat, now, 5.0, height, false, false, false, true);
                let b = raster(&song, &notes, now, 5.0, height, false, false, false);
                let differing: Vec<_> = a.iter().zip(&b).enumerate().filter(|(_, (x, y))| x != y).map(|(i, _)| (i / 48, i % 48)).take(4).collect();
                assert!(differing.is_empty(), "height={height} now={now}: first differing (row, col) {differing:?}");
            }
        }
    }

    #[test]
    fn chunked_flatten_with_updates_equals_one_pass() {
        let mut seed = 99u64;
        let mut rand = move |m: u64| { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; seed % m };
        let mut notes: Vec<_> = (0..600).map(|_| {
            let start = rand(200_000);
            let track = rand(3);
            NoteSegment { source: track as u32, start_us: start, end_us: start + 1_000 + rand(120_000),
                track: track as u16, channel: track as u8, pitch: 58 + rand(5) as u8 }
        }).collect();
        notes.sort_by_key(|n| (n.start_us, n.source));
        let mut whole = flatten_all(&notes, 10_000_000);
        // Same notes fed 7 ms at a time; a consumer keeps segments by id and applies updates.
        let mut keys: Vec<_> = (0..128).map(|_| FlattenKey::default()).collect();
        let mut store: std::collections::HashMap<NoteId, NoteSegment> = Default::default();
        let step = 7_000u64;
        let mut horizon = step;
        while horizon < 400_000 {
            let chunk: Vec<_> = notes.iter().copied().filter(|n| n.start_us < horizon && n.start_us >= horizon - step).collect();
            let out = flatten_chunk(&mut keys, Decoded::new(chunk), horizon, 0);
            for u in out.updates { assert!(store.insert(note_id(&u), u).is_some(), "update for a segment never sent: {u:?}"); }
            for n in out.notes { assert!(store.insert(note_id(&n), n).is_none(), "segment sent twice: {n:?}"); }
            horizon += step;
        }
        let mut chunked: Vec<_> = store.into_values().collect();
        let key = |n: &NoteSegment| (n.pitch, n.start_us, n.track);
        chunked.sort_by_key(key);
        whole.sort_by_key(key);
        assert_eq!(chunked.len(), whole.len());
        for (a, b) in chunked.iter().zip(&whole) { assert_eq!(a, b); }
    }

    /// A format-1 file of `tracks` tracks with random overlapping notes on keys 59-62.
    fn overlapping_song(tracks: usize, notes: usize, seed: u64) -> VisualSong {
        let mut seed = seed;
        let mut rand = move |m: u64| { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; seed % m };
        let mut file = b"MThd\0\0\0\x06\0\x01".to_vec();
        file.extend_from_slice(&(tracks as u16).to_be_bytes());
        file.extend_from_slice(&480u16.to_be_bytes()); // 120 BPM: ~1042 us per tick
        for track in 0..tracks {
            let mut events: Vec<(u64, u8, u8)> = Vec::new();
            for _ in 0..notes {
                let start = rand(5_000);
                let key = 59 + rand(4) as u8;
                events.push((start, 0x90 | track as u8, key));
                events.push((start + 5 + rand(1_500), 0x80 | track as u8, key));
            }
            events.sort_by_key(|e| (e.0, e.1 >> 4)); // offs (8) before ons (9) at a tick
            let mut body = Vec::new();
            let mut last = 0;
            for (tick, status, key) in events {
                let mut delta = tick - last;
                last = tick;
                let mut bytes = vec![(delta & 127) as u8];
                delta >>= 7;
                while delta > 0 { bytes.push((delta & 127) as u8 | 128); delta >>= 7; }
                body.extend(bytes.iter().rev());
                body.extend_from_slice(&[status, key, if status >> 4 == 9 { 100 } else { 0 }]);
            }
            body.extend_from_slice(&[0, 0xff, 0x2f, 0]);
            file.extend_from_slice(b"MTrk");
            file.extend_from_slice(&(body.len() as u32).to_be_bytes());
            file.extend(body);
        }
        let midi = Arc::new(parser::MidiFile::parse_stream(
            Box::new(parser::Plain(std::io::Cursor::new(file))), 127, &parser::Progress::default(),
        ).unwrap());
        let tempo = Arc::new(song::TempoMap::from_midi(&midi));
        VisualSong::new(Arc::new(SongData { path: String::new(), midi, tempo, load_time: Duration::ZERO }))
    }

    #[test]
    fn flatten_pipeline_matches_the_exact_painter_frame_after_frame() {
        let song = overlapping_song(3, 250, 5);
        let mut readers = vec![TrackReader::default(); song.data.midi.tracks.len()];
        let mut exact = decode_notes(&song.data, &mut readers, 0, 30_000_000).notes;
        exact.sort_by_key(|n| (n.start_us, n.source));
        let mut visible = VisibleNotes::default();
        assert!(visible.collapse_overlaps);
        let speed = 0.5f32;
        let mut now = 0u64;
        while now < 5_400_000 {
            let end = now + (speed as f64 * 1e6) as u64;
            visible.advance(&song.data, now, end, 5000);
            // Covered once the frontier passed the window, or the next waiting batch starts beyond it.
            let cutoff = song.data.tempo.tick_to_time_approx(song.data.tempo.time_to_tick(end), &mut 0); // as `advance` rounds it
            let covered = |v: &VisibleNotes| v.through >= end || v.worker.as_ref().is_some_and(|w|
                w.pending.as_ref().is_some_and(|b| b.notes.first().is_some_and(|n| n.start_us >= cutoff)));
            for _ in 0..2000 {
                if covered(&visible) { break; }
                std::thread::sleep(Duration::from_millis(1));
                visible.advance(&song.data, now, end, 5000);
            }
            assert!(covered(&visible), "worker never caught up at {now}: through={} end={end} first pending start={:?}", visible.through, visible.worker.as_ref().and_then(|w| w.pending.as_ref()).map(|b| b.notes.first().map(|n| n.start_us)));
            let flat: Vec<_> = visible.window.iter().copied().collect();
            for height in [100, 300] {
                let a = raster_with(&song, &flat, now, speed, height, false, false, false, true);
                let b = raster(&song, &exact, now, speed, height, false, false, false);
                // Within 8 rows of either window edge the reference darkens and outlines any note
                // its clamped quad makes thin or cuts off; a piece of a longer note does neither.
                let differing: Vec<_> = a.iter().zip(&b).enumerate().filter(|(i, (x, y))| x != y && (8..height as usize - 8).contains(&(i / 48)))
                    .map(|(i, _)| (i / 48, i % 48)).take(3).collect();
                if let Some(&(row, col)) = differing.first() {
                    let t = now as f64 + (row as f64 + 0.5) / height as f64 * speed as f64 * 1e6;
                    let near = |notes: &[NoteSegment]| notes.iter().filter(|n| n.pitch == 59 + (col / 12) as u8 && (n.start_us as f64) < t + 3000.0 && n.end_us as f64 > t - 3000.0)
                        .map(|n| (n.start_us, n.end_us, n.source, n.channel)).collect::<Vec<_>>();
                    panic!("now={now} height={height}: first differing (row, col) {differing:?}, row time {t:.0}
 flat: {:?}
 exact: {:?}", near(&flat), near(&exact));
                }
            }
            now += 33_000;
        }
    }

    #[test]
    fn no_overlap_flatten_matches_exact_notes_and_pixels() {
        let song = test_song();
        let mut readers = vec![TrackReader::default(); song.data.midi.tracks.len()];
        let enabled = render_tracks(&song.data);
        let exact = decode_tracks(&song.data, &mut readers, 0, 20_000_000, Some(&enabled)).notes;
        let fields = |notes: &[NoteSegment]| notes.iter().map(|n| (n.start_us, n.end_us, n.track, n.channel, n.pitch)).collect::<Vec<_>>();
        assert_eq!(fields(&flatten_all(&exact, 20_000_000)), fields(&exact));
        let mut visible = indexed(exact, false);
        assert_raster_equal(&song, &mut visible, 123_456, 3.0, 900);
    }

    #[test]
    fn equal_start_batches_preserve_track_order() {
        let song = test_song();
        let mut readers = vec![TrackReader::default(); song.data.midi.tracks.len()];
        let decoded = decode_notes(&song.data, &mut readers, 0, 1_000_000);
        // Identical spans on all three tracks: highest track must win.
        assert_eq!(decoded.notes.len(), 1);
        assert_eq!(decoded.notes[0].track, 2);
        assert_eq!(decoded.notes[0].start_us, 0);
    }

    #[test]
    fn mass_page_expiry_is_spread_over_frames() {
        // One sustained note pins every page; when it ends they are all due at once.
        let pages = 700;
        let note = |i: usize| NoteSegment { source: 0, start_us: i as u64, end_us: 1_000_000,
            track: 0, channel: 0, pitch: 60 };
        let mut window = NoteWindow::from((0..NOTE_PAGE * pages).map(note).collect::<Vec<_>>());
        let mut calls = 0;
        while window.live > 0 {
            let before = window.live;
            window.expire(2_000_000);
            assert!(before - window.live <= 256 * NOTE_PAGE, "released too many pages in one frame");
            calls += 1;
            assert!(calls < 100);
        }
        assert!(calls >= 3);
        let mut ranges = 0;
        while window.retired_range().is_some() { ranges += 1; }
        assert_eq!(ranges, pages);
    }

    #[test]
    fn expired_pages_preserve_note_ids_and_partial_tail_reuse() {
        let count = NOTE_PAGE * 2 + 37;
        let note = |i: usize| NoteSegment { source: 0,
            start_us: i as u64, end_us: if i < NOTE_PAGE { i as u64 + 10 } else { 1_000_000 },
            track: 0, channel: 0, pitch: 60,
        };
        let mut window = NoteWindow::from((0..count).map(note).collect::<Vec<_>>());
        window.expire(20_000);
        assert!(window.get(0).is_none());
        assert_eq!(window[NOTE_PAGE], note(NOTE_PAGE));
        assert_eq!(window.iter().count(), NOTE_PAGE + 37);
        window.expire(1_100_000);
        let fresh = NoteSegment { start_us: 2_000_000, end_us: 2_100_000, ..note(0) };
        window.extend(vec![fresh]);
        assert_eq!(window.len(), count + 1);
        assert_eq!(window[count], fresh);
        assert_eq!(window.iter().count(), 1);
        assert_eq!(window.entries_range(0, count - 1).count(), 0);
        let mut retired = Vec::new();
        while let Some((first, end)) = window.retired_range() { retired.extend(first..end); }
        assert_eq!(retired, (0..count).collect::<Vec<_>>());
    }

    #[test]
    fn page_expiration_and_index_cleanup_keep_cached_pixels_equal() {
        let song = test_song();
        let notes = (0..20_000u64).map(|i| NoteSegment { source: 0,
            start_us: i * 10, end_us: i * 10 + 100_000 + i % 1000,
            track: (i % 3) as u16, channel: (i % 3) as u8, pitch: 60 + (i % 12) as u8,
        }).collect();
        let mut visible = indexed(notes, false);
        let mut instances = Arc::new(gpu::InstanceRing::default());
        build_note_instances(&song, &mut visible, 260_000, 760_000, 0.5, 100, true, &mut instances);
        let mut readers = vec![TrackReader::default(); song.data.midi.tracks.len()];
        decode_notes(&song.data, &mut readers, 0, 20_000_000);
        visible.worker = Some(DecodeWorker::new(song.data.clone(), readers, 20_000_000, visible.generation, None, Vec::new()));
        visible.through = 20_000_000;
        visible.time_us = 260_000;
        visible.advance(&song.data, 280_000, 780_000, 5000);
        assert!(visible.window.get(0).is_none());
        assert!(visible.window.live < visible.window.len());
        for frame in 0..20 {
            assert_raster_equal(&song, &mut visible, 280_000 + frame * 1000, 0.5, 100);
        }
    }

    #[test]
    fn sparse_cache_expires_notes_and_updates_colour_on_palette_change() {
        let mut song = test_song();
        let mut visible = indexed(vec![
            NoteSegment { source: 0, start_us: 0, end_us: 2000, track: 1, channel: 2, pitch: 60 },
            NoteSegment { source: 0, start_us: 5000, end_us: 10_000, track: 1, channel: 2, pitch: 60 },
        ], false);
        let mut instances = Arc::new(gpu::InstanceRing::default());
        build_note_instances(&song, &mut visible, 0, 10_000, 0.01, 100, false, &mut instances);
        assert_eq!(instances.len(), 2);
        build_note_instances(&song, &mut visible, 4000, 14_000, 0.01, 100, false, &mut instances);
        assert_eq!(instances.len(), 1);
        let old_colour = instances[0].key_color;
        song.set_palette(theme::NotePalette::Pitch, &[]);
        visible.instance_ring_id = u64::MAX; // what the UI does on a palette change
        let (active, colours) = build_note_instances(&song, &mut visible, 6000, 16_000, 0.01, 100, false, &mut instances);
        assert_ne!(old_colour, instances[0].key_color);
        assert!(active[60]);
        assert_eq!(colours[60], song.color_for(&visible.window[1]));
    }
}
