use std::{mem, sync::{Arc, Weak}, collections::BTreeMap};

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
#[derive(Clone, Copy, Debug)]
struct SlotMeta { end: u64, version: u64, drawn: u64 }

#[derive(Clone, Debug)]
pub struct InstanceRing {
    id: u64,
    now: u64,
    draw_epoch: u64,
    slots: Vec<NoteInstance>,
    /// End, version and last-drawn epoch of a slot side by side: one cache line per push.
    meta: Vec<SlotMeta>,
    order: Vec<u32>,
    /// Naturals collected during a single packing pass; `finish` appends them after the sharps.
    tail: Vec<u32>,
    tail_mode: bool,
    expiry: BTreeMap<u64, Vec<u32>>,
    expiry_records: usize,
    /// Slots created this frame and last: sizes the expiry budget.
    created: [usize; 2],
    free: Vec<u32>,
    revision: u64,
}

impl Default for InstanceRing {
    fn default() -> Self {
        static ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self { id: ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed), now: 0, draw_epoch: 0, slots: Vec::new(),
            meta: Vec::new(), order: Vec::new(), tail: Vec::new(), tail_mode: false,
            expiry: BTreeMap::new(), expiry_records: 0, created: [0; 2], free: Vec::new(), revision: 0 }
    }
}

const EXPIRY_SHIFT: u32 = 14;

