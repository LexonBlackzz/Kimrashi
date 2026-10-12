use std::{mem, sync::{Arc, Weak}, collections::{HashMap, BinaryHeap}, cmp::Reverse};
use std::time::{Duration, Instant};

use bytemuck::{Pod, Zeroable};
use eframe::{egui, wgpu};
use egui::epaint::{PaintCallbackInfo, ViewportInPixels};
use egui_wgpu::{Callback, CallbackResources, CallbackTrait, ScreenDescriptor};

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq)]
pub struct NoteInstance {
    /// Absolute song microseconds, split so scrolling loses no f32 precision.
    pub start_us: [u32; 2],
    pub len: f32,
    pub key_color: u32,
}

/// Stable instance slots plus a small front-to-back draw list. Slots survive
/// pixel-selection changes and are reused only after their note expires.
#[derive(Clone, Default, Debug)]
pub struct InstanceRing {
    slots: Vec<NoteInstance>,
    versions: Vec<u64>,
    order: Vec<u32>,
    lookup: HashMap<[u32; 4], u32>,
    expiry: BinaryHeap<Reverse<(u64, u32)>>,
    free: Vec<u32>,
    revision: u64,
}

impl InstanceRing {
    pub fn len(&self) -> usize { self.order.len() }
    pub fn is_empty(&self) -> bool { self.order.is_empty() }
    pub fn iter(&self) -> impl Iterator<Item = &NoteInstance> {
        self.order.iter().map(|&slot| &self.slots[slot as usize])
    }
    fn key(note: &NoteInstance) -> [u32; 4] {
        [note.start_us[0], note.start_us[1], note.len.to_bits(), note.key_color]
    }
    pub fn begin(&mut self, now: u64) {
        self.order.clear();
        while self.expiry.peek().is_some_and(|x| x.0.0 <= now) {
            let Reverse((_, slot)) = self.expiry.pop().unwrap();
            self.lookup.remove(&Self::key(&self.slots[slot as usize]));
            self.free.push(slot);
        }
    }
    pub fn push(&mut self, note: NoteInstance, expiry: u64) -> u32 {
        let key = Self::key(&note);
        let slot = if let Some(&slot) = self.lookup.get(&key) { slot } else {
            static VERSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            self.revision = VERSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let slot = if let Some(slot) = self.free.pop() {
                self.slots[slot as usize] = note;
                self.versions[slot as usize] = self.revision;
                slot
            } else {
                let slot = self.slots.len() as u32;
                self.slots.push(note);
                self.versions.push(self.revision);
                slot
            };
            self.lookup.insert(key, slot);
            self.expiry.push(Reverse((expiry, slot)));
            slot
        };
        self.order.push(slot);
        slot
    }
    pub fn push_slot(&mut self, slot: u32) { self.order.push(slot); }
    pub fn from_ordered(notes: impl IntoIterator<Item = NoteInstance>) -> Self {
        let mut ring = Self::default();
        for note in notes { ring.push(note, u64::MAX); }
        ring
    }
}

impl std::ops::Index<usize> for InstanceRing {
    type Output = NoteInstance;
    fn index(&self, i: usize) -> &Self::Output { &self.slots[self.order[i] as usize] }
}
impl PartialEq for InstanceRing {
    fn eq(&self, other: &Self) -> bool { self.iter().eq(other.iter()) }
}

#[repr(C, align(16))]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub struct KeyPosition {
    pub left: f32,
    pub right: f32,
    pub _pad0: f32,
    pub _pad1: f32,
}

#[repr(C, align(16))]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
struct GlobalsUniform {
    height_time: f32,
    win_width: f32,
    win_height: f32,
    direction_sign: f32,
    flat_mode: u32,
    /// Instances drawn this frame (for per-instance depth).
    instance_count: u32,
    now_us: [u32; 2],
}

/// Depth buffer bits requested from eframe (see `NativeOptions::depth_buffer`), and
/// the matching format. Notes use depth so covered pixels are skipped (early-z).
pub const DEPTH_BITS: u8 = 32;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

