//! The interface: toolbar pieces, the Settings window, the benchmark window, the piano roll and stats overlay.

use super::*;
use crate::settings::KeyboardSide;

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
            .add_enabled(has_song, egui::Button::new("⏹ Stop"))
            .on_hover_text("Silence every note and go back to the start")
            .clicked()
        {
            self.stop();
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
            .button("Diagnose MIDI…")
            .on_hover_text("Pick a file to get its note count, tracks, size, memory, length, PPQN, tempo, max NPS and polyphony, without playing it")
            .clicked()
        {
            self.start_diagnose(ui.ctx());
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

    /// Pick a file and diagnose it on a worker thread; it is parsed like a load but never played.
    fn start_diagnose(&mut self, ctx: &egui::Context) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("MIDI file or archive", &["mid", "midi", "zip", "7z", "rar", "xz", "gz", "zst", "tar", "tgz", "txz"])
            .add_filter("All files", &["*"])
            .pick_file()
        else {
            return;
        };
        if let Some(d) = self.diagnose.take() {
            d.progress.cancel();
        }
        let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let progress = Arc::new(parser::Progress::default());
        progress.verify_all.store(self.cfg.verify_repeats, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (ctx, vel_levels, p) = (ctx.clone(), self.cfg.velocity_levels, progress.clone());
        std::thread::spawn(move || {
            let _ = tx.send(SongData::load(&path, vel_levels, &p).map(|s| diagnose::run(&s)).map_err(|e| e.to_string()));
            ctx.request_repaint();
        });
        self.diagnose = Some(DiagRun { name, rx, progress, report: None, started: Instant::now() });
    }

    pub(crate) fn diagnose_window(&mut self, ctx: &egui::Context) {
        let Some(d) = &mut self.diagnose else { return };
        if let Ok(r) = d.rx.try_recv() {
            d.report = Some(r);
        }
        let mut open = true;
        egui::Window::new(format!("Diagnose · {}", d.name))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                let r = match &d.report {
                    None => {
                        ui.label(format!("Reading {}… {}", d.name, secs(d.started.elapsed().as_secs_f64())));
                        ctx.request_repaint_after(Duration::from_millis(250));
                        return;
                    }
                    Some(Err(e)) => {
                        ui.colored_label(Color32::from_rgb(255, 110, 100), format!("⚠ {e}"));
                        return;
                    }
                    Some(Ok(r)) => r,
                };
                let size = match r.archive_bytes {
                    Some(a) => format!("{} ({} in the archive)", bytes(r.file_bytes as f64), bytes(a as f64)),
                    None => bytes(r.file_bytes as f64),
                };
                let tempo = if r.tempo_changes > 1 {
                    format!("{:.2} BPM at start ({:.2}–{:.2}, {} changes)", r.first_bpm, r.min_bpm, r.max_bpm, r.tempo_changes)
                } else {
                    format!("{:.2} BPM", r.first_bpm)
                };
                let rows = [
                    ("Notes", format!("{} ({})", commas(r.notes), si(r.notes as f64))),
                    ("Tracks", commas(r.tracks as u64)),
                    ("File size", size),
                    ("Memory to open", bytes(r.packed_bytes as f64)),
                    ("Time", format_timestamp(r.duration_us)),
                    ("PPQN", r.ppqn.to_string()),
                    ("Tempo", tempo),
                    ("Max NPS", format!("{} ({})", commas(r.max_nps), si(r.max_nps as f64))),
                    ("Max polyphony", format!("{} ({})", commas(r.max_polyphony), si(r.max_polyphony as f64))),
                ];
                egui::Grid::new("diagnose_results").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
                    for (k, v) in &rows {
                        ui.label(*k);
                        ui.monospace(v);
                        ui.end_row();
                    }
                });
                ui.add_space(6.0);
                if ui.button("Copy").clicked() {
                    ctx.copy_text(rows.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join("\n"));
                }
            });
        if !open {
            d.progress.cancel(); // a file still parsing stops at its next read
            self.diagnose = None;
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
                            (SettingsTab::Effects, "Effects"),
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
                    if show(Display, "View vertical horizontal parallax perspective") {
                        use crate::settings::{KeyboardSide, ViewMode};
                        ui.label("View");
                        ui.horizontal(|ui| {
                            ui.selectable_value(&mut c.view, ViewMode::Vertical, "Vertical");
                            ui.selectable_value(&mut c.view, ViewMode::Horizontal, "Horizontal");
                            ui.selectable_value(&mut c.view, ViewMode::Parallax, "Parallax")
                                .on_hover_text("No keyboard: notes cross a line in the middle, every track at its own speed");
                            ui.selectable_value(&mut c.view, ViewMode::Perspective, "Perspective")
                                .on_hover_text("The vertical view at an angle: far notes small at the top");
                            ui.selectable_value(&mut c.view, ViewMode::Blocks, "Blocks")
                                .on_hover_text("Perspective in 3D: notes are blocks, black keys stand up");
                            ui.selectable_value(&mut c.view, ViewMode::Matrix, "Matrix")
                                .on_hover_text("Velocity matrix: keys across, velocity up, a cell lit while a note sounds on it");
                        });
                        ui.end_row();
                        shown += 1;
                        if c.view == ViewMode::Horizontal {
                            ui.label("Keyboard");
                            ui.horizontal(|ui| {
                                ui.selectable_value(&mut c.keyboard_side, KeyboardSide::Left, "Left");
                                ui.selectable_value(&mut c.keyboard_side, KeyboardSide::Right, "Right");
                                ui.checkbox(&mut c.horizontal_keyboard, "Show");
                            }).response.on_hover_text("Which side notes travel to; hide the keyboard and they run to that edge");
                            ui.end_row();
                        }
                        if c.view == ViewMode::Parallax {
                            ui.label("Far speed");
                            // Applied when the drag ends: every change rebuilds all the layers.
                            let id = egui::Id::new("parallax_far_drag");
                            let mut far = ui.data(|d| d.get_temp::<f32>(id)).unwrap_or(c.parallax_far);
                            let r = ui.add(egui::Slider::new(&mut far, 0.1..=1.0).fixed_decimals(2).suffix("×"))
                                .on_hover_text("The farthest track's speed, as a fraction of the nearest's");
                            if r.dragged() {
                                ui.data_mut(|d| d.insert_temp(id, far));
                            } else {
                                ui.data_mut(|d| d.remove::<f32>(id));
                                c.parallax_far = far;
                            }
                            ui.end_row();
                        }
                        if c.view == ViewMode::Matrix {
                            ui.label("Fade");
                            ui.add(egui::Slider::new(&mut c.matrix_fade, 0.0..=2.0).fixed_decimals(2).suffix(" s"))
                                .on_hover_text("How long a cell takes to fade out after its last note ends (0: at once)");
                            ui.end_row();
                        }
                        if matches!(c.view, ViewMode::Perspective | ViewMode::Blocks) {
                            ui.label("Far width");
                            ui.add(egui::Slider::new(&mut c.perspective_far, 0.1..=0.9).fixed_decimals(2).suffix("×"))
                                .on_hover_text("How wide the far (top) edge is, as a fraction of the keyboard's width");
                            ui.end_row();
                        }
                    }
                    if show(Effects, "Effects presets subtle sapphire neon party key glow light bloom intensity decay spread tail reach glass keys rainbow bar confetti particles") {
                        if c.view != crate::settings::ViewMode::Vertical {
                            ui.label("Effects");
                            ui.label("Effects show in the Vertical view.");
                            ui.end_row();
                        } else {
                            ui.label("Presets");
                            ui.horizontal(|ui| {
                                let current = c.effects_preset();
                                for p in crate::settings::EffectsPreset::ALL {
                                    if ui.selectable_label(current == Some(p), p.name()).clicked() { c.apply_effects(p); }
                                }
                            }).response.on_hover_text("One-click looks; adjust the sliders from there (Off keeps them)");
                            ui.end_row();
                            ui.label("Key glow");
                            ui.checkbox(&mut c.glow, "On").on_hover_text("Hit keys light up and spill light over the keys and up into the notes");
                            ui.end_row();
                            if c.glow {
                                ui.label("  Light");
                                ui.horizontal(|ui| {
                                    { ui.label("Intensity"); ui.add(egui::Slider::new(&mut c.glow_intensity, 0.0..=3.0).fixed_decimals(2)) };
                                    { ui.label("Decay"); ui.add(egui::Slider::new(&mut c.glow_decay, 0.05..=2.0).fixed_decimals(2).suffix(" s")) }
                                        .on_hover_text("How fast the light fades after a key is released");
                                });
                                ui.end_row();
                                ui.label("");
                                ui.horizontal(|ui| {
                                    { ui.label("Spread"); ui.add(egui::Slider::new(&mut c.glow_spread, 0.5..=8.0).fixed_decimals(1).suffix(" keys")) }
                                        .on_hover_text("How far the light spills sideways");
                                    { ui.label("Tail"); ui.add(egui::Slider::new(&mut c.glow_tail, 0.0..=0.9).fixed_decimals(2)) }
                                        .on_hover_text("Sapphire-style: wider, fainter glows stacked around the light (0: one soft glow)");
                                });
                                ui.end_row();
                                ui.label("");
                                ui.horizontal(|ui| {
                                    ui.label("Reach");
                                    ui.add(egui::Slider::new(&mut c.glow_reach, 0.02..=1.0).fixed_decimals(2))
                                        .on_hover_text("How far up the light rises, as a fraction of the note area");
                                });
                                ui.end_row();
                                ui.label("  Keys");
                                ui.horizontal(|ui| {
                                    ui.checkbox(&mut c.glow_glass, "Glass keys").on_hover_text("Frosted keys, dark at rest, lit from inside in their notes' colours");
                                    { ui.label("On keys"); ui.add(egui::Slider::new(&mut c.glow_on_keys, 0.0..=1.0).fixed_decimals(2)) }
                                        .on_hover_text("How strongly the light colours the keys themselves");
                                });
                                ui.end_row();
                            }
                            ui.label("Rainbow bar");
                            ui.checkbox(&mut c.glow_bar, "On").on_hover_text("A moving rainbow along the top of the keys that flares where keys are hit");
                            ui.end_row();
                            if c.glow_bar {
                                ui.label("");
                                ui.horizontal(|ui| {
                                    { ui.label("Height"); ui.add(egui::Slider::new(&mut c.glow_bar_height, 0.02..=0.5).fixed_decimals(2)) }
                                        .on_hover_text("As a fraction of the keys' height");
                                    { ui.label("Speed"); ui.add(egui::Slider::new(&mut c.glow_bar_speed, -1.0..=1.0).fixed_decimals(2).suffix(" turns/s")) }
                                        .on_hover_text("How fast the rainbow moves (negative: the other way; 0: still)");
                                });
                                ui.end_row();
                                ui.label("");
                                ui.horizontal(|ui| {
                                    { ui.label("Brightness"); ui.add(egui::Slider::new(&mut c.glow_bar_rest, 0.0..=1.0).fixed_decimals(2)) }
                                        .on_hover_text("How bright the bar is at rest; hits flare it toward white");
                                    ui.checkbox(&mut c.glow_bar_single, "One colour").on_hover_text("One colour instead of the moving rainbow");
                                    if c.glow_bar_single { ui.color_edit_button_srgb(&mut c.glow_bar_color); }
                                });
                                ui.end_row();
                            }
                            ui.label("Confetti");
                            ui.checkbox(&mut c.confetti, "On").on_hover_text("Keys throw paper confetti up into the notes as their notes start");
                            ui.end_row();
                            if c.confetti {
                                ui.label("");
                                ui.horizontal(|ui| {
                                    ui.label("Amount");
                                    ui.add(egui::Slider::new(&mut c.confetti_amount, 1..=40)).on_hover_text("Pieces per note start");
                                    ui.label("Size");
                                    ui.add(egui::Slider::new(&mut c.confetti_size, 0.005..=0.05).fixed_decimals(3))
                                        .on_hover_text("A piece's length, as a fraction of the note area's height");
                                });
                                ui.end_row();
                                ui.label("");
                                ui.horizontal(|ui| {
                                    ui.label("Gravity");
                                    ui.add(egui::Slider::new(&mut c.confetti_gravity, 0.0..=5.0).fixed_decimals(2))
                                        .on_hover_text("How hard pieces are pulled back down (0: they float)");
                                    ui.label("Life");
                                    ui.add(egui::Slider::new(&mut c.confetti_life, 0.2..=5.0).fixed_decimals(1).suffix(" s"))
                                        .on_hover_text("How long a piece lasts before it shrinks away");
                                });
                                ui.end_row();
                                ui.label("");
                                ui.checkbox(&mut c.confetti_rainbow, "Rainbow colours").on_hover_text("Random bright colours instead of the note's");
                                ui.end_row();
                            }
                        }
                        shown += 1;
                    }
                    if c.view == crate::settings::ViewMode::Vertical && show(Display, "Notes fall rise direction") {
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
                            let mut style = if c.pfa_style { 2 } else { c.flat_notes as u8 };
                            ui.selectable_value(&mut style, 0, "Shaded");
                            ui.selectable_value(&mut style, 1, "Flat");
                            ui.selectable_value(&mut style, 2, "PFA").on_hover_text("Piano From Above's notes and keyboard");
                            (c.pfa_style, c.flat_notes) = (style == 2, style == 1);
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
                            if c.note_palette == crate::theme::NotePalette::Rainbow {
                                ui.horizontal(|ui| {
                                    ui.label("Steps");
                                    egui::ComboBox::from_id_salt("rainbow_size")
                                        .selected_text(c.rainbow_size.to_string())
                                        .show_ui(ui, |ui| {
                                            for n in crate::theme::RAINBOW_SIZES { ui.selectable_value(&mut c.rainbow_size, n, n.to_string()); }
                                        });
                                });
                            }
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
                    if show(Loading, "FL Studio project flp play while loading quick start") {
                        ui.label("FL projects");
                        ui.checkbox(&mut c.flp_quick_start, "Play FL projects while loading").on_hover_text(
                            "Packs the first 45 s of an .flp first and starts playing it while the rest                              loads; the whole song then takes over at the same place. If the opening                              runs out first, playback waits for it.",
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
        let stats = p.flp_stats.lock().map(|s| s.clone()).unwrap_or_default();
        if !stats.is_empty() {
            ui.label("FL project");
            ui.label(stats);
            ui.end_row();
        }
        let stage = p.flp_stage.lock().map(|s| *s).unwrap_or_default();
        if !stage.is_empty() {
            let (d, t) = (p.flp_done.load(Relaxed), p.flp_total.load(Relaxed));
            let pct = if t > 0 { format!(" {:.0}%", d as f64 * 100.0 / t as f64) } else { String::new() };
            let detail = match stage {
                "Packing" => format!(" · {} notes packed", si(p.flp_notes.load(Relaxed) as f64)),
                _ if t > 0 => format!(" · {} of {}", mb(d), mb(t)),
                _ => format!(" · {}", mb(d)),
            };
            ui.label("FL → MIDI");
            ui.label(format!("{stage}…{pct}{detail}"));
            ui.end_row();
        }
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
        ui.label(match total {
            0 if !stage.is_empty() => "after the FL project is turned into MIDI".to_owned(),
            0 => "waiting for the header…".to_owned(),
            _ => format!("{done} of {total}"),
        });
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
    note_mode: u32,
    piano_layout: bool,
    (view, keyboard_side, parallax_far, horizontal_keyboard, perspective_far, matrix_fade): (crate::settings::ViewMode, KeyboardSide, f32, bool, f32, f32),
    (stats_mode, synth_stats): (StatsMode, bool),
    visible: &mut VisibleNotes,
    parallax: &mut crate::parallax::Parallax,
    matrix: &mut crate::matrix::Matrix,
    (glow, glow_look): (&mut crate::glow::Glow, Option<crate::glow::GlowLook>),
    (confetti, confetti_look): (&mut crate::confetti::Confetti, Option<crate::confetti::ConfettiLook>),
    instances: &mut Arc<gpu::InstanceRing>,
    times: &mut FrameTimes,
) {
    let (response, painter) = ui.allocate_painter(ui.available_size(), Sense::hover());
    let rect = response.rect;

    let (pitch_min, pitch_max) = if let Some(song) = song {
        (song.pitch_min, song.pitch_max)
    } else {
        (DEFAULT_PITCH_MIN, DEFAULT_PITCH_MAX)
    };

    // Horizontal: the classic keyboard on its side (PFA's keyboard is only drawn along the bottom).
    let side = (view == crate::settings::ViewMode::Horizontal).then_some(keyboard_side);
    // Parallax: no keyboard, notes fill the view, a playhead down the middle.
    let in_parallax = view == crate::settings::ViewMode::Parallax;
    // Perspective: Vertical's layout, notes through the perspective (0 = off).
    let blocks = view == crate::settings::ViewMode::Blocks;
    let perspective = blocks || view == crate::settings::ViewMode::Perspective;
    // Horizontal without its keyboard: notes run to the edge.
    let bare = side.is_some() && !horizontal_keyboard;
    // Velocity matrix: no keyboard, no notes, a grid of key by velocity (see `crate::matrix`).
    let in_matrix = view == crate::settings::ViewMode::Matrix;
    if !in_parallax { parallax.clear(); }
    if !in_matrix { matrix.clear(); }
    // The keyboard grows to PFA's size and back, easing out in both directions.
    let pfa = note_mode == 2 && side.is_none() && !in_parallax && !in_matrix;
    let linear = ui.ctx().animate_bool_with_time(egui::Id::new("pfa_keyboard"), pfa, 0.4);
    let eased = if pfa { egui::emath::easing::cubic_out(linear) } else { 1.0 - egui::emath::easing::cubic_out(1.0 - linear) };
    let classic_height = (rect.height() * 0.12).clamp(36.0, 120.0);
    let keyboard_height = classic_height + (crate::pfa::key_height(rect.height(), rect.width(), pitch_min, pitch_max) - classic_height) * eased;
    // Perspective: one tilted plane, the keyboard's bottom edge full width (see `crate::Perspective`).
    let persp = if perspective {
        let p = crate::Perspective::new(perspective_far, keyboard_height / rect.height().max(1.0));
        if blocks { p.blocks() } else { p }
    }
        else { crate::Perspective::OFF };
    let keyboard_width = (rect.width() * 0.12).clamp(36.0, 120.0);
    let (note_area, keyboard_rect, edge) = match side {
        _ if in_parallax || in_matrix || bare => (rect, Rect::from_min_max(rect.max, rect.max), [rect.max, rect.max]),
        None => {
            let y = rect.bottom() - keyboard_height;
            (Rect::from_min_max(rect.min, Pos2::new(rect.right(), y)), Rect::from_min_max(Pos2::new(rect.left(), y), rect.max),
                [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)])
        }
        Some(KeyboardSide::Left) => {
            let x = rect.left() + keyboard_width;
            (Rect::from_min_max(Pos2::new(x, rect.top()), rect.max), Rect::from_min_max(rect.min, Pos2::new(x, rect.bottom())),
                [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())])
        }
        Some(KeyboardSide::Right) => {
            let x = rect.right() - keyboard_width;
            (Rect::from_min_max(rect.min, Pos2::new(x, rect.bottom())), Rect::from_min_max(Pos2::new(x, rect.top()), rect.max),
                [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())])
        }
    };

    painter.rect_filled(rect, CornerRadius::ZERO, Color32::from_rgb(10, 10, 12));
    painter.rect_filled(note_area, CornerRadius::ZERO, Color32::from_rgb(20, 20, 24));
    if !persp.is_on() { painter.rect_filled(keyboard_rect, CornerRadius::ZERO, Color32::from_rgb(235, 235, 240)); }

    let speed = note_speed_seconds.clamp(0.01, 15.0);
    let future_us = (speed * 1_000_000.0).max(1.0) as u64;
    let window_end = now_us
        .saturating_add(future_us)
        .min(duration_us.max(now_us));

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

    if !in_parallax && !in_matrix && !bare && !persp.is_on() { painter.line_segment(edge, Stroke::new(1.0_f32, Color32::from_rgb(200, 200, 210))); }

    if let Some(song) = song {
        let ppp = ui.ctx().pixels_per_point();
        // Pixels along the time axis.
        let time_px = if side.is_some() || in_parallax { (note_area.right() * ppp).round() - (note_area.left() * ppp).round() }
            else { (note_area.bottom() * ppp).round() - (note_area.top() * ppp).round() }.max(1.0);
        if in_matrix {
            let t0 = Instant::now();
            matrix.advance(&song.data, song.data.tempo.time_to_tick(now_us));
            let mut mesh = egui::Mesh::default();
            for (k, v, c, a) in matrix.shown(&song.data, now_us, (matrix_fade.max(0.0) * 1e6) as u64) {
                mesh.add_colored_rect(crate::matrix::cell_rect(note_area, k, v), crate::matrix::color(song, k, c, a));
            }
            painter.add(egui::Shape::mesh(mesh));
            times.build = t0.elapsed();
        } else if in_parallax {
            let t0 = Instant::now();
            let layers = parallax.frame(song, now_us, duration_us, speed, parallax_far, time_px as u32, note_mode,
                visible.collapse_overlaps, visible.max_instances, piano_layout);
            times.build = t0.elapsed();
            (times.advance, times.decoded, times.profile) = Default::default(); // per-layer numbers aren't summed
            times.instances = layers.iter().map(|l| l.instances.len()).sum();
            if let Some(state) = frame.wgpu_render_state() {
                if !gpu::no_notes() {
                    // Nearest first, so farther layers skip pixels it already covers.
                    for (k, layer) in layers.into_iter().enumerate().filter(|(_, l)| !l.instances.is_empty()) {
                        painter.add(gpu::NotesCallback::new(note_area, state.target_format, k, layer.instances, gpu_key_positions, layer.view));
                    }
                }
            }
            let x = note_area.center().x;
            painter.line_segment([Pos2::new(x, note_area.top()), Pos2::new(x, note_area.bottom())],
                Stroke::new(2.0_f32, Color32::from_rgb(255, 220, 140)));
        } else {
            let px_us = future_us as f32 / time_px;
            visible.advance(&song.data, now_us, window_end, px_us as u64);
            times.advance = visible.timing;
            times.decoded = visible.last_batch;
            let t0 = Instant::now();

            (active_keys, active_colors) = build_note_instances(
                song, visible, now_us, window_end, speed, time_px as u32, piano_layout, persp, instances,
            );

            times.build = t0.elapsed();
            times.profile = visible.build_profile;
            times.instances = instances.len();
            if let Some(state) = frame.wgpu_render_state() {
                if !instances.is_empty() && !gpu::no_notes() {
                    painter.add(gpu::NotesCallback::new(
                        note_area,
                        state.target_format,
                        0,
                        instances.clone(),
                        gpu_key_positions,
                        gpu::NoteView { horizontal: side.is_some(), perspective_far: persp.far, perspective_near: persp.near, block_height: persp.block, ..gpu::NoteView::vertical(speed, now_us, match side {
                            Some(KeyboardSide::Right) => 1.0, Some(KeyboardSide::Left) => -1.0, None if notes_upwards && !persp.is_on() => 1.0, None => -1.0,
                        }, note_mode) },
                    ));
                }
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

    if persp.is_on() {
        let mut quads = if pfa {
            let (whites, sharps) = crate::pfa::layout(piano_layout.then_some(&columns), pitch_min, pitch_max, keyboard_rect.left(), keyboard_rect.width());
            crate::pfa::keyboard(keyboard_rect.left(), keyboard_rect.top(), keyboard_rect.width(), keyboard_rect.height(), [20, 20, 24],
                &whites, if persp.block > 0.0 { &[] } else { &sharps }, &|p| lit_color(active_keys[p as usize], active_colors[p as usize]))
        } else {
            classic_quads(keyboard_rect, pitch_min, pitch_max, piano_layout.then_some(&columns), &active_keys, &active_colors, persp.block == 0.0)
        };
        persp.bend(rect, keyboard_rect, &mut quads);
        if persp.block > 0.0 {
            quads.extend(persp.raise(rect, keyboard_rect,
                &black_keys(keyboard_rect, pitch_min, pitch_max, piano_layout.then_some(&columns), pfa, &active_keys, &active_colors)));
        }
        paint_quads(&painter, &quads);
    } else if eased > 0.0 && side.is_none() && !in_parallax && !in_matrix {
        draw_pfa_keyboard(&painter, keyboard_rect, pitch_min, pitch_max, piano_layout.then_some(&columns), &active_keys, &active_colors);
    }
    if eased < 1.0 && !in_parallax && !in_matrix && !bare && !persp.is_on() {
        // Switching styles: PFA lies underneath and the classic keyboard fades out over it (or in,
        // shrinking back), so the two crossfade instead of PFA popping in at the end.
        let mut classic = painter.clone();
        if side.is_none() && eased > 0.0 { classic.multiply_opacity(1.0 - eased); }
        draw_keyboard(
            &classic,
            keyboard_rect,
            pitch_min,
            pitch_max,
            piano_layout.then_some(&columns),
            side,
            &active_keys,
            &active_colors,
        );
    }

    // Key glow (Vertical only): light from hit keys, added over the keys and up into the notes.
    let along_bottom = side.is_none() && !in_parallax && !in_matrix && !persp.is_on();
    match glow_look.filter(|_| along_bottom && song.is_some()) {
        Some(look) => {
            glow.update(now_us, &active_keys, &active_colors, &look);
            if look.glow && look.glass {
                paint_quads(&painter, &glass_quads(keyboard_rect, pitch_min, pitch_max, piano_layout.then_some(&columns), glow, look.intensity));
            }
            if look.glow && !glow.is_dark() { paint_glow(&painter, glow, &columns, note_area, keyboard_rect, &look); }
            if look.bar_height > 0.0 { paint_bar(&painter, glow, &columns, note_area, keyboard_rect, &look, now_us); }
        }
        None => *glow = Default::default(),
    }
    match confetti_look.filter(|_| along_bottom && song.is_some()) {
        Some(look) => {
            confetti.update(now_us, &active_keys, &active_colors, &columns, &look);
            let quads = confetti.quads(note_area.left(), note_area.width(), note_area.bottom(), note_area.height(), &look);
            paint_quads(&painter.with_clip_rect(note_area.union(keyboard_rect)), &quads);
        }
        None => *confetti = Default::default(),
    }

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

/// `draw_keyboard` in Piano From Above's style (see `pfa`).
fn draw_pfa_keyboard(painter: &egui::Painter, rect: Rect, lo: u8, hi: u8, columns: Option<&[(f32, f32); 128]>,
    active: &[bool; 128], colors: &[Color32; 128])
{
    if lo > hi { return; }
    let (whites, sharps) = crate::pfa::layout(columns, lo, hi, rect.left(), rect.width());
    let quads = crate::pfa::keyboard(rect.left(), rect.top(), rect.width(), rect.height(), [20, 20, 24], &whites, &sharps,
        &|p| lit_color(active[p as usize], colors[p as usize]));
    paint_quads(painter, &quads);
}

/// Room above the keyboard line for the glow (the note area's height).
fn line_room(notes: Rect) -> f32 { notes.height() }

/// Heights above the line to sample the glow at: fine near the line (the core), coarser away from
/// it, on until the light (with its `tail`, see `glow::stacked`) has faded below one colour step,
/// clamped to the `room` there is (so a long reach runs to the top, and a short one leaves no edge).
fn up_rows(reach: f32, room: f32, tail: f32) -> Vec<f32> {
    let mut up: Vec<f32> = [0.0, 2.0, 5.0, 10.0, reach * 0.5, reach, reach * 2.0, reach * 3.5, reach * 5.0]
        .map(|d| d.min(room)).to_vec();
    let mut d = reach * 5.0;
    while d < room && crate::glow::stacked(d, reach, tail) >= 1.0 / 255.0 {
        d *= 1.5;
        up.push(d.min(room));
    }
    up.sort_by(f32::total_cmp);
    up.dedup();
    up
}

/// The key glow as additive gradients (premultiplied colours with alpha 0 add to what is below):
/// columns 2 points wide, blended onto the keys (`glow::on_keys`) and falling off up into the notes.
fn paint_glow(painter: &egui::Painter, glow: &crate::glow::Glow, columns: &[(f32, f32); 128], notes: Rect, keys: Rect,
    look: &crate::glow::GlowLook)
{
    let bins = (notes.width() / 2.0).ceil().max(1.0) as usize;
    let (strip, core, wide) = (glow.strip(columns, bins, look), glow.core(columns, bins, look), glow.wide(columns, bins, look));
    let reach = (look.reach * notes.height()).max(1.0);
    let kh = keys.height().max(1.0);
    let over = |c: [f32; 3]| {
        let (tint, alpha) = crate::glow::on_keys(c, look.keys_tint());
        Color32::from_rgba_premultiplied((tint[0] * 255.0) as u8, (tint[1] * 255.0) as u8, (tint[2] * 255.0) as u8, (alpha * 255.0) as u8)
    };
    // Rows to sample the light at: fine near the line (the core), coarser away from it.
    let up = up_rows(reach, line_room(notes), look.tail);
    let mut down: Vec<f32> = [0.0, 2.0, 5.0, kh * 0.25, kh * 0.5, kh].into_iter().filter(|&d| d <= kh).collect();
    down.sort_by(f32::total_cmp);
    down.dedup();
    let add = |c: [f32; 3], f: f32| Color32::from_rgba_premultiplied(
        (c[0] * f * 255.0) as u8, (c[1] * f * 255.0) as u8, (c[2] * f * 255.0) as u8, 0);
    let (line, w) = (notes.bottom(), notes.width() / bins as f32);
    let mut mesh = egui::Mesh::default();
    let mut band = |x0: f32, x1: f32, top: f32, bottom: f32, ct: Color32, cb: Color32| {
        let base = mesh.vertices.len() as u32;
        mesh.colored_vertex(Pos2::new(x0, top), ct);
        mesh.colored_vertex(Pos2::new(x1, top), ct);
        mesh.colored_vertex(Pos2::new(x1, bottom), cb);
        mesh.colored_vertex(Pos2::new(x0, bottom), cb);
        mesh.add_triangle(base, base + 1, base + 2);
        mesh.add_triangle(base, base + 2, base + 3);
    };
    for i in 0..bins {
        let (h, c, wd) = (strip[i], core[i], wide[i]);
        if h.iter().chain(&c).chain(&wd).all(|&v| v < 1.0 / 255.0) { continue; }
        let (x0, x1) = (notes.left() + i as f32 * w, notes.left() + (i + 1) as f32 * w);
        let at = |dy: f32| crate::glow::mix(if dy < 0.0 { wd } else { h }, c, crate::glow::weights(dy, reach, kh, look.tail));
        for s in down.windows(2) { band(x0, x1, line + s[0], line + s[1], over(at(-s[0])), over(at(-s[1]))); }
        for s in up.windows(2) { band(x0, x1, line - s[1], line - s[0], add(at(s[1]), 1.0), add(at(s[0]), 1.0)); }
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// The rainbow bar (`Glow::bar`) over the top of the keys, and its glow up into the notes.
fn paint_bar(painter: &egui::Painter, glow: &crate::glow::Glow, columns: &[(f32, f32); 128], notes: Rect, keys: Rect,
    look: &crate::glow::GlowLook, now_us: u64)
{
    let bins = (notes.width() / 2.0).ceil().max(1.0) as usize;
    let (w, h, line) = (notes.width() / bins as f32, (look.bar_height * keys.height()).max(1.0), keys.top());
    let rgb = |c: [f32; 3], f: f32, a: u8| Color32::from_rgba_premultiplied((c[0] * f * 255.0) as u8, (c[1] * f * 255.0) as u8, (c[2] * f * 255.0) as u8, a);
    let up = up_rows(h * 2.0, line_room(notes), look.tail);
    let mut mesh = egui::Mesh::default();
    let mut band = |x0: f32, x1: f32, top: f32, bottom: f32, ct: Color32, cb: Color32| {
        let base = mesh.vertices.len() as u32;
        for (p, c) in [(Pos2::new(x0, top), ct), (Pos2::new(x1, top), ct), (Pos2::new(x1, bottom), cb), (Pos2::new(x0, bottom), cb)] { mesh.colored_vertex(p, c); }
        mesh.add_triangle(base, base + 1, base + 2);
        mesh.add_triangle(base, base + 2, base + 3);
    };
    for (i, c) in glow.bar(columns, bins, look, crate::glow::bar_phase(now_us, look)).into_iter().enumerate() {
        let (x0, x1) = (notes.left() + i as f32 * w, notes.left() + (i + 1) as f32 * w + 0.5);
        band(x0, x1, line, line + h, rgb(c, 1.0, 255), rgb(c, 1.0, 255));
        let at = |dy: f32| rgb(c, crate::glow::bar_glow(dy, h, look.tail), 0);
        for s in up.windows(2) { band(x0, x1, line - s[1], line - s[0], at(s[1]), at(s[0])); }
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// A glass key's colours: its body from top to bottom, and (black keys) its rim.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GlassKey { pub top: crate::pfa::Rgb, pub bottom: crate::pfa::Rgb, pub rim: crate::pfa::Rgb }

/// Frosted keys lit from inside by `level` (0..1) of the light `col` (0..1 per channel): white keys
/// dark at rest, lit saturated at the bottom and pale toward the top; black keys black, glowing at
/// the rim and dimly inside.
pub(crate) fn glass_colors(black: bool, level: f32, col: [f32; 3]) -> GlassKey {
    let l = level.clamp(0.0, 1.0);
    let mix = |a: [f32; 3], b: [f32; 3]| -> crate::pfa::Rgb { std::array::from_fn(|i| ((a[i] + (b[i] - a[i]) * l) * 255.0).round() as u8) };
    let toward_white = |t: f32| col.map(|c| c + (1.0 - c) * t);
    if black {
        GlassKey { top: mix([0.04; 3], col.map(|c| c * 0.3)), bottom: mix([0.035; 3], col.map(|c| c * 0.25)), rim: mix([0.2; 3], toward_white(0.3)) }
    } else {
        GlassKey { top: mix([0.16, 0.16, 0.18], toward_white(0.6)), bottom: mix([0.11, 0.11, 0.13], col), rim: [8, 8, 10] }
    }
}

/// The glass keyboard (see `glass_colors`) in `rect`, lit by `glow` (levels relative to `intensity`),
/// as quads for the live mesh and for export.
pub(crate) fn glass_quads(rect: Rect, lo: u8, hi: u8, columns: Option<&[(f32, f32); 128]>, glow: &crate::glow::Glow,
    intensity: f32) -> Vec<crate::pfa::Quad>
{
    let quad = |r: Rect, top: crate::pfa::Rgb, bottom: crate::pfa::Rgb| crate::pfa::Quad {
        p: [[r.left(), r.top()], [r.right(), r.top()], [r.right(), r.bottom()], [r.left(), r.bottom()]], c: [top, top, bottom, bottom] };
    let mut q = vec![quad(rect, [8, 8, 10], [8, 8, 10])];
    for (p, r) in keyboard_rects(rect, lo, hi, columns, None) {
        let (level, col) = glow.key(p);
        let black = is_black_key(p);
        let k = glass_colors(black, level / intensity.max(1e-3), col);
        if black {
            q.push(quad(r, k.rim, k.rim));
            q.push(quad(Rect::from_min_max(Pos2::new(r.left() + 1.5, r.top()), Pos2::new(r.right() - 1.5, r.bottom() - 1.5)), k.top, k.bottom));
        } else {
            q.push(quad(Rect::from_min_max(Pos2::new(r.left() + 1.0, r.top()), Pos2::new(r.right() - 1.0, r.bottom() - 1.0)), k.top, k.bottom));
        }
    }
    q
}

/// A sounding key's colour for the PFA keyboard: its note's, or the default highlight.
fn lit_color(active: bool, c: Color32) -> Option<crate::pfa::Rgb> {
    active.then(|| if c == Color32::TRANSPARENT { [255, 220, 140] } else { [c.r(), c.g(), c.b()] })
}

/// Quads (two triangles each, colour per corner) as one mesh.
fn paint_quads(painter: &egui::Painter, quads: &[crate::pfa::Quad]) {
    let mut mesh = egui::Mesh::default();
    for q in quads {
        let base = mesh.vertices.len() as u32;
        for (p, c) in q.p.iter().zip(q.c) { mesh.colored_vertex(Pos2::new(p[0], p[1]), Color32::from_rgb(c[0], c[1], c[2])); }
        mesh.add_triangle(base, base + 1, base + 2);
        mesh.add_triangle(base, base + 2, base + 3);
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// The classic keyboard (see `draw_keyboard`) as quads: per key an edge, then its body inset by
/// a pixel on every side but the one facing the notes. For drawing it bent (`Perspective::bend`).
pub(crate) fn classic_quads(rect: Rect, lo: u8, hi: u8, columns: Option<&[(f32, f32); 128]>,
    active: &[bool; 128], colors: &[Color32; 128], with_black: bool) -> Vec<crate::pfa::Quad>
{
    let quad = |r: Rect, c: crate::pfa::Rgb| crate::pfa::Quad {
        p: [[r.left(), r.top()], [r.right(), r.top()], [r.right(), r.bottom()], [r.left(), r.bottom()]], c: [c; 4] };
    keyboard_rects(rect, lo, hi, columns, None).into_iter().filter(|&(p, _)| with_black || !is_black_key(p)).flat_map(|(p, r)| {
        let (edge, idle, lit) = if is_black_key(p) { ([80; 3], [25, 25, 30], [255, 160, 100]) } else { ([150, 150, 160], [245, 245, 250], [255, 220, 140]) };
        let c = colors[p as usize];
        let body = if !active[p as usize] { idle } else if c == Color32::TRANSPARENT { lit } else { [c.r(), c.g(), c.b()] };
        let inner = Rect::from_min_max(Pos2::new(r.left() + 1.0, r.top()), Pos2::new(r.right() - 1.0, r.bottom() - 1.0));
        [quad(r, edge), quad(inner, body)]
    }).collect()
}

/// Key rectangles, whites first then sharps. `side: None` lays the keyboard along the bottom,
/// with sharps hanging from its top edge (the one facing the notes). `Some(side)` stands it on
/// its side at that screen edge: low pitch at the bottom, sharps reaching in from the edge
/// facing the notes, 65 % of the way across.
// Where the notes' columns are, when they follow the keys (see `key_columns`);
// otherwise the notes are on an even grid and the keys are laid out on their own.
pub(crate) fn keyboard_rects(rect: Rect, pitch_min: u8, pitch_max: u8, columns: Option<&[(f32, f32); 128]>,
    side: Option<KeyboardSide>) -> Vec<(u8, Rect)>
{
    let mut keys = Vec::new();
    if pitch_min > pitch_max { return keys; }
    // White-key index per pitch (None for black keys).
    let mut white_index = [None::<u32>; 128];
    let mut whites = 0;
    for pitch in pitch_min..=pitch_max {
        if !is_black_key(pitch) { white_index[pitch as usize] = Some(whites); whites += 1; }
    }
    if whites == 0 { return keys; }
    let white = 1.0 / whites as f32;
    // (pitch, along-the-keys span as fractions, depth from the note side as a fraction)
    let mut span = |pitch: u8, (a, b): (f32, f32), depth: f32| {
        let r = match side {
            None => Rect::from_min_max(Pos2::new(rect.left() + a * rect.width(), rect.top()),
                Pos2::new(rect.left() + b * rect.width(), rect.top() + depth * rect.height())),
            Some(side) => {
                let (y0, y1) = (rect.bottom() - b * rect.height(), rect.bottom() - a * rect.height());
                let (x0, x1) = match side {
                    KeyboardSide::Left => (rect.right() - depth * rect.width(), rect.right()),
                    KeyboardSide::Right => (rect.left(), rect.left() + depth * rect.width()),
                };
                Rect::from_min_max(Pos2::new(x0, y0), Pos2::new(x1, y1))
            }
        };
        keys.push((pitch, r));
    };
    for pitch in pitch_min..=pitch_max {
        let Some(i) = white_index[pitch as usize] else { continue };
        span(pitch, columns.map_or((i as f32 * white, (i + 1) as f32 * white), |c| c[pitch as usize]), 1.0);
    }
    for pitch in (pitch_min..=pitch_max).filter(|&p| is_black_key(p)) {
        // Centred on the line between its white neighbours (or on its note column, when the
        // notes follow the keys, so a black key at the edge is drawn too).
        let prev = white_index[pitch.saturating_sub(1) as usize];
        let ab = match (columns, prev) {
            (Some(c), _) => c[pitch as usize],
            (None, Some(i)) => { let c = (i + 1) as f32 * white; (c - white * 0.35, c + white * 0.35) }
            (None, None) => continue,
        };
        span(pitch, ab, 0.65);
    }
    keys
}

pub(crate) fn draw_keyboard(
    painter: &egui::Painter,
    rect: Rect,
    pitch_min: u8,
    pitch_max: u8,
    columns: Option<&[(f32, f32); 128]>,
    side: Option<KeyboardSide>,
    active_keys: &[bool; 128],
    active_colors: &[Color32; 128],
) {
    for (pitch, key_rect) in keyboard_rects(rect, pitch_min, pitch_max, columns, side) {
        let black = is_black_key(pitch);
        let (idle, lit, edge, round) = if black {
            (Color32::from_rgb(25, 25, 30), Color32::from_rgb(255, 160, 100), Color32::from_rgb(80, 80, 80), 1)
        } else {
            (Color32::from_rgb(245, 245, 250), Color32::from_rgb(255, 220, 140), Color32::from_rgb(150, 150, 160), 2)
        };
        let channel = active_colors[pitch as usize];
        let color = if !active_keys[pitch as usize] { idle } else if channel == Color32::TRANSPARENT { lit } else { channel };
        painter.rect_filled(key_rect, CornerRadius::same(1), color);
        painter.rect_stroke(key_rect, CornerRadius::same(round), Stroke::new(1.0_f32, edge), StrokeKind::Inside);
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

/// 2294213080 -> "2,294,213,080"
pub(crate) fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::KeyboardSide;

    #[test]
    fn a_long_reach_glows_all_the_way_up() {
        let rows = up_rows(300.0, 500.0, 0.5);
        assert_eq!(rows.last(), Some(&500.0), "{rows:?}");
        // Short reach, long tail: drawn until the light has faded (no hard edge), short of the top.
        for tail in [0.0, 0.5, 0.9] {
            let rows = up_rows(20.0, 2000.0, tail);
            let last = *rows.last().unwrap();
            assert!(last < 2000.0 && crate::glow::stacked(last, 20.0, tail) < 1.0 / 255.0, "tail {tail}: {rows:?}");
        }
    }

    #[test]
    fn glass_keys_light_from_inside_with_a_pale_top() {
        let red = [1.0, 0.0, 0.0];
        let dark = |c: crate::pfa::Rgb| c.iter().all(|&v| v < 60);
        let w = glass_colors(false, 0.0, red);
        assert!(dark(w.top) && dark(w.bottom), "unlit white keys are dark frosted panels: {w:?}");
        let lit = glass_colors(false, 1.0, red);
        assert!(lit.bottom[0] > 200 && lit.bottom[1] < 90, "saturated at the bottom: {lit:?}");
        let min = |c: crate::pfa::Rgb| *c.iter().min().unwrap() as i32;
        assert!(lit.top[0] > 200 && min(lit.top) > min(lit.bottom) + 60, "pale toward the top: {lit:?}");
        let half = glass_colors(false, 0.5, red);
        assert!(half.bottom[0] > w.bottom[0] && half.bottom[0] < lit.bottom[0], "the level fades it: {half:?}");
        let b = glass_colors(true, 0.0, red);
        assert!(b.bottom.iter().all(|&v| v < 20) && b.rim.iter().all(|&v| v < 70), "black keys are black: {b:?}");
        let bl = glass_colors(true, 1.0, red);
        assert!(bl.rim[0] > 180 && bl.bottom[0] < bl.rim[0], "a lit black key glows at its rim: {bl:?}");
    }

    #[test]
    fn sideways_keyboard_is_the_bottom_keyboard_rotated() {
        let close = |a: f32, b: f32| (a - b).abs() < 1e-3;
        let bottom = keyboard_rects(Rect::from_min_size(Pos2::ZERO, Vec2::new(600.0, 100.0)), 48, 72, None, None);
        for side in [KeyboardSide::Left, KeyboardSide::Right] {
            let tall = keyboard_rects(Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 600.0)), 48, 72, None, Some(side));
            assert_eq!(bottom.iter().map(|k| k.0).collect::<Vec<_>>(), tall.iter().map(|k| k.0).collect::<Vec<_>>());
            for (&(pitch, b), &(_, t)) in bottom.iter().zip(&tall) {
                // Along the keys: low pitch at the bottom.
                assert!(close(t.top(), 600.0 - b.right()) && close(t.bottom(), 600.0 - b.left()), "{pitch}: {b:?} {t:?}");
                // Across: as deep as before, from the edge facing the notes.
                assert!(close(t.width(), b.height()), "{pitch}: {b:?} {t:?}");
                let inner = match side { KeyboardSide::Left => t.right(), KeyboardSide::Right => t.left() };
                assert!(close(inner, if side == KeyboardSide::Left { 100.0 } else { 0.0 }), "{pitch} {side:?}: {t:?}");
            }
        }
        assert!(bottom.iter().any(|k| is_black_key(k.0) && k.1.height() < 100.0), "sharps are shorter");
    }
}

/// The black keys' flat footprints and top colours, for standing them up (`Perspective::raise`):
/// the classic keyboard's black keys, or the PFA sharps.
pub(crate) fn black_keys(rect: Rect, lo: u8, hi: u8, columns: Option<&[(f32, f32); 128]>, pfa: bool,
    active: &[bool; 128], colors: &[Color32; 128]) -> Vec<(Rect, crate::pfa::Rgb)>
{
    let lit = |p: u8, idle: crate::pfa::Rgb| {
        let c = colors[p as usize];
        if !active[p as usize] { idle } else if c == Color32::TRANSPARENT { if pfa { [255, 220, 140] } else { [255, 160, 100] } } else { [c.r(), c.g(), c.b()] }
    };
    if pfa {
        let (top, bottom) = crate::pfa::sharp_rows(rect.top(), rect.height());
        crate::pfa::layout(columns, lo, hi, rect.left(), rect.width()).1.into_iter()
            .map(|(p, l, r)| (Rect::from_min_max(Pos2::new(l, top), Pos2::new(r, bottom)), lit(p, [40, 40, 44]))).collect()
    } else {
        keyboard_rects(rect, lo, hi, columns, None).into_iter().filter(|&(p, _)| is_black_key(p))
            .map(|(p, r)| (r, lit(p, [25, 25, 30]))).collect()
    }
}