impl InstanceRing {
    pub fn id(&self) -> u64 { self.id }
    /// 0 = the slot is free or expired. Truncated to 32 bits (never 0): a reference is
    /// checked within seconds of being made, far fewer than 2^32 allocations later.
    pub fn version(&self, slot: u32) -> u32 {
        match self.meta.get(slot as usize) {
            Some(m) if m.end > self.now => (m.version as u32).max(1),
            _ => 0,
        }
    }
    pub fn len(&self) -> usize { self.order.len() }
    pub fn is_empty(&self) -> bool { self.order.is_empty() }
    pub fn iter(&self) -> impl Iterator<Item = &NoteInstance> {
        self.order.iter().map(|&slot| &self.slots[slot as usize])
    }
    pub fn begin(&mut self, now: u64) {
        self.order.clear();
        self.tail.clear();
        self.tail_mode = false;
        self.now = now;
        self.draw_epoch = self.draw_epoch.wrapping_add(1);
        // Reclaim as fast as slots are made: a fixed 4096 let the backlog (and the hash
        // tables holding it) grow to millions at high note density, rehashing for 100+ ms.
        self.created = [0, self.created[0]];
        let mut budget = 4096.max(self.created[1].saturating_mul(2));
        // Expiry is bucketed (~16 ms): one Vec per distinct end time made every new
        // instance allocate. A bucket is reclaimed once all of it has passed.
        while budget > 0 {
            let Some(mut entry) = self.expiry.first_entry() else { break };
            let bucket_end = (*entry.key() + 1) << EXPIRY_SHIFT;
            if bucket_end > now { break; }
            let slot = entry.get_mut().pop().unwrap();
            if entry.get().is_empty() { entry.remove(); }
            self.expiry_records -= 1;
            budget -= 1;
            let m = self.meta[slot as usize];
            // 0 = already freed; later than this bucket = extended, a newer record covers it.
            if m.end == 0 || m.end > bucket_end { continue; }
            self.meta[slot as usize].end = 0;
            self.free.push(slot);
        }
        if self.expiry_records > (self.slots.len() - self.free.len()).max(4096) * 2 {
            let meta = &self.meta;
            let mut records = 0;
            self.expiry.retain(|&bucket, slots| {
                slots.retain(|&slot| { let end = meta[slot as usize].end; end != 0 && (end >> EXPIRY_SHIFT) == bucket });
                records += slots.len();
                !slots.is_empty()
            });
            self.expiry_records = records;
        }
    }
    /// Route following pushes to the naturals list (or back to the main one).
    pub fn route_naturals(&mut self, on: bool) { self.tail_mode = on; }
    /// End a single-pass pack: naturals go behind the sharps.
    pub fn finish(&mut self) { self.order.append(&mut self.tail); self.tail_mode = false; }
    /// A note stopped using the slot it saw at `version`: free it unless it was replaced.
    pub fn release(&mut self, slot: u32, version: u32) {
        if self.meta.get(slot as usize).is_some_and(|m| m.end != 0 && (m.version as u32).max(1) == version) {
            self.retire(slot);
        }
    }
    pub fn retire(&mut self, slot: u32) {
        if self.meta.get(slot as usize).is_none_or(|m| m.end == 0) { return; }
        self.meta[slot as usize].end = 0;
        self.free.push(slot);
    }
    /// Every push takes a fresh slot. Identical rectangles are not shared: row selection
    /// already drops a rectangle another one fully covers, so duplicates never get here.
    pub fn push(&mut self, note: NoteInstance, expiry: u64) -> u32 {
        static VERSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        self.revision = VERSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let meta = SlotMeta { end: expiry, version: self.revision, drawn: u64::MAX };
        let slot = if let Some(slot) = self.free.pop() {
            self.slots[slot as usize] = note;
            self.meta[slot as usize] = meta;
            slot
        } else {
            self.slots.push(note);
            self.meta.push(meta);
            self.slots.len() as u32 - 1
        };
        self.created[0] += 1;
        self.expiry.entry(expiry >> EXPIRY_SHIFT).or_default().push(slot);
        self.expiry_records += 1;
        self.push_slot(slot);
        slot
    }
    pub fn push_slot(&mut self, slot: u32) {
        let m = &mut self.meta[slot as usize];
        if m.end > self.now && m.drawn != self.draw_epoch {
            if self.tail_mode { self.tail.push(slot); } else { self.order.push(slot); }
            m.drawn = self.draw_epoch;
        }
    }
    pub fn cap(&mut self, max: usize, _now: u64, piano: bool) {
        if max == 0 || self.order.len() <= max { return; }
        let want = self.order.len() - max;
        // Synthesia-style: shed sharps from the top of the screen first, then naturals.
        // Within each piano pass the draw list is newest-first, so a forward walk visits
        // the furthest-future start first. One walk collects the first `want` of each class
        // and stops once the sharps alone cover it. Nothing is remembered: the choice is
        // remade every frame, so a note shed at the top comes back as it scrolls down.
        let mut found: [Vec<usize>; 2] = Default::default();
        for pos in 0..self.order.len() {
            let note = &self.slots[self.order[pos] as usize];
            let class = !(piano && crate::is_black_key((note.key_color & 255) as u8)) as usize;
            if found[class].len() < want {
                found[class].push(pos);
                if class == 0 && found[0].len() == want { break; }
            }
        }
        let mut left = want;
        for class in found {
            for pos in class.into_iter().take(left) {
                self.meta[self.order[pos] as usize].drawn = u64::MAX; // marks it shed
                left -= 1;
            }
            if left == 0 { break; }
        }
        let (meta, epoch) = (&self.meta, self.draw_epoch);
        self.order.retain(|&slot| meta[slot as usize].drawn == epoch);
    }
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
        let limit = if self.relative { device.limits().max_buffer_size }
            else { device.limits().max_buffer_size.min(device.limits().max_storage_buffer_binding_size as u64) };
        let max = (limit / mem::size_of::<NoteInstance>() as u64).min(u32::MAX as u64) as usize;
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
                // Coalesce changed slots into few writes: reused slots are scattered, so runs
                // separated by up to 64 unchanged slots go out together (the gap is rewritten
                // with the data already there).
                self.versions.resize(needed, 0);
                let mut i = 0;
                while i < needed {
                    if self.versions[i] == data.meta[i].version { i += 1; continue; }
                    let first = i;
                    let mut end;
                    loop {
                        while i < needed && self.versions[i] != data.meta[i].version {
                            self.versions[i] = data.meta[i].version;
                            i += 1;
                        }
                        end = i;
                        let next = (i..needed.min(i + 64)).find(|&j| self.versions[j] != data.meta[j].version);
                        match next { Some(j) => i = j, None => break }
                    }
                    queue.write_buffer(&self.instance_buffer, (first * mem::size_of::<NoteInstance>()) as u64,
                        bytemuck::cast_slice(&data.slots[first..end]));
                }
                let order: Vec<_> = data.order.iter().copied().filter(|&slot| (slot as usize) < needed).collect();
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
        const B: u64 = 1 << EXPIRY_SHIFT; // expiry is reclaimed per bucket
        let a = NoteInstance { start_us: [10, 0], len: 1.0, key_color: 60 };
        let b = NoteInstance { start_us: [20, 0], len: 2.0, key_color: 61 };
        let mut ring = InstanceRing::default();
        ring.begin(0);
        let slot_a = ring.push(a, 100 * B);
        let slot_b = ring.push(b, 200 * B);
        let version = ring.meta[slot_b as usize].version;
        ring.begin(50 * B);
        ring.push_slot(slot_b);
        ring.push_slot(slot_a);
        assert_eq!(ring.len(), 2);
        assert_eq!(ring.meta[slot_b as usize].version, version);
        ring.begin(101 * B);
        let c = NoteInstance { start_us: [30, 0], ..a };
        assert_eq!(ring.push(c, 300 * B), slot_a);
        assert_eq!(ring.slots.len(), 2);
        assert_ne!(ring.meta[slot_a as usize].version, version);
        assert_eq!(ring.iter().copied().collect::<Vec<_>>(), vec![c]);
    }

    #[test]
    fn cap_sheds_sharps_then_naturals_newest_first_and_is_not_sticky() {
        let n = |start: u32, pitch| NoteInstance { start_us: [start, 0], len: 1.0, key_color: pitch };
        // Newest-first draw list: sharps pass, then naturals pass.
        let notes = [n(30, 61), n(10, 61), n(40, 60), n(20, 60), n(0, 62)];
        let mut ring = InstanceRing::from_ordered(notes);
        ring.cap(4, 100, true);
        assert_eq!(ring.iter().copied().collect::<Vec<_>>(), vec![n(10, 61), n(40, 60), n(20, 60), n(0, 62)]);
        let mut ring = InstanceRing::from_ordered(notes);
        ring.cap(2, 100, true);
        assert_eq!(ring.iter().copied().collect::<Vec<_>>(), vec![n(20, 60), n(0, 62)]);
        let original = ring.clone();
        ring.cap(0, 100, true);
        assert_eq!(ring, original);
        // A fresh frame with the same notes shows them all again.
        ring.begin(100);
        for slot in 0..5 { ring.push_slot(slot); }
        assert_eq!(ring.len(), 5);
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

