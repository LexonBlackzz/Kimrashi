// Kimrashi - note rendering shader.
// Based on shader code from Wasabi (https://github.com/BlackMIDIDevs/wasabi),
// Copyright (c) the Wasabi authors, licensed under the GNU GPL v3.
// This file is distributed under GPL-3.0-or-later; see LICENSE.

struct KeyPosition {
    left: f32,
    right: f32,
    _pad0: f32,
    _pad1: f32,
};

struct Globals {
    height_time: f32,
    win_width: f32,
    win_height: f32,
    direction_sign: f32,
    flat_mode: u32,
    instance_count: u32,
    now_lo: u32,
    now_hi: u32,
    time_shift: f32,
    /// 1: the time axis runs across the screen and the keys up it (low pitch at the bottom).
    horizontal: u32,
    /// Fraction of the time axis where notes light up and the past fades; < 0 = none.
    playhead: f32,
    brightness: f32,
    depth_base: f32,
    depth_scale: f32,
    /// Perspective (vertical only): the far edge's width as a fraction of the near; 0 = off.
    perspective_far: f32,
    /// Perspective: depth at the keyboard line (the screen's bottom edge is 1; below 1 counts as 1).
    perspective_near: f32,
    /// Blocks: notes stand this high off the plane (a fraction of the camera's height); 0 = flat.
    block_height: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
};

@group(0) @binding(0)
var<uniform> key_positions: array<KeyPosition, 256u>;

@group(0) @binding(1)
var<uniform> globals: Globals;

struct NoteInstance {
    start_us: vec2<u32>,
    length: f32,
    key_color: u32,
};
@group(0) @binding(2)
var<storage, read> instances: array<NoteInstance>;

struct VertexInput {
    @location(0) slot: u32,
    @builtin(vertex_index) vertex_index: u32,
    @builtin(instance_index) instance_index: u32,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) note_size: vec2<f32>,
    @location(3) win_size: vec2<f32>,
    @location(4) border_width: f32,
    @location(5) @interpolate(flat) edges: u32,
    @location(6) @interpolate(flat) thin: u32,
    @location(7) time_norm: f32,
    @location(8) @interpolate(flat) lit: u32,
    /// Blocks: 0 top, 1 near end, 2 side.
    @location(9) @interpolate(flat) face: u32,
    /// Perspective: the note's width in pixels here ((right - left)/z is affine on screen, so exact).
    @location(10) width_px: f32,
};

fn microseconds_as_float(lo: u32, hi: u32) -> f32 {
    if hi == 0u { return f32(lo); }
    // Round the integer once, including ties to even. Rounding the low word first
    // can change an end pixel when an hours-long note reaches the current window.
    let shift = firstLeadingBit(hi) + 9u;
    var significant: u32;
    var round_up: bool;
    if shift < 32u {
        significant = (hi << (32u - shift)) | (lo >> shift);
        let remainder = lo & ((1u << shift) - 1u);
        let half = 1u << (shift - 1u);
        round_up = remainder > half || (remainder == half && (significant & 1u) != 0u);
    } else if shift == 32u {
        significant = hi;
        round_up = lo > 2147483648u || (lo == 2147483648u && (significant & 1u) != 0u);
    } else {
        let high_shift = shift - 32u;
        significant = hi >> high_shift;
        let remainder = hi & ((1u << high_shift) - 1u);
        let half = 1u << (high_shift - 1u);
        round_up = remainder > half || (remainder == half && (lo != 0u || (significant & 1u) != 0u));
    }
    significant += select(0u, 1u, round_up);
    return f32(significant) * exp2(f32(shift));
}

