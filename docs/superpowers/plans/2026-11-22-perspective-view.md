# Perspective View Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Perspective view: the vertical piano roll seen at an angle, with the note area a trapezoid narrowing toward the top and culling that stays tied to screen pixels.

**Architecture:** The note shader maps each note corner through a fixed perspective when a new `perspective_far` uniform is non-zero. `select_pixels` samples rows at perspective-correct times, the inverse of the shader's y mapping. The live view and export pass the setting through, the same way the other views do.

**Tech Stack:** Rust 2021, eframe/egui 0.32, wgpu, WGSL.

**Spec:** `docs/superpowers/specs/2026-11-22-perspective-view-design.md`

## Global Constraints

- Vertical, Horizontal and Parallax pixels are unchanged: `cargo run --release -- --benchmark-render nps --verify` must still pass, and `perspective_far = 0` means off everywhere.
- `perspective_far`: default 0.3, range 0.1–0.9 (the far edge's width as a fraction of the bottom).
- `Z = 1 / perspective_far`. Time `t` seconds ahead (0 ≤ t ≤ T = note speed) gives depth `z = 1 + (Z − 1)·t/T`.
- Screen position:
  - Height above the keyboard line: `y = (1 − 1/z) / (1 − 1/Z)` (0 at the keyboard line, 1 at the top).
  - Horizontal: `x = 0.5 + (x_key − 0.5)/z`.
- Inverse, for row sampling: `z = 1 / (1 − y·(1 − 1/Z))`, then `t = (z − 1)/(Z − 1)·T`.
- Notes always fall toward a bottom keyboard in this view. The keyboard, including PFA style, is unchanged.
- Commits: no `Co-Authored-By` or "Generated with" lines.
- Benchmarks at note speed 0.25 s, on files read directly.

## Review Focus

1. **Sounding notes** (start < now): clamp their time to 0, as the vertical view does, so they never map behind the keyboard. Test in Task 2.
2. **Far width at the ends of its range:** 0.9 makes an almost flat view, 0.1 a strong squeeze. No NaN or inf from the mapping (`1 − 1/Z` > 0 for all valid values). Test in Task 3 (row times are finite and increasing for 0.1 and 0.9).
3. **Dense songs:** a note that is the newest one covering any far row's time must still be selected (no LOD holes). Test in Task 3.
4. **Switching between Vertical and Perspective mid-song:** the vertical row cache must not be reused with perspective rows, or the other way round. Test in Task 3.
5. **Export lead-in** (`time_shift` > 0 before the first note): notes still map inside the trapezoid. Covered by clamping in Task 2, and checked by eye in Task 4.

---

### Task 1: Perspective view setting

**Files:** Modify `src/settings.rs` (`ViewMode`, `Settings`, `Default`, tests), `src/ui.rs` (Display rows).

**Interfaces:** Produces `ViewMode::Perspective` and `Settings::perspective_far: f32` (default 0.3).

- [ ] **Step 1: Write the failing test** in `settings.rs` tests:
  ```rust
  #[test]
  fn perspective_settings_default_and_round_trip() {
      let s = Settings::from_json(r#"{"view":"Perspective"}"#);
      assert_eq!((s.view, s.perspective_far), (ViewMode::Perspective, 0.3));
  }
  ```
- [ ] **Step 2: Run it to make sure it fails.** `cargo test --bin kimrashi perspective_settings` should fail to compile.
- [ ] **Step 3: Implement.** Add the variant and the field.
  - **View row:** add a "Perspective" button, with the hover text "The vertical view at an angle: far notes small at the top".
  - **Far width:** a slider `0.1..=0.9` shown only in Perspective, applied on release like Parallax's "Far speed".
  - **Notes Fall/Rise row:** already shown only for Vertical, so nothing to change.
- [ ] **Step 4: Run the test to make sure it passes,** then run `cargo test --bin kimrashi` and make sure it's green.
- [ ] **Step 5: Commit** with the message `Add the Perspective view setting`.

### Task 2: Shader perspective mapping

**Files:** Modify `shaders/notes.wgsl` (`Globals`, `vs_notes`), `src/gpu.rs` (`GlobalsUniform`, `NoteView`, tests).

**Interfaces:**
- Produces `NoteView::perspective_far: f32` (0 = off; `NoteView::vertical` sets 0).
- `GlobalsUniform._pad[0]` becomes `perspective_far: f32`, so the uniform stays 64 bytes. WGSL `Globals` gains `perspective_far: f32` after `depth_scale`.

- [ ] **Step 1: Write the failing GPU tests** in `gpu.rs` tests, using the existing `one_note`, `keys` and `draw` helpers. The 64×64 view is `NoteView { perspective_far: 0.3, ..NoteView::vertical(1.0, NOW, -1.0, 1) }`, and the key spans 0.25–0.5 (x 16..32 in vertical).
  - `perspective_keeps_near_notes_and_squeezes_far_ones`:
    - A note at offset 0, length 0.05, is red at (20, 62), the same as vertical.
    - A note at offset 0.9, length 0.1, is red at (29, 1), black at (18, 1), and red in no pixel with x < 26 in rows 0–3. Its columns converge, since the key centre 0.375 maps to x ≈ 0.461.
  - `perspective_clamps_sounding_notes_to_the_keyboard_line`: a note starting 0.5 s *before* `NOW` (length 0.6) is red at (20, 62) and draws no pixel below row 63.
- [ ] **Step 2: Run them to make sure they fail.** `cargo test --bin kimrashi gpu::tests::perspective` should fail.
- [ ] **Step 3: Implement.**
  - **Vertex mapping:** when `perspective_far > 0 && horizontal == 0`, map each corner with the Global Constraints formulas.
    - Time: `n = clamp(time / height_time, 0, 1)`; `x_key` is the key fraction `key_pos.left/right` lerped by `uv.x`.
    - Output: `x_clip = 2x − 1`, `y_clip = 2y − 1`, so the keyboard line sits at clip −1 (bottom).
  - **Fragment inputs:**
    - `note_size`: the mapped width at the note's near (bottom) end, and the mapped height.
    - `thin`: measured from the mapped height in pixels.
  - **The other views:** everything else is unchanged.
- [ ] **Step 4: Run the tests to make sure they pass,** then run the full suite.
- [ ] **Step 5: Verify** with `cargo run --release -- --benchmark-render nps --verify`. Every frame should report "GPU pixels equal".
- [ ] **Step 6: Commit** with the message `Note shader: perspective mapping`.

### Task 3: Perspective row sampling in the culling

**Files:** Modify `src/main.rs` (`VisibleNotes::select_pixels`, `build_note_instances`, all callers, tests), `src/parallax.rs` and `src/ui.rs`/`src/export.rs` call sites (pass `0.0`), `src/render_bench.rs` if it calls them.

**Interfaces:**
- Produces in `main.rs`:
  ```rust
  /// Sample time of each screen row (row 0 at the keyboard line), even when `far == 0`.
  fn row_times(now_us: u64, height_time: f32, height_px: u32, far: f32) -> Vec<u64>
  ```
- `select_pixels(.., height_px: u32, far: f32)` and `build_note_instances(.., piano_layout: bool, far: f32, instances)` gain `far`, with every existing caller passing `0.0`.
- When `far > 0`:
  - Rows are rebuilt every frame: no row-cache shift, and `key.rows` is cleared when the view changes. `row_view` includes `far.to_bits()`.
  - `guard` uses the top row's time span instead of `px_us`.

- [ ] **Step 1: Write the failing tests** in `main.rs` tests:
  - `row_times_follow_the_perspective`:
    - With `far = 0`, `row_times(0, 1.0, 100, 0.0)` gives row 0 = 5_000 and row 99 = 995_000 (today's even rows).
    - With `far` 0.3, 0.1 and 0.9, row times are finite and strictly increasing, the last is < 1_000_000, and the top row's step is larger than the bottom row's (by more than 5× for 0.3).
  - `perspective_culling_keeps_every_row_winner`:
    - Build `indexed(notes, true)` from about 5,000 random overlapping notes on keys 59–62 over 0–5 s (seeded like `overlapping_song`).
    - Run `select_pixels(0, 5_000_000, 5.0, 200, 0.3)`.
    - For every key and every time in `row_times(0, 5.0, 200, 0.3)`, the newest note (highest window index) covering that time must be in `visible.selected`.
  - `switching_row_sampling_rebuilds_the_rows`: select with `far 0.0`, then `0.3`, then `0.0` on the same `VisibleNotes`. The third selection equals a fresh `VisibleNotes`'s selection with `far 0.0`.
- [ ] **Step 2: Run them to make sure they fail.** `cargo test --bin kimrashi row_times perspective_culling switching_row` should fail.
- [ ] **Step 3: Implement** as specified in the Interfaces block. `select_pixels` uses `row_times` for `times`. The vertical path must give exactly today's values.
- [ ] **Step 4: Run `cargo test --bin kimrashi`** and make sure everything passes, including all existing culling and flatten tests.
- [ ] **Step 5: Verify** with `cargo run --release -- --benchmark-render nps --verify`. Every frame should report "GPU pixels equal".
- [ ] **Step 6: Commit** with the message `Culling: rows sampled at perspective times`.

### Task 4: Perspective view, live and exported

**Files:** Modify `src/ui.rs` (`draw_piano_roll`), `src/export.rs` (`run`, `run_from_args`, tests), `src/main.rs` (call site: pass `perspective_far`).

**Interfaces:** `draw_piano_roll`'s view tuple gains `perspective_far: f32`.

- [ ] **Step 1: Write the failing test** in `export.rs`: `perspective_export_narrows_notes_with_distance`.
  - Using `first_frame` with `speed 1.0` (set `Settings.speed`), render Vertical and Perspective.
  - In each frame, take the highest note-area row containing note colour (not `[20,20,24]`), and count its note-coloured pixels. The perspective count is smaller than the vertical count.
- [ ] **Step 2: Run it to make sure it fails.**
- [ ] **Step 3: Implement.**
  - **Live:** Perspective uses Vertical's layout and keyboard (PFA included).
    - Notes are drawn with `NoteView { perspective_far, ..NoteView::vertical(speed, now, -1.0, mode) }`.
    - `build_note_instances(.., far = perspective_far)`.
  - **Export:** the same. Add `--view perspective` and `--perspective-far <0.1..0.9>` (clamped), and update the module doc line.
- [ ] **Step 4: Run `cargo test --bin kimrashi`** and make sure everything passes.
- [ ] **Step 5: Check by eye.** Export the Septette with `--view perspective --speed 0.25 --width 960 --height 540`, then grab a still with `ffmpeg -ss 60`. The note area should be a trapezoid, with notes converging at the top and full width at the keyboard.
- [ ] **Step 6: Commit** with the message `Perspective view, live and exported`.

### Task 5: Black MIDI check

- [ ] **Step 1:** Export the Septette (14.9 M notes) for 45 s each, Vertical then Perspective: 1280×720, `--speed 0.25`, `--encoder nvenc`, `--lead 0 --tail 0 --no-title --no-watermark`. Record both realtime factors.
- [ ] **Step 2:** If Perspective is under half of Vertical, implement the spec's fallback (cache rows by time range and reuse unchanged ones) with a test, then measure again. Report both numbers.
