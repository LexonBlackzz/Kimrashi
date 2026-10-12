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
};

@group(0) @binding(0)
var<uniform> key_positions: array<KeyPosition, 256u>;

@group(0) @binding(1)
var<uniform> globals: Globals;

struct VertexInput {
    @location(0) start_us: vec2<u32>,
    @location(1) length: f32,
    @location(2) key_color: u32,
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

    let key = input.key_color & 0xFFu;
    let color_packed = input.key_color >> 8u;
    let key_pos = key_positions[key];

    let left = key_pos.left * 2.0 - 1.0;
    let right = key_pos.right * 2.0 - 1.0;

    let safe_height = max(globals.height_time, 1e-6);
    // Subtract integer microseconds before converting, just as the CPU used to.
    // Absolute f32 seconds would lose visible detail in long songs at close zoom.
    let negative = input.start_us.y < globals.now_hi ||
        (input.start_us.y == globals.now_hi && input.start_us.x < globals.now_lo);
    let later = select(input.start_us, vec2<u32>(globals.now_lo, globals.now_hi), negative);
    let earlier = select(vec2<u32>(globals.now_lo, globals.now_hi), input.start_us, negative);
    let lo = later.x - earlier.x;
    let hi = later.y - earlier.y - select(0u, 1u, later.x < earlier.x);
    let delta = microseconds_as_float(lo, hi);
    // A GPU division may multiply by a rounded reciprocal. Include its low part
    // with one rounding, matching the CPU's f32 / 1_000_000 at raster boundaries.
    let seconds = fma(delta, 0.000001, delta * 2.52475729e-15);
    let start = select(seconds, -seconds, negative);
    let start_norm = clamp(start / safe_height, 0.0, 2.0);
    let end_norm = clamp((start + input.length) / safe_height, 0.0, 2.0);

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
        let border = input.uv.x < horiz_margin ||
            input.uv.x > 1.0 - horiz_margin ||
            input.uv.y < vert_margin ||
            input.uv.y > 1.0 - vert_margin;
        let thickness = input.note_size.y * input.win_size.y;
        let dark = srgb * 0.4;

        if border || thickness < 7.0 {
            color = dark;
        } else {
            color = srgb;
        }
    }

    return vec4<f32>(color, 1.0);
}
