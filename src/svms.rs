//! SuperVirtualMIDISynth (SVMS) instrumentation. When the KDMAPI DLL is SVMS, its
//! native API (`SVMS_GetInterface`, see SVMS's `include/svmsapi.h`) gives a
//! record of every audio callback; `Monitor` drains it in the background, keeps
//! a per-second summary for the overlay and optionally writes every callback to CSV.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::clock::PlaybackClock;

const ABI_VERSION_1: u32 = 1;
const STRUCT_VERSION_1: u32 = 1;
const RESULT_OK: i32 = 0;
const CAP_EXACT_QPC_TIMESTAMPS: u64 = 1 << 0;
const CAP_SHORT_EVENT_BATCH: u64 = 1 << 1;
const CAP_SYSTEM_EXCLUSIVE: u64 = 1 << 2;
const CAP_TELEMETRY_V2: u64 = 1 << 15;
const CAP_CALLBACK_TRACE: u64 = 1 << 16;

pub type GetInterfaceFn = unsafe extern "C" fn(u32, u32, *mut Interface) -> i32;
type Session = u64;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ShortEvent {
    timestamp_qpc: u64, // 0 = at submission
    packed_message: u32,
    reserved: u32,
}

/// `SVMS_Interface`, ABI 1. Entries this module doesn't call are placeholders.
#[repr(C)]
pub struct Interface {
    struct_size: u32,
    struct_version: u32,
    abi_version: u32,
    reserved0: u32,
    capabilities: u64,
    product: [u32; 4],
    create_session: Option<unsafe extern "C" fn(*const u8, *mut Session) -> i32>,
    destroy_session: Option<unsafe extern "C" fn(Session) -> i32>,
    _send_short: [usize; 2],
    send_short_batch: Option<unsafe extern "C" fn(Session, *const ShortEvent, u32) -> i32>,
    send_system_exclusive: Option<unsafe extern "C" fn(Session, *const u8, u32) -> i32>,
    _reset_telemetry: [usize; 2],
    get_runtime_clock: Option<unsafe extern "C" fn(*mut u64, *mut u64) -> i32>,
    _unused: [usize; 18],
    get_telemetry_v2: Option<unsafe extern "C" fn(Session, *mut TelemetryV2) -> i32>,
    enable_callback_trace: Option<unsafe extern "C" fn(Session, u32) -> i32>,
    read_callback_trace:
        Option<unsafe extern "C" fn(Session, *mut u64, *mut CallbackTrace, u32, *mut u32) -> i32>,
}

/// `SVMS_TelemetryV2` (280 bytes).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct TelemetryV2 {
    struct_size: u32,
    struct_version: u32,
    callback_count: u64,
    submitted_events: u64,
    accepted_events: u64,
    dispatched_events: u64,
    note_ons: u64,
    matched_regions: u64,
    configured_voices: u64,
    voice_steals: u64,
    active_voices: u32,
    free_voices: u32,
    sample_rate: u32,
    buffer_frames: u32,
    soundfont_loaded: u32,
    audio_running: u32,
    render_time_ms: f32,
    render_peak: f32,
    late_events: u64,
    late_clamped_events: u64,
    late_clamp_max_lateness_frames: u64,
    late_clamp_block_pileup_max: u32,
    stale_note_ons_skipped: u64,
    stale_note_offs_compacted: u64,
    fence_suppressed_note_ons: u64,
    cc_collapsed_events: u64,
    shed_note_ons: u64,
    sequence_gaps: u64,
    dropped_events: u64,
    cancelled_submissions: u64,
    scheduled_events: u32,
    render_paths: u32,
    plan_refusal_type: u8,
    plan_refusal_data1: u8,
    backend_kind: u16,
    primary_span_calls: u64,
    primary_span_frames: u64,
    over_budget_callbacks: u64,
    max_consecutive_over_budget: u32,
    callback_p95_percent: f32,
    callback_p99_percent: f32,
    callback_p999_percent: f32,
    /// Exact note-on merging: voices the pool would hold unmerged, its peak, and
    /// the cumulative note-ons folded into other voices (low/high halves).
    effective_voices: u32,
    effective_voices_peak: u32,
    note_merged_low: u32,
    note_merged_high: u32,
    reserved: u32,
}

