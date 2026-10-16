//! The Export video window: settings, live preview, and the encode-speed readout.

use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
use crate::export::{run, Encoder, Settings, PRESETS};

enum Msg {
    Frame { frame: u64, total: u64, speed: f64, preview: Option<egui::ColorImage> },
    Done(Result<(), String>),
}

struct Running {
    rx: crossbeam_channel::Receiver<Msg>,
    cancel: Arc<AtomicBool>,
    frame: u64,
    total: u64,
    speed: f64,
}

#[derive(Default)]
pub struct ExportUi {
    pub open: bool,
    cfg: Settings,
    running: Option<Running>,
    preview: Option<egui::TextureHandle>,
    result: Option<Result<(), String>>,
}

const RESOLUTIONS: [(&str, u32, u32); 4] = [("1280x720", 1280, 720), ("1920x1080", 1920, 1080), ("2560x1440", 2560, 1440), ("3840x2160", 3840, 2160)];

fn encoder_label(e: Encoder) -> &'static str {
    match e { Encoder::X264 => "x264 (CPU)", Encoder::Nvenc => "NVENC (NVIDIA)", Encoder::Qsv => "QSV (Intel)", Encoder::Amf => "AMF (AMD)" }
}

impl ExportUi {
    /// Opens the window for the loaded song, starting from the live note speed.
    pub fn show_for(&mut self, song_path: &str, look: &crate::settings::Settings) {
        self.open = true;
        if self.running.is_none() {
            self.cfg.input = song_path.to_owned();
            self.cfg.speed = look.note_speed;
            self.cfg.look = look.clone();
            if self.cfg.output.is_empty() || self.result.is_some() {
                self.cfg.output = std::path::Path::new(song_path).with_extension("mp4").to_string_lossy().into_owned();
            }
            self.result = None;
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
        let (tx, rx) = crossbeam_channel::unbounded();
        let cancel = Arc::new(AtomicBool::new(false));
        let (cfg, flag, ctx) = (self.cfg.clone(), cancel.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = run(&cfg, |r| {
                // A small nearest-neighbour preview every few frames keeps the render fast.
                let preview = (r.frame % 6 == 1).then(|| {
                    let (w, h) = ((cfg.width & !1) as usize, (cfg.height & !1) as usize);
                    let step = w.div_ceil(480).max(1);
                    let (pw, ph) = (w / step, h / step);
                    let mut pixels = Vec::with_capacity(pw * ph);
                    for y in 0..ph {
                        for x in 0..pw {
                            let i = ((y * step) * w + x * step) * 4;
                            pixels.push(egui::Color32::from_rgb(r.rgba[i], r.rgba[i + 1], r.rgba[i + 2]));
                        }
                    }
                    egui::ColorImage { size: [pw, ph], source_size: egui::vec2(pw as f32, ph as f32), pixels }
                });
                let _ = tx.send(Msg::Frame { frame: r.frame, total: r.total, speed: r.speed, preview });
                ctx.request_repaint();
                !flag.load(Ordering::Relaxed)
            });
            let _ = tx.send(Msg::Done(result));
            ctx.request_repaint();
        });
        self.result = None;
        self.running = Some(Running { rx, cancel, frame: 0, total: 1, speed: 0.0 });
    }

