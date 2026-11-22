use std::{mem, sync::{Arc, OnceLock, Weak, atomic::{AtomicU64, Ordering}}, collections::BTreeMap};

use std::time::{Duration, Instant};

static PREPARE_NS: AtomicU64 = AtomicU64::new(0);
static PAINT_NS: AtomicU64 = AtomicU64::new(0);
/// Instance-slot diff + writes (ns), draw-order build + diff + write (ns), `write_buffer` calls, bytes.
static UPLOAD: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// What `upload` did since the last `take_upload`, for `KIM_FRAMELOG`.
#[derive(Default, Clone, Copy)]
pub struct UploadStats { pub slots: Duration, pub order: Duration, pub writes: u64, pub bytes: u64 }

pub fn take_upload() -> UploadStats {
    let take = |i: usize| UPLOAD[i].swap(0, Ordering::Relaxed);
    UploadStats { slots: Duration::from_nanos(take(0)), order: Duration::from_nanos(take(1)), writes: take(2), bytes: take(3) }
}

/// CPU time the notes callback spent in `prepare` (upload) since the last call, for `KIM_FRAMELOG`.
pub fn take_prepare() -> Duration { Duration::from_nanos(PREPARE_NS.swap(0, Ordering::Relaxed)) }
/// The same for `paint` (recording the draw call).
pub fn take_paint() -> Duration { Duration::from_nanos(PAINT_NS.swap(0, Ordering::Relaxed)) }

/// Debug switches for finding where a frame goes: `KIM_NO_DRAW` uploads the notes but does
/// not draw them; `KIM_NO_NOTES` leaves the notes callback out altogether.
pub fn no_draw() -> bool { static F: OnceLock<bool> = OnceLock::new(); *F.get_or_init(|| std::env::var_os("KIM_NO_DRAW").is_some()) }
pub fn no_notes() -> bool { static F: OnceLock<bool> = OnceLock::new(); *F.get_or_init(|| std::env::var_os("KIM_NO_NOTES").is_some()) }

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

impl NoteInstance {
    /// Pitch fills the low 7 bits of `key_color` (bit 7 = no start border), colour the top 24.
    /// A negative `len` means no end border (a piece of a longer note). The top 16 bits of
    /// the start's high word hold the whole note's length in seconds as a bfloat16, which the
    /// shader uses to darken thin notes; starts therefore must fit in 48 bits.
    pub fn new(note: &crate::NoteSegment, color: egui::Color32, whole_us: u64) -> Self {
        let len = ((note.end_us - note.start_us) as f32 / 1e6).max(0.000_1);
        let whole = (whole_us as f32 / 1e6).to_bits() >> 16;
        Self {
            start_us: [note.start_us as u32, ((note.start_us >> 32) as u32 & 0xFFFF) | whole << 16],
            // A cut end runs 2 us under the newer note that cut it (drawn in front), so a pixel
            // centre on the cut belongs to the newer note whatever the f32 rounding of the edge.
            len: if note.channel & crate::NO_END_BORDER != 0 { -(len + 2e-6) } else { len },
            key_color: note.pitch as u32 | if note.channel & crate::NO_START_BORDER != 0 { 0x80 } else { 0 }
                | ((color.r() as u32) << 24) | ((color.g() as u32) << 16) | ((color.b() as u32) << 8),
        }
    }
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
    /// Every slot pushed, oldest first; `pushed_first` pushes came before `pushed[0]`. A pipeline
    /// that remembers how many pushes it has uploaded visits only the newer ones instead of
    /// comparing every slot ever allocated (a table that never shrinks) on every frame.
    pushed: Vec<u32>,
    pushed_first: u64,
}

/// Pushes kept beyond one per slot before the oldest half is dropped.
const PUSH_HISTORY: usize = 1 << 16;