/// `SVMS_CallbackTrace` (208 bytes): one audio callback.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CallbackTrace {
    index: u64,
    callback: u64,
    start_qpc: u64,
    output_frame: u64,
    frames: u32,
    total_ns: u32,
    schedule_ns: u32,
    render_ns: u32,
    post_ns: u32,
    events: u32,
    render_events: u32,
    late_events: u32,
    max_lateness_frames: u32,
    skipped_frames: u32,
    scheduled_backlog: u32,
    ingress_backlog: u32,
    compiled_backlog: u32,
    active_voices: u32,
    releasing_voices: u32,
    render_paths: u32,
    note_ons: u64,
    voice_steals: u64,
    producer_waits: u64,
    producer_wait_ns: u64,
    cycles: u64,
    wv_plan_cycles: u64,
    wv_jobs_cycles: u64,
    wv_post_cycles: u64,
    class_voices: [u32; 7],
    wv_serial_jobs: u32,
    wv_kernel_frames: u64,
    wv_fallback_frames: u64,
}

const _: () = assert!(std::mem::size_of::<TelemetryV2>() == 280);
const _: () = assert!(std::mem::size_of::<CallbackTrace>() == 208);

/// A native SVMS session next to the KDMAPI stream (same engine).
pub struct Link {
    api: Interface,
    session: Session,
}

// The interface is a table of plain function pointers into the DLL.
unsafe impl Send for Link {}
unsafe impl Sync for Link {}

impl Link {
    /// Open a session if `get_interface` belongs to an SVMS with callback tracing.
    pub fn open(get_interface: GetInterfaceFn) -> Option<Link> {
        // Zeroed: entries an older runtime doesn't fill stay None.
        let mut api: Interface = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<Interface>() as u32;
        if unsafe { get_interface(ABI_VERSION_1, size, &mut api) } != RESULT_OK
            || api.capabilities & (CAP_TELEMETRY_V2 | CAP_CALLBACK_TRACE)
                != CAP_TELEMETRY_V2 | CAP_CALLBACK_TRACE
        {
            return None;
        }
        let complete = api.destroy_session.is_some()
            && api.get_runtime_clock.is_some()
            && api.get_telemetry_v2.is_some()
            && api.enable_callback_trace.is_some()
            && api.read_callback_trace.is_some();
        let create = api.create_session.filter(|_| complete)?;
        let mut session = 0;
        if unsafe { create(std::ptr::null(), &mut session) } != RESULT_OK {
            return None;
        }
        Some(Link { api, session })
    }

    pub fn can_batch(&self) -> bool {
        self.api.capabilities & CAP_SHORT_EVENT_BATCH != 0 && self.api.send_short_batch.is_some()
    }

    /// Events can carry exact times (QPC ticks of `clock`): the synth schedules them.
    pub fn can_timestamp(&self) -> bool {
        self.api.capabilities & CAP_EXACT_QPC_TIMESTAMPS != 0 && self.can_batch()
    }

    /// SysEx goes straight to the engine (not through the KDMAPI facade's checks).
    pub fn can_sysex(&self) -> bool {
        self.api.capabilities & CAP_SYSTEM_EXCLUSIVE != 0 && self.api.send_system_exclusive.is_some()
    }

    /// Send one complete SysEx message (F0 .. F7); the synth copies it before returning.
    pub fn send_sysex(&self, bytes: &[u8]) {
        if let Some(f) = self.api.send_system_exclusive {
            unsafe { f(self.session, bytes.as_ptr(), bytes.len() as u32) };
        }
    }

    /// Submit messages in one call, each with its own timestamp (0 = at submission).
    pub fn send_batch(&self, events: &[ShortEvent]) {
        if let Some(f) = self.api.send_short_batch {
            for chunk in events.chunks(u32::MAX as usize) {
                unsafe { f(self.session, chunk.as_ptr(), chunk.len() as u32) };
            }
        }
    }

    /// The synth's clock: (ticks now, ticks per second), the domain of event timestamps.
    pub fn clock(&self) -> (u64, u64) {
        let (mut now, mut freq) = (0, 1);
        unsafe { (self.api.get_runtime_clock.unwrap())(&mut now, &mut freq) };
        (now, freq.max(1))
    }

    fn telemetry(&self) -> Option<TelemetryV2> {
        let mut t = TelemetryV2 {
            struct_size: std::mem::size_of::<TelemetryV2>() as u32,
            struct_version: STRUCT_VERSION_1,
            ..Default::default()
        };
        let ok = unsafe { (self.api.get_telemetry_v2.unwrap())(self.session, &mut t) } == RESULT_OK;
        ok.then_some(t)
    }

    fn set_trace(&self, on: bool) -> bool {
        unsafe { (self.api.enable_callback_trace.unwrap())(self.session, on as u32) == RESULT_OK }
    }