pub struct NotesCallback {
    viewport: egui::Rect,
    target_format: wgpu::TextureFormat,
    instances: Arc<InstanceRing>,
    key_positions: [KeyPosition; 256],
    height_time: f32,
    now_us: u64,
    direction_sign: f32,
    flat_notes: bool,
}

impl NotesCallback {
    pub fn new(
        viewport: egui::Rect,
        target_format: wgpu::TextureFormat,
        instances: Arc<InstanceRing>,
        key_positions: [KeyPosition; 256],
        height_time: f32,
        now_us: u64,
        direction_sign: f32,
        flat_notes: bool,
    ) -> egui::PaintCallback {
        Callback::new_paint_callback(
            viewport,
            Self {
                viewport,
                target_format,
                instances,
                key_positions,
                height_time: height_time.max(1e-6),
                now_us,
                direction_sign,
                flat_notes,
            },
        )
    }
}

impl CallbackTrait for NotesCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen: &ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let viewport_px = ViewportInPixels::from_points(
            &self.viewport,
            screen.pixels_per_point,
            screen.size_in_pixels,
        );

        let win_width = viewport_px.width_px.max(1) as f32;
        let win_height = viewport_px.height_px.max(1) as f32;

        let globals = GlobalsUniform {
            height_time: self.height_time,
            win_width,
            win_height,
            direction_sign: self.direction_sign,
            flat_mode: if self.flat_notes { 1 } else { 0 },
            instance_count: 0, // set in `upload`
            now_us: [self.now_us as u32, (self.now_us >> 32) as u32],
        };

        let pipeline = resources
            .entry::<NotePipeline>()
            .or_insert_with(|| NotePipeline::new(device, self.target_format));

        pipeline.ensure_format(device, self.target_format);
        pipeline.upload(device, queue, &self.instances, &self.key_positions, globals);

        Vec::new()
    }

    fn paint(
        &self,
        _info: PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        resources: &CallbackResources,
    ) {
        let Some(pipeline) = resources.get::<NotePipeline>() else {
            return;
        };
        pipeline.draw(render_pass);
    }
}

struct NotePipeline {
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    instance_buffer: wgpu::Buffer,
    instance_capacity: usize,
    /// Instances uploaded this frame (may be fewer than requested, see `upload`).
    instance_count: u32,
    uploaded: Option<Weak<InstanceRing>>,
    versions: Vec<u64>,
    order: Vec<u32>,
    order_buffer: wgpu::Buffer,
    order_capacity: usize,
    relative: bool,
    key_buffer: wgpu::Buffer,
    globals_buffer: wgpu::Buffer,
    target_format: wgpu::TextureFormat,
}

