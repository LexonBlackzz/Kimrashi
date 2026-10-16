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

    let x = left + (right - left) * uv.x;
    let y = start_clip + (end_clip - start_clip) * uv.y;

    let r = f32((color_packed >> 16u) & 0xFFu) / 255.0;
    let g = f32((color_packed >> 8u) & 0xFFu) / 255.0;
    let b = f32(color_packed & 0xFFu) / 255.0;

    var out: VertexOutput;
    // Instances arrive newest (topmost) first: increasing depth, so with a Less depth
    // test, notes drawn later only fill pixels no newer note already covers.
    let depth = (f32(input.instance_index) + 1.0) / (f32(globals.instance_count) + 2.0);
    out.clip_position = vec4<f32>(x, y, depth, 1.0);
    out.color = vec3<f32>(r, g, b);
    out.uv = uv;
    out.note_size = vec2<f32>(right - left, abs(start_clip - end_clip));
    out.win_size = vec2<f32>(globals.win_width, globals.win_height);
    out.border_width = 1.0;
    out.edges = edges;
    // Thin is under ~3.5 px. Flattened pieces measure the whole note (however much of it a
    // cut or the window shows); a whole length of 0 measures the clamped quad, as before.
    let measured = select(abs(start_clip - end_clip) * globals.win_height,
        whole_seconds / safe_height * globals.win_height * 2.0, whole_seconds > 0.0);
    out.thin = select(0u, 1u, measured < 7.0);
    return out;
}

@fragment
fn fs_notes(input: VertexOutput) -> @location(0) vec4<f32> {
    var color = input.color;

    if (globals.flat_mode == 0u) {
        let srgb = input.color * input.color;
        let horiz_width_pixels = max(input.note_size.x * 0.5 * input.win_size.x, 1.0);
        let vert_width_pixels = max(input.note_size.y * 0.5 * input.win_size.y, 1.0);
        let horiz_margin = input.border_width / horiz_width_pixels;
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
    }

    return vec4<f32>(color, 1.0);
}
