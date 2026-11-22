# Perspective view (2D MIDITrail-style)

## Intent

A view of the vertical piano roll seen at an angle: the keyboard stays flat and full width at
the bottom, and the note area becomes a trapezoid narrowing toward the top. Far notes (the
future) are small and packed together near the top; they widen and speed up as they approach
the keyboard. The user wants it to stay fast on black MIDIs, with the level of detail
dropping with distance.

```
        ┌──┐            far: narrow, notes packed tight
       /    \
      /      \
     /        \         near: full width, notes spread out
    └──────────┘
    [ keyboard ]        unchanged, flat
```

## Goals

- A new view mode, **Perspective**, next to Vertical / Horizontal / Parallax.
- A fixed camera. Notes are flat strips, not 3D blocks.
- Cost stays tied to screen pixels, not to how much song time is visible: dense culling keeps
  about one note per screen row per key, with rows sampled at perspective-correct times.
- Live and video export, the same as the other views.

## Non-goals

- A movable camera, 3D note blocks, lighting, or a 3D keyboard.
- Perspective for horizontal or parallax (possible later, since the culling change is shared).

## Anti-goals

- The vertical, horizontal and parallax views must not get slower or change pixels
  (`--benchmark-render nps --verify` still passes).
- No render-to-texture warp: it blurs the far end and gives no LOD.

## Approaches considered

1. **Warp in the note shader, with per-row LOD (chosen).** The vertex shader maps (key
   position, time) through the trapezoid. The culling samples screen rows at the perspective's
   uneven times.
2. **Render flat, then warp the image.** Least code, but the far end blurs or shimmers, the
   texture must be huge to stay sharp, and culling still spends full effort on far rows.
   Rejected.
3. **A general 3D camera with matrices.** Overkill for a fixed tilt. Rejected.

## The mapping

- **Settings:** a new `Settings::perspective_far` (my call): the width of the far (top) edge
  as a fraction of the bottom, default 0.3, range 0.1–0.9, as a "Far width" slider shown only
  in this view.
- **Depth:** with `Z = 1 / perspective_far`, a note time `t` seconds ahead (0 ≤ t ≤ T, where
  T = note speed) sits at depth `z = 1 + (Z − 1)·t/T`.
- **Screen position:**
  - Height above the keyboard line: `y = (1 − 1/z) / (1 − 1/Z)` of the note area height.
    Depth 1 is at the keyboard line and depth Z at the top edge.
  - Horizontal: `x = 0.5 + (x_key − 0.5) / z` across the note area, so columns converge on
    the top centre.
- **Note speed** keeps its meaning (my call): everything up to `note_speed` seconds ahead is
  on screen, with the top edge being exactly `note_speed` ahead.
- **Note shape:** each note is a quad. Its four corners are mapped through the formulas
  above, giving a trapezoid. Mapping four corners is exact here, because this mapping sends
  straight lines to straight lines. Borders are computed in the note's own (uv) space as now,
  so they thin with distance.
- **Direction:** always falling toward a bottom keyboard. "Notes rise" doesn't apply (the row
  is hidden in this view, like in Horizontal).
- **Keyboard:** unchanged, including PFA style.

## The LOD change

- `VisibleNotes::select_pixels` builds `times[row] = now + (row + 0.5)·px_us`, a fixed step,
  and caches rows per key, shifting the cache as time advances.
- In this view, row times come from the inverse of the y mapping: for row centre `y`
  (0 = keyboard line, 1 = top), `z = 1 / (1 − y·(1 − 1/Z))` and `t = (z − 1)/(Z − 1)·T`.
- `select_pixels` takes the row times as a slice, or a mapping function, instead of deriving
  them from `px_us`.
- With uneven rows, the row cache can't be shifted by a whole number of rows each frame. In
  this view it is rebuilt every frame (my call, measured first). The vertical view keeps its
  shifting cache unchanged.
- The edge guard (`guard`, extra time around row samples) uses the widest row's time span,
  which is the top row.

## Export

- `--view perspective` and `--perspective-far <0.1..0.9>` flags. The saved setting is used
  otherwise.
- Same keyboard and layout as Vertical.

## Left to the builder

- How the shader gets the mode: a `perspective_far` uniform (0 = off) fits the existing
  per-draw uniforms.
- How `build_note_instances`' sparse path (no row culling) is affected: it draws every
  visible note, which is correct as is.
- `active_keys` (lit keys) is unchanged, since it only looks at `now`.

## Risks

- Rebuilding the row cache every frame may cost more than the vertical view's shifted cache
  on black MIDIs. Measure on the Septette at 0.25 s note speed, export throughput as before.
  If it's too slow, cache rows by their time instead, and reuse rows whose time range is
  unchanged.
