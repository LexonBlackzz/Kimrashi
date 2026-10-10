mod omnimidi;
mod output;
mod archive;
mod bench;
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
    if bench::run_from_args() {
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
        for (sum, d) in self.parts.iter_mut().zip([a.retain, a.decode, a.sort, a.cull, t.build]) {
            *sum += d;
        }
        self.instances = self.instances.max(t.instances);
        self.decoded += t.decoded;
        let wall = self.since.elapsed();
        if wall >= Duration::from_secs(1) {
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
#[derive(Clone, Copy)]
struct NoteSegment {
    start_us: u64,
    end_us: u64,
    track: u16,
    channel: u8,
    pitch: u8,
}

impl NoteSegment {
    /// If this note, drawn after `prev` on the same key, covers all of it but at most
    /// `slack_us` at either end, fold `prev` into it and return true. With no slack that
    /// is exact (the picture doesn't change): same start, and at least as long. With a
    /// pixel of slack, a sliver of `prev` may take the colour of this note instead.
    /// The merged note keeps `prev`'s start: dropping that sliver instead leaves gaps,
    /// since pixel rows don't line up with any fixed time grid while notes scroll.
    fn absorb(&self, prev: &mut NoteSegment, slack_us: u64) -> bool {
        let covers = self.start_us.saturating_sub(prev.start_us) <= slack_us && self.end_us + slack_us >= prev.end_us;
        if covers {
            *prev = NoteSegment { start_us: prev.start_us.min(self.start_us), end_us: prev.end_us.max(self.end_us), ..*self };
        }
        covers
    }
}

/// Push `note`, or merge it into the last note pushed on its key (see `absorb`).
/// `last` holds, per key, the index in `out` of the last note pushed there; only
/// those from `base` on count (earlier ones belong to other tracks).
/// This is what keeps polyphony bombs bounded: at most ~one note per key per pixel row.
fn push_culled(out: &mut Vec<NoteSegment>, last: &mut [usize; 128], base: usize, note: NoteSegment, slack_us: u64) {
    let k = note.pitch as usize & 127;
    let prev = if last[k] >= base { out.get_mut(last[k]) } else { None };
    if !prev.is_some_and(|prev| note.absorb(prev, slack_us)) {
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

/// Notes kept on screen while culling is exact. Past this it also merges notes that are
/// nearly hidden, back to exact once there are a third as many.
const EXACT_MAX: usize = 3_000_000;

/// Notes on screen. Each note is decoded from the packed tracks once, when it enters
/// the window, and dropped once it has finished.
#[derive(Default)]
struct VisibleNotes {
    time_us: u64,
    /// Per track: position of the first record not yet decoded.
    readers: Vec<TrackReader>,
    window: Vec<NoteSegment>,
    /// Coarsest time-per-pixel used to cull notes now in `window`.
    px_us: u64,
    /// Notes decoded last frame (after culling), and records looked at, to decide
    /// whether decoding is worth parallelizing.
    last_batch: usize,
    last_scanned: usize,
    /// Notes in `window` after the last sweep of finished ones, and frames since.
    alive: usize,
    frames_since_sweep: u32,
    /// Whether culling merges nearly-hidden notes (see `EXACT_MAX`).
    lossy: bool,
    /// Where the last `advance` spent its time (for `FrameLog`).
    timing: AdvanceTimes,
}

#[derive(Default, Clone, Copy)]
struct AdvanceTimes {
    retain: Duration,
    decode: Duration,
    sort: Duration,
    cull: Duration,
}

impl VisibleNotes {
    /// `px_us`: microseconds per vertical pixel, for culling hidden notes.
    fn advance(&mut self, song: &SongData, now_us: u64, window_end: u64, px_us: u64) {
        let tracks = &song.midi.tracks;
        let px_us = px_us.max(1);
        // Rewound, new song, or zoomed in well past the culling resolution: decode again.
        if now_us < self.time_us || self.readers.len() != tracks.len() || px_us * 2 < self.px_us {
            *self = VisibleNotes {
                readers: vec![TrackReader::default(); tracks.len()],
                ..Default::default()
            };
        }
        self.px_us = self.px_us.max(px_us);
        let jumped = now_us - self.time_us > 1_000_000;
        self.time_us = now_us;
        // Finished notes are skipped when drawing, and swept out now and then (a sweep
        // visits every note on screen).
        let t0 = Instant::now();
        self.frames_since_sweep += 1;
        if self.window.len() >= self.alive.max(4096) * 2 || self.frames_since_sweep >= 240 {
            self.window.retain(|n| n.end_us + NOTE_HOLD_MICROS > now_us);
            self.alive = self.window.len();
            self.frames_since_sweep = 0;
        }
        self.timing.retain = t0.elapsed();
        let t0 = Instant::now();
        // Merging notes that are nearly hidden changes the picture a little (a note
        // 1-2 pixels long can lose half its pixels to the colour of a neighbour), so
        // it only happens when there are too many notes to keep them all.
        if self.window.len() > EXACT_MAX {
            self.lossy = true;
        } else if self.window.len() < EXACT_MAX / 3 {
            self.lossy = false;
        }
        let slack_us = if self.lossy { px_us } else { 0 };

        let tempo = &song.tempo;
        let now_tick = tempo.time_to_tick(now_us);
        let end_tick = tempo.time_to_tick(window_end);
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
                    push_culled(&mut acc.notes, &mut acc.last, base, note, slack_us);
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
            if last[k] == usize::MAX || !note.absorb(&mut self.window[last[k]], slack_us) {
                self.window[w] = note;
                last[k] = w;
                w += 1;
            }
        }
        self.window.truncate(w);
        self.timing.cull = t0.elapsed();
    }
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
                    );
                    cancel = ui.button("Cancel").clicked();
                });
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

    fn settings_window(&mut self, ctx: &egui::Context) {
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
                });
            });
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
             output={output}\nskip_notes={}\nsynth_limit={}\nvelocity_levels={}\nsvms_batch={}\ndetailed_stats={}\nsynth_stats={}\n",
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

        // Reuses last frame's allocation once the GPU callback has dropped its clone.
        let buf = Arc::make_mut(instances);
        buf.clear();
        // Newest first: the GPU draws front to back (see the depth test in gpu.rs). With
        // the piano layout the sharps' columns overlap the naturals', and sharps go in
        // front (first), as in Piano From Above.
        let passes = if piano_layout { 2 } else { 1 };
        for note in (0..passes).flat_map(|pass| {
            visible
                .window
                .iter()
                .rev()
                .filter(move |n| !piano_layout || is_black_key(n.pitch) == (pass == 0))
        })
        .filter(|n| n.start_us < window_end && n.end_us + NOTE_HOLD_MICROS > now_us)
        {
            let color = song.color_for(note, color_by_channel);
            // The newest sounding note on a key is the one shown on top.
            if note.start_us <= now_us && note.end_us > now_us && !active_keys[note.pitch as usize] {
                let idx = note.pitch as usize;
                active_keys[idx] = true;
                active_colors[idx] = color;
            }

            // Start is relative to `now_us` and can be negative (in the past); the shader clips.
            let start_secs = (note.start_us as i64 - now_us as i64) as f32 / 1_000_000.0;
            let len_secs = (note.end_us - note.start_us) as f32 / 1_000_000.0;
            let color_u32 =
                ((color.r() as u32) << 16) | ((color.g() as u32) << 8) | (color.b() as u32);
            buf.push(gpu::NoteInstance {
                start: start_secs,
                len: len_secs.max(0.000_1),
                key_color: note.pitch as u32 | (color_u32 << 8),
                border_width: 1,
            });
        }

        times.build = t0.elapsed();
        times.instances = buf.len();
        if let Some(state) = frame.wgpu_render_state() {
            if !buf.is_empty() {
                painter.add(gpu::NotesCallback::new(
                    note_area,
                    state.target_format,
                    instances.clone(),
                    gpu_key_positions,
                    speed,
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
        v if v >= 1e9 => format!("{:.2}B", v / 1e9),
        v if v >= 1e6 => format!("{:.2}M", v / 1e6),
        v if v >= 1e3 => format!("{:.1}K", v / 1e3),
        v => format!("{v:.0}"),
    }
}

fn bytes(v: f64) -> String {
    match v {
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
        assert!(!n(101, 500).absorb(&mut prev, 0)); // starts a µs later: prev shows above it
        assert!(!n(100, 499).absorb(&mut prev, 0)); // ends sooner: prev shows below it
        assert!(n(100, 500).absorb(&mut prev, 0)); // same span: hidden
        assert!(n(100, 800).absorb(&mut prev, 0)); // same start, longer: hidden
        assert_eq!((prev.start_us, prev.end_us), (100, 800));
    }

    #[test]
    fn culls_only_fully_hidden_notes() {
        let n = |start_us, end_us, pitch| NoteSegment { start_us, end_us, track: 0, channel: 0, pitch };
        let px = 100; // 100 µs per pixel row
        let mut out = vec![];
        let mut last = [usize::MAX; 128];
        push_culled(&mut out, &mut last, 0, n(0, 500, 60), px);
        push_culled(&mut out, &mut last, 0, n(10, 500, 60), px); // within a pixel, same end: merged
        push_culled(&mut out, &mut last, 0, n(20, 300, 60), px); // ends well earlier: still visible
        push_culled(&mut out, &mut last, 0, n(30, 900, 61), px); // other key
        push_culled(&mut out, &mut last, 0, n(150, 900, 60), px); // over a pixel later: visible
        push_culled(&mut out, &mut last, 0, n(240, 1000, 60), px); // merged, keeping start 150
        let got: Vec<_> = out.iter().map(|n| (n.start_us, n.end_us, n.pitch)).collect();
        assert_eq!(got, [(0, 500, 60), (20, 300, 60), (30, 900, 61), (150, 1000, 60)]);
    }
}
