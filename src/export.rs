//! Video export: `--export-video <midi> <out.mp4> [--width n] [--height n] [--fps n] [--speed s]`
//! `[--lead s] [--tail s] [--encoder x264|nvenc|qsv|amf] [--preset ultrafast..veryslow] [--crf n]`
//! `[--view vertical|horizontal|parallax] [--keyboard-side left|right] [--parallax-far 0.1..1]`
//! `[--perspective-far 0.1..0.9]` (default; `--view perspective` too): the saved settings)
//! Renders the real note pipeline offscreen at deterministic song time and pipes raw RGBA to ffmpeg.
//! The keyboard is drawn on the CPU (the live view draws it with egui).

use std::{io::Write, process::{Command, Stdio}, sync::Arc, time::Instant};
use crate::{gpu, parser::Progress, settings::{KeyboardSide, ViewMode}, song::SongData, VisibleNotes, VisualSong};

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

/// Adds the key glow's `strip` (one colour per column) over the keys at `ON_KEYS` and, falling off,
/// up from the keyboard line at row `line` (see `crate::glow`, and `ui::paint_glow` for the window).
fn add_glow(buf: &mut [u8], width: u32, line: u32, strip: &[[f32; 3]], reach_px: f32) {
    let height = (buf.len() / 4 / width as usize) as u32;
    for y in 0..height {
        let f = if y >= line { crate::glow::ON_KEYS } else { crate::glow::falloff((line - y) as f32 - 0.5, reach_px) };
        if f < 1.0 / 255.0 { continue; }
        let row = &mut buf[y as usize * width as usize * 4..][..width as usize * 4];
        for (px, c) in row.chunks_exact_mut(4).zip(strip) {
            for k in 0..3 { px[k] = (px[k] as f32 + c[k] * f * 255.0).min(255.0) as u8; }
        }
    }
}

/// Fills `r` (pixels, rounded) with `color`, clipped to the frame.
fn fill_rect(buf: &mut [u8], width: u32, r: egui::Rect, color: [u8; 3]) {
    let height = (buf.len() / 4 / width as usize) as u32;
    let y0 = r.top().round().max(0.0) as u32;
    let y1 = (r.bottom().round().max(0.0) as u32).min(height);
    fill(buf, width, r.left(), r.right(), y0, y1, color);
}

