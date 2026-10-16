//! Video export: `--export-video <midi> <out.mp4> [--width n] [--height n] [--fps n] [--speed s]`
//! `[--lead s] [--tail s] [--encoder x264|nvenc|qsv|amf] [--preset ultrafast..veryslow] [--crf n]`
//! Renders the real note pipeline offscreen at deterministic song time and pipes raw RGBA to ffmpeg.
//! The keyboard is drawn on the CPU (the live view draws it with egui).

use std::{io::Write, process::{Command, Stdio}, sync::Arc, time::Instant};
use crate::{gpu, parser::Progress, song::SongData, VisibleNotes, VisualSong};

pub const PRESETS: [&str; 9] = ["ultrafast", "superfast", "veryfast", "faster", "fast", "medium", "slow", "slower", "veryslow"];

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Encoder { X264, Nvenc, Qsv, Amf }

impl Encoder {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s { "x264" => Self::X264, "nvenc" => Self::Nvenc, "qsv" => Self::Qsv, "amf" => Self::Amf, _ => return None })
    }
    fn args(self, preset: usize, quality: u32) -> Vec<String> {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        match self {
            Self::X264 => s(&["-c:v", "libx264", "-preset", PRESETS[preset], "-crf", &quality.to_string()]),
            // p1 (fastest) .. p7 (slowest), spread over the nine names.
            Self::Nvenc => s(&["-c:v", "h264_nvenc", "-preset", &format!("p{}", 1 + preset * 6 / 8), "-rc", "vbr", "-cq", &quality.to_string(), "-b:v", "0"]),
            // QSV has no ultrafast/superfast.
            Self::Qsv => s(&["-c:v", "h264_qsv", "-preset", PRESETS[preset.max(2)], "-global_quality", &quality.to_string()]),
            Self::Amf => s(&["-c:v", "h264_amf", "-quality", ["speed", "balanced", "quality"][preset / 3], "-rc", "cqp", "-qp_i", &quality.to_string(), "-qp_p", &quality.to_string()]),
        }
    }
}

#[derive(Clone)]
pub struct Settings {
    pub input: String,
    pub output: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Seconds of song visible at once (the live "Note speed").
    pub speed: f32,
    /// Seconds of empty video before the first moment of the song, and after the last.
    pub lead: f32,
    pub tail: f32,
    pub encoder: Encoder,
    pub preset: usize,
    pub quality: u32,
    pub ffmpeg: String,
    /// Optional audio, starting at the song's first note (trim any silence before it).
    pub audio: String,
    /// Song name fading in and out over the delay before the first note.
    pub title: bool,
    /// "Rendered with Kimrashi vX" across the top.
    pub watermark: bool,
    /// The user's display settings (palette, layout, direction, flat notes, overlaps, caps).
    pub look: crate::settings::Settings,
}

impl Default for Settings {
    fn default() -> Self {
        Self { input: String::new(), output: String::new(), width: 1920, height: 1080, fps: 60, speed: 5.0, lead: 3.0, tail: 3.0,
            encoder: Encoder::X264, preset: 5, quality: 20, ffmpeg: "ffmpeg".into(), audio: String::new(), title: true, watermark: true, look: Default::default() }
    }
}

pub struct Report<'a> {
    pub frame: u64,
    pub total: u64,
    /// Video seconds produced per wall second since the start.
    pub speed: f64,
    pub rgba: &'a [u8],
}

fn fill(buf: &mut [u8], width: u32, x0: f32, x1: f32, y0: u32, y1: u32, color: [u8; 3]) {
    let (x0, x1) = (x0.round().max(0.0) as u32, (x1.round().max(0.0) as u32).min(width));
    for y in y0..y1 {
        let row = y as usize * width as usize * 4;
        for x in x0..x1 { buf[row + x as usize * 4..][..3].copy_from_slice(&color); }
    }
}