impl Default for InstanceRing {
    fn default() -> Self {
        static ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self { id: ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed), now: 0, draw_epoch: 0, slots: Vec::new(),
            meta: Vec::new(), order: Vec::new(), tail: Vec::new(), tail_mode: false,
            expiry: BTreeMap::new(), expiry_records: 0, created: [0; 2], free: Vec::new(), revision: 0,
            pushed: Vec::new(), pushed_first: 0 }
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
    /// How many pushes this ring has seen (see `pushed_since`).
    fn pushes(&self) -> u64 { self.pushed_first + self.pushed.len() as u64 }
    /// The slots pushed after the first `cursor` pushes, or `None` if that is older than the
    /// history kept (the caller then compares every slot).
    fn pushed_since(&self, cursor: u64) -> Option<&[u32]> {
        self.pushed.get(cursor.checked_sub(self.pushed_first)? as usize..)
    }
    pub fn begin(&mut self, now: u64) {
        if self.pushed.len() > self.slots.len() + PUSH_HISTORY {
            let cut = self.pushed.len() / 2;
            self.pushed.drain(..cut);
            self.pushed_first += cut as u64;
        }
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
        self.pushed.push(slot);
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
            let class = !(piano && crate::is_black_key((note.key_color & 0x7F) as u8)) as usize;
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
    /// Seconds added to every note's start: negative song time (export lead-in).
    time_shift: f32,
    horizontal: u32,
    playhead: f32,
    brightness: f32,
    depth_base: f32,
    depth_scale: f32,
    perspective_far: f32,
    perspective_near: f32,
    block_height: f32,
    _pad: [f32; 3],
}

/// How one layer of notes maps to the screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NoteView {
    /// Seconds the time axis spans.
    pub height_time: f32,
    /// Song time at the time axis origin.
    pub now_us: u64,
    /// Seconds added to every start (negative song time).
    pub time_shift: f32,
    /// Vertical: +1 rise, -1 fall. Horizontal: +1 origin at the right, -1 at the left.
    pub direction_sign: f32,
    /// 0 shaded, 1 flat, 2 Piano From Above.
    pub flat_mode: u32,
    /// The time axis runs across the screen, the keys up it (low pitch at the bottom).
    pub horizontal: bool,
    /// Fraction of the time axis where notes light up and the past fades; < 0 = none.
    pub playhead: f32,
    /// Colour multiplier, 1.0 = unchanged.
    pub brightness: f32,
    /// [base, scale]: this draw's depth range inside 0..1 (lower is nearer).
    pub depth: [f32; 2],
    /// Perspective (vertical only): the far edge's width as a fraction of the screen's; 0 = off.
    pub perspective_far: f32,
    /// Perspective: depth at the keyboard line (see `crate::Perspective`; below 1 counts as 1).
    pub perspective_near: f32,
    /// Blocks view: notes stand this high off the perspective plane (see `crate::Perspective::lift`); 0 = flat.
    pub block_height: f32,
}

impl NoteView {
    pub fn vertical(height_time: f32, now_us: u64, direction_sign: f32, flat_mode: u32) -> Self {
        Self { height_time, now_us, time_shift: 0.0, direction_sign, flat_mode, horizontal: false,
            playhead: -1.0, brightness: 1.0, depth: [0.0, 1.0], perspective_far: 0.0, perspective_near: 0.0, block_height: 0.0 }
    }

    fn globals(&self, win_width: f32, win_height: f32) -> GlobalsUniform {
        GlobalsUniform {
            height_time: self.height_time.max(1e-6), win_width, win_height, direction_sign: self.direction_sign,
            flat_mode: self.flat_mode, instance_count: 0, // set in `upload`
            now_us: [self.now_us as u32, (self.now_us >> 32) as u32], time_shift: self.time_shift,
            horizontal: self.horizontal as u32, playhead: self.playhead, brightness: self.brightness,
            depth_base: self.depth[0], depth_scale: self.depth[1], perspective_far: self.perspective_far, perspective_near: self.perspective_near,
            block_height: self.block_height, _pad: [0.0; 3],
        }
    }
}

/// One layer to draw: its notes and how they map to the screen.
#[derive(Clone)]
pub struct LayerDraw {
    pub instances: Arc<InstanceRing>,
    pub view: NoteView,
}

/// Depth buffer bits requested from eframe (see `NativeOptions::depth_buffer`), and
/// the matching format. Notes use depth so covered pixels are skipped (early-z).
pub const DEPTH_BITS: u8 = 32;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

pub struct NotesCallback {
    viewport: egui::Rect,
    target_format: wgpu::TextureFormat,
    /// Which of this frame's layers: each has its own GPU buffers.
    layer: usize,
    instances: Arc<InstanceRing>,
    key_positions: [KeyPosition; 256],
    view: NoteView,
}

impl NotesCallback {
    pub fn new(
        viewport: egui::Rect,
        target_format: wgpu::TextureFormat,
        layer: usize,
        instances: Arc<InstanceRing>,
        key_positions: [KeyPosition; 256],
        view: NoteView,
    ) -> egui::PaintCallback {
        Callback::new_paint_callback(viewport, Self { viewport, target_format, layer, instances, key_positions, view })
    }
}

/// One compiled pipeline's buffer sets, one per layer (see `NotesCallback::layer`).
struct NotePipelines(Vec<NotePipeline>);

