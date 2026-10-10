use std::{mem, sync::Arc};

use bytemuck::{Pod, Zeroable};
use eframe::{egui, wgpu};
use egui::epaint::{PaintCallbackInfo, ViewportInPixels};
use egui_wgpu::{Callback, CallbackResources, CallbackTrait, ScreenDescriptor};

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub struct NoteInstance {
    pub start: f32,
    pub len: f32,
    pub key_color: u32,
    pub border_width: u32,
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
    _pad: [u32; 2],
}

/// Depth buffer bits requested from eframe (see `NativeOptions::depth_buffer`), and
/// the matching format. Notes use depth so covered pixels are skipped (early-z).
pub const DEPTH_BITS: u8 = 32;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

pub struct NotesCallback {
    viewport: egui::Rect,
    target_format: wgpu::TextureFormat,
    instances: Arc<Vec<NoteInstance>>,
    key_positions: [KeyPosition; 256],
    height_time: f32,
    direction_sign: f32,
    flat_notes: bool,
}

impl NotesCallback {
    pub fn new(
        viewport: egui::Rect,
        target_format: wgpu::TextureFormat,
        instances: Arc<Vec<NoteInstance>>,
        key_positions: [KeyPosition; 256],
        height_time: f32,
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
            _pad: [0; 2],
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
    key_buffer: wgpu::Buffer,
    globals_buffer: wgpu::Buffer,
    target_format: wgpu::TextureFormat,
}

impl NotePipeline {
    fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("notes.bind_group_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
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
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("notes.shader"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(include_str!(
                "../shaders/notes.wgsl"
            ))),
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
                    array_stride: std::mem::size_of::<NoteInstance>() as wgpu::BufferAddress,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &[
                        wgpu::VertexAttribute {
                            offset: 0,
                            shader_location: 0,
                            format: wgpu::VertexFormat::Float32x2,
                        },
                        wgpu::VertexAttribute {
                            offset: 8,
                            shader_location: 1,
                            format: wgpu::VertexFormat::Uint32,
                        },
                        wgpu::VertexAttribute {
                            offset: 12,
                            shader_location: 2,
                            format: wgpu::VertexFormat::Uint32,
                        },
                    ],
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
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let key_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("notes.keys"),
            size: (std::mem::size_of::<KeyPosition>() * 256) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
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
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: key_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: globals_buffer.as_entire_binding(),
                },
            ],
        });

        Self {
            pipeline,
            bind_group,
            instance_buffer,
            instance_capacity: 16,
            instance_count: 0,
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
        instances: &[NoteInstance],
        keys: &[KeyPosition; 256],
        mut globals: GlobalsUniform,
    ) {
        // Never exceed the device's buffer limit (wgpu panics if we do); draw what fits.
        let max = (device.limits().max_buffer_size / mem::size_of::<NoteInstance>() as u64)
            .min(u32::MAX as u64) as usize;
        let instances = &instances[..instances.len().min(max)];
        if instances.len() > self.instance_capacity {
            self.grow_instances(device, instances.len(), max);
        }
        self.instance_count = instances.len() as u32;
        globals.instance_count = self.instance_count;

        queue.write_buffer(&self.instance_buffer, 0, bytemuck::cast_slice(instances));
        queue.write_buffer(&self.key_buffer, 0, bytemuck::cast_slice(&keys[..]));
        queue.write_buffer(&self.globals_buffer, 0, bytemuck::bytes_of(&globals));
    }

    fn grow_instances(&mut self, device: &wgpu::Device, needed: usize, max: usize) {
        let capacity = needed.next_power_of_two().min(max);
        let new_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("notes.instances.grow"),
            size: (capacity * std::mem::size_of::<NoteInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.instance_buffer = new_buffer;
        self.instance_capacity = capacity;
    }

    fn draw(&self, render_pass: &mut wgpu::RenderPass<'static>) {
        if self.instance_count == 0 {
            return;
        }
        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.bind_group, &[]);
        let slice = self.instance_buffer.slice(..);
        let slice_static: wgpu::BufferSlice<'static> = unsafe { mem::transmute(slice) };
        render_pass.set_vertex_buffer(0, slice_static);
        render_pass.draw(0..6, 0..self.instance_count);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn note_shader_is_valid() {
        use egui_wgpu::wgpu::naga;
        let module = naga::front::wgsl::parse_str(include_str!("../shaders/notes.wgsl")).unwrap();
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::empty())
            .validate(&module)
            .unwrap();
    }
}
