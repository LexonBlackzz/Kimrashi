# Horizontal and parallax note views

## Intent

Kimrashi only shows notes as a vertical piano roll: notes fall toward (or rise from) a
keyboard along the bottom. The user wants other ways to watch songs, mostly live. Export
should stay as easy as it is for the default view, so a new view must come along with
video export at no extra cost.

This is the first of several view projects. It covers **horizontal** and **parallax**,
which build on each other. Later projects, to be designed separately: velocity colouring
(needs velocity in the visual note data first), perspective (a MIDITrail-like 3D camera),
and key bars (a cheap keyboard-only view).

## Goals

- **Horizontal mode.** The keyboard stands on its side at the left or the right edge
  (a setting), and notes travel along the time axis toward it. Pitch runs bottom (low)
  to top (high). Per-frame cost is the same as the vertical view.
- **Parallax mode.** Horizontal with no keyboard. A playhead line runs down the middle
  of the note area. Notes scroll in from one side, sound as they cross the line, and
  keep going out the other side, fading as they leave.
  - Every **track** is a layer with its own scroll speed, assigned automatically. Tracks
    are spread evenly across a range set by one "near … far" setting (e.g. near 1×,
    far 0.3× of the normal note speed). Each layer crosses the middle at the correct
    time, so a fast layer spreads its notes out and a slow one packs them close.
  - Speeds snap to a fixed number of **speed steps** (64 to start). Tracks on the same
    step move together. A song with fewer tracks than steps gets one exact speed per
    track. The user accepted this so black MIDIs stay fast; revisit after implementation.
  - Parallax must play **black MIDIs** at usable frame rates, not just ordinary songs.
  - Depth comes from speed alone. Nothing shrinks: every layer uses the full height,
    and a given pitch sits at the same height in every layer.
  - Slower layers draw behind faster ones and are slightly dimmer.
  - A note lights up while it crosses the playhead, since there is no keyboard to show
    what's playing.
- Both modes work in video export with the same flags and the same effort as today.

## Non-goals

- Per-track speed overrides in settings (black MIDIs have thousands of tracks; only
  revisit if the automatic order bothers the user in practice).
- Choosing channel instead of track as the layer key.
- Shrinking or scaling layers for depth.
- Perspective, velocity, and key bars (separate projects, above).

## Anti-goals

- The default vertical view must not get slower or heavier in memory or upload
  bandwidth because of this work. The `NoteInstance` stays 16 bytes.
- No second render pipeline that export would have to be wired up to separately.

## Constraints (from the code today)

- Notes are drawn by one instanced pipeline: `shaders/notes.wgsl`, driven by
  `gpu::NotesCallback` and `GlobalsUniform` (`height_time`, `direction_sign`,
  `flat_mode`, `now_us`, `time_shift`). Instances live in a storage buffer of stable
  slots (`InstanceRing`); the vertex buffer is a 4-byte draw-order list. The same
  pipeline renders offscreen for export (`gpu::Offscreen`, `export.rs`).
- The vertex shader maps key → horizontal span via `key_positions` (from
  `ui::key_columns`) and time → vertical position. Horizontal needs those axes swapped.
- `NoteInstance` packs start (48 bits), the whole note's length (bf16), length, pitch,
  border flags and colour into 16 bytes. It has no track; `NoteSegment` does
  (`track: u16`).
- The visible-note window is `now .. now + note_speed` (`VisibleNotes::advance`), and
  instance slots expire when a note ends. Parallax needs both a past window and a
  longer future window.
- The keyboard is drawn by egui live (`ui::draw_keyboard`, `draw_pfa_keyboard`) and on
  the CPU for export (`export::keyboard`, `pfa::keyboard`). Both need a sideways form.
- Note mode today: `Settings::note_mode()` → 0 shaded, 1 flat, 2 PFA; `notes_rise`
  flips direction.

## Approaches considered

1. **One shader, a speed for each note (chosen).** Extend `notes.wgsl` with an axis
   (vertical/horizontal), a direction, and a playhead position (edge or middle). In
   parallax mode a second storage buffer, parallel to the instance slots, holds each
   slot's speed factor (f32, or f16 if it fits). It is written alongside the slot from
   the note's track and is bound and filled only in parallax mode, so the default view
   pays nothing. Export works automatically. Every track gets an exact speed.
