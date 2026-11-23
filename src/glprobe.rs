//! Diagnostics for an HD 4000 that draws no text on OpenGL: shader variants passing the texture
//! coordinate different ways. Delete once that is fixed.

use eframe::wgpu;

const COMMON: &str = "@group(0) @binding(0) var t: texture_2d<f32>;
@group(0) @binding(1) var s: sampler;
struct In { @location(0) pos: vec2<f32>, @location(1) uv: vec2<f32>, @location(2) col: u32 };
fn unpack(c: u32) -> vec4<f32> { return vec4<f32>(f32(c & 255u), f32((c >> 8u) & 255u), f32((c >> 16u) & 255u), f32((c >> 24u) & 255u)) / 255.0; }
";

const VS: &str = "@vertex fn vs(i: In) -> O { var o: O; o.uv = i.uv; o.col = unpack(i.col); o.p = vec4<f32>(i.pos, 0.0, 1.0); return o; }";
const O_UV_COL: &str = "struct O { @location(0) uv: vec2<f32>, @location(1) col: vec4<f32>, @builtin(position) p: vec4<f32> };";

/// Each variant draws a 4-texel strip (white red green blue) over 64 x 16 px; x 8 should read
/// white and x 40 green ("constant" puts one coordinate on every corner: both green).
pub fn shader_variants(device: &wgpu::Device, queue: &wgpu::Queue) -> String {
    let variants: Vec<(&str, bool, String)> = vec![
        ("baseline", false, format!("{O_UV_COL}\n{VS}\n@fragment fn fs(i: O) -> @location(0) vec4<f32> {{ return textureSample(t, s, i.uv) * i.col; }}")),
        ("constant", true, format!("{O_UV_COL}\n{VS}\n@fragment fn fs(i: O) -> @location(0) vec4<f32> {{ return textureSample(t, s, i.uv) * i.col; }}")),
        ("packed", false, "struct O { @location(0) d: vec4<f32>, @location(1) col: vec4<f32>, @builtin(position) p: vec4<f32> };
@vertex fn vs(i: In) -> O { var o: O; o.d = vec4<f32>(i.uv, 0.0, 1.0); o.col = unpack(i.col); o.p = vec4<f32>(i.pos, 0.0, 1.0); return o; }
@fragment fn fs(i: O) -> @location(0) vec4<f32> { return textureSample(t, s, i.d.xy) * i.col; }".into()),
        ("swapped", false, format!("struct O {{ @location(0) col: vec4<f32>, @location(1) uv: vec2<f32>, @builtin(position) p: vec4<f32> }};\n{VS}
@fragment fn fs(i: O) -> @location(0) vec4<f32> {{ return textureSample(t, s, i.uv) * i.col; }}")),
        ("texel fetch", false, format!("{O_UV_COL}\n{VS}
@fragment fn fs(i: O) -> @location(0) vec4<f32> {{ return textureLoad(t, vec2<i32>(i.uv * vec2<f32>(4.0, 1.0)), 0) * i.col; }}")),
        ("from position", false, "struct O { @location(0) col: vec4<f32>, @builtin(position) p: vec4<f32> };
@vertex fn vs(i: In) -> O { var o: O; o.col = unpack(i.col); o.p = vec4<f32>(i.pos, 0.0, 1.0); return o; }
@fragment fn fs(i: O) -> @location(0) vec4<f32> { return textureSample(t, s, i.p.xy / vec2<f32>(64.0, 16.0)) * i.col; }".into()),
        ("uv only", false, "struct O { @location(0) uv: vec2<f32>, @builtin(position) p: vec4<f32> };
@vertex fn vs(i: In) -> O { var o: O; o.uv = i.uv; o.p = vec4<f32>(i.pos, 0.0, 1.0); return o; }
@fragment fn fs(i: O) -> @location(0) vec4<f32> { return textureSample(t, s, i.uv); }".into()),
    ];
    let (w, h) = (64u32, 16u32);
    let size = |w, h| wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 };
    let texture = |label, w, h, usage| device.create_texture(&wgpu::TextureDescriptor { label: Some(label), size: size(w, h), mip_level_count: 1,
        sample_count: 1, dimension: wgpu::TextureDimension::D2, format: wgpu::TextureFormat::Rgba8Unorm, usage, view_formats: &[] });
    let strip = texture("variants strip", 4, 1, wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST);
    queue.write_texture(strip.as_image_copy(), &[255, 255, 255, 255, 255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255],
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(16), rows_per_image: None }, size(4, 1));
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor::default());
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: None, entries: &[
        wgpu::BindGroupLayoutEntry { binding: 0, visibility: wgpu::ShaderStages::FRAGMENT, count: None,
            ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2, multisampled: false } },
        wgpu::BindGroupLayoutEntry { binding: 1, visibility: wgpu::ShaderStages::FRAGMENT, count: None,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering) },
    ] });
    let view = strip.create_view(&Default::default());
    let group = device.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &layout, entries: &[
        wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
    ] });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[&layout], push_constant_ranges: &[] });
    // Two triangles over the whole target: u runs left to right, v top to bottom (or one fixed coordinate).
    let quad = |fixed: Option<[f32; 2]>| -> Vec<u8> {
        [(-1.0f32, -1.0f32), (1.0, -1.0), (1.0, 1.0), (-1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)].iter().flat_map(|&(x, y)| {
            let [u, v] = fixed.unwrap_or([(x + 1.0) / 2.0, (1.0 - y) / 2.0]);
            [x, y, u, v].iter().flat_map(|f| f.to_le_bytes()).chain(u32::MAX.to_le_bytes()).collect::<Vec<u8>>()
        }).collect()
    };
    let mut out = String::from("  shader variants (x 8 should be white, x 40 green; constant: both green):");
    for (name, constant, body) in variants {
        device.push_error_scope(wgpu::ErrorFilter::Validation);
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(name), source: wgpu::ShaderSource::Wgsl(format!("{COMMON}{body}").into()) });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(name), layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState { module: &module, entry_point: Some("vs"), compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout { array_stride: 20, step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Uint32] }] },
            fragment: Some(wgpu::FragmentState { module: &module, entry_point: Some("fs"), compilation_options: Default::default(),
                targets: &[Some(wgpu::TextureFormat::Rgba8Unorm.into())] }),
            primitive: Default::default(), depth_stencil: None, multisample: Default::default(), multiview: None, cache: None,
        });
        let verts = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: 120,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        queue.write_buffer(&verts, 0, &quad(constant.then_some([0.6, 0.5])));
        let target = texture("variants target", w, h, wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC);
        let target_view = target.create_view(&Default::default());
        let readback = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: (256 * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor { label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &target_view, resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store } })],
                depth_stencil_attachment: None, timestamp_writes: None, occlusion_query_set: None });
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &group, &[]);
            pass.set_vertex_buffer(0, verts.slice(..));
            pass.draw(0..6, 0..1);
        }
        encoder.copy_texture_to_buffer(target.as_image_copy(), wgpu::TexelCopyBufferInfo { buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(256), rows_per_image: None } }, size(w, h));
        queue.submit([encoder.finish()]);
        if let Some(e) = crate::gpu::pollster_block_on(device.pop_error_scope()) { out += &format!("\n    {name}: error {e}"); continue; }
        readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        if device.poll(wgpu::PollType::Wait).is_err() { out += &format!("\n    {name}: readback failed"); continue; }
        let data = readback.slice(..).get_mapped_range();
        let px = |x: usize| { let i = 8 * 256 + x * 4; [data[i], data[i + 1], data[i + 2], data[i + 3]] };
        out += &format!("\n    {name}: {:?} {:?}", px(8), px(40));
    }
    out
}
