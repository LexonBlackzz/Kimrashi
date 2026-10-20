//! The interface: toolbar pieces, the Settings window, the benchmark window, the piano roll and stats overlay.

use super::*;

impl AppState {
    pub(crate) fn transport_controls(&mut self, ui: &mut egui::Ui, current_time_us: u64) {
        if ui.button("Open MIDI…").on_hover_text("Ctrl+O").clicked() {
            self.open_file();
        }

        let has_song = self.current_song.is_some();
        let label = if self.last_telemetry.is_playing { "⏸ Pause" } else { "▶ Play" };
        if ui.add_enabled(has_song, egui::Button::new(label)).on_hover_text("Space").clicked() {
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

        if ui
            .add_enabled(has_song, egui::Button::new("Export video…"))
            .on_hover_text("Render the song to an MP4 with ffmpeg")
            .clicked()
        {
            if let Some(song) = &self.current_song {
                self.export.show_for(&song.data.path, &self.cfg);
            }
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

    pub(crate) fn start_benchmark(&mut self, ctx: &egui::Context) {
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

    pub(crate) fn benchmark_window(&mut self, ctx: &egui::Context) {
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

    pub(crate) fn settings_window(&mut self, ctx: &egui::Context) {
        let mut start_benchmark = false;
        let mut open = self.show_settings;
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Comma)) {
            open = !open;
        }
        egui::Window::new("Settings")
            .open(&mut open)
            // Top right, clear of the stats overlay (still draggable).
            .pivot(egui::Align2::RIGHT_TOP)
            .default_pos(ctx.screen_rect().right_top() + Vec2::new(-12.0, 64.0))
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.add(egui::TextEdit::singleline(&mut self.settings_search).hint_text("🔍 Search settings").desired_width(f32::INFINITY));
                let query = self.settings_search.trim().to_lowercase();
                if query.is_empty() {
                    ui.horizontal(|ui| {
                        for (tab, name) in [
                            (SettingsTab::Display, "Display"),
                            (SettingsTab::Audio, "Audio"),
                            (SettingsTab::Loading, "Loading"),
                            (SettingsTab::Performance, "Performance"),
                            (SettingsTab::Benchmark, "Benchmark"),
                        ] {
                            ui.selectable_value(&mut self.settings_tab, tab, name);
                        }
                    });
                }
                ui.separator();
                // A row shows on its tab, or anywhere its label or keywords match the search.
                let tab_now = self.settings_tab;
                let show = |tab: SettingsTab, words: &str| {
                    if query.is_empty() { tab == tab_now } else { words.to_lowercase().contains(&query) }
                };
                let mut shown = 0;
                egui::Grid::new("settings").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                    use SettingsTab::*;
                    let c = &mut self.cfg;
                    if show(Display, "Notes fall rise direction") {
                        ui.label("Notes");
                        ui.horizontal(|ui| {
                            ui.selectable_value(&mut c.notes_rise, false, "Fall");
                            ui.selectable_value(&mut c.notes_rise, true, "Rise");
                        });
                        ui.end_row();
                        shown += 1;
                    }
                    if show(Display, "Layout even piano keys") {
                        ui.label("Layout");
                        ui.horizontal(|ui| {
                            ui.selectable_value(&mut c.piano_layout, false, "Even").on_hover_text("Every key the same width");
                            ui.selectable_value(&mut c.piano_layout, true, "Piano")
                                .on_hover_text("Like the keys: sharps narrower, drawn over the naturals");
                        });
                        ui.end_row();
                        shown += 1;
                    }
                    if show(Display, "Shading flat shaded") {
                        ui.label("Shading");
                        ui.horizontal(|ui| {
                            ui.selectable_value(&mut c.flat_notes, false, "Shaded");
                            ui.selectable_value(&mut c.flat_notes, true, "Flat");
                        });
                        ui.end_row();
                        shown += 1;
                    }
                    if show(Display, "Theme palette colors dark light midnight forest rose interface") {
                        ui.label("Theme");
                        egui::ComboBox::from_id_salt("ui_theme")
                            .selected_text(crate::theme::UiTheme::ALL.iter().find(|t| t.0 == c.ui_theme).map_or("", |t| t.1))
                            .show_ui(ui, |ui| {
                                for (t, name) in crate::theme::UiTheme::ALL { ui.selectable_value(&mut c.ui_theme, t, name); }
                            });
                        ui.end_row();
                        shown += 1;
                    }
                    if show(Display, "Note palette colors rainbow golden pitch custom") {
                        ui.label("Note palette");
                        ui.vertical(|ui| {
                            egui::ComboBox::from_id_salt("note_palette")
                                .selected_text(crate::theme::NotePalette::ALL.iter().find(|t| t.0 == c.note_palette).map_or("", |t| t.1))
                                .show_ui(ui, |ui| {
                                    for (p, name) in crate::theme::NotePalette::ALL { ui.selectable_value(&mut c.note_palette, p, name); }
                                });
                            if c.note_palette == crate::theme::NotePalette::Custom {
                                use crate::theme::{colors_from_png, random_colors, random_from_png, MAX_CUSTOM};
                                ui.horizontal_wrapped(|ui| {
                                    ui.spacing_mut().item_spacing.x = 4.0;
                                    for rgb in c.custom_colors.iter_mut() { ui.color_edit_button_srgb(rgb); }
                                });
                                ui.horizontal(|ui| {
                                    if ui.button("Load PNG…").on_hover_text("Use the distinct colors of an image, in reading order").clicked() {
                                        if let Some(path) = rfd::FileDialog::new().add_filter("PNG image", &["png"]).pick_file() {
                                            match std::fs::read(&path).map_err(|e| e.to_string()).and_then(|b| colors_from_png(&b, MAX_CUSTOM)) {
                                                Ok(colors) => (c.custom_colors, c.palette_image) = (colors, path.to_string_lossy().into_owned()),
                                                Err(e) => self.status_message = Some(format!("Couldn't read palette: {e}")),
                                            }
                                        }
                                    }
                                    if ui.button("Randomize").on_hover_text("Random colors from the loaded PNG, or random hues without one").clicked() {
                                        let n = c.custom_colors.len().max(1);
                                        c.custom_colors = random_from_png(&c.palette_image, n).unwrap_or_else(|| random_colors(n));
                                    }
                                    if ui.add_enabled(c.custom_colors.len() < MAX_CUSTOM, egui::Button::new("+")).clicked() {
                                        c.custom_colors.push(random_colors(1)[0]);
                                    }
                                    if ui.add_enabled(c.custom_colors.len() > 1, egui::Button::new("−")).clicked() { c.custom_colors.pop(); }
                                });
                            }
                        });
                        ui.end_row();
                        shown += 1;
                    }
                    if show(Display, "Overlaps hide covered notes flatten") {
                        ui.label("Overlaps");
                        ui.checkbox(&mut c.collapse_overlaps, "Hide covered notes")
                            .on_hover_text("Keeps only what is visible on each key: the newest note on top, an older one showing again when it ends, hidden notes dropped. Borders only mark real starts and ends. Off draws every original note.");
                        ui.end_row();
                        shown += 1;
                    }
                    if show(Display, "Stats overlay compact detailed notes count synth") {
                        ui.label("Stats overlay");
                        ui.horizontal(|ui| {
                            ui.selectable_value(&mut c.stats, StatsMode::Count, "Notes");
                            ui.selectable_value(&mut c.stats, StatsMode::Compact, "Compact").on_hover_text("One line: notes, NPS, polyphony, FPS");
                            ui.selectable_value(&mut c.stats, StatsMode::Detailed, "Detailed");
                        });
                        ui.end_row();
                        ui.label("");
                        ui.add_enabled(c.stats == StatsMode::Detailed, egui::Checkbox::new(&mut c.synth_stats, "Synth / player / SVMS timing"));
                        ui.end_row();
                        shown += 1;
                    }

                    if show(Audio, "Output midi device synth") {
                        ui.label("Output");
                        let selected = self.outputs.get(self.output_idx).map_or("No MIDI output", |o| o.1.as_str());
                        let mut picked = None;
                        egui::ComboBox::from_id_salt("output").selected_text(selected).show_ui(ui, |ui| {
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
                        shown += 1;
                    }
                    if show(Audio, "Skip notes falls behind") {
                        ui.label("Skip notes");
                        if ui
                            .checkbox(&mut c.skip_notes, "When the synth falls behind")
                            .on_hover_text(
                                "Merge rapid re-strikes so playback stays in time when the synth \
                                 can't keep up. Turn off to stress-test a synth: every note is sent, \
                                 and Lag shows how far behind it falls.",
                            )
                            .changed()
                        {
                            let _ = self.cmd_tx.send(PlayerCommand::SetSkipping(c.skip_notes));
                        }
                        ui.end_row();
                        shown += 1;
                    }
                    if show(Audio, "Synth limit notes per second") {
                        ui.label("Synth limit");
                        let response = ui
                            .add(
                                egui::Slider::new(&mut c.synth_limit, 1_000.0..=SYNTH_LIMIT_OFF)
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
                            let limit = if c.synth_limit >= SYNTH_LIMIT_OFF { 0 } else { c.synth_limit as u32 };
                            let _ = self.cmd_tx.send(PlayerCommand::SetSynthLimit(limit));
                        }
                        ui.end_row();
                        shown += 1;
                    }
                    if show(Audio, "SVMS submit batch api") {
                        ui.label("SVMS submit");
                        if ui
                            .checkbox(&mut c.svms_batch, "Batch API")
                            .on_hover_text(
                                "SuperVirtualMIDISynth only: send each tick's messages in one \
                                 send_short_batch call instead of one SendDirectData per message. \
                                 Compare ns/msg with it on and off.",
                            )
                            .changed()
                        {
                            let _ = self.cmd_tx.send(PlayerCommand::SetSvmsBatch(c.svms_batch));
                        }
                        ui.end_row();
                        ui.label("");
                        if ui
                            .checkbox(&mut c.svms_direct, "Direct event pages")
                            .on_hover_text(
                                "SuperVirtualMIDISynth builds with the direct page feed only: batches \
                                 go straight into the synth's event pages instead of through its ring \
                                 and compiler thread, which costs the sender and compiler less per \
                                 note. Falls back by itself when the synth's settings can't take it \
                                 (minimum note length, note-on coalescing, shards).",
                            )
                            .changed()
                        {
                            let _ = self.cmd_tx.send(PlayerCommand::SetSvmsDirect(c.svms_direct));
                        }
                        ui.end_row();
                        shown += 1;
                    }

                    if show(Audio, "KDMAPI long data batch running status sysex") {
                        ui.label("KDMAPI submit");
                        if ui
                            .checkbox(&mut c.long_data, "Long data batches")
                            .on_hover_text(
                                "Send each slice as one raw MIDI byte stream (running status, \
                                 note-offs as velocity-0 note-ons) through SendDirectLongData instead \
                                 of one SendDirectData per message. For synths that read long \
                                 messages as event streams, like OmniMIDI; others may ignore them. \n                                 Not used with SVMS, which only reads SysEx there (use its batch API).",
                            )
                            .changed()
                        {
                            let _ = self.cmd_tx.send(PlayerCommand::SetLongData(c.long_data));
                        }
                        ui.end_row();
                        shown += 1;
                    }

                    if show(Loading, "Velocity levels precision") {
                        ui.label("Velocity");
                        ui.add(egui::Slider::new(&mut c.velocity_levels, 1..=127).custom_formatter(|v, _| match v as u8 {
                            127 => "exact".to_owned(),
                            1 => "single".to_owned(),
                            n => format!("{n} levels"),
                        }))
                        .on_hover_text(
                            "Velocity precision for files opened from now on. Fewer levels means \
                             more notes repeat their track's velocity, which packs them smaller in \
                             memory. Exact keeps every velocity; single plays every note at full \
                             velocity.",
                        );
                        ui.end_row();
                        shown += 1;
                    }
                    if show(Loading, "Repeats read every repeated track xz") {
                        ui.label("Repeats");
                        ui.checkbox(&mut c.verify_repeats, "Read every repeated track").on_hover_text(
                            "A file of one track repeated thousands of times (in xz) is loaded by reading \
                             a few copies and counting the rest, if the sizes add up to whole copies. \
                             Tick this to read every copy instead: exact, but a huge file can take hours.",
                        );
                        ui.end_row();
                        shown += 1;
                    }
                    if show(Loading, "Details stats window while loading") {
                        ui.label("Details");
                        ui.checkbox(&mut c.loading_details, "Show a stats window while loading")
                            .on_hover_text("Hovering the progress bar shows the same stats.");
                        ui.end_row();
                        shown += 1;
                    }
                    if show(Loading, "Updates check release version") {
                        self.updater.settings(ui);
                        shown += 1;
                    }

                    if show(Performance, "VSync sync display refresh") {
                        ui.label("VSync");
                        let label = match self.refresh_rate_hz {
                            Some(hz) => format!("Sync to display ({hz:.0} Hz)"),
                            None => "Sync to display".to_owned(),
                        };
                        ui.checkbox(&mut c.vsync, label).on_hover_text("Takes effect after restarting the player.");
                        ui.end_row();
                        shown += 1;
                    }
                    if show(Performance, "FPS limit frame rate cap") {
                        ui.label("FPS limit");
                        ui.add(egui::DragValue::new(&mut c.fps_limit).range(0..=1000).speed(1).custom_formatter(|v, _| if v == 0.0 { "Unlimited".into() } else { format!("{v:.0}") }))
                            .on_hover_text("0 = unlimited. Drag or double-click to type a value.");
                        ui.end_row();
                        shown += 1;
                    }
                    if show(Performance, "Instance limit max notes drawn") {
                        ui.label("Instance limit");
                        ui.add(egui::DragValue::new(&mut c.max_instances).range(0..=10_000_000).speed(1024))
                            .on_hover_text("0 disables the limit. Drops oldest sounding notes first; protects sharps in piano layout.");
                        ui.end_row();
                        shown += 1;
                    }
                    if show(Performance, "Stats log csv") {
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
                        shown += 1;
                    }

                    if show(Benchmark, "Benchmark score run") {
                        ui.label("Benchmark");
                        ui.add_enabled_ui(self.benchmark.is_none(), |ui| {
                            if ui
                                .button("Run (~1.5 min)")
                                .on_hover_text(
                                    "Times loading and playback code on made-up data and scores it. Close other \
                                     programs first, and pause playback.",
                                )
                                .clicked()
                            {
                                start_benchmark = true;
                            }
                        });
                        ui.end_row();
                        shown += 1;
                    }
                });
                if shown == 0 {
                    ui.weak("No settings match.");
                }
            });
        if start_benchmark {
            self.start_benchmark(ctx);
        }
        if self.show_settings && !open {
            self.save_settings();
        }
        self.show_settings = open;
    }
}
/// A byte count with a unit (decimal: kB to PB).
pub(crate) fn si_bytes(b: u64) -> String {
    let (mut v, mut unit) = (b as f64, 0);
    while v >= 1000.0 && unit < 5 {
        v /= 1000.0;
        unit += 1;
    }
    format!("{v:.1} {}", ["B", "kB", "MB", "GB", "TB", "PB"][unit])
}

/// The load's counters (all relaxed atomics the worker bumps once per track): rates are
/// averages since the start, so nothing is sampled.
pub(crate) fn loading_details(ui: &mut egui::Ui, l: &Loading) {
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

pub(crate) fn draw_piano_roll(
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
    piano_layout: bool,
    (stats_mode, synth_stats): (StatsMode, bool),
    visible: &mut VisibleNotes,
    instances: &mut Arc<gpu::InstanceRing>,
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
            piano_layout, instances,
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

    let detailed_stats = stats_mode == StatsMode::Detailed;
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
    } else if stats_mode == StatsMode::Compact {
        format!(
            "{} notes · {}/s · poly {} · {:.0} fps",
            si(telemetry.notes_played as f64),
            si(telemetry.nps as f64),
            telemetry.polyphony,
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
pub(crate) fn key_columns(piano: bool, pitch_min: u8, pitch_max: u8) -> [(f32, f32); 128] {
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

pub(crate) fn draw_keyboard(
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


pub(crate) fn is_black_key(pitch: u8) -> bool {
    matches!(pitch % 12, 1 | 3 | 6 | 8 | 10)
}

/// In a wrapping row, start a new line if a label + slider won't fit (egui doesn't
/// wrap a slider as one unit, so it would get clipped at the window edge).
pub(crate) fn wrap_for_slider(ui: &mut egui::Ui) {
    let needed = ui.spacing().slider_width + 180.0;
    if ui.available_size_before_wrap().x < needed {
        ui.end_row();
    }
}

/// One-line load summary plus a per-phase breakdown (for the hover).
pub(crate) fn load_info(song: &SongData) -> (String, String) {
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
pub(crate) fn si(v: f64) -> String {
    match v {
        v if v >= 1e12 => format!("{:.2}T", v / 1e12),
        v if v >= 1e9 => format!("{:.2}B", v / 1e9),
        v if v >= 1e6 => format!("{:.2}M", v / 1e6),
        v if v >= 1e3 => format!("{:.1}K", v / 1e3),
        v => format!("{v:.0}"),
    }
}

pub(crate) fn bytes(v: f64) -> String {
    match v {
        v if v >= 1e12 => format!("{:.2} TB", v / 1e12),
        v if v >= 1e9 => format!("{:.2} GB", v / 1e9),
        v if v >= 1e6 => format!("{:.1} MB", v / 1e6),
        v if v >= 1e3 => format!("{:.1} KB", v / 1e3),
        v => format!("{v:.0} B"),
    }
}

pub(crate) fn secs(s: f64) -> String {
    if s >= 1.0 {
        format!("{s:.2} s")
    } else {
        format!("{:.1} ms", s * 1e3)
    }
}

/// The SVMS section of the stats overlay: the synth's own view of the last second.
pub(crate) fn svms_text(s: &svms::Stats) -> String {
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

pub(crate) fn format_timestamp(us: u64) -> String {
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