2. **Speed tiers.** Group tracks into ~32 speed bands and draw each band in its own pass
   with its own speed uniform. No per-note growth and the draw order is free, but tracks
   in the same band share a speed, which falls short of "every track gets a speed."
3. **A separate parallax renderer.** Full freedom, but export would have to be wired up
   again and there would be two pipelines to maintain. Rejected.

The user picked approach 1, then, once black MIDI support became a must, accepted
snapping speeds to steps (the core of approach 2) for culling. Exact speed for every
track would need the culling and "Hide covered notes" bookkeeping per track (128 keys ×
thousands of tracks), which costs more than it saves. With steps it runs per step
(128 keys × 64), which stays correct and cheap.

## Why steps: the two shortcuts

The default view stays fast on black MIDIs because of two shortcuts that assume every
note moves at one speed:

- **Dense selection** (`VisibleNotes::dense` / `select_pixels`): keeps about one note per
  pixel row per key.
- **Hide covered notes** (`collapse_overlaps`, on by default): `FlattenKey::sweep`,
  `push_culled`/`absorb` flatten each key across all tracks into one topmost timeline,
  and `render_tracks` drops tracks identical to a later one.

In parallax both must run **per speed step**: each step has its own 128-key flatten
state and its own pixel selection, using that step's time-to-pixel mapping. Notes on
different steps never cut or hide each other. Identical-track dedup applies only
between tracks on the same step.

## Decided design

- **Settings:** a view selector (Vertical / Horizontal / Parallax). Horizontal adds a
  keyboard side (Left / Right). Parallax adds a far speed (fraction of the near speed;
  near is the normal note speed). Existing note speed, palettes and flat/shaded styles
  still apply.
- **Track → speed:** a track's speed factor is a linear interpolation from near (1.0)
  to far, by its rank among the song's tracks that have notes (first track nearest),
  then snapped to the nearest of the speed steps.
- **Draw order in parallax:** slower behind faster. Use the depth buffer, with a note's
  depth derived from its speed factor (instead of, or combined with, the current
  per-instance depth), so no CPU sort is needed.
- **Dimming:** a note's brightness scales with its speed factor (far is a little
  dimmer). The exact curve is a builder call; aim for subtle.
- **Playhead lighting:** a note that overlaps the playhead line is drawn brighter.
- **Past half:** notes past the line keep moving and fade toward the edge.
- **Visible window in parallax:** the far layer shows the most song time, so the culling
  window is `now - span/far .. now + span/far` on each side (span = half the screen in
  note-speed time). Instance slots must stay alive until the note has left the past
  side, not just until it ends.
- **Performance:** at far = 0.3× with the past half, a dense song can draw several times
  as many notes as the default view, even with per-step culling. The existing instance
  cap (`max_instances`) still applies. The far speed setting is how the user trades
  looks for frame rate.

## Left to the builder

- Exact shader changes (uniform layout, how the axis swap is expressed).
- How the GPU learns each note's speed: a per-slot speed (or step index) buffer, or one
  draw per step with a speed uniform. Either is fine; per-step culling already groups
  notes by step.
- The exact number of speed steps, if 64 measures badly.
- How the parallax past window interacts with `InstanceRing` expiry and `cap`.
- Sideways keyboard drawing for egui and for export.
- Export CLI flags for the new view, keeping today's flags' style.

## Builder's calls (marked)

- *My call:* PFA keyboard style is not offered in horizontal mode for the first cut;
  horizontal uses the classic keyboard, turned sideways. PFA's sideways form can come
  later.
- *My call:* In parallax, the near layer takes one note-speed to travel from the screen
  edge to the playhead, the same feel as the vertical view (so span = note speed).
- *My call:* When the instance cap bites in parallax, it sheds the farthest (slowest,
  dimmest) steps first, instead of today's furthest-future-sharps-first order.
- *My call:* A layer is the note's source track (`NoteSegment.track`).
- *My call:* The parallax playhead is a thin line drawn on top of the notes, in the
  keyboard's highlight colour.
