# Blocks view (3D notes and keys)

## Intent

Perspective with depth: falling notes are solid blocks standing on the tilted plane, and the black
keys stick up like on a real piano, lit keys still taking their note's colour. A view mode of its own
next to Perspective, live and exported. The user accepts the extra drawing cost.

## Goals / non-goals / anti-goals

- Goals: a `Blocks` view; notes as blocks (top, near end, inner side); raised black keys; same
  culling, far-width slider and export flags as Perspective (`--view blocks`).
- Non-goals: a movable camera, lighting models, 3D white keys, blocks in other views.
- Anti-goals: Vertical, Horizontal, Parallax and Perspective change no pixels (`--verify` passes,
  existing GPU tests unchanged).

## Approaches

1. **Blocks in the existing note shader (chosen by the user).** 18 vertices per instance instead of
   6: top face, near-end face, inner side face. Real depth (affine in 1/z) in the depth buffer.
2. Full 3D camera with matrices: free camera, but a second renderer and culling. Rejected (fixed camera).
3. Fake extrusion with offset copies: cheap, wrong edges and overlaps. Rejected.

## The geometry (the user's calls: approach 1, raised black keys, lit colouring kept)

- Perspective's plane is a camera looking down it: a point at depth z, height h (as a fraction of the
  camera's height above the plane) sits `(1 − (1 − h)/z) / (1 − far)` up the view, x as now. h = 0 is
  today's mapping.
- Block height (my call): a constant `BLOCK_H = 0.015` of the camera height, for notes and black keys.
- Faces: top at h = BLOCK_H over the note's footprint; near end at the note's start depth, h 0..BLOCK_H;
  the side facing the centre line (right side for keys left of centre, else left). The far end and
  the outer side never face the camera.
- Colours (my call): top as today (shaded / flat / PFA); near end the base colour × 0.7, side × 0.5.
- Depth: `(1 − 1/z) · 0.999 + 1e-5 · instance fraction`, so nearer surfaces win and coincident
  ones (overlapping notes) still go newest first. Only in Blocks; other views keep instance depth.
- Keyboard: the flat keyboard without its black keys, bent as in Perspective; then per black key a
  box (inner side, near end, top) mapped with the same lift; tops keep the idle or lit colour.

## Left to the builder

Uniform layout (a `block_height` field, 0 = flat), how the draw gets 18 vertices, test shapes.

# Plan (native, TDD)

1. `ViewMode::Blocks` + `Perspective::block` + `lift`; settings/UI/export flag treat Blocks like
   Perspective. Test: `lift(x, z, 0)` equals `point`, and a lifted point sits higher.
2. Shader blocks: `block_height` uniform (Globals 80 bytes), 18 vertices when on, faces and depth.
   GPU tests: a raised note covers a pixel above its flat far end, and its near face is darker than
   its top; a nearer block hides a newer farther one where they meet.
3. Raised black keys (both keyboard styles), live and export. Test: a Blocks export frame's keyboard
   differs from Perspective's, and other views are unchanged (`--benchmark-render nps --verify`).
4. Eye check on the Septette, self-review (the user asked for no subagents), commit.