/// Mirrors `ui::draw_keyboard`; `columns` is `Some` for the piano layout, else keys sit on their own grid.
fn keyboard(buf: &mut [u8], width: u32, top: u32, bottom: u32, lo: u8, hi: u8, columns: Option<&[(f32, f32); 128]>,
    active: &[bool; 128], colors: &[egui::Color32; 128])
{
    let w = width as f32;
    let black_h = top + ((bottom - top) as f32 * 0.65) as u32;
    let white_index = |p: u8| (lo..p).filter(|&q| !crate::is_black_key(q)).count() as f32;
    let white_w = w / (lo..=hi).filter(|&q| !crate::is_black_key(q)).count().max(1) as f32;
    for black in [false, true] {
        for p in lo..=hi {
            if crate::is_black_key(p) != black { continue; }
            let (l, r) = match columns {
                Some(c) => (c[p as usize].0 * w, c[p as usize].1 * w),
                None if black => { let c = white_index(p) * white_w; (c - white_w * 0.35, c + white_w * 0.35) }
                None => (white_index(p) * white_w, (white_index(p) + 1.0) * white_w),
            };
            let (edge, idle, lit) = if black { ([80; 3], [25, 25, 30], [255, 160, 100]) } else { ([150, 150, 160], [245, 245, 250], [255, 220, 140]) };
            let c = colors[p as usize];
            let body = if !active[p as usize] { idle } else if c == egui::Color32::TRANSPARENT { lit } else { [c.r(), c.g(), c.b()] };
            let y1 = if black { black_h } else { bottom };
            fill(buf, width, l, r, top, y1, edge);
            fill(buf, width, l + 1.0, r - 1.0, top, y1 - 1, body);
        }
    }
}

/// Opacity of the title and watermark `v` seconds into a `lead`-second intro.
fn intro_fade(v: f64, lead: f64) -> f64 {
    if v < 0.0 || v >= lead { 0.0 } else if v < 0.6 { v / 0.6 } else if v < lead - 0.6 { 1.0 } else { ((lead - v) / 0.6).max(0.0) }
}

/// The title's band across the video: (top row, height, font size).
fn title_band(height: u32) -> (u32, u32, u32) {
    let font = height / 18;
    ((height as f32 * 0.38) as u32, font * 5 / 2, font)
}

/// ffmpeg drawtext for the title and watermark (text goes through files, so no escaping).
/// Uses egui's own font. ponytail: its Latin/Cyrillic/Greek glyphs only, so CJK titles come out blank.
fn overlay_filter(cfg: &Settings, height: u32) -> Option<String> {
    let dir = std::env::temp_dir();
    let font_path = dir.join("kimrashi-font.ttf");
    let fonts = egui::FontDefinitions::default();
    std::fs::write(&font_path, &*fonts.font_data.get("Ubuntu-Light")?.font).ok()?;
    let escape = |p: &std::path::Path| p.to_string_lossy().replace('\\', "/").replace(':', "\\:"); // for drawtext's own option parser
    let write = |name: &str, text: &str| {
        let path = dir.join(format!("kimrashi-{name}.txt"));
        std::fs::write(&path, text).ok().map(|_| escape(&path))
    };
    let font = escape(&font_path);
    let l = cfg.lead;
    let alpha = format!("if(lt(t,0.6),t/0.6,if(lt(t,{l}-0.6),1,max(0,({l}-t)/0.6)))");
    let text = |file: &str, size: u32, alpha: &str, y: &str| format!(
        "drawtext=fontfile='{font}':textfile='{file}':fontsize={size}:fontcolor=white:alpha='{alpha}':x=(w-text_w)/2:y={y}:shadowcolor=black@0.6:shadowx=2:shadowy=2:enable='lt(t,{l})'");
    let mut parts = Vec::new();
    if cfg.lead >= 1.0 {
        if cfg.title {
            let name = std::path::Path::new(&cfg.input).file_stem()?.to_string_lossy().into_owned();
            let (top, band, size) = title_band(height);
            parts.push(text(&write("title", &name)?, size, &alpha, &format!("{top}+({band}-text_h)/2")));
        }
        if cfg.watermark {
            let line = format!("Rendered with Kimrashi v{}", env!("CARGO_PKG_VERSION"));
            parts.push(text(&write("watermark", &line)?, (height / 45).max(10), &format!("0.8*({alpha})"), "h/60"));
        }
    }
    (!parts.is_empty()).then(|| parts.join(","))
}

