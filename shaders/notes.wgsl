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
    _pad1: u32,
    _pad2: u32,
};

@group(0) @binding(0)
var<storage, read> key_positions: array<KeyPosition, 256u>;

@group(0) @binding(1)
var<uniform> globals: Globals;

struct VertexInput {
    @location(0) start_length: vec2<f32>,
    @location(1) key_color: u32,
    @location(2) border_width: u32,
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
    let start_norm = clamp(input.start_length.x / safe_height, 0.0, 2.0);
    let end_norm = clamp((input.start_length.x + input.start_length.y) / safe_height, 0.0, 2.0);

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
    out.border_width = f32(input.border_width);
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