impl NotePipeline {
    fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        Self::with_shader(device, target_format, include_str!("../shaders/notes.wgsl"), false)
    }

    fn with_shader(device: &wgpu::Device, target_format: wgpu::TextureFormat, source: &str, relative: bool) -> Self {
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("notes.bind_group_layout"),
            entries: &{ let mut entries = vec![
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ];
            if !relative { entries.push(wgpu::BindGroupLayoutEntry {
                binding: 2, visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false, min_binding_size: None }, count: None,
            }); }
            entries
            },
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("notes.shader"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(source)),
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("notes.pipeline_layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("notes.pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_notes"),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: if relative { mem::size_of::<NoteInstance>() as u64 } else { 4 },
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &if relative { vec![
                        wgpu::VertexAttribute { offset: 0, shader_location: 0, format: wgpu::VertexFormat::Float32x2 },
                        wgpu::VertexAttribute { offset: 8, shader_location: 1, format: wgpu::VertexFormat::Uint32 },
                        wgpu::VertexAttribute { offset: 12, shader_location: 2, format: wgpu::VertexFormat::Uint32 },
                    ] } else { vec![
                        wgpu::VertexAttribute { offset: 0, shader_location: 0, format: wgpu::VertexFormat::Uint32 },
                    ] },
                }],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_notes"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: None, // notes are opaque
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            // Instances come front to back (newest first) with increasing depth, so the
            // GPU rejects covered pixels before shading them. Dense, overlapping notes
            // would otherwise be filled many times over.
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: true,
                depth_compare: wgpu::CompareFunction::Less,
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("notes.instances"),
            size: 256,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let order_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("notes.order"), size: 256,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false,
        });

        let key_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("notes.keys"),
            size: (std::mem::size_of::<KeyPosition>() * 256) as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let globals_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("notes.globals"),
            size: std::mem::size_of::<GlobalsUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("notes.bind_group"),
            layout: &bind_group_layout,
            entries: &{ let mut entries = vec![
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: key_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: globals_buffer.as_entire_binding(),
                },
            ];
            if !relative { entries.push(wgpu::BindGroupEntry { binding: 2, resource: instance_buffer.as_entire_binding() }); }
            entries
            },
        });

        Self {
            pipeline,
            bind_group,
            instance_buffer,
            instance_capacity: 16,
            instance_count: 0,
            uploaded: None,
            versions: Vec::new(), order: Vec::new(), order_buffer, order_capacity: 64, relative,
            key_buffer,
            globals_buffer,
            target_format,
        }
    }

    fn ensure_format(&mut self, device: &wgpu::Device, format: wgpu::TextureFormat) {
        if self.target_format == format {
            return;
        }
        *self = Self::new(device, format);
    }

    fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        data: &Arc<InstanceRing>,
        keys: &[KeyPosition; 256],
        mut globals: GlobalsUniform,
    ) {
        let max = (device.limits().max_buffer_size.min(device.limits().max_storage_buffer_binding_size as u64)
            / mem::size_of::<NoteInstance>() as u64).min(u32::MAX as u64) as usize;
        let needed = if self.relative { data.len() } else { data.slots.len() }.min(max);
        if needed > self.instance_capacity { self.grow_instances(device, needed, max); }
        if data.len() > self.order_capacity {
            self.order_capacity = data.len().next_power_of_two();
            self.order_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("notes.order.grow"), size: (self.order_capacity * 4) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false,
            });
            self.order.clear();
        }
        let source = Arc::downgrade(data);
        if !self.uploaded.as_ref().is_some_and(|old| Weak::ptr_eq(old, &source)) {
            if self.relative {
                let notes: Vec<_> = data.iter().take(max).copied().collect();
                queue.write_buffer(&self.instance_buffer, 0, bytemuck::cast_slice(&notes));
                self.instance_count = notes.len() as u32;
            } else {
                // Coalesce adjacent newly allocated/reused slots into one write.
                self.versions.resize(needed, 0);
                let mut i = 0;
                while i < needed {
                    if self.versions[i] == data.versions[i] { i += 1; continue; }
                    let first = i;
                    while i < needed && self.versions[i] != data.versions[i] {
                        self.versions[i] = data.versions[i];
                        i += 1;
                    }
                    queue.write_buffer(&self.instance_buffer, (first * mem::size_of::<NoteInstance>()) as u64,
                        bytemuck::cast_slice(&data.slots[first..i]));
                }
                let order: Vec<_> = data.order.iter().copied().filter(|&slot| (slot as usize) < max).collect();
                if self.order != order {
                    let first = self.order.iter().zip(&order).take_while(|(a,b)| a == b).count();
                    if first < order.len() {
                        queue.write_buffer(&self.order_buffer, (first * 4) as u64, bytemuck::cast_slice(&order[first..]));
                    }
                    self.order = order;
                }
                self.instance_count = self.order.len() as u32;
            }
            self.uploaded = Some(source);
        }
        globals.instance_count = self.instance_count;
        queue.write_buffer(&self.key_buffer, 0, bytemuck::cast_slice(&keys[..]));
        queue.write_buffer(&self.globals_buffer, 0, bytemuck::bytes_of(&globals));
    }

    fn grow_instances(&mut self, device: &wgpu::Device, needed: usize, max: usize) {
        let capacity = needed.next_power_of_two().min(max);
        let new_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("notes.instances.grow"),
            size: (capacity * std::mem::size_of::<NoteInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.instance_buffer = new_buffer;
        self.instance_capacity = capacity;
        self.uploaded = None;
        self.versions.clear();
        if !self.relative {
            self.bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("notes.bind_group.grow"), layout: &self.pipeline.get_bind_group_layout(0),
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: self.key_buffer.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: self.globals_buffer.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 2, resource: self.instance_buffer.as_entire_binding() },
                ],
            });
        }
    }

    fn draw(&self, render_pass: &mut wgpu::RenderPass<'static>) {
        if self.instance_count == 0 {
            return;
        }
        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.bind_group, &[]);
        let slice = if self.relative { self.instance_buffer.slice(..) } else { self.order_buffer.slice(..) };
        let slice_static: wgpu::BufferSlice<'static> = unsafe { mem::transmute(slice) };
        render_pass.set_vertex_buffer(0, slice_static);
        render_pass.draw(0..6, 0..self.instance_count);
    }
}