    pub fn window(&mut self, ctx: &egui::Context) {
        if let Some(run) = &mut self.running {
            let mut done = None;
            while let Ok(m) = run.rx.try_recv() {
                match m {
                    Msg::Frame { frame, total, speed, preview } => {
                        (run.frame, run.total, run.speed) = (frame, total, speed);
                        if let Some(img) = preview {
                            match &mut self.preview {
                                Some(t) => t.set(img, egui::TextureOptions::NEAREST),
                                None => self.preview = Some(ctx.load_texture("export.preview", img, egui::TextureOptions::NEAREST)),
                            }
                        }
                    }
                    Msg::Done(r) => done = Some(r),
                }
            }
            if done.is_some() {
                self.running = None;
                self.result = done;
            }
        }
        if !self.open {
            return;
        }
        let busy = self.running.is_some();
        let mut open = true;
        let mut start = false;
        egui::Window::new("Export video").open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                egui::Grid::new("export_grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    let c = &mut self.cfg;
                    ui.label("Output");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut c.output).desired_width(260.0));
                        if ui.button("…").clicked() {
                            if let Some(p) = rfd::FileDialog::new().add_filter("MP4 video", &["mp4"]).set_file_name("midi.mp4").save_file() {
                                c.output = p.to_string_lossy().into_owned();
                            }
                        }
                    });
                    ui.end_row();
                    ui.label("Resolution");
                    egui::ComboBox::from_id_salt("export_res").selected_text(format!("{}x{}", c.width, c.height)).show_ui(ui, |ui| {
                        for (name, w, h) in RESOLUTIONS {
                            if ui.selectable_label((c.width, c.height) == (w, h), name).clicked() {
                                (c.width, c.height) = (w, h);
                            }
                        }
                    });
                    ui.end_row();
                    ui.label("FPS");
                    ui.add(egui::Slider::new(&mut c.fps, 24..=144));
                    ui.end_row();
                    ui.label("Note speed");
                    ui.add(egui::Slider::new(&mut c.speed, 0.01..=12.0).logarithmic(true).suffix(" s"));
                    ui.end_row();
                    ui.label("Delay before first note");
                    ui.add(egui::Slider::new(&mut c.lead, 0.0..=10.0).suffix(" s"));
                    ui.end_row();
                    ui.label("Delay after last note");
                    ui.add(egui::Slider::new(&mut c.tail, 0.0..=10.0).suffix(" s"));
                    ui.end_row();
                    ui.label("Encoder");
                    egui::ComboBox::from_id_salt("export_enc").selected_text(encoder_label(c.encoder)).show_ui(ui, |ui| {
                        for e in [Encoder::X264, Encoder::Nvenc, Encoder::Qsv, Encoder::Amf] {
                            ui.selectable_value(&mut c.encoder, e, encoder_label(e));
                        }
                    });
                    ui.end_row();
                    ui.label("Speed preset");
                    ui.add(egui::Slider::new(&mut c.preset, 0..=8).custom_formatter(|v, _| PRESETS[v as usize].to_owned()));
                    ui.end_row();
                    ui.label("Quality");
                    ui.add(egui::Slider::new(&mut c.quality, 10..=40)).on_hover_text("Lower is better quality and a bigger file");
                    ui.end_row();
                    ui.label("Overlays");
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut c.title, "Song title");
                        ui.checkbox(&mut c.watermark, "Watermark").on_hover_text("\"Rendered with Kimrashi\" at the top. Optional, but appreciated!");
                    });
                    ui.end_row();
                    ui.label("Audio");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut c.audio).hint_text("optional, starts at the first note").desired_width(260.0));
                        if ui.button("…").clicked() {
                            if let Some(p) = rfd::FileDialog::new().add_filter("Audio", &["wav", "flac", "mp3", "ogg", "m4a", "aac"]).pick_file() {
                                c.audio = p.to_string_lossy().into_owned();
                            }
                        }
                    });
                    ui.end_row();
                    ui.label("ffmpeg");
                    ui.add(egui::TextEdit::singleline(&mut c.ffmpeg).desired_width(260.0));
                    ui.end_row();
                });
            });
            ui.add_space(6.0);
            match &self.running {
                Some(r) => {
                    let frac = r.frame as f32 / r.total.max(1) as f32;
                    let eta = (r.total - r.frame) as f64 / self.cfg.fps as f64 / r.speed.max(1e-6);
                    ui.add(egui::ProgressBar::new(frac).text(format!("{:.0}% · {:.1}x realtime · {} left", frac * 100.0, r.speed, crate::secs(eta))));
                    if ui.button("Cancel").clicked() {
                        r.cancel.store(true, Ordering::Relaxed);
                    }
                }
                None => {
                    if ui.add_enabled(!self.cfg.output.is_empty(), egui::Button::new("Start export")).clicked() {
                        start = true;
                    }
                    match &self.result {
                        Some(Ok(())) => {
                            ui.colored_label(egui::Color32::from_rgb(110, 210, 130), "Done! Your video is ready.");
                            let link = format!("https://github.com/{}", crate::update::REPO);
                            ui.weak("If you share it, a link to Kimrashi in the description would mean a lot. No pressure at all, thanks for using it!");
                            ui.horizontal(|ui| {
                                if ui.button("Copy GitHub link").on_hover_text(&link).clicked() {
                                    ui.ctx().copy_text(link.clone());
                                }
                                if ui.button("Show in folder").clicked() {
                                    // ponytail: Windows explorer only; the app is Windows-only today.
                                    let _ = std::process::Command::new("explorer")
                                        .arg(format!("/select,{}", self.cfg.output.replace('/', "\\")))
                                        .spawn();
                                }
                            });
                        }
                        Some(Err(e)) => { ui.colored_label(egui::Color32::from_rgb(255, 110, 100), format!("⚠ {e}")); }
                        None => {}
                    }
                }
            }
            if let Some(t) = &self.preview {
                ui.add_space(6.0);
                let w = ui.available_width().min(480.0);
                ui.image((t.id(), egui::vec2(w, w * t.size_vec2().y / t.size_vec2().x)));
            }
        });
        if start {
            self.start(ctx);
        }
        if !open {
            if let Some(r) = &self.running {
                r.cancel.store(true, Ordering::Relaxed);
            }
            self.open = false;
        }
    }
}