    /// Records from `cursor` on, oldest first, into `buf`; advances `cursor`.
    fn read_trace(&self, cursor: &mut u64, buf: &mut [CallbackTrace]) -> usize {
        let mut n = 0;
        let f = self.api.read_callback_trace.unwrap();
        let ok = unsafe { f(self.session, cursor, buf.as_mut_ptr(), buf.len() as u32, &mut n) };
        if ok == RESULT_OK { n as usize } else { 0 }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        unsafe { (self.api.destroy_session.unwrap())(self.session) };
    }
}

pub fn short_event(msg: u32) -> ShortEvent {
    short_event_at(msg, 0)
}

pub fn short_event_at(msg: u32, timestamp_qpc: u64) -> ShortEvent {
    ShortEvent { timestamp_qpc, packed_message: msg, reserved: 0 }
}

/// Synth-side numbers over the last second.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub callbacks: u32,
    /// Callback time as a percentage of its budget (frames / sample rate).
    pub load_avg: f32,
    pub load_max: f32,
    /// Song position of the slowest callback.
    pub worst_at_us: u64,
    pub over_budget: u32,
    /// Average share of the budget per phase.
    pub schedule_pct: f32,
    pub render_pct: f32,
    pub post_pct: f32,
    pub events_per_s: f64,
    pub late_per_s: f64,
    pub max_late_ms: f32,
    pub skipped_ms: f32,
    pub note_ons_per_s: f64,
    pub steals_per_s: f64,
    /// Time submitters (us) spent blocked by SVMS backpressure, per second.
    pub blocked_ms_per_s: f32,
    pub active_voices: u32,
    pub peak_voices: u32,
    pub max_voices: u32,
    pub releasing_voices: u32,
    pub scheduled_backlog: u32,
    pub ingress_backlog: u32,
    pub compiled_backlog: u32,
    /// Render paths used: bit0 whole-voice, bit1 dense, bit2 sparse.
    pub paths: u32,
    pub p99: f32,
    pub sample_rate: u32,
    pub buffer_frames: u32,
    /// Trace records overwritten before they were read.
    pub lost: u64,
}

/// Running per-second sums.
#[derive(Default)]
struct Window {
    s: Stats,
    load_sum: f64,
    phase_sum: [f64; 3],
    events: u64,
    late: u64,
    skipped_frames: u64,
    note_ons: u64,
    steals: u64,
    wait_ns: u64,
}

/// Drains the callback trace on a background thread while an SVMS output is open.
pub struct Monitor {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub stats: Arc<Mutex<Option<Stats>>>,
}