/// Offscreen use of the real pipeline for `--benchmark-render`.
pub struct RenderBenchmark {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: NotePipeline,
    reference: NotePipeline,
    color_texture: wgpu::Texture,
    color: wgpu::TextureView,
    depth: wgpu::TextureView,
    queries: wgpu::QuerySet,
    resolved: wgpu::Buffer,
    readback: wgpu::Buffer,
    width: u32,
    height: u32,
}

impl RenderBenchmark {
    pub fn new(device: wgpu::Device, queue: wgpu::Queue, width: u32, height: u32) -> Self {
        let texture = |format, label, usage| device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage,
            view_formats: &[],
        });
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let color_texture = texture(format, "bench.color", wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC);
        let color = color_texture.create_view(&Default::default());
        let depth = texture(DEPTH_FORMAT, "bench.depth", wgpu::TextureUsages::RENDER_ATTACHMENT).create_view(&Default::default());
        let pipeline = NotePipeline::new(&device, format);
        // The previous shader/vertex layout, for an actual GPU pixel comparison.
        let mut source = include_str!("../shaders/notes-reference.wgsl").replace("\r\n", "\n").replace(
            "@location(0) start_us: vec2<u32>,\n    @location(1) length: f32,\n    @location(2) key_color: u32,",
            "@location(0) start_length: vec2<f32>,\n    @location(1) key_color: u32,\n    @location(2) border_width: u32,",
        );
        let first = source.find("    // Subtract integer microseconds before").unwrap();
        let end = source.find("    var start_clip").unwrap();
        source.replace_range(first..end,
            "    let start_norm = clamp(input.start_length.x / safe_height, 0.0, 2.0);\n    let end_norm = clamp((input.start_length.x + input.start_length.y) / safe_height, 0.0, 2.0);\n\n");
        source = source.replace("out.border_width = 1.0;", "out.border_width = f32(input.border_width);");
        let reference = NotePipeline::with_shader(&device, format, &source, true);
        let queries = device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("bench.timestamps"), ty: wgpu::QueryType::Timestamp, count: 2,
        });
        let buffer = |usage, label| device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label), size: 16, usage, mapped_at_creation: false,
        });
        let resolved = buffer(wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC, "bench.resolved");
        let readback = buffer(wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST, "bench.readback");
        Self { device, queue, pipeline, reference, color_texture, color, depth, queries, resolved, readback, width, height }
    }

    /// CPU upload time and GPU vertex/fragment time (timestamp queries, excluding readback).
    pub fn frame(&mut self, instances: &Arc<InstanceRing>, keys: &[KeyPosition; 256], height_time: f32, now_us: u64) -> (Duration, Duration) {
        let t0 = Instant::now();
        self.pipeline.upload(&self.device, &self.queue, instances, keys, GlobalsUniform {
            height_time, win_width: self.width as f32, win_height: self.height as f32,
            direction_sign: 1.0, flat_mode: 0, instance_count: 0,
            now_us: [now_us as u32, (now_us >> 32) as u32],
        });
        let upload = t0.elapsed();
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("bench.notes"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.color, resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Store }),
                    stencil_ops: None,
                }),
                timestamp_writes: Some(wgpu::RenderPassTimestampWrites {
                    query_set: &self.queries, beginning_of_pass_write_index: Some(0), end_of_pass_write_index: Some(1),
                }),
                occlusion_query_set: None,
            }).forget_lifetime();
            self.pipeline.draw(&mut pass);
        }
        encoder.resolve_query_set(&self.queries, 0..2, &self.resolved, 0);
        encoder.copy_buffer_to_buffer(&self.resolved, 0, &self.readback, 0, 16);
        self.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        self.readback.slice(..).map_async(wgpu::MapMode::Read, move |r| { let _ = tx.send(r); });
        self.device.poll(wgpu::PollType::Wait).unwrap();
        rx.recv().unwrap().unwrap();
        let data = self.readback.slice(..).get_mapped_range();
        let times: &[u64] = bytemuck::cast_slice(&data);
        let ns = (times[1] - times[0]) as f64 * self.queue.get_timestamp_period() as f64;
        drop(data);
        self.readback.unmap();
        (upload, Duration::from_secs_f64(ns * 1e-9))
    }

    pub fn reference_frame(&mut self, instances: &Arc<InstanceRing>, keys: &[KeyPosition; 256], height_time: f32, now_us: u64) {
        mem::swap(&mut self.pipeline, &mut self.reference);
        self.frame(instances, keys, height_time, now_us);
        mem::swap(&mut self.pipeline, &mut self.reference);
    }

    pub fn pixels(&self) -> Vec<u8> {
        let stride = (self.width * 4).div_ceil(256) * 256;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("bench.pixels"), size: stride as u64 * self.height as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false,
        });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &self.color_texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo { buffer: &buffer, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(stride), rows_per_image: None } },
            wgpu::Extent3d { width: self.width, height: self.height, depth_or_array_layers: 1 },
        );
        self.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| { let _ = tx.send(r); });
        self.device.poll(wgpu::PollType::Wait).unwrap();
        rx.recv().unwrap().unwrap();
        let data = buffer.slice(..).get_mapped_range();
        data.chunks(stride as usize).flat_map(|row| row[..self.width as usize * 4].iter().copied()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn instance_slots_survive_selection_changes_and_reuse_expired_space() {
        let a = NoteInstance { start_us: [10, 0], len: 1.0, key_color: 60 };
        let b = NoteInstance { start_us: [20, 0], len: 2.0, key_color: 61 };
        let mut ring = InstanceRing::default();
        ring.begin(0);
        let slot_a = ring.push(a, 100);
        let slot_b = ring.push(b, 200);
        let version = ring.versions[slot_b as usize];
        ring.begin(50);
        assert_eq!(ring.push(b, 200), slot_b);
        assert_eq!(ring.versions[slot_b as usize], version);
        assert_eq!(ring.push(a, 100), slot_a);
        ring.begin(100);
        let c = NoteInstance { start_us: [30, 0], ..a };
        assert_eq!(ring.push(c, 300), slot_a);
        assert_eq!(ring.slots.len(), 2);
        assert_ne!(ring.versions[slot_a as usize], version);
        assert_eq!(ring.iter().copied().collect::<Vec<_>>(), vec![c]);
    }

    #[test]
    fn note_shader_is_valid() {
        use egui_wgpu::wgpu::naga;
        let module = naga::front::wgsl::parse_str(include_str!("../shaders/notes.wgsl")).unwrap();
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::empty())
            .validate(&module)
            .unwrap();
    }
}