/// Draws `ui::keyboard_rects` (as `ui::draw_keyboard` does) into `rect`, in pixels.
fn keyboard(buf: &mut [u8], width: u32, rect: egui::Rect, lo: u8, hi: u8, columns: Option<&[(f32, f32); 128]>,
    side: Option<KeyboardSide>, active: &[bool; 128], colors: &[egui::Color32; 128])
{
    use egui::{pos2, Rect};
    for (p, r) in crate::ui::keyboard_rects(rect, lo, hi, columns, side) {
        let black = crate::is_black_key(p);
        let (edge, idle, lit) = if black { ([80; 3], [25, 25, 30], [255, 160, 100]) } else { ([150, 150, 160], [245, 245, 250], [255, 220, 140]) };
        let c = colors[p as usize];
        let body = if !active[p as usize] { idle } else if c == egui::Color32::TRANSPARENT { lit } else { [c.r(), c.g(), c.b()] };
        fill_rect(buf, width, r, edge);
        // A 1 px edge on every side but the one facing the notes.
        let inner = match side {
            None => Rect::from_min_max(pos2(r.left() + 1.0, r.top()), pos2(r.right() - 1.0, r.bottom() - 1.0)),
            Some(KeyboardSide::Left) => Rect::from_min_max(pos2(r.left() + 1.0, r.top() + 1.0), pos2(r.right(), r.bottom() - 1.0)),
            Some(KeyboardSide::Right) => Rect::from_min_max(pos2(r.left(), r.top() + 1.0), pos2(r.right() - 1.0, r.bottom() - 1.0)),
        };
        fill_rect(buf, width, inner, body);
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
    visual.set_palette(cfg.look.note_palette, &cfg.look.custom_colors, cfg.look.rainbow_size);
    // Horizontal: the keyboard is a strip `keyboard_h` wide at `side`, and notes fill the rest.
    // Parallax: no keyboard, notes fill the frame, a playhead down the middle.
    let parallax = cfg.look.view == ViewMode::Parallax;
    // Velocity matrix: no keyboard and no notes, a grid of key by velocity (see `crate::matrix`).
    let matrix = cfg.look.view == ViewMode::Matrix;
    let (horizontal, side) = (cfg.look.view == ViewMode::Horizontal, cfg.look.keyboard_side);
    // Perspective: Vertical's layout, notes through the perspective (0 = off).
    // Horizontal without its keyboard: notes run to the edge.
    let bare = horizontal && !cfg.look.horizontal_keyboard;
    let keyboard_h = if parallax || bare || matrix { 0 }
        else if horizontal { (width as f32 * 0.12) as u32 }
        else if cfg.look.pfa_style { crate::pfa::key_height(height as f32, width as f32, visual.pitch_min, visual.pitch_max) as u32 }
        else { (height as f32 * 0.12) as u32 };
    let note_h = if horizontal || parallax || matrix { height } else { height - keyboard_h };
    // Perspective: one tilted plane, the keyboard's bottom edge full width (see `crate::Perspective`).
    let persp = if matches!(cfg.look.view, ViewMode::Perspective | ViewMode::Blocks) { 
        let p = crate::Perspective::new(cfg.look.perspective_far, keyboard_h as f32 / height as f32);
        if cfg.look.view == ViewMode::Blocks { p.blocks() } else { p }
    }
        else { crate::Perspective::OFF };
    let note_rect = match (horizontal, side) {
        _ if parallax || bare || matrix => [0, 0, width, height],
        (false, _) => [0, 0, width, note_h],
        (true, KeyboardSide::Left) => [keyboard_h + 1, 0, width - keyboard_h - 1, height],
        (true, KeyboardSide::Right) => [0, 0, width - keyboard_h - 1, height],
    };
    // Pixels along the time axis.
    let time_px = if horizontal || parallax { note_rect[2] } else { note_h };
    let mut gpu = gpu::Offscreen::new(width, height)?;

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

    let mut layers = crate::parallax::Parallax::default();
    let mut cells = crate::matrix::Matrix::new();
    let (mut glow, glow_look) = (crate::glow::Glow::default(), look.glow_look());
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
        let mut frame = free_rx.try_recv().unwrap_or_default();
        let (mut active, colors) = if matrix {
            gpu.render(&[], &keys, note_rect, [20, 20, 24], &mut frame);
            cells.advance(&visual.data, visual.data.tempo.time_to_tick(now_us));
            let area = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width as f32, height as f32));
            for (k, v, c, a) in cells.shown(&visual.data, now_us, (look.matrix_fade.max(0.0) * 1e6) as u64) {
                let col = crate::matrix::color(&visual, k, c, a);
                fill_rect(&mut frame, width, crate::matrix::cell_rect(area, k, v), [col.r(), col.g(), col.b()]);
            }
            ([false; 128], [egui::Color32::TRANSPARENT; 128])
        } else if parallax {
            let mut draws = layers.frame(&visual, now_us, duration_us, speed, look.parallax_far, time_px, look.note_mode(),
                look.collapse_overlaps, look.max_instances, piano);
            for d in &mut draws { d.view.time_shift += deficit; }
            gpu.render(&draws, &keys, note_rect, [20, 20, 24], &mut frame);
            ([false; 128], [egui::Color32::TRANSPARENT; 128])
        } else {
            visible.advance(&visual.data, now_us, window_end, future_us / time_px as u64);
            let lit = crate::build_note_instances(&visual, &mut visible, now_us, window_end, speed, time_px, piano, persp, &mut instances);
            let sign = match (horizontal, side) { (true, KeyboardSide::Right) => 1.0, (true, _) => -1.0, _ if look.notes_rise && !persp.is_on() => 1.0, _ => -1.0 };
            let view = gpu::NoteView { time_shift: deficit, horizontal, perspective_far: persp.far, perspective_near: persp.near, block_height: persp.block, ..gpu::NoteView::vertical(speed, now_us, sign, look.note_mode()) };
            gpu.render(&[gpu::LayerDraw { instances: instances.clone(), view }], &keys, note_rect, [20, 20, 24], &mut frame);
            lit
        };
        if deficit > 0.0 { active = [false; 128]; }
        if cfg.title && cfg.lead >= 1.0 {
            // Half-dark band behind the title, fading with it (the text itself is drawn by ffmpeg).
            let dim = 1.0 - 0.5 * intro_fade(f as f64 / cfg.fps as f64, cfg.lead as f64);
            if dim < 1.0 {
                let (top, band, _) = title_band(height);
                let rows = &mut frame[top as usize * width as usize * 4..((top + band) as usize).min(note_h as usize) * width as usize * 4];
                for px in rows.chunks_exact_mut(4) { for c in &mut px[..3] { *c = (*c as f64 * dim) as u8; } }
            }
        }
        if parallax {
            let x = width / 2;
            fill(&mut frame, width, (x - 1) as f32, (x + 1) as f32, 0, height, [255, 220, 140]);
        } else if bare || matrix {
        } else if horizontal {
            let (line, strip) = match side { KeyboardSide::Left => (keyboard_h, 0), KeyboardSide::Right => (width - keyboard_h - 1, width - keyboard_h) };
            fill(&mut frame, width, line as f32, (line + 1) as f32, 0, height, [200, 200, 210]);
            let rect = egui::Rect::from_min_max(egui::pos2(strip as f32, 0.0), egui::pos2((strip + keyboard_h) as f32, height as f32));
            fill_rect(&mut frame, width, rect, [235, 235, 240]);
            keyboard(&mut frame, width, rect, lo, hi, piano.then_some(&columns), Some(side), &active, &colors);
        } else if persp.is_on() {
            let pt = |x: u32, y: u32| egui::pos2(x as f32, y as f32);
            let kb = egui::Rect::from_min_max(pt(0, note_h), pt(width, height));
            fill_rect(&mut frame, width, kb, [20, 20, 24]);
            let mut quads = if look.pfa_style {
                let (whites, sharps) = crate::pfa::layout(piano.then_some(&columns), lo, hi, 0.0, width as f32);
                let lit = |p: u8| active[p as usize].then(|| { let c = colors[p as usize]; if c == egui::Color32::TRANSPARENT { [255, 220, 140] } else { [c.r(), c.g(), c.b()] } });
                crate::pfa::keyboard(0.0, note_h as f32, width as f32, keyboard_h as f32, [20, 20, 24], &whites,
                    if persp.block > 0.0 { &[] } else { &sharps }, &lit)
            } else {
                crate::ui::classic_quads(kb, lo, hi, piano.then_some(&columns), &active, &colors, persp.block == 0.0)
            };
            let view = egui::Rect::from_min_max(pt(0, 0), pt(width, height));
            persp.bend(view, kb, &mut quads);
            if persp.block > 0.0 {
                quads.extend(persp.raise(view, kb, &crate::ui::black_keys(kb, lo, hi, piano.then_some(&columns), look.pfa_style, &active, &colors)));
            }
            crate::pfa::fill(&mut frame, width, &quads);
        } else {
            fill(&mut frame, width, 0.0, width as f32, note_h, note_h + 1, [200, 200, 210]);
            fill(&mut frame, width, 0.0, width as f32, note_h + 1, height, [235, 235, 240]);
            if look.pfa_style {
                let (whites, sharps) = crate::pfa::layout(piano.then_some(&columns), lo, hi, 0.0, width as f32);
                let lit = |p: u8| active[p as usize].then(|| { let c = colors[p as usize]; if c == egui::Color32::TRANSPARENT { [255, 220, 140] } else { [c.r(), c.g(), c.b()] } });
                let quads = crate::pfa::keyboard(0.0, note_h as f32, width as f32, keyboard_h as f32, [20, 20, 24], &whites, &sharps, &lit);
                crate::pfa::fill(&mut frame, width, &quads);
            } else {
                keyboard(&mut frame, width, egui::Rect::from_min_max(egui::pos2(0.0, (note_h + 1) as f32), egui::pos2(width as f32, height as f32)),
                    lo, hi, piano.then_some(&columns), None, &active, &colors);
            }
            if let Some(gl) = &glow_look {
                glow.update(now_us, &active, &colors, gl);
                if !glow.is_dark() { add_glow(&mut frame, width, note_h, &glow.strip(&columns, width as usize, gl), gl.reach * note_h as f32); }
            }
        }
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
    let mut look = crate::settings::Settings::load();
    if let Some(v) = value("--view") {
        look.view = match v.as_str() { "vertical" => ViewMode::Vertical, "horizontal" => ViewMode::Horizontal, "parallax" => ViewMode::Parallax, "perspective" => ViewMode::Perspective, "blocks" => ViewMode::Blocks, "matrix" => ViewMode::Matrix,
            _ => panic!("view: vertical, horizontal, parallax, perspective, blocks or matrix") };
    }
    if let Some(v) = value("--keyboard-side") {
        look.keyboard_side = match v.as_str() { "left" => KeyboardSide::Left, "right" => KeyboardSide::Right, _ => panic!("keyboard side: left or right") };
    }
    if args.iter().any(|a| a == "--glow") { look.glow = true; }
    if let Some(v) = value("--glow-intensity") { look.glow_intensity = v.parse::<f32>().expect("bad --glow-intensity").clamp(0.0, 3.0); }
    if let Some(v) = value("--glow-decay") { look.glow_decay = v.parse::<f32>().expect("bad --glow-decay").clamp(0.05, 2.0); }
    if let Some(v) = value("--glow-spread") { look.glow_spread = v.parse::<f32>().expect("bad --glow-spread").clamp(0.5, 8.0); }
    if let Some(v) = value("--glow-reach") { look.glow_reach = v.parse::<f32>().expect("bad --glow-reach").clamp(0.02, 1.0); }
    if let Some(v) = value("--matrix-fade") { look.matrix_fade = v.parse::<f32>().expect("bad --matrix-fade").clamp(0.0, 2.0); }
    if let Some(v) = value("--perspective-far") { look.perspective_far = v.parse::<f32>().expect("bad --perspective-far").clamp(0.1, 0.9); }
    if let Some(v) = value("--parallax-far") { look.parallax_far = v.parse::<f32>().expect("bad --parallax-far").clamp(0.1, 1.0); }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// One note on key 100 for a beat, written to the temp dir.
    fn tiny_midi(name: &str) -> String {
        let mut file = b"MThd\0\0\0\x06\0\0\0\x01\x01\xe0MTrk".to_vec();
        let body = [0, 0x90, 100, 100, 0x83, 0x60, 0x80, 100, 0, 0, 0xff, 0x2f, 0];
        file.extend_from_slice(&(body.len() as u32).to_be_bytes());
        file.extend_from_slice(&body);
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, file).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// The first exported frame (160 x 90), or None without ffmpeg or a GPU.
    pub(crate) fn first_frame(name: &str, look: crate::settings::Settings, speed: f32) -> Option<Vec<u8>> {
        let out = std::env::temp_dir().join(format!("{name}.mp4"));
        let cfg = Settings { input: tiny_midi(&format!("{name}.mid")), output: out.to_string_lossy().into_owned(),
            width: 160, height: 90, speed, lead: 0.0, tail: 0.0, title: false, watermark: false, look, ..Default::default() };
        let mut frame = None;
        let result = run(&cfg, |r| { frame = Some(r.rgba.to_vec()); false });
        if frame.is_none() { eprintln!("export unavailable, skipped: {result:?}"); }
        frame
    }

    pub(crate) fn pixel(frame: &[u8], x: u32, y: u32) -> [u8; 3] {
        let i = (y as usize * 160 + x as usize) * 4;
        [frame[i], frame[i + 1], frame[i + 2]]
    }

    #[test]
    fn horizontal_export_puts_the_keyboard_on_its_side() {
        let look = crate::settings::Settings { view: ViewMode::Horizontal, keyboard_side: KeyboardSide::Right, ..Default::default() };
        let Some(frame) = first_frame("kimrashi-horizontal", look, 5.0) else { return };
        let key = pixel(&frame, 155, 45);
        assert!([[245, 245, 250], [25, 25, 30], [150, 150, 160], [80, 80, 80]].contains(&key), "keyboard on the right: {key:?}");
        assert_eq!(pixel(&frame, 80, 89), [20, 20, 24], "no keyboard along the bottom");
    }
    #[test]
    fn parallax_export_draws_a_playhead_and_no_keyboard() {
        let look = crate::settings::Settings { view: ViewMode::Parallax, ..Default::default() };
        let Some(frame) = first_frame("kimrashi-parallax", look, 5.0) else { return };
        assert_eq!(pixel(&frame, 80, 45), [255, 220, 140], "playhead in the middle");
        assert_eq!(pixel(&frame, 20, 85), [20, 20, 24], "no keyboard along the bottom");
    }
    #[test]
    fn horizontal_export_can_hide_the_keyboard() {
        let look = crate::settings::Settings { view: ViewMode::Horizontal, keyboard_side: KeyboardSide::Right,
            horizontal_keyboard: false, ..Default::default() };
        let Some(frame) = first_frame("kimrashi-horizontal-bare", look, 5.0) else { return };
        assert_eq!(pixel(&frame, 155, 45), [20, 20, 24], "notes reach the edge, no keyboard");
    }
    #[test]
    fn perspective_export_narrows_notes_with_distance() {
        // Mean x of the note's far (highest) row: key 100 sits right of centre, so perspective pulls it in.
        let far_end_x = |frame: &[u8]| {
            (0..78).find_map(|y| {
                let xs: Vec<u32> = (0..160).filter(|&x| pixel(frame, x, y) != [20, 20, 24]).collect();
                (!xs.is_empty()).then(|| xs.iter().sum::<u32>() as f32 / xs.len() as f32)
            }).unwrap()
        };
        let look = |view| crate::settings::Settings { view, ..Default::default() };
        let Some(vertical) = first_frame("kimrashi-vertical", look(ViewMode::Vertical), 1.0) else { return };
        let Some(perspective) = first_frame("kimrashi-perspective", look(ViewMode::Perspective), 1.0) else { return };
        let (v, p) = (far_end_x(&vertical), far_end_x(&perspective));
        assert!(p + 20.0 < v, "vertical {v}, perspective {p}");
    }
    #[test]
    fn perspective_export_bends_the_keyboard() {
        let look = crate::settings::Settings { view: ViewMode::Perspective, ..Default::default() };
        let Some(frame) = first_frame("kimrashi-perspective-keys", look, 5.0) else { return };
        assert_ne!(pixel(&frame, 1, 89), [20, 20, 24], "the bottom edge is full width");
        assert_eq!(pixel(&frame, 1, 81), [20, 20, 24], "the keyboard line is narrower: background beside it");
    }
    #[test]
    fn glow_brightens_above_the_sounding_key_only() {
        let look = |glow| crate::settings::Settings { glow, ..Default::default() };
        let Some(plain) = first_frame("kimrashi-glow-off", look(false), 1.0) else { return };
        let Some(lit) = first_frame("kimrashi-glow-on", look(true), 1.0) else { return };
        let sum = |f: &[u8], y| (0..160).map(|x| pixel(f, x, y).iter().map(|&c| c as u32).sum::<u32>()).sum::<u32>();
        assert!(sum(&lit, 70) > sum(&plain, 70) + 100, "light rises above the keyboard line");
        assert_eq!(pixel(&lit, 2, 70), pixel(&plain, 2, 70), "far from key 100 nothing changes");
    }
    #[test]
    fn matrix_export_lights_the_sounding_cell() {
        let look = crate::settings::Settings { view: ViewMode::Matrix, ..Default::default() };
        let Some(frame) = first_frame("kimrashi-matrix", look, 1.0) else { return };
        // Key 100 at velocity 100: x 125..126, y 19..20 of 160 x 90 (velocity 127 at the top).
        assert_ne!(pixel(&frame, 125, 19), [20, 20, 24], "the sounding cell is lit");
        assert_eq!(pixel(&frame, 80, 45), [20, 20, 24], "an unused cell is background");
        assert_eq!(pixel(&frame, 125, 89), [20, 20, 24], "no keyboard");
    }
    #[test]
    fn blocks_export_raises_the_black_keys() {
        let look = |view, pfa_style| crate::settings::Settings { view, pfa_style, ..Default::default() };
        for pfa in [false, true] {
            let Some(flat) = first_frame("kimrashi-blocks-flat", look(ViewMode::Perspective, pfa), 5.0) else { return };
            let Some(raised) = first_frame("kimrashi-blocks", look(ViewMode::Blocks, pfa), 5.0) else { return };
            let changed = (78..90).flat_map(|y| (0..160).map(move |x| (x, y))).filter(|&(x, y)| pixel(&flat, x, y) != pixel(&raised, x, y)).count();
            assert!(changed > 20, "pfa {pfa}: {changed} keyboard pixels changed");
        }
    }
}