impl Monitor {
    /// `log`: where to write per-callback CSV rows (None = don't), checked continuously.
    pub fn start(link: Arc<Link>, clock: Arc<PlaybackClock>, log: Arc<Mutex<Option<PathBuf>>>) -> Option<Self> {
        if !link.set_trace(true) {
            return None;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Mutex::new(None));
        let thread = {
            let (stop, stats) = (stop.clone(), stats.clone());
            std::thread::Builder::new()
                .name("svms-monitor".into())
                .spawn(move || run(&link, &clock, &log, &stop, &stats))
                .ok()?
        };
        Some(Self { stop, thread: Some(thread), stats })
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct CsvLog {
    path: PathBuf,
    file: std::io::BufWriter<std::fs::File>,
    /// Per-window deltas of the flow counters (coalescing, collapsing, shedding).
    flow: Option<std::io::BufWriter<std::fs::File>>,
}

const CSV_HEADER: &str = "song_s,wall_ms,callback,frames,load_pct,total_us,schedule_us,render_us,post_us,\
events,render_events,late,max_late_ms,skipped_frames,scheduled_backlog,ingress_backlog,compiled_backlog,\
active_voices,releasing_voices,paths,note_ons,steals,blocked_us,lost,wv_plan_us,wv_jobs_us,wv_post_us,sustained_loop,sustained_oneshot,transient_loop,transient_oneshot,release_loop,release_oneshot,generic,wv_kernel_frames,wv_fallback_frames,wv_serial_jobs";

fn run(
    link: &Link,
    clock: &PlaybackClock,
    log_target: &Mutex<Option<PathBuf>>,
    stop: &AtomicBool,
    out: &Mutex<Option<Stats>>,
) {
    let mut buf = vec![CallbackTrace::default(); 1024];
    // The ring is engine-wide and outlives sessions: skip what's already in it.
    let mut cursor = 0;
    while link.read_trace(&mut cursor, &mut buf) == buf.len() {}
    let mut prev: Option<CallbackTrace> = None;
    let mut telemetry = link.telemetry().unwrap_or_default();
    let (start_qpc, _) = link.clock();
    let mut log: Option<CsvLog> = None;
    let mut w = Window::default();
    let mut window_start = Instant::now();
    // SVMS_STATS_SECS shortens the stats row period (default 1 s) for burst analysis.
    let stats_window: f64 = std::env::var("SVMS_STATS_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(1.0);

    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(50));

        // Follow the player's stats logging.
        let target = log_target.lock().unwrap().clone();
        if log.as_ref().map(|l| &l.path) != target.as_ref() {
            log = target.and_then(|path| {
                let file = std::fs::File::create(&path)
                    .map_err(|e| eprintln!("Could not create SVMS trace log: {e}"))
                    .ok()?;
                let mut file = std::io::BufWriter::new(file);
                writeln!(file, "{CSV_HEADER}").ok()?;
                let flow = std::fs::File::create(path.with_extension("flow.csv")).ok().map(|f| {
                    let mut f = std::io::BufWriter::new(f);
                    let _ = writeln!(f, "song_s,wall_ms,submitted,accepted,dispatched,late,late_clamped,stale_on_skipped,stale_off_compacted,fence_suppressed,cc_collapsed,shed,dropped,cancelled,seq_gaps,not_accepted,note_merged,effective_voices,effective_peak");
                    f
                });
                Some(CsvLog { path, file, flow })
            });
        }

        // Anchor for mapping callback times to song positions.
        let (qpc_now, freq) = link.clock();
        let song_now = clock.now_us();
        let speed = if clock.is_paused() { 0.0 } else { clock.speed() };
        let rate = telemetry.sample_rate.max(1) as f64;

        loop {
            let n = link.read_trace(&mut cursor, &mut buf);
            for r in &buf[..n] {
                let lost = prev.map_or(0, |p| r.index.saturating_sub(p.index + 1));
                let (ons, steals, wait_ns) = prev.map_or((0, 0, 0), |p| {
                    (
                        r.note_ons.saturating_sub(p.note_ons),
                        r.voice_steals.saturating_sub(p.voice_steals),
                        r.producer_wait_ns.saturating_sub(p.producer_wait_ns),
                    )
                });
                let (prev_kernel, prev_fallback) =
                    prev.map_or((r.wv_kernel_frames, r.wv_fallback_frames), |p| (p.wv_kernel_frames, p.wv_fallback_frames));
                prev = Some(*r);
                let ago_us = qpc_now.saturating_sub(r.start_qpc) as f64 * 1e6 / freq as f64;
                let song_us = (song_now as f64 - ago_us * speed).max(0.0) as u64;
                let budget_ns = r.frames as f64 * 1e9 / rate;
                let load = r.total_ns as f64 / budget_ns * 100.0;
                // A share of the callback's cycles, as microseconds.
                let share = |c: u64| c as f64 / r.cycles.max(1) as f64 * r.total_ns as f64 / 1e3;

                let s = &mut w.s;
                s.callbacks += 1;
                s.lost += lost;
                if load as f32 > s.load_max {
                    s.load_max = load as f32;
                    s.worst_at_us = song_us;
                }
                s.over_budget += (load > 100.0) as u32;
                s.max_late_ms = s.max_late_ms.max((r.max_lateness_frames as f64 / rate * 1e3) as f32);
                s.active_voices = r.active_voices;
                s.peak_voices = s.peak_voices.max(r.active_voices);
                s.releasing_voices = r.releasing_voices;
                s.scheduled_backlog = r.scheduled_backlog;
                s.ingress_backlog = r.ingress_backlog;
                s.compiled_backlog = r.compiled_backlog;
                s.paths |= r.render_paths;
                w.load_sum += load;
                for (sum, ns) in w.phase_sum.iter_mut().zip([r.schedule_ns, r.render_ns, r.post_ns]) {
                    *sum += ns as f64 / budget_ns * 100.0;
                }
                w.events += r.events as u64;
                w.late += r.late_events as u64;
                w.skipped_frames += r.skipped_frames as u64;
                w.note_ons += ons;
                w.steals += steals;
                w.wait_ns += wait_ns;

                if let Some(l) = &mut log {
                    let _ = writeln!(
                        l.file,
                        "{:.4},{:.3},{},{},{:.1},{:.1},{:.1},{:.1},{:.1},{},{},{},{:.3},{},{},{},{},{},{},{},{},{},{:.1},{},{:.1},{:.1},{:.1},{},{},{},{},{},{},{},{},{},{}",
                        song_us as f64 / 1e6,
                        r.start_qpc.saturating_sub(start_qpc) as f64 * 1e3 / freq as f64,
                        r.callback,
                        r.frames,
                        load,
                        r.total_ns as f64 / 1e3,
                        r.schedule_ns as f64 / 1e3,
                        r.render_ns as f64 / 1e3,
                        r.post_ns as f64 / 1e3,
                        r.events,
                        r.render_events,
                        r.late_events,
                        r.max_lateness_frames as f64 / rate * 1e3,
                        r.skipped_frames,
                        r.scheduled_backlog,
                        r.ingress_backlog,
                        r.compiled_backlog,
                        r.active_voices,
                        r.releasing_voices,
                        paths(r.render_paths),
                        ons,
                        steals,
                        wait_ns as f64 / 1e3,
                        lost,
                        share(r.wv_plan_cycles),
                        share(r.wv_jobs_cycles),
                        share(r.wv_post_cycles),
                        r.class_voices[0],
                        r.class_voices[1],
                        r.class_voices[2],
                        r.class_voices[3],
                        r.class_voices[4],
                        r.class_voices[5],
                        r.class_voices[6],
                        r.wv_kernel_frames.saturating_sub(prev_kernel),
                        r.wv_fallback_frames.saturating_sub(prev_fallback),
                        r.wv_serial_jobs,
                    );
                }
            }
            if n < buf.len() {
                break;
            }
        }
        if let Some(l) = &mut log {
            let _ = l.file.flush();
        }

        let secs = window_start.elapsed().as_secs_f64();
        if secs >= stats_window {
            let before = telemetry;
            telemetry = link.telemetry().unwrap_or(telemetry);
            if let Some(l) = &mut log {
                if let Some(f) = &mut l.flow {
                    let d = |a: u64, b: u64| a.saturating_sub(b);
                    let _ = writeln!(f, "{:.4},{:.3},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
                        song_now as f64 / 1e6, qpc_now.saturating_sub(start_qpc) as f64 * 1e3 / freq as f64,
                        d(telemetry.submitted_events, before.submitted_events), d(telemetry.accepted_events, before.accepted_events),
                        d(telemetry.dispatched_events, before.dispatched_events), d(telemetry.late_events, before.late_events),
                        d(telemetry.late_clamped_events, before.late_clamped_events), d(telemetry.stale_note_ons_skipped, before.stale_note_ons_skipped),
                        d(telemetry.stale_note_offs_compacted, before.stale_note_offs_compacted), d(telemetry.fence_suppressed_note_ons, before.fence_suppressed_note_ons),
                        d(telemetry.cc_collapsed_events, before.cc_collapsed_events), d(telemetry.shed_note_ons, before.shed_note_ons),
                        d(telemetry.dropped_events, before.dropped_events), d(telemetry.cancelled_submissions, before.cancelled_submissions),
                        d(telemetry.sequence_gaps, before.sequence_gaps),
                        d(telemetry.submitted_events.saturating_sub(telemetry.accepted_events), before.submitted_events.saturating_sub(before.accepted_events)),
                        d(telemetry.note_merged_low as u64 | (telemetry.note_merged_high as u64) << 32, before.note_merged_low as u64 | (before.note_merged_high as u64) << 32),
                        telemetry.effective_voices, telemetry.effective_voices_peak);
                    let _ = f.flush();
                }
            }
            let mut s = std::mem::take(&mut w.s);
            let n = s.callbacks.max(1) as f64;
            s.load_avg = (w.load_sum / n) as f32;
            [s.schedule_pct, s.render_pct, s.post_pct] = w.phase_sum.map(|p| (p / n) as f32);
            s.events_per_s = w.events as f64 / secs;
            s.late_per_s = w.late as f64 / secs;
            s.skipped_ms = (w.skipped_frames as f64 / rate * 1e3) as f32;
            s.note_ons_per_s = w.note_ons as f64 / secs;
            s.steals_per_s = w.steals as f64 / secs;
            s.blocked_ms_per_s = (w.wait_ns as f64 / 1e6 / secs) as f32;
            s.max_voices = telemetry.active_voices + telemetry.free_voices;
            s.p99 = telemetry.callback_p99_percent;
            s.sample_rate = telemetry.sample_rate;
            s.buffer_frames = telemetry.buffer_frames;
            *out.lock().unwrap() = Some(s);
            w = Window::default();
            window_start = Instant::now();
        }
    }
    link.set_trace(false);
}

/// Render path bits as letters: w(hole-voice), d(ense), s(parse).
pub fn paths(bits: u32) -> String {
    let s: String = [(1, 'w'), (2, 'd'), (4, 's')]
        .iter()
        .filter(|(b, _)| bits & b != 0)
        .map(|&(_, c)| c)
        .collect();
    if s.is_empty() { "-".into() } else { s }
}