@vertex
fn vs_notes(input: VertexInput) -> VertexOutput {
    let note = instances[input.slot];
    let corners = array<vec2<f32>, 6u>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
    );

    let vidx = input.vertex_index % 6u;
    let uv = corners[vidx];

    // Pitch is the low 7 bits; bit 7 = no start border, a negative length = no end border
    // (the note is a piece of a longer one, cut by or exposed again by a newer note).
    let key = note.key_color & 0x7Fu;
    let note_length = abs(note.length);
    let edges = select(1u, 0u, (note.key_color & 0x80u) != 0u) | select(2u, 0u, note.length < 0.0);
    let color_packed = note.key_color >> 8u;
    let key_pos = key_positions[key];

    let left = key_pos.left * 2.0 - 1.0;
    let right = key_pos.right * 2.0 - 1.0;

    let safe_height = max(globals.height_time, 1e-6);
    // Subtract integer microseconds before converting, just as the CPU used to.
    // Absolute f32 seconds would lose visible detail in long songs at close zoom.
    // The top 16 bits of the high word are the whole note's length (bfloat16 seconds).
    let note_start = vec2<u32>(note.start_us.x, note.start_us.y & 0xFFFFu);
    let whole_seconds = bitcast<f32>(note.start_us.y & 0xFFFF0000u);
    let negative = note_start.y < globals.now_hi ||
        (note_start.y == globals.now_hi && note_start.x < globals.now_lo);
    let later = select(note_start, vec2<u32>(globals.now_lo, globals.now_hi), negative);
    let earlier = select(vec2<u32>(globals.now_lo, globals.now_hi), note_start, negative);
    let lo = later.x - earlier.x;
    let hi = later.y - earlier.y - select(0u, 1u, later.x < earlier.x);
    let delta = microseconds_as_float(lo, hi);
    // A GPU division may multiply by a rounded reciprocal. Include its low part
    // with one rounding, matching the CPU's f32 / 1_000_000 at raster boundaries.
    let seconds = fma(delta, 0.000001, delta * 2.52475729e-15);
    let start = select(seconds, -seconds, negative) + globals.time_shift;
    let start_norm = clamp(start / safe_height, 0.0, 2.0);
    let end_norm = clamp((start + note_length) / safe_height, 0.0, 2.0);

    var start_clip = 1.0 - start_norm * 2.0;
    var end_clip = 1.0 - end_norm * 2.0;
    if (globals.direction_sign < 0.0) {
        start_clip = -start_clip;
        end_clip = -end_clip;
    }
    start_clip = clamp(start_clip, -1.0, 1.0);
    end_clip = clamp(end_clip, -1.0, 1.0);

    let key_clip = left + (right - left) * uv.x;
    let time_clip = start_clip + (end_clip - start_clip) * uv.y;
    let horizontal = globals.horizontal != 0u;
    var x = select(key_clip, time_clip, horizontal);
    var y = select(time_clip, key_clip, horizontal);
    // Clip-space size: across the keys, along time.
    var size = vec2<f32>(right - left, abs(start_clip - end_clip));
    // The whole note's length in clip units along time (for `thin`).
    var whole_px = whole_seconds / safe_height * 2.0;
    let perspective = globals.perspective_far > 0.0 && !horizontal;
    // Blocks, 6 vertices per piece: the top near and far of where sinking ends, the near end, the
    // side (facing the centre line) near and far. `face` is what shading sees: top, near end, side.
    let piece = input.vertex_index / 6u;
    let face = select(select(0u, 1u, piece == 2u), 2u, piece >= 3u);
    var z_block = 1.0;
    var uv_out = uv;
    var width_px = 1e9;
    if (perspective) {
        // Time n (0 at the keyboard line, 1 at the top) is depth z = 1 + (Z - 1) n; a point
        // sits (1 - 1/z) / (1 - 1/Z) up the view, and its key position shrinks toward the centre by 1/z.
        let far_z = 1.0 / globals.perspective_far;
        // The notes run from the keyboard line's depth, `line_z`, up to `far_z`; heights are of the
        // whole view (keyboard included, `line` of it below the notes), then of the note area.
        let line_z = max(globals.perspective_near, 1.0);
        let z0 = line_z + (far_z - line_z) * clamp(start / safe_height, 0.0, 1.0);
        let z1 = line_z + (far_z - line_z) * clamp((start + note_length) / safe_height, 0.0, 1.0);
        let up = 1.0 / (1.0 - 1.0 / far_z);
        let line = (1.0 - 1.0 / line_z) * up;
        let y0 = ((1.0 - 1.0 / z0) * up - line) / (1.0 - line);
        let y1 = ((1.0 - 1.0 / z1) * up - line) / (1.0 - line);
        // Blocks: a point `lift` off the plane (camera height 1) sits (1 - (1 - lift)/z) up the view.
        // Over the last `sink` of depth before the keyboard line, blocks flatten onto the plane, so
        // sounding notes meet their keys flush. Flat (no blocks): one piece, the whole note.
        let sink = 0.06 * (far_z - line_z);
        let zk = select(z1, clamp(line_z + sink, z0, z1), globals.block_height > 0.0);
        let near_piece = piece == 0u || piece == 3u;
        var z = select(select(zk, z0, near_piece), select(z1, zk, near_piece), uv.y > 0.5);
        var key = key_pos.left + (key_pos.right - key_pos.left) * uv.x;
        var tall = 1.0;
        if (face == 1u) {
            // A cut piece (no start border) continues an older part: no near end (zero height).
            z = z0;
            tall = select(0.0, uv.y, (edges & 1u) != 0u);
        } else if (face == 2u) {
            key = select(key_pos.left, key_pos.right, key_pos.left + key_pos.right < 1.0);
            tall = uv.x;
        }
        let lift = globals.block_height * clamp((z - line_z) / max(sink, 1e-6), 0.0, 1.0) * tall;
        // The top's two pieces share the note's uv (borders at its real ends, not at the split).
        if (face == 0u && globals.block_height > 0.0) { uv_out.y = (z - z0) / max(z1 - z0, 1e-6); }
        x = (0.5 + (key - 0.5) / z) * 2.0 - 1.0;
        y = ((1.0 - (1.0 - lift) / z) * up - line) / (1.0 - line) * 2.0 - 1.0;
        z_block = z;
        width_px = (key_pos.right - key_pos.left) / z * globals.win_width;
        size = vec2<f32>((key_pos.right - key_pos.left) * 2.0 / z0, (y1 - y0) * 2.0);
        // A cut piece measures its whole note, scaled as at the piece's near end (dy/dt there).
        whole_px = whole_seconds / safe_height * (far_z - line_z) * up / (z0 * z0) / (1.0 - line) * 2.0;
    }
    // Pixels along the key axis, then along the time axis.
    let win = select(vec2<f32>(globals.win_width, globals.win_height), vec2<f32>(globals.win_height, globals.win_width), horizontal);

    let r = f32((color_packed >> 16u) & 0xFFu) / 255.0;
    let g = f32((color_packed >> 8u) & 0xFFu) / 255.0;
    let b = f32(color_packed & 0xFFu) / 255.0;

    var out: VertexOutput;
    // Instances arrive newest (topmost) first: increasing depth, so with a Less depth
    // test, notes drawn later only fill pixels no newer note already covers.
    let order = (f32(input.instance_index) + 1.0) / (f32(globals.instance_count) + 2.0);
    var depth = globals.depth_base + globals.depth_scale * order;
    // Blocks: real depth (1 - 1/z is affine on screen, so faces interpolate it exactly); instance
    // order only breaks ties between coincident faces.
    if (globals.block_height > 0.0) {
        depth = (1.0 - 1.0 / z_block) * 0.999 + 1e-5 * order;
    }
    out.clip_position = vec4<f32>(x, y, depth, 1.0);
    out.color = vec3<f32>(r, g, b);
    out.uv = uv_out;
    out.width_px = width_px;
    out.note_size = size;
    out.win_size = win;
    out.time_norm = start_norm + (end_norm - start_norm) * uv.y;
    out.lit = select(0u, 1u, globals.playhead >= 0.0 && start_norm <= globals.playhead && end_norm >= globals.playhead);
    out.border_width = 1.0;
    out.edges = edges;
    out.face = face;
    // Thin is under ~3.5 px. Flattened pieces measure the whole note (however much of it a
    // cut or the window shows); a whole length of 0 measures the clamped quad, as before.
    let measured = select(size.y * win.y,
        whole_px * win.y, whole_seconds > 0.0);
    out.thin = select(0u, 1u, measured < 7.0);
    return out;
}