impl CallbackTrait for NotesCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen: &ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let started = Instant::now();
        let viewport_px = ViewportInPixels::from_points(
            &self.viewport,
            screen.pixels_per_point,
            screen.size_in_pixels,
        );

        let win_width = viewport_px.width_px.max(1) as f32;
        let win_height = viewport_px.height_px.max(1) as f32;

        let globals = self.view.globals(win_width, win_height);

        let layers = &mut resources
            .entry::<NotePipelines>()
            .or_insert_with(|| NotePipelines(vec![NotePipeline::new(device, self.target_format)])).0;
        if layers[0].target_format != self.target_format {
            layers.clear();
            layers.push(NotePipeline::new(device, self.target_format));
        }
        while layers.len() <= self.layer { let next = layers[0].sibling(device); layers.push(next); }
        layers[self.layer].upload(device, queue, &self.instances, &self.key_positions, globals);

        PREPARE_NS.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        Vec::new()
    }

    fn paint(
        &self,
        _info: PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        resources: &CallbackResources,
    ) {
        let started = Instant::now();
        let Some(pipeline) = resources.get::<NotePipelines>().and_then(|p| p.0.get(self.layer)) else {
            return;
        };
        if !no_draw() {
            pipeline.draw(render_pass);
        }
        PAINT_NS.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
}

struct NotePipeline {
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    instance_buffer: wgpu::Buffer,
    instance_capacity: usize,
    /// Instances uploaded this frame (may be fewer than requested, see `upload`).
    instance_count: u32,
    /// Vertices per instance: 6 (a quad), or 30 for a block's pieces (see the note shader).
    vertices: u32,
    uploaded: Option<Weak<InstanceRing>>,
    versions: Vec<u64>,
    /// The ring (0 = none) and how many of its pushes `versions` is up to date through.
    ring_id: u64,
    cursor: u64,
    order: Vec<u32>,
    order_buffer: wgpu::Buffer,
    order_capacity: usize,
    relative: bool,
    key_buffer: wgpu::Buffer,
    globals_buffer: wgpu::Buffer,
    target_format: wgpu::TextureFormat,
}

/// The instance buffer's usage: storage only where the shader reads it as storage.
fn instance_usage(relative: bool) -> wgpu::BufferUsages {
    let usage = wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST;
    if relative { usage } else { usage | wgpu::BufferUsages::STORAGE }
}

/// How the note shader gets its notes.
#[derive(Clone, Copy, PartialEq, Debug)]
enum NoteInput {
    /// A storage buffer of slots, indexed through a per-instance slot vertex buffer (the default).
    Slots,
    /// Notes in draw order as per-instance vertex data: for GPUs without storage buffers in vertex
    /// shaders (e.g. Intel HD 4000, OpenGL 4.0 only on Windows). Re-uploads every note each change.
    Vertex,
    /// The previous shader's vertex layout (`RenderBenchmark`'s pixel comparison).
    Reference,
}

