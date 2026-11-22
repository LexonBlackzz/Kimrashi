# Key glow ("fake HDR")

## Intent

Keys that are hit emit light that blooms over the keyboard and up into the note area, spills onto
neighbouring keys, and decays after release. Only the keys glow (the user's call); everything is
adjustable. First of three effects: glow, then a rainbow bar, then confetti.

## Approaches

1. **A light strip computed on the CPU, drawn additively (chosen).** Per key a light level; summed
   across the screen with a soft sideways falloff; tone-mapped; drawn as additive gradients (live:
   an egui mesh with premultiplied colours of alpha 0, which add; export: added into the frame).
   Fits the current renderer, identical live and in export.
2. Bloom as a GPU post-process over everything: needs notes and keyboard drawn into one offscreen
   image first (a refactor). Not needed while only keys glow.

## The model

- Per key a level `L` and colour. While a key sounds, `L` is held at `intensity`; after release
  it decays as `exp(-dt / decay)`. A jump back in time (seek) clears it.
- The strip: for each screen column x, `light(x) = Σ_k L_k · colour_k · exp(-((x − x_k) / spread)²)`,
  with `spread` in key widths. Tone map per channel: `1 − exp(−light)`. Dense passages saturate
  smoothly toward white instead of clipping.
- Above the keyboard line the light falls off as `exp(−y / reach)` (reach: a fraction of the note
  area's height); over the keys it is added at half strength, uniformly.

## Settings

Glow on/off (default off), intensity 0–3 (1), decay 0.05–2 s (0.3), spread 0.5–8 keys (2),
reach 0.02–1 of the note area (0.25). Export flags: `--glow`, `--glow-intensity`, `--glow-decay`,
`--glow-spread`, `--glow-reach`.

## Scope (my calls)

- Vertical view only (both keyboard styles), where the keyboard is a straight line along the
  bottom. Horizontal and Perspective later.
- Brightness doesn't follow how many notes hit a key: the on-screen notes merge overlapping notes,
  so the count isn't known there (the Matrix walker could supply it later).

## Plan (native, TDD)

1. `glow.rs`: levels, decay, strip, tone map. Tests: a released key halves after `decay · ln 2`;
   the strip peaks at the key and is lower two spreads away; tone-mapped values stay below 1 and
   rise with light.
2. Draw live and in export; settings and flags. Test: an exported frame with glow is brighter just
   above a sounding key than without, and unchanged far away.
3. Eye check, commit.