@fragment
fn fs_notes(input: VertexOutput) -> @location(0) vec4<f32> {
    var color = input.color;
    // Perspective: a note is a trapezoid drawn as two triangles, so uv.x stretches differently in
    // each; side borders are measured in screen pixels instead (uv.x per pixel, in this triangle).
    let perspective = globals.perspective_far > 0.0 && globals.horizontal == 0u;
    let uv_x_per_px = max(fwidth(input.uv.x), 1e-6);
    // Perspective: a far note under 8 px wide fades to its border colour (all of it under 5.5 px). Its inside would be a
    // pixel or two that comes and goes with alignment, drawing moire swirls across the distance.
    let narrow = select(1.0, smoothstep(5.5, 8.0, input.width_px), perspective);

    if (globals.flat_mode == 0u) {
        let srgb = input.color * input.color;
        let horiz_width_pixels = max(input.note_size.x * 0.5 * input.win_size.x, 1.0);
        let vert_width_pixels = max(input.note_size.y * 0.5 * input.win_size.y, 1.0);
        let horiz_margin = select(input.border_width / horiz_width_pixels, input.border_width * uv_x_per_px, perspective);
        let vert_margin = input.border_width / vert_width_pixels;
        let start_border = (input.edges & 1u) != 0u;
        let end_border = (input.edges & 2u) != 0u;
        let border = input.uv.x < horiz_margin ||
            input.uv.x > 1.0 - horiz_margin ||
            (start_border && input.uv.y < vert_margin) ||
            (end_border && input.uv.y > 1.0 - vert_margin);
        let dark = srgb * 0.4;

        if border || input.thin != 0u {
            color = dark;
        } else {
            color = srgb;
        }
        color = mix(dark, color, narrow);
    }

    // Piano From Above: a border of the color at 20% brightness (key-width scaled, 1-3 px),
    // inside it a gradient from the full color on the left to 60% on the right.
    if (globals.flat_mode == 2u) {
        var size_px = input.note_size * 0.5 * input.win_size;
        let d = clamp(round(size_px.x * 0.075), 1.0, 3.0);
        var p = input.uv * size_px;
        if (perspective) {
            size_px.x = 1.0 / uv_x_per_px;
            p.x = input.uv.x * size_px.x;
        }
        let border = p.x < d || p.x > size_px.x - d ||
            ((input.edges & 1u) != 0u && p.y < d) ||
            ((input.edges & 2u) != 0u && p.y > size_px.y - d);
        color = select(input.color * mix(1.0, 0.6, input.uv.x), input.color * 0.2, border || input.thin != 0u);
        color = mix(input.color * 0.2, color, narrow);
    }

    // Blocks: the near end and side are the base colour, darkened.
    if (input.face != 0u) {
        color = select(input.color, input.color * input.color, globals.flat_mode == 0u) * select(0.5, 0.7, input.face == 1u);
    }

    // Parallax: a note on the playhead is lit, and the past fades out toward the edge.
    if (input.lit != 0u) {
        color = mix(color, vec3<f32>(1.0), 0.4);
    }
    if (globals.playhead > 0.0 && input.time_norm < globals.playhead) {
        color = color * mix(0.15, 1.0, input.time_norm / globals.playhead);
    }
    return vec4<f32>(color * globals.brightness, 1.0);
}