impl NotePipeline {
    /// The storage-buffer pipeline, or the vertex-input fallback where the device rejects it
    /// (or `KIM_VERTEX_NOTES` is set, to try the fallback anywhere).
    fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        if std::env::var_os("KIM_VERTEX_NOTES").is_none() {
            device.push_error_scope(wgpu::ErrorFilter::Validation);
            let pipeline = Self::with_input(device, target_format, NoteInput::Slots);
            match pollster_block_on(device.pop_error_scope()) {
                None => return pipeline,
                Some(e) => eprintln!("notes: storage buffers unsupported, using vertex input ({e})"),
            }
        }
        Self::with_input(device, target_format, NoteInput::Vertex)
    }

    fn with_input(device: &wgpu::Device, target_format: wgpu::TextureFormat, input: NoteInput) -> Self {
        let source = include_str!("../shaders/notes.wgsl").replace("\r\n", "\n");
        if input != NoteInput::Vertex { return Self::with_shader(device, target_format, &source, input); }
        let mut source = source;
        for (from, to) in [
            ("@group(0) @binding(2)\nvar<storage, read> instances: array<NoteInstance>;\n", ""),
            ("    @location(0) slot: u32,", "    @location(0) start_us: vec2<u32>,\n    @location(1) length: f32,\n    @location(2) key_color: u32,"),
            ("    let note = instances[input.slot];", "    let note = NoteInstance(input.start_us, input.length, input.key_color);"),
        ] {
            assert!(source.contains(from), "notes.wgsl changed: {from}");
            source = source.replace(from, to);
        }
        Self::with_shader(device, target_format, &source, NoteInput::Vertex)
    }

    fn with_shader(device: &wgpu::Device, target_format: wgpu::TextureFormat, source: &str, input: NoteInput) -> Self {
        let relative = input != NoteInput::Slots;
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
                    attributes: &match input {
                    NoteInput::Reference => vec![
                        wgpu::VertexAttribute { offset: 0, shader_location: 0, format: wgpu::VertexFormat::Float32x2 },
                        wgpu::VertexAttribute { offset: 8, shader_location: 1, format: wgpu::VertexFormat::Uint32 },
                        wgpu::VertexAttribute { offset: 12, shader_location: 2, format: wgpu::VertexFormat::Uint32 },
                    ],
                    NoteInput::Vertex => vec![
                        wgpu::VertexAttribute { offset: 0, shader_location: 0, format: wgpu::VertexFormat::Uint32x2 },
                        wgpu::VertexAttribute { offset: 8, shader_location: 1, format: wgpu::VertexFormat::Float32 },
                        wgpu::VertexAttribute { offset: 12, shader_location: 2, format: wgpu::VertexFormat::Uint32 },
                    ],
                    NoteInput::Slots => vec![
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
        Self::from_pipeline(device, pipeline, relative, target_format)
    }

    /// Another buffer set drawing with the same compiled pipeline (no shader compile).
    fn sibling(&self, device: &wgpu::Device) -> Self {
        Self::from_pipeline(device, self.pipeline.clone(), self.relative, self.target_format)
    }

    fn from_pipeline(device: &wgpu::Device, pipeline: wgpu::RenderPipeline, relative: bool, target_format: wgpu::TextureFormat) -> Self {
        let bind_group_layout = pipeline.get_bind_group_layout(0);
        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("notes.instances"),
            size: 256,
            usage: instance_usage(relative),
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
            vertices: 6,
            uploaded: None,
            versions: Vec::new(), ring_id: 0, cursor: 0, order: Vec::new(), order_buffer, order_capacity: 64, relative,
            key_buffer,
            globals_buffer,
            target_format,
        }
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
                let t_slots = Instant::now();
                // Only slots pushed since the last upload can differ, so visit just those; a new
                // ring, a regrown buffer or a gap in the history compares every slot instead.
                let pushed = if self.ring_id == data.id() && self.versions.len() <= needed { data.pushed_since(self.cursor) } else { None };
                self.versions.resize(needed, 0);
                let differs = |s: usize| self.versions[s] != data.meta[s].version;
                let mut changed: Vec<u32> = match pushed {
                    Some(pushed) => pushed.iter().copied().filter(|&s| (s as usize) < needed && differs(s as usize)).collect(),
                    None => (0..needed).filter(|&s| differs(s)).map(|s| s as u32).collect(),
                };
                changed.sort_unstable();
                changed.dedup();
                let mut k = 0;
                while k < changed.len() {
                    let first = changed[k] as usize;
                    let mut end = first + 1;
                    k += 1;
                    // Coalesce into few writes: reused slots are scattered, so runs separated by
                    // up to 64 unchanged slots go out together (the gap is rewritten with the
                    // data already there).
                    while k < changed.len() && (changed[k] as usize) < end + 64 { end = changed[k] as usize + 1; k += 1; }
                    for s in first..end { self.versions[s] = data.meta[s].version; }
                    queue.write_buffer(&self.instance_buffer, (first * mem::size_of::<NoteInstance>()) as u64,
                        bytemuck::cast_slice(&data.slots[first..end]));
                    UPLOAD[2].fetch_add(1, Ordering::Relaxed);
                    UPLOAD[3].fetch_add(((end - first) * mem::size_of::<NoteInstance>()) as u64, Ordering::Relaxed);
                }
                (self.ring_id, self.cursor) = (data.id(), data.pushes());
                UPLOAD[0].fetch_add(t_slots.elapsed().as_nanos() as u64, Ordering::Relaxed);
                let t_order = Instant::now();
                let order: Vec<_> = data.order.iter().copied().filter(|&slot| (slot as usize) < needed).collect();
                if self.order != order {
                    let first = self.order.iter().zip(&order).take_while(|(a,b)| a == b).count();
                    if first < order.len() {
                        queue.write_buffer(&self.order_buffer, (first * 4) as u64, bytemuck::cast_slice(&order[first..]));
                        UPLOAD[2].fetch_add(1, Ordering::Relaxed);
                        UPLOAD[3].fetch_add(((order.len() - first) * 4) as u64, Ordering::Relaxed);
                    }
                    self.order = order;
                }
                UPLOAD[1].fetch_add(t_order.elapsed().as_nanos() as u64, Ordering::Relaxed);
                self.instance_count = self.order.len() as u32;
            }
            self.uploaded = Some(source);
        }
        globals.instance_count = self.instance_count;
        self.vertices = if globals.block_height > 0.0 { 30 } else { 6 };
        queue.write_buffer(&self.key_buffer, 0, bytemuck::cast_slice(&keys[..]));
        queue.write_buffer(&self.globals_buffer, 0, bytemuck::bytes_of(&globals));
    }

    fn grow_instances(&mut self, device: &wgpu::Device, needed: usize, max: usize) {
        let capacity = needed.next_power_of_two().min(max);
        let new_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("notes.instances.grow"),
            size: (capacity * std::mem::size_of::<NoteInstance>()) as u64,
            usage: instance_usage(self.relative),
            mapped_at_creation: false,
        });
        self.instance_buffer = new_buffer;
        self.instance_capacity = capacity;
        self.uploaded = None;
        self.versions.clear();
        self.ring_id = 0;
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
        render_pass.draw(0..self.vertices, 0..self.instance_count);
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
        let reference = NotePipeline::with_shader(&device, format, &source, NoteInput::Reference);
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
        self.pipeline.upload(&self.device, &self.queue, instances, keys,
            NoteView::vertical(height_time, now_us, 1.0, 0).globals(self.width as f32, self.height as f32));
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

