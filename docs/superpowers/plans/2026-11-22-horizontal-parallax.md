# Horizontal and Parallax Views Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a horizontal piano roll (keyboard on the left or right) and a parallax view (no keyboard, playhead in the middle, every track scrolling at its own snapped speed), live and in video export, fast on black MIDIs.

**Architecture:** The note shader learns a horizontal axis plus per-draw uniforms (playhead, brightness, depth range). `NotePipeline` splits into one compiled pipeline and per-layer buffer sets, so one frame can draw several layers. Parallax gives every speed step its own `VisibleNotes` (restricted to that step's tracks by a track mask) and its own `InstanceRing`. Each step is culled with its own time window, and the existing culling and "Hide covered notes" code runs per step unchanged. Each step is then drawn with its own speed uniform. The default vertical view keeps a 16-byte `NoteInstance` and one draw.

**Tech Stack:** Rust 2021, eframe/egui 0.32 with wgpu, WGSL, rayon, serde.

**Spec:** `docs/superpowers/specs/2026-11-22-horizontal-parallax-design.md`

## Global Constraints

- The default vertical view must not get slower or heavier: `NoteInstance` stays 16 bytes, and vertical mode stays a single draw with today's culling.
- `cargo run --release -- --benchmark-render nps --verify` must still pass after every GPU change. It checks the live pipeline against `shaders/notes-reference.wgsl`.
- One render pipeline serves live and export. There is no separate parallax pipeline.
- Speed steps: 64. A song with ≤ 64 source tracks gets one step per track.
- Parallax far speed: default 0.3, range 0.1–1.0. Near is always 1.0 × note speed.
- In parallax the near layer takes one note-speed (`Settings::note_speed` seconds) to travel from the screen edge to the playhead.
- A layer is a source track: `song.midi.track_origin[packed_track]`.
- Settings files without the new fields must load (struct is `#[serde(default)]`; keep it so).
- Before starting, the user's uncommitted work in `src/main.rs`, `src/ui.rs`, `src/settings.rs` (and others in `git status`) must be committed by the user or deliberately built on. Never discard it.
- Commit messages: no `Co-Authored-By` or "Generated with" lines.
- Benchmarks and manual checks use note speed 0.25 s or lower, and read archives directly (never extract them).

## Review Focus

1. **Parallax at song start:** `now < span`, so the shifted time origin would go negative. Expected: notes before the first note's arrival still draw in the right place via `time_shift`, with no underflow panic. Test in Task 5.
2. **A song with a single track, or with more than 64 tracks:** one track gives one layer at speed 1.0. With 3,000 tracks, steps hold many tracks each and every step's speed lies in [far, 1]. Test in Task 5.
3. **Identical packed tracks on different steps:** both must stay visible. Today's `render_tracks` would drop the earlier copy. Test in Task 5.
4. **Switching view mode or seeking mid-song:** layers must reset cleanly, with no stale notes from the previous mode or position. Test in Task 6 (seek backward resets every layer).
5. **Instance cap hit in parallax:** the far layers are dropped first, and the near layer is never cut while a farther one draws. Test in Task 5.

---

### Task 1: View settings and their Display rows

**Files:**
- Modify: `src/settings.rs` (struct, `Default`)
- Modify: `src/ui.rs:264-292` (Display rows)

**Interfaces:**
- Produces, in `settings.rs`:
  ```rust
  #[derive(Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Debug)]
  pub enum ViewMode { #[default] Vertical, Horizontal, Parallax }
  #[derive(Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Debug)]
  pub enum KeyboardSide { #[default] Left, Right }
  // new Settings fields
  pub view: ViewMode,            // default Vertical
  pub keyboard_side: KeyboardSide, // default Left
  pub parallax_far: f32,         // default 0.3
  ```

- [ ] **Step 1: Write the failing test** in `settings.rs` `#[cfg(test)] mod tests`:
  ```rust
  #[test]
  fn old_settings_files_get_the_vertical_view() {
      let s: Settings = serde_json::from_str(r#"{"note_speed": 2.0}"#).unwrap();
      assert_eq!((s.view, s.keyboard_side, s.parallax_far), (ViewMode::Vertical, KeyboardSide::Left, 0.3));
      let back: Settings = serde_json::from_str(&serde_json::to_string(&Settings { view: ViewMode::Parallax, ..s.clone() }).unwrap()).unwrap();
      assert_eq!(back.view, ViewMode::Parallax);
  }
  ```
- [ ] **Step 2: Run it to make sure it fails.** `cargo test old_settings_files_get_the_vertical_view` should fail to compile with no `ViewMode`.
- [ ] **Step 3: Add the enums and fields.** In `ui.rs`, add a "View" row (Vertical / Horizontal / Parallax, keywords `"View vertical horizontal parallax"`). Show "Keyboard side" (Left / Right) only for Horizontal, and a "Far speed" slider `0.1..=1.0` only for Parallax. Hide the existing Notes Fall/Rise row unless the view is Vertical.
- [ ] **Step 4: Run the test to make sure it passes.**
- [ ] **Step 5: Commit** with the message `Add view mode settings`.

### Task 2: Shader horizontal axis and per-draw uniforms

**Files:**
- Modify: `shaders/notes.wgsl` (`Globals`, `vs_notes`, `fs_notes`)
- Modify: `src/gpu.rs` (`GlobalsUniform`, `NotesCallback`, `NotePipeline`, `Offscreen`, `RenderBenchmark`)
- Modify callers: `src/ui.rs` (`draw_piano_roll`), `src/export.rs` (`run`), `src/render_bench.rs`

**Interfaces:**
- Produces, in `gpu.rs`:
  ```rust
  /// How one layer of notes maps to the screen.
  #[derive(Clone, Copy, Debug, PartialEq)]
  pub struct NoteView {
      pub height_time: f32,    // seconds the time axis spans
      pub now_us: u64,         // song time at the time axis origin
      pub time_shift: f32,     // seconds added to every start (negative song time)
      pub direction_sign: f32, // vertical: +1 rise / -1 fall; horizontal: +1 origin at right, -1 at left
      pub flat_mode: u32,
      pub horizontal: bool,
      pub playhead: f32,       // fraction of the time axis where notes light up and the past fades; < 0 = none
      pub brightness: f32,     // colour multiplier, 1.0 = unchanged
      pub depth: [f32; 2],     // [base, scale]: this draw's depth range inside 0..1
  }
  impl NoteView { pub fn vertical(height_time: f32, now_us: u64, direction_sign: f32, flat_mode: u32) -> Self } // playhead -1, brightness 1, depth [0,1], time_shift 0
  pub struct LayerDraw { pub instances: Arc<InstanceRing>, pub view: NoteView }
  NotesCallback::new(viewport: egui::Rect, target_format: wgpu::TextureFormat, layer: usize,
      instances: Arc<InstanceRing>, key_positions: [KeyPosition; 256], view: NoteView) -> egui::PaintCallback
  Offscreen::new(width: u32, height: u32) -> Result<Self, String>
  Offscreen::render(&mut self, layers: &[LayerDraw], keys: &[KeyPosition; 256],
      viewport: [u32; 4] /* x, y, w, h in px */, background: [u8; 3], out: &mut Vec<u8>)
  ```
- `GlobalsUniform` becomes 64 bytes: the old fields, then `horizontal: u32, playhead: f32, brightness: f32, depth_base: f32, depth_scale: f32, _pad: [f32; 3]`. `win_width`/`win_height` stay the viewport's pixel size. The WGSL `Globals` struct must match field for field.

- [ ] **Step 1: Write the failing GPU tests** in `gpu.rs` tests. Each test starts with `let Ok(mut gpu) = Offscreen::new(64, 64) else { eprintln!("no GPU, skipped"); return };`. Use one note, pitch 60, keys with `keys[60] = {left: 0.25, right: 0.5}`, start at `now`, length 0.5 s, `height_time: 1.0`, flat mode 1, colour (255,0,0), background black.
  - `horizontal_mode_turns_the_vertical_picture_sideways`: vertical with `direction_sign -1` (fall, origin at bottom) lights pixel (x 24, y 48) red and leaves (x 24, y 8) black. Horizontal with `direction_sign -1` (origin left) lights (x 8, y 40). Key axis: low pitch at the bottom, so the 0.25–0.5 key span covers y 32–48. It leaves (x 56, y 40) black.
  - `playhead_lights_crossing_notes_and_fades_the_past`: horizontal, `playhead 0.5`, note from norm 0.25 to 0.75. The pixel at norm 0.5 is brighter than at norm 0.7, and the pixel at norm 0.3 is darker than at 0.7.
  - `brightness_and_depth_ranges_order_layers`: two `LayerDraw`s of the same note, the first `{depth [0, 0.5], colour red}`, the second `{depth [0.5, 0.5], colour green, brightness 0.5}`, drawn in that order. The pixel is red, since the nearer depth wins. Swap the depth ranges and the pixel is green at about half intensity (≤ 140).
- [ ] **Step 2: Run them to make sure they fail.** `cargo test --bin kimrashi gpu::tests` should fail to compile on `NoteView`.
- [ ] **Step 3: Implement.**
  - **Shader axis swap:** keep computing `x` from the key span and `y` from the time span exactly as now. When `horizontal != 0u`, output `vec2(time_clip, key_clip)` instead, where `key_clip` is the key span (low pitch at clip −1, the bottom) and `time_clip` is today's start/end clip times `direction_sign`. Swap `win_size` to (height, width) for the fragment stage. Measure `thin` against the time-axis pixel count. `uv.x` stays the key axis.
  - **Depth:** `depth_base + depth_scale * (index + 1) / (count + 2)`.
  - **Brightness:** multiplies the final colour.
  - **Playhead:** when `playhead >= 0`, pass the fragment's time-axis norm as a varying. A note whose `[start_norm, end_norm]` contains `playhead` is mixed 40 % toward white (*my call*). Fragments with norm < playhead are scaled by `mix(0.15, 1.0, norm / playhead)` (*my call*).
  - **Pipeline split:** split `NotePipeline` into a compiled `wgpu::RenderPipeline` plus bind group layout, created once, and a `LayerBuffers` struct holding today's per-draw buffers, bind group and upload state. Keep `upload`'s incremental slot logic per `LayerBuffers`.
  - **Live resources:** a `NotePipelines { pipeline, layers: Vec<LayerBuffers> }` in `CallbackResources`. `NotesCallback.layer` indexes `layers` and grows it on demand.
  - **Callers:** update every caller with `NoteView::vertical(...)`. The export caller passes `viewport [0, 0, width, note_h]` and the old `time_shift`.
- [ ] **Step 4: Run the tests to make sure they pass.** `cargo test` should be fully green, including `note_shader_is_valid`.
- [ ] **Step 5: Verify that nothing changed for the default view.** `cargo run --release -- --benchmark-render nps --verify` should report a match with the reference. Compare frame times with the run on the commit before.
- [ ] **Step 6: Commit** with the message `Note shader: horizontal axis, playhead, brightness and depth per draw`.

### Task 3: Sideways keyboard layout shared by the UI and export

**Files:**
- Modify: `src/ui.rs` (`draw_keyboard`, plus a new layout function)
- Modify: `src/export.rs` (`keyboard`)

**Interfaces:**
- Produces, in `ui.rs`:
  ```rust
  /// Key rectangles, whites first then sharps. `side: None` = along the bottom (today's look);
  /// Some(side) = standing on its side at that screen edge, low pitch at the bottom, sharps
  /// reaching from the outer edge 65 % of the way across.
  pub(crate) fn keyboard_rects(rect: Rect, lo: u8, hi: u8, columns: Option<&[(f32, f32); 128]>,
      side: Option<KeyboardSide>) -> Vec<(u8, Rect)>
  pub(crate) fn draw_keyboard(painter, rect, lo, hi, columns, side: Option<KeyboardSide>, active_keys, active_colors)
  ```
  Export's `keyboard()` takes the same `side` and fills each rect from `keyboard_rects` with today's edge and body colours. It no longer has its own layout math.

- [ ] **Step 1: Write the failing test** in `ui.rs` tests: `sideways_keyboard_is_the_bottom_keyboard_rotated`. For range 48–72, compare `keyboard_rects` with `side None` on a 600×100 rect against `Some(Left)` on a 100×600 rect. Every pitch's sideways rect equals the bottom rect rotated: its y-range is the mirror of the bottom x-range, so low pitch is at the bottom, and the widths match. With `Left`, sharps touch x = 0. With `Right`, sharps touch the right edge. The same pitches come out in the same order.
- [ ] **Step 2: Run it to make sure it fails.**
- [ ] **Step 3: Implement.** Move `draw_keyboard`'s layout into `keyboard_rects`, then paint from it. Rewrite `export::keyboard` on top of it, with pixel rects rounded the way `fill` rounds today.
- [ ] **Step 4: Run `cargo test`** and make sure everything passes.
- [ ] **Step 5: Commit** with the message `Share the keyboard layout between the UI and export, with a sideways form`.

### Task 4: Horizontal view, live and exported

**Files:**
- Modify: `src/ui.rs:706-` (`draw_piano_roll`)
- Modify: `src/main.rs:~2000` (the `draw_piano_roll` call: pass `view` and `keyboard_side`)
- Modify: `src/export.rs` (`run`, `run_from_args`)

**Interfaces:**
- Consumes: `NoteView`, `LayerDraw`, `Offscreen::render` (Task 2), `keyboard_rects`/`draw_keyboard` (Task 3), `ViewMode`/`KeyboardSide` (Task 1).
- Produces: `draw_piano_roll(..., view: ViewMode, keyboard_side: KeyboardSide, parallax: &mut crate::parallax::Parallax, ...)`. The `parallax` parameter is added in Task 6; this task adds only `view` and `keyboard_side`.

- [ ] **Step 1: Write the failing test** in `export.rs` tests: `horizontal_export_puts_the_keyboard_on_its_side`. Build `Settings { look: Settings { view: Horizontal, keyboard_side: Right, .. }, width: 160, height: 90, .. }` on a tiny MIDI written to the scratch dir. Run `export::run` with an `on_frame` that captures frame 1 and returns `false`. Skip if ffmpeg or a GPU is missing. In frame 1, the rightmost 12 % columns hold keyboard colours (245,245,250 or 25,25,30) at the vertical centre, and the bottom row of the note area is the note background (20,20,24) rather than keyboard.
- [ ] **Step 2: Run it to make sure it fails.**
- [ ] **Step 3: Implement.**
  - **Live layout:** in Horizontal, the keyboard rect is a strip of width `(rect.width() * 0.12).clamp(36, 120)` on `keyboard_side`, and the note area is the rest. `px_us` and `build_note_instances`' `height_px` use the note area's pixel *width*.
  - **Shader values:** `NoteView { horizontal: true, direction_sign: if Right { 1.0 } else { -1.0 }, .. }`.
  - **PFA:** the PFA keyboard animation applies only to Vertical. Horizontal always uses `draw_keyboard(.., Some(side), ..)`, while notes keep the PFA shading if chosen (*spec: my call*).
  - **Export:** compute the same split in pixels. Render notes with a viewport covering the note area, draw the sideways keyboard with `keyboard(.., Some(side), ..)`, and draw a 1 px separator line at the keyboard edge. Add `--view vertical|horizontal|parallax` and `--keyboard-side left|right` to `run_from_args`, overriding the saved settings in the same `value(..)` style.
- [ ] **Step 4: Run `cargo test`** and make sure everything passes.
- [ ] **Step 5: Check by hand.** `cargo run --release -- <a song>`: switch View to Horizontal, flip the side, and check the notes land on the keys at 0.25 s note speed.
- [ ] **Step 6: Commit** with the message `Horizontal view, live and exported`.

### Task 5: Parallax layers (track steps, per-step culling, cap)

**Files:**
- Create: `src/parallax.rs`
- Modify: `src/main.rs`: `mod parallax;`, `render_tracks` → `render_tracks_in`, a `VisibleNotes.tracks` mask, `DecodeWorker::new` taking the mask, and a shared decode pool

**Interfaces:**
- Produces, in `main.rs`:
  ```rust
  /// Like render_tracks, but only among `members`; tracks outside it are disabled and
  /// never hide a member.
  fn render_tracks_in(song: &SongData, members: &[bool]) -> Vec<bool>
  // render_tracks(song) becomes render_tracks_in(song, &vec![true; n])
  // VisibleNotes gets `tracks: Option<Arc<[bool]>>` (None = render_tracks(song)), kept across
  // every `*self = Self { .. }` reset just like collapse_overlaps.
  fn set_tracks(&mut self, tracks: Option<Arc<[bool]>>) // resets like set_collapse when it changes
  ```
  The `note-decode-*` rayon pool becomes one process-wide `static` (`OnceLock<rayon::ThreadPool>`), with the same thread count as today, so 64 workers don't make 256 threads.
- Produces, in `parallax.rs`:
  ```rust
  pub const STEPS: usize = 64;
  /// Speed step for each packed track, by its source track's rank among source tracks that
  /// have at least one note (first = step 0 = nearest); note-less tracks get step 0.
  pub fn track_steps(song: &SongData) -> (Vec<u8>, usize /* step count */)
  pub fn step_speed(step: usize, steps: usize, far: f32) -> f32 // 1.0 at 0, `far` at steps-1, linear; 1.0 when steps == 1
  pub struct Parallax { /* song ptr, far, layers: Vec<Layer { visible: VisibleNotes, instances: Arc<InstanceRing>, speed: f32 }> */ }
  impl Parallax {
      /// Advances and builds every layer for this frame, nearest first, and returns what to draw.
      /// `span_s` = note speed; `time_px` = pixels along the time axis; `max_instances` total cap.
      pub fn frame(&mut self, song: &VisualSong, now_us: u64, duration_us: u64, span_s: f32, far: f32,
          time_px: u32, flat_mode: u32, collapse: bool, max_instances: usize) -> Vec<gpu::LayerDraw>
  }
  ```
  Per layer *k* with speed *f*:
  - **Time window:** `half = span_s / f`. The layer shows `[now − half, now + half]` with `NoteView { horizontal: true, direction_sign: -1.0, height_time: 2·half, playhead: 0.5, brightness: 1.0 − 0.3·k/(steps−1) (my call), depth: [k/steps, 1/steps] }`.
  - **Shifted origin:** `origin = now.saturating_sub(half_us)`, and `time_shift = (half_us − (now − origin)) / 1e6`, which is non-zero only near song start. `visible.advance(song, origin, min(now + half_us, duration), ..)` and `build_note_instances(song, visible, origin, window_end, 2·half, time_px, false, ring)`.
  - **Cap:** walk the layers nearest first with a running total. A layer that would pass `max_instances` gets `ring.cap(remaining, ..)`. Every layer after the total is reached is neither advanced nor drawn that frame.
  - **Rebuild:** rebuild all layers when the song or `far` changes. `track_steps` is recomputed only on song change.

- [ ] **Step 1: Write the failing tests** in `parallax.rs` (reuse `main.rs` test helpers like `overlapping_song` by making them `pub(crate)` in a `test_util` module, or copy the 3-track MIDI builder):
  - `steps_spread_tracks_near_to_far`: a 3-track song gives steps `[0, 1, 2]` and count 3, with `step_speed(0,3,0.3) == 1.0` and `step_speed(2,3,0.3) == 0.3`. A 1-track song gives count 1 and speed 1.0. A 200-track song gives count 64, non-decreasing steps by rank, first 0, last 63.
  - `identical_tracks_on_different_steps_both_draw`: the 3 identical-track MIDI from `identical_packed_tracks_only_drop_fully_hidden_earlier_copies`. `render_tracks_in(song, &[true, false, true])` is `[false, false, true]` within that step. With members `[true, false, false]`, it is `[true, false, false]`, so the earlier copy survives in its own step.
  - `a_masked_layer_only_decodes_its_tracks`: a `VisibleNotes` with `set_tracks(Some([false, true, false]))` on `test_song()` has only track-1 notes after `advance`.
  - `layer_windows_scale_with_speed_and_survive_song_start`: on a 3-track song with `far 0.5` and `span_s 1.0`, call `frame` at `now = 2 s`. Layer 0 has `height_time 2.0` and `now_us 1_000_000`. Layer 2 has `height_time 4.0`, `now_us 0`, and `time_shift 0.0`. At `now = 0.5 s`, layer 2 has `now_us 0` and `time_shift 1.5`, with no panic.
  - `cap_drops_far_layers_first`: with `max_instances` below layer 0's own count, only one `LayerDraw` comes back, it is layer 0, and its ring length equals the cap.
- [ ] **Step 2: Run them to make sure they fail.** `cargo test parallax` should fail.
- [ ] **Step 3: Implement** as specified in the Interfaces block.
- [ ] **Step 4: Run `cargo test`** and make sure it is fully green, including every existing flatten and culling test, which must not change.
- [ ] **Step 5: Commit** with the message `Parallax layers: per-track speed steps with per-step culling`.

### Task 6: Parallax view, live and exported

**Files:**
- Modify: `src/ui.rs` (`draw_piano_roll`)
- Modify: `src/main.rs` (`AppState`: a `parallax: parallax::Parallax` field passed to `draw_piano_roll`; reset on seek and on song change)
- Modify: `src/export.rs` (`run`, `run_from_args`)

**Interfaces:**
- Consumes: `Parallax::frame` (Task 5), `NotesCallback::new(.., layer, ..)` and `Offscreen::render` (Task 2).

- [ ] **Step 1: Write the failing tests.**
  - In `parallax.rs`, `seeking_back_resets_every_layer`: call `frame` at 10 s, then at 2 s. Every layer's notes are within its new window.
  - In `export.rs`, `parallax_export_draws_a_playhead_and_no_keyboard`: the same harness as Task 4, with `view Parallax`. Frame 1's centre column is the playhead colour (255,220,140), and the bottom 12 % rows are not keyboard white.
- [ ] **Step 2: Run them to make sure they fail.**
- [ ] **Step 3: Implement.**
  - **Live:** the note area is the whole rect, with no keyboard. Call `parallax.frame(..)` and add one `NotesCallback` per returned layer, `layer` = its index, in the order returned (nearest first, for early depth rejection). Draw a 2 px vertical playhead line at the centre in (255,220,140) over the notes (*spec: my call*). Stats overlay unchanged.
  - **Export:** the same, passing all layers to one `Offscreen::render`, then fill the playhead column. Add `--parallax-far <x>`.
  - **Mode switch:** switching away from Parallax drops `parallax`'s layers so their workers stop.
- [ ] **Step 4: Run `cargo test`** and make sure everything passes.
- [ ] **Step 5: Commit** with the message `Parallax view, live and exported`.

### Task 7: Black MIDI check

- [ ] **Step 1:** With note speed 0.25 s, play the same dense black MIDI (read straight from its archive) in Vertical, then Parallax at far 0.3, with `KIM_FRAMELOG=<scratch>/vertical.log` and `<scratch>/parallax.log`. Record the average fps for each.
- [ ] **Step 2:** Report both numbers to the user. If parallax is unusable, try `STEPS = 16` and report that too. Don't change the default without the user's say-so (the spec: "revisit after implementation").