/// Start of the earliest note. ponytail: probes doubling windows from a fresh decode each time.
fn first_note_us(song: &Arc<SongData>) -> u64 {
    let end = song.tempo.duration_us();
    let mut window = 1_000_000u64;
    loop {
        let mut readers = vec![crate::TrackReader::default(); song.midi.tracks.len()];
        let found = crate::decode_notes(song, &mut readers, 0, window.min(end)).notes.iter().map(|n| n.start_us).min();
        if let Some(t) = found { return t; }
        if window >= end { return 0; }
        window *= 2;
    }
}

/// ponytail: sparse writer thread + 3 recycled buffers; encode overlaps the next render.
pub fn run(cfg: &Settings, mut on_frame: impl FnMut(&Report) -> bool) -> Result<(), String> {
    let (width, height) = (cfg.width & !1, cfg.height & !1);
    let song = SongData::load(&cfg.input, 127, &Progress::default()).map_err(|e| format!("{}: {e}", cfg.input))?;
    let duration_us = song.tempo.duration_us();
    let first_us = first_note_us(&song);
    let mut visual = VisualSong::new(song);
    visual.set_palette(cfg.look.note_palette, &cfg.look.custom_colors);
    let keyboard_h = (height as f32 * 0.12) as u32;
    let note_h = height - keyboard_h;
    let mut gpu = gpu::Offscreen::new(width, height, note_h)?;

    let mut ffmpeg = Command::new(&cfg.ffmpeg);
    ffmpeg.args(["-y", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "rgba", "-s", &format!("{width}x{height}"),
        "-r", &cfg.fps.to_string(), "-i", "-"]);
    // The first note is `lead` seconds into the video, and the audio starts on its first note.
    let total_s = cfg.lead as f64 + (duration_us - first_us) as f64 / 1e6 + cfg.tail as f64;
    if !cfg.audio.is_empty() {
        ffmpeg.args(["-itsoffset", &cfg.lead.to_string(), "-i", &cfg.audio, "-map", "0:v", "-map", "1:a", "-c:a", "aac", "-b:a", "192k"]);
    }
    if let Some(graph) = overlay_filter(cfg, height) { ffmpeg.args(["-vf", &graph]); }
    ffmpeg.args(cfg.encoder.args(cfg.preset.min(8), cfg.quality))
        .args(["-pix_fmt", "yuv420p", "-movflags", "+faststart", "-t", &format!("{total_s:.3}"), &cfg.output])
        .stdin(Stdio::piped());
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut ffmpeg, 0x0800_0000); // CREATE_NO_WINDOW
    let mut child = ffmpeg.spawn().map_err(|e| format!("can't start {}: {e}", cfg.ffmpeg))?;
    let mut stdin = child.stdin.take().unwrap();
    let (full_tx, full_rx) = crossbeam_channel::bounded::<Vec<u8>>(3);
    let (free_tx, free_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
    let writer = std::thread::spawn(move || {
        for frame in full_rx {
            if stdin.write_all(&frame).is_err() { break; } // ffmpeg died: its stderr says why
            let _ = free_tx.send(frame);
        }
    });

    let speed = cfg.speed.clamp(0.01, 15.0);
    let future_us = (speed * 1e6) as u64;
    let (lo, hi) = (visual.pitch_min, visual.pitch_max);
    let look = &cfg.look;
    let piano = look.piano_layout;
    let columns = crate::key_columns(piano, lo, hi);
    let mut keys = [gpu::KeyPosition { left: 0.0, right: 0.0, _pad0: 0.0, _pad1: 0.0 }; 256];
    for p in lo..=hi { keys[p as usize].left = columns[p as usize].0; keys[p as usize].right = columns[p as usize].1; }

    let mut visible = VisibleNotes::default();
    visible.set_collapse(look.collapse_overlaps);
    visible.set_max_instances(look.max_instances);
    let mut instances = Arc::new(gpu::InstanceRing::default());
    let total = (total_s * cfg.fps as f64).ceil() as u64;
    let start = Instant::now();
    let mut result: Result<(), String> = Ok(());
    for f in 0..total {
        let t = f as f64 / cfg.fps as f64 - cfg.lead as f64 + first_us as f64 / 1e6; // song seconds; before the first note's lead-in it can be negative
        let now_us = (t.max(0.0) * 1e6) as u64;
        let deficit = (-t).max(0.0) as f32;
        let window_end = (now_us + future_us).saturating_sub((deficit * 1e6) as u64).max(now_us).min(duration_us.max(now_us));
        visible.advance(&visual.data, now_us, window_end, future_us / note_h as u64);
        let (mut active, colors) = crate::build_note_instances(&visual, &mut visible, now_us, window_end, speed, note_h, piano, &mut instances);
        if deficit > 0.0 { active = [false; 128]; }
        let mut frame = free_rx.try_recv().unwrap_or_default();
        gpu.render(&instances, &keys, speed, now_us, deficit, [20, 20, 24], if look.notes_rise { 1.0 } else { -1.0 }, look.flat_notes, &mut frame);
        if cfg.title && cfg.lead >= 1.0 {
            // Half-dark band behind the title, fading with it (the text itself is drawn by ffmpeg).
            let dim = 1.0 - 0.5 * intro_fade(f as f64 / cfg.fps as f64, cfg.lead as f64);
            if dim < 1.0 {
                let (top, band, _) = title_band(height);
                let rows = &mut frame[top as usize * width as usize * 4..((top + band) as usize).min(note_h as usize) * width as usize * 4];
                for px in rows.chunks_exact_mut(4) { for c in &mut px[..3] { *c = (*c as f64 * dim) as u8; } }
            }
        }
        fill(&mut frame, width, 0.0, width as f32, note_h, note_h + 1, [200, 200, 210]);
        fill(&mut frame, width, 0.0, width as f32, note_h + 1, height, [235, 235, 240]);
        keyboard(&mut frame, width, note_h + 1, height, lo, hi, piano.then_some(&columns), &active, &colors);
        let report = Report { frame: f + 1, total, speed: (f + 1) as f64 / cfg.fps as f64 / start.elapsed().as_secs_f64(), rgba: &frame };
        if !on_frame(&report) { result = Err("cancelled".into()); break; }
        if full_tx.send(frame).is_err() { break; }
    }
    drop(full_tx);
    let _ = writer.join();
    let status = child.wait().map_err(|e| e.to_string())?;
    result?;
    if status.success() { Ok(()) } else { Err(format!("ffmpeg failed ({status})")) }
}