/// Offscreen notes for video export: the notes pipeline in the top `note_height` rows,
/// the rest left to the caller (keyboard), read back as RGBA.
pub struct Offscreen {
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// One buffer set per layer, sharing the first's compiled pipeline.
    layers: Vec<NotePipeline>,
    color_texture: wgpu::Texture,
    color: wgpu::TextureView,
    depth: wgpu::TextureView,
    readback: wgpu::Buffer,
    stride: u32,
    width: u32,
    height: u32,
}

impl Offscreen {
    pub fn new(width: u32, height: u32) -> Result<Self, String> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let adapter = pollster_block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .map_err(|e| format!("no GPU adapter: {e}"))?;
        let (device, queue) = pollster_block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            required_limits: adapter.limits(), ..Default::default()
        })).map_err(|e| format!("GPU device: {e}"))?;
        let texture = |format, label, usage| device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label), size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2, format, usage, view_formats: &[],
        });
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let color_texture = texture(format, "export.color", wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC);
        let color = color_texture.create_view(&Default::default());
        let depth = texture(DEPTH_FORMAT, "export.depth", wgpu::TextureUsages::RENDER_ATTACHMENT).create_view(&Default::default());
        let pipeline = NotePipeline::new(&device, format);
        let stride = (width * 4).div_ceil(256) * 256;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("export.readback"), size: stride as u64 * height as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false,
        });
        Ok(Self { device, queue, layers: vec![pipeline], color_texture, color, depth, readback, stride, width, height })
    }

    /// Draws `layers` in order into `viewport` (x, y, width, height in pixels) and copies the
    /// whole target into `out` (width * height * 4 bytes).
    pub fn render(&mut self, layers: &[LayerDraw], keys: &[KeyPosition; 256], viewport: [u32; 4],
        background: [u8; 3], out: &mut Vec<u8>)
    {
        while self.layers.len() < layers.len() { let next = self.layers[0].sibling(&self.device); self.layers.push(next); }
        let [vx, vy, vw, vh] = viewport.map(|v| v as f32);
        for (pipeline, layer) in self.layers.iter_mut().zip(layers) {
            pipeline.upload(&self.device, &self.queue, &layer.instances, keys, layer.view.globals(vw, vh));
        }
        let bg = background.map(|c| c as f64 / 255.0);
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("export.notes"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.color, resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color { r: bg[0], g: bg[1], b: bg[2], a: 1.0 }), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Store }),
                    stencil_ops: None,
                }),
                timestamp_writes: None, occlusion_query_set: None,
            }).forget_lifetime();
            pass.set_viewport(vx, vy, vw, vh, 0.0, 1.0);
            for pipeline in &self.layers[..layers.len()] { pipeline.draw(&mut pass); }
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &self.color_texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo { buffer: &self.readback, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(self.stride), rows_per_image: None } },
            wgpu::Extent3d { width: self.width, height: self.height, depth_or_array_layers: 1 },
        );
        self.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        self.readback.slice(..).map_async(wgpu::MapMode::Read, move |r| { let _ = tx.send(r); });
        self.device.poll(wgpu::PollType::Wait).unwrap();
        rx.recv().unwrap().unwrap();
        let row = self.width as usize * 4;
        out.clear();
        {
            let data = self.readback.slice(..).get_mapped_range();
            for rows in data.chunks(self.stride as usize).take(self.height as usize) { out.extend_from_slice(&rows[..row]); }
        }
        self.readback.unmap();
    }
}

