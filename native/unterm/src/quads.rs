//! Instanced rounded-rectangle renderer for solid fills.
//!
//! One draw call fills N anti-aliased rounded rects via a rounded-box SDF in
//! the fragment shader. The terminal uses it for per-cell background colors,
//! the cursor block/outline, and the selection highlight (radius 0 = sharp).

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

/// A rounded rectangle in pixel coordinates (origin top-left).
#[derive(Clone, Copy)]
pub struct Quad {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub color: [f32; 4],
    pub radius: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Instance {
    rect: [f32; 4],
    color: [f32; 4],
    radius: f32,
    _pad: [f32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    resolution: [f32; 2],
    _pad: [f32; 2],
}

pub struct QuadRenderer {
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    uniform_buf: wgpu::Buffer,
    instances: Option<wgpu::Buffer>,
    count: u32,
}

impl QuadRenderer {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("unterm-quad-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("unterm-quad-uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("unterm-quad-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("unterm-quad-bg"),
            layout: &bind_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("unterm-quad-pl"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });

        let instance_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Instance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &[
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x4,
                    offset: 0,
                    shader_location: 0,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x4,
                    offset: 16,
                    shader_location: 1,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32,
                    offset: 32,
                    shader_location: 2,
                },
            ],
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("unterm-quad-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[instance_layout],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        Self {
            pipeline,
            bind_group,
            uniform_buf,
            instances: None,
            count: 0,
        }
    }

    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        resolution: (f32, f32),
        quads: &[Quad],
    ) {
        queue.write_buffer(
            &self.uniform_buf,
            0,
            bytemuck::bytes_of(&Uniforms {
                resolution: [resolution.0, resolution.1],
                _pad: [0.0; 2],
            }),
        );

        let data: Vec<Instance> = quads
            .iter()
            .map(|q| Instance {
                rect: [q.x, q.y, q.w, q.h],
                color: q.color,
                radius: q.radius,
                _pad: [0.0; 3],
            })
            .collect();
        self.count = data.len() as u32;
        self.instances = if data.is_empty() {
            None
        } else {
            Some(
                device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("unterm-quad-instances"),
                    contents: bytemuck::cast_slice(&data),
                    usage: wgpu::BufferUsages::VERTEX,
                }),
            )
        };
    }

    pub fn render<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>) {
        let Some(instances) = &self.instances else {
            return;
        };
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, instances.slice(..));
        pass.draw(0..6, 0..self.count);
    }
}

const SHADER: &str = r#"
struct Uniforms { resolution: vec2<f32>, _pad: vec2<f32> };
@group(0) @binding(0) var<uniform> u: Uniforms;

struct VsOut {
  @builtin(position) pos: vec4<f32>,
  @location(0) local: vec2<f32>,
  @location(1) half_size: vec2<f32>,
  @location(2) color: vec4<f32>,
  @location(3) radius: f32,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32,
      @location(0) rect: vec4<f32>,
      @location(1) color: vec4<f32>,
      @location(2) radius: f32) -> VsOut {
  var corners = array<vec2<f32>, 6>(
    vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
    vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0));
  let c = corners[vi];
  let px = rect.xy + c * rect.zw;
  var out: VsOut;
  let ndc = vec2<f32>(px.x / u.resolution.x * 2.0 - 1.0,
                      1.0 - px.y / u.resolution.y * 2.0);
  out.pos = vec4<f32>(ndc, 0.0, 1.0);
  out.half_size = rect.zw * 0.5;
  out.local = (c - vec2<f32>(0.5, 0.5)) * rect.zw;
  out.color = color;
  out.radius = radius;
  return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
  let q = abs(in.local) - (in.half_size - vec2<f32>(in.radius));
  let dist = length(max(q, vec2<f32>(0.0))) - in.radius;
  let alpha = 1.0 - smoothstep(-1.0, 1.0, dist);
  return vec4<f32>(in.color.rgb, in.color.a * alpha);
}
"#;