pub fn run_from_args() -> bool {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let Some(i) = args.iter().position(|a| a == "--export-video") else { return false };
    let (Some(input), Some(output)) = (args.get(i + 1), args.get(i + 2)) else { panic!("--export-video needs <midi> <out.mp4>") };
    let value = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1));
    let num = |name: &str, default: f32| value(name).map_or(default, |s| s.parse().unwrap_or_else(|_| panic!("bad {name}")));
    let look = crate::settings::Settings::load();
    let d = Settings { speed: look.note_speed, ..Default::default() };
    let cfg = Settings {
        look,
        input: input.clone(), output: output.clone(),
        width: num("--width", d.width as f32) as u32, height: num("--height", d.height as f32) as u32,
        fps: num("--fps", d.fps as f32) as u32, speed: num("--speed", d.speed), lead: num("--lead", d.lead), tail: num("--tail", d.tail),
        quality: num("--crf", d.quality as f32) as u32,
        encoder: value("--encoder").map_or(d.encoder, |s| Encoder::parse(s).expect("encoder: x264, nvenc, qsv or amf")),
        preset: value("--preset").map_or(d.preset, |s| PRESETS.iter().position(|p| p == s).expect("preset: ultrafast..veryslow")),
        title: !args.iter().any(|a| a == "--no-title"), watermark: !args.iter().any(|a| a == "--no-watermark"),
        audio: value("--audio").cloned().unwrap_or_default(),
        ffmpeg: value("--ffmpeg").cloned().unwrap_or(d.ffmpeg),
    };
    let result = run(&cfg, |r| {
        if r.frame % 60 == 0 || r.frame == r.total {
            eprint!("\r{}/{} frames, {:.1}x realtime   ", r.frame, r.total, r.speed);
        }
        true
    });
    eprintln!();
    if let Err(e) = result { eprintln!("export failed: {e}"); std::process::exit(1); }
    true
}