fn pollster_block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    use std::{sync::Arc, task::{Context, Poll, Wake, Waker}};
    struct Unpark(std::thread::Thread);
    impl Wake for Unpark { fn wake(self: Arc<Self>) { self.0.unpark(); } }
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
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
    fn pushed_since_lists_only_newer_slots_and_none_once_trimmed() {
        let n = NoteInstance { start_us: [0, 0], len: 1.0, key_color: 60 };
        let (mut ring, end) = (InstanceRing::default(), 1u64 << 40);
        ring.begin(0);
        let a = ring.push(n, end);
        let cursor = ring.pushes();
        let (b, c) = (ring.push(n, end), ring.push(n, end));
        assert_eq!(ring.pushed_since(cursor), Some(&[b, c][..]));
        assert_eq!(ring.pushed_since(0), Some(&[a, b, c][..]));
        assert_eq!(ring.pushed_since(ring.pushes()), Some(&[][..]));
        // Reusing three slots over and over keeps the table small but the history long: trimmed.
        for _ in 0..3 * PUSH_HISTORY { let s = ring.push(n, end); ring.retire(s); }
        let total = ring.pushes();
        ring.begin(1);
        assert_eq!(ring.pushes(), total, "trimming drops old pushes, it does not forget how many there were");
        assert_eq!(ring.pushed_since(0), None);
        assert_eq!(ring.pushed_since(total), Some(&[][..]));
    }

    const NOW: u64 = 1_000_000;

    /// One pitch-60 note, `offset` seconds after `NOW`, `len` seconds long, flat colour.
    fn one_note(offset: f32, len: f32, [r, g, b]: [u8; 3]) -> Arc<InstanceRing> {
        let start = NOW + (offset * 1e6) as u64;
        Arc::new(InstanceRing::from_ordered([NoteInstance { start_us: [start as u32, 0], len,
            key_color: 60 | (r as u32) << 24 | (g as u32) << 16 | (b as u32) << 8 }]))
    }

    fn keys() -> [KeyPosition; 256] {
        let mut keys = [KeyPosition { left: 0.0, right: 0.0, _pad0: 0.0, _pad1: 0.0 }; 256];
        (keys[60].left, keys[60].right) = (0.25, 0.5);
        keys
    }

    fn draw(gpu: &mut Offscreen, layers: &[LayerDraw]) -> impl Fn(u32, u32) -> [u8; 3] {
        let mut out = Vec::new();
        gpu.render(layers, &keys(), [0, 0, 64, 64], [0, 0, 0], &mut out);
        move |x, y| { let i = (y as usize * 64 + x as usize) * 4; [out[i], out[i + 1], out[i + 2]] }
    }

    #[test]
    fn horizontal_mode_turns_the_vertical_picture_sideways() {
        let Ok(mut gpu) = Offscreen::new(64, 64) else { eprintln!("no GPU, skipped"); return };
        let instances = one_note(0.0, 0.5, [255, 0, 0]);
        let view = NoteView::vertical(1.0, NOW, -1.0, 1);
        let px = draw(&mut gpu, &[LayerDraw { instances: instances.clone(), view }]);
        assert_eq!((px(24, 48), px(24, 8)), ([255, 0, 0], [0, 0, 0]));
        let px = draw(&mut gpu, &[LayerDraw { instances, view: NoteView { horizontal: true, ..view } }]);
        assert_eq!((px(8, 40), px(56, 40), px(24, 48)), ([255, 0, 0], [0, 0, 0], [0, 0, 0]));
    }

    #[test]
    fn playhead_lights_crossing_notes_and_fades_the_past() {
        let Ok(mut gpu) = Offscreen::new(64, 64) else { eprintln!("no GPU, skipped"); return };
        let view = NoteView { horizontal: true, playhead: 0.5, ..NoteView::vertical(1.0, NOW, -1.0, 1) };
        // Time norm n is at x = 64 n (origin on the left); the key spans y 32..48.
        let crossing = draw(&mut gpu, &[LayerDraw { instances: one_note(0.25, 0.5, [255, 0, 0]), view }]);
        let future = draw(&mut gpu, &[LayerDraw { instances: one_note(0.6, 0.3, [255, 0, 0]), view }]);
        assert!(crossing(36, 40)[1] > 60, "a note on the playhead is lit: {:?}", crossing(36, 40));
        assert_eq!(future(50, 40), [255, 0, 0]);
        let past = draw(&mut gpu, &[LayerDraw { instances: one_note(0.1, 0.3, [255, 0, 0]), view }]);
        assert!(past(9, 40)[0] + 40 < past(22, 40)[0], "older is darker: {:?} {:?}", past(9, 40), past(22, 40));
    }

    #[test]
    fn brightness_and_depth_ranges_order_layers() {
        let Ok(mut gpu) = Offscreen::new(64, 64) else { eprintln!("no GPU, skipped"); return };
        let view = NoteView::vertical(1.0, NOW, -1.0, 1);
        let red = LayerDraw { instances: one_note(0.0, 0.5, [255, 0, 0]), view: NoteView { depth: [0.0, 0.5], ..view } };
        let green = LayerDraw { instances: one_note(0.0, 0.5, [0, 255, 0]), view: NoteView { depth: [0.5, 0.5], brightness: 0.5, ..view } };
        let px = draw(&mut gpu, &[red.clone(), green.clone()]);
        assert_eq!(px(24, 48), [255, 0, 0]);
        let red = LayerDraw { view: NoteView { depth: [0.5, 0.5], ..red.view }, ..red };
        let green = LayerDraw { view: NoteView { depth: [0.0, 0.5], ..green.view }, ..green };
        let [r, g, b] = draw(&mut gpu, &[red, green])(24, 48);
        assert!(r == 0 && b == 0 && (100..=140).contains(&g), "dim green wins: {r} {g} {b}");
    }

    fn persp() -> NoteView { NoteView { perspective_far: 0.3, ..NoteView::vertical(1.0, NOW, -1.0, 1) } }

    #[test]
    fn perspective_keeps_near_notes_and_squeezes_far_ones() {
        let Ok(mut gpu) = Offscreen::new(64, 64) else { eprintln!("no GPU, skipped"); return };
        let near = draw(&mut gpu, &[LayerDraw { instances: one_note(0.0, 0.05, [255, 0, 0]), view: persp() }]);
        assert_eq!(near(20, 62), [255, 0, 0]);
        let far = draw(&mut gpu, &[LayerDraw { instances: one_note(0.9, 0.1, [255, 0, 0]), view: persp() }]);
        assert_eq!((far(29, 1), far(18, 1)), ([255, 0, 0], [0, 0, 0]));
        assert!((0..4).all(|y| (0..26).all(|x| far(x, y) == [0, 0, 0])), "far columns converge on the centre");
    }

    #[test]
    fn perspective_clamps_sounding_notes_to_the_keyboard_line() {
        let Ok(mut gpu) = Offscreen::new(64, 64) else { eprintln!("no GPU, skipped"); return };
        let mut ring = InstanceRing::default();
        ring.begin(0);
        let start = NOW - 500_000;
        ring.push(NoteInstance { start_us: [start as u32, 0], len: 0.6, key_color: 60 | 255 << 24 }, u64::MAX);
        let px = draw(&mut gpu, &[LayerDraw { instances: Arc::new(ring), view: persp() }]);
        assert_eq!(px(20, 62), [255, 0, 0]);
        assert_eq!(px(20, 40), [0, 0, 0], "ends 0.1 s ahead, near the keyboard");
    }

    #[test]
    fn perspective_borders_are_even_on_both_sides() {
        let Ok(mut gpu) = Offscreen::new(64, 64) else { eprintln!("no GPU, skipped"); return };
        // Shaded: the inside is the colour squared (255 stays 255), the border 40 % of it.
        let view = NoteView { flat_mode: 0, ..persp() };
        let px = draw(&mut gpu, &[LayerDraw { instances: one_note(0.0, 1.0, [255, 0, 0]), view }]);
        // Row 20: the note spans about x 23.6..32 there (8.4 px, wide enough not to fade).
        assert!(px(24, 20)[0] < 150 && px(31, 20)[0] < 150, "both sides bordered: {:?} {:?}", px(24, 20), px(31, 20));
        assert_eq!(px(28, 20)[0], 255);
    }

    #[test]
    fn perspective_measures_cut_pieces_by_their_whole_note() {
        let Ok(mut gpu) = Offscreen::new(64, 64) else { eprintln!("no GPU, skipped"); return };
        // A 2 px piece of a half-second note: thin on screen, but not a thin note.
        let whole = (0.5f32.to_bits() >> 16) << 16;
        let start = NOW;
        // A cut piece: no start border (bit 7), no end border (negative length).
        let piece = NoteInstance { start_us: [start as u32, whole], len: -0.01, key_color: 60 | 0x80 | 255 << 24 };
        let view = NoteView { flat_mode: 0, ..persp() };
        let px = draw(&mut gpu, &[LayerDraw { instances: Arc::new(InstanceRing::from_ordered([piece])), view }]);
        assert_eq!(px(24, 63)[0], 255, "drawn in its colour, not darkened as thin");
    }

    #[test]
    fn perspective_near_narrows_the_keyboard_line() {
        let Ok(mut gpu) = Offscreen::new(64, 64) else { eprintln!("no GPU, skipped"); return };
        // The keyboard line at depth 1.5: key 0.25..0.5 lands at about x 0.33..0.5 there.
        let view = NoteView { perspective_near: 1.5, ..persp() };
        let px = draw(&mut gpu, &[LayerDraw { instances: one_note(0.0, 0.05, [255, 0, 0]), view }]);
        assert_eq!((px(18, 62), px(26, 62)), ([0, 0, 0], [255, 0, 0]));
    }

    fn blocks() -> NoteView { NoteView { block_height: 0.15, ..persp() } }

    #[test]
    fn blocks_stand_up_from_the_plane() {
        let Ok(mut gpu) = Offscreen::new(64, 64) else { eprintln!("no GPU, skipped"); return };
        // In flight 0.2..0.5 s ahead: flat it spans rows ~35..15, its top lifted to rows ~26..8.
        let flat = draw(&mut gpu, &[LayerDraw { instances: one_note(0.2, 0.3, [255, 0, 0]), view: persp() }]);
        let raised = draw(&mut gpu, &[LayerDraw { instances: one_note(0.2, 0.3, [255, 0, 0]), view: blocks() }]);
        assert_eq!((flat(28, 12), raised(28, 12)), ([0, 0, 0], [255, 0, 0]), "the top rises above the flat note");
        // Below the top's near edge is the near-end face, darker than the top.
        let [r, g, b] = raised(26, 30);
        assert!((170..=190).contains(&r) && g == 0 && b == 0, "near face {:?}", [r, g, b]);
    }

    #[test]
    fn cut_pieces_have_no_near_end() {
        let Ok(mut gpu) = Offscreen::new(64, 64) else { eprintln!("no GPU, skipped"); return };
        // Bit 7: the piece continues an older part of its note, so it has no near end of its own.
        let start = NOW + 200_000;
        let piece = NoteInstance { start_us: [start as u32, 0], len: 0.3, key_color: 60 | 0x80 | 255 << 24 };
        let px = draw(&mut gpu, &[LayerDraw { instances: Arc::new(InstanceRing::from_ordered([piece])), view: blocks() }]);
        assert_eq!(px(26, 30), [0, 0, 0]);
    }

    #[test]
    fn sounding_blocks_sink_into_the_keyboard() {
        let Ok(mut gpu) = Offscreen::new(64, 64) else { eprintln!("no GPU, skipped"); return };
        // At the keyboard line a sounding block is flat: its top, not a near-end wall.
        let px = draw(&mut gpu, &[LayerDraw { instances: one_note(-0.1, 0.4, [255, 0, 0]), view: blocks() }]);
        assert_eq!(px(20, 62), [255, 0, 0]);
    }

    #[test]
    fn nearer_blocks_hide_farther_ones() {
        let Ok(mut gpu) = Offscreen::new(64, 64) else { eprintln!("no GPU, skipped"); return };
        // Green is newer (drawn on top by instance order) but starts where red ends, farther away:
        // red's raised top covers green's near face.
        let n = |offset: f32, c: u32| NoteInstance { start_us: [(NOW + (offset * 1e6) as u64) as u32, 0], len: 0.3, key_color: 60 | c << 8 };
        let ring = Arc::new(InstanceRing::from_ordered([n(0.3, 0x00FF00), n(0.0, 0xFF0000)]));
        let px = draw(&mut gpu, &[LayerDraw { instances: ring, view: blocks() }]);
        assert_eq!(px(27, 22), [255, 0, 0]);
    }

    #[test]
    fn perspective_keeps_narrow_far_notes_dark() {
        let Ok(mut gpu) = Offscreen::new(64, 64) else { eprintln!("no GPU, skipped"); return };
        // Near the far edge (rows 1..3) this note is about 5 px wide: a 1 px PFA border leaves a
        // 1..3 px inside that comes and goes with pixel alignment (moire). It should be border colour (20 %).
        let view = NoteView { flat_mode: 2, ..persp() };
        let px = draw(&mut gpu, &[LayerDraw { instances: one_note(0.8, 0.2, [255, 0, 0]), view }]);
        let brightest = (1..4).flat_map(|y| (24..40).map(move |x| (x, y))).map(|(x, y)| px(x, y)[0]).max().unwrap();
        assert!((40..=60).contains(&brightest), "brightest {brightest}");
    }

    #[test]
    fn vertex_input_fallback_draws_like_storage() {
        let Ok(mut gpu) = Offscreen::new(64, 64) else { eprintln!("no GPU, skipped"); return };
        let n = |offset: f32, len: f32, key: u32, c: u32| NoteInstance { start_us: [(NOW as i64 + (offset * 1e6) as i64) as u32, 0], len, key_color: key | c << 8 };
        let ring = Arc::new(InstanceRing::from_ordered([n(0.3, 0.3, 60, 0x00FF00), n(-0.1, 0.5, 60 | 0x80, 0xFF0000), n(0.1, 0.2, 60, 0x2040FF)]));
        let views = [NoteView::vertical(1.0, NOW, -1.0, 0), NoteView { flat_mode: 2, ..persp() }, blocks()];
        let storage: Vec<_> = views.iter().map(|&view| draw(&mut gpu, &[LayerDraw { instances: ring.clone(), view }])).collect();
        gpu.layers = vec![NotePipeline::with_input(&gpu.device, wgpu::TextureFormat::Rgba8Unorm, NoteInput::Vertex)];
        for (i, &view) in views.iter().enumerate() {
            let fallback = draw(&mut gpu, &[LayerDraw { instances: ring.clone(), view }]);
            assert!((0..64).all(|y| (0..64).all(|x| fallback(x, y) == storage[i](x, y))), "view {i} differs");
        }
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

