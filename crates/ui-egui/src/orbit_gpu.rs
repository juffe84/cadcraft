//! Resident world-space model vertices for the orbit camera.
use std::ops::Range;
use std::sync::{Arc, Weak};

use cadcraft_color::Rgb;
use cadcraft_geom::Vec3;
use cadcraft_render::{DisplayList, Kind};
use egui::{Painter, Rect};
use egui_wgpu::wgpu::{self, util::DeviceExt};

const SHADER: &str = r#"
struct View {
    x: vec4f, y: vec4f, z: vec4f, w: vec4f,
    flags: vec4f,
};
@group(0) @binding(0) var<uniform> view: View;
struct Out {
    @builtin(position) position: vec4f,
    @location(0) color: vec4f,
};
@vertex fn vs(@location(0) point: vec3f, @location(1) color: vec4f) -> Out {
    let p = vec4f(point, 1.0);
    var out: Out;
    out.position = vec4f(dot(view.x, p), dot(view.y, p), dot(view.z, p), dot(view.w, p));
    let rgb = select(color.rgb, select(color.rgb / 12.92, pow((color.rgb + 0.055) / 1.055, vec3f(2.4)), color.rgb >= vec3f(0.04045)), view.flags.x > 0.5);
    out.color = vec4f(rgb * color.a, color.a);
    return out;
}
@fragment fn fs(input: Out) -> @location(0) vec4f {
    return input.color;
}
"#;

#[derive(Clone, Copy, PartialEq)]
enum Shape {
    Fill = 0,
    Line = 1,
    Point = 2,
}

struct Run {
    shape: Shape,
    vertices: Range<u32>,
}

struct TransparentTriangle {
    center: Vec3,
    depth: f64,
    start: u32,
}

struct TransparentPass {
    triangles: Vec<TransparentTriangle>,
    indices: wgpu::Buffer,
    sorted_axis: Option<Vec3>,
}

impl TransparentPass {
    fn prepare(&mut self, queue: &wgpu::Queue, axis: Vec3) {
        if self.sorted_axis == Some(axis) {
            return;
        }
        self.sorted_axis = Some(axis);
        for triangle in &mut self.triangles {
            triangle.depth = axis.dot(triangle.center);
        }
        self.triangles.sort_unstable_by(|a, b| a.depth.total_cmp(&b.depth).then_with(|| a.start.cmp(&b.start)));
        let bytes: Vec<_> = self.triangles.iter().flat_map(|t| (t.start..t.start + 3).flat_map(u32::to_le_bytes)).collect();
        queue.write_buffer(&self.indices, 0, &bytes);
    }
}

struct Resident {
    list: Weak<DisplayList>,
    bg: Rgb,
    buffer: wgpu::Buffer,
    runs: Vec<Run>,
    transparent: Option<TransparentPass>,
}

struct Resources {
    pipelines: [wgpu::RenderPipeline; 3],
    transparent_fill: wgpu::RenderPipeline,
    uniform: wgpu::Buffer,
    bind: wgpu::BindGroup,
    linear: bool,
    resident: Option<Resident>,
}

pub fn install(rs: &egui_wgpu::RenderState, target: crate::gpu::GpuTarget) {
    let Some(format) = target.stencil.filter(wgpu::TextureFormat::has_depth_aspect) else { return };
    let device = &rs.device;
    let shader =
        device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("orbit model"), source: wgpu::ShaderSource::Wgsl(SHADER.into()) });
    let bindings = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("orbit model view"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX,
            ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        }],
    });
    let uniform = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("orbit model view"),
        size: 80,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("orbit model view"),
        layout: &bindings,
        entries: &[wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() }],
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("orbit model"),
        bind_group_layouts: &[Some(&bindings)],
        immediate_size: 0,
    });
    const ATTRIBUTES: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![0 => Float32x3, 1 => Unorm8x4];
    let create = |topology, depth_write| {
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("orbit model"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout { array_stride: 16, step_mode: wgpu::VertexStepMode::Vertex, attributes: &ATTRIBUTES })],
            },
            primitive: wgpu::PrimitiveState { topology, ..Default::default() },
            depth_stencil: Some(wgpu::DepthStencilState {
                format,
                depth_write_enabled: Some(depth_write),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target.format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        })
    };
    let pipelines = [wgpu::PrimitiveTopology::TriangleList, wgpu::PrimitiveTopology::LineList, wgpu::PrimitiveTopology::PointList]
        .map(|topology| create(topology, true));
    let transparent_fill = create(wgpu::PrimitiveTopology::TriangleList, false);
    rs.renderer.write().callback_resources.insert(Resources {
        pipelines,
        transparent_fill,
        uniform,
        bind,
        linear: target.format.is_srgb(),
        resident: None,
    });
}

pub fn available(target: Option<crate::gpu::GpuTarget>) -> bool {
    target.and_then(|target| target.stencil).is_some_and(|format| format.has_depth_aspect())
}

/// A finite segment long enough to draw model-space rays while staying within f32 precision.
pub fn infinite(base: Vec3, dir: Vec3, ray: bool, center: Vec3, span: f64) -> [Vec3; 2] {
    let range = (span * 512.0 + (base - center).len() * 2.0).max(1.0);
    let step = dir.normalized() * range;
    [if ray { base } else { base - step }, base + step]
}

pub fn paint(painter: &Painter, rect: Rect, list: Arc<DisplayList>, camera: &crate::cloud_gpu::Camera, origin: Vec3, span: f64, bg: Rgb) {
    painter.add(egui_wgpu::Callback::new_paint_callback(rect, callback(rect, list, camera, origin, span, bg)));
}

pub(super) fn callback(
    rect: Rect,
    list: Arc<DisplayList>,
    camera: &crate::cloud_gpu::Camera,
    origin: Vec3,
    span: f64,
    bg: Rgb,
) -> impl egui_wgpu::CallbackTrait + use<> {
    let rows = camera.rows(rect, origin);
    Callback { list, rows, origin, span, bg, depth_axis: camera.axes[2] }
}

struct Callback {
    list: Arc<DisplayList>,
    rows: [[f32; 4]; 4],
    depth_axis: Vec3,
    origin: Vec3,
    span: f64,
    bg: Rgb,
}

impl Callback {
    fn upload(&self, device: &wgpu::Device) -> Option<Resident> {
        let mut bytes = Vec::new();
        let mut runs: Vec<Run> = Vec::new();
        let mut triangles = Vec::new();
        for prim in &self.list.prims {
            let start = (bytes.len() / 16) as u32;
            let rgb = if prim.kind == Kind::Mask { self.bg } else { prim.display_rgb(self.bg) };
            let color = [rgb.0, rgb.1, rgb.2, prim.alpha()];
            let mut push = |world: Vec3| {
                let p = world - self.origin;
                for v in [p.x, p.y, p.z] {
                    bytes.extend_from_slice(&(v as f32).to_le_bytes());
                }
                bytes.extend_from_slice(&color);
            };
            let mut points = self.list.spatial_points(prim);
            let shape = match prim.kind {
                Kind::Tris if prim.transparency > 0 => {
                    let mut vertex = start;
                    while let (Some(a), Some(b), Some(c)) = (points.next(), points.next(), points.next()) {
                        let center = ((a - self.origin) + (b - self.origin) + (c - self.origin)) * (1.0 / 3.0);
                        triangles.push(TransparentTriangle { center, depth: 0.0, start: vertex });
                        for point in [a, b, c] {
                            push(point);
                        }
                        vertex += 3;
                    }
                    continue;
                }
                Kind::Tris | Kind::Mask => {
                    for point in points {
                        push(point);
                    }
                    Shape::Fill
                }
                Kind::Polyline => {
                    if let Some(mut a) = points.next() {
                        for b in points {
                            push(a);
                            push(b);
                            a = b;
                        }
                    }
                    Shape::Line
                }
                Kind::Infinite { ray } => {
                    if let Some((base, dir)) = points.next().zip(points.next()) {
                        for point in infinite(base, dir, ray, self.origin, self.span) {
                            push(point);
                        }
                    }
                    Shape::Line
                }
                Kind::Point => {
                    if let Some(point) = points.next() {
                        push(point);
                    }
                    Shape::Point
                }
                Kind::Image { .. } => continue,
            };
            let end = (bytes.len() / 16) as u32;
            if start == end {
                continue;
            }
            if let Some(last) = runs.last_mut().filter(|r| r.shape == shape && r.vertices.end == start) {
                last.vertices.end = end;
            } else {
                runs.push(Run { shape, vertices: start..end });
            }
        }
        if bytes.is_empty() || bytes.len() as u64 > device.limits().max_buffer_size {
            return None;
        }
        let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("orbit model geometry"),
            contents: &bytes,
            usage: wgpu::BufferUsages::VERTEX,
        });
        let transparent = (!triangles.is_empty()).then(|| {
            let indices = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("orbit transparent triangles"),
                size: triangles.len() as u64 * 3 * 4,
                usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            TransparentPass { triangles, indices, sorted_axis: None }
        });
        Some(Resident { list: Arc::downgrade(&self.list), bg: self.bg, buffer, runs, transparent })
    }
}

impl egui_wgpu::CallbackTrait for Callback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _: &egui_wgpu::ScreenDescriptor,
        _: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(res) = resources.get_mut::<Resources>() else { return Vec::new() };
        let list = Arc::downgrade(&self.list);
        if res.resident.as_ref().is_none_or(|r| !Weak::ptr_eq(&r.list, &list) || r.bg != self.bg) {
            res.resident = self.upload(device);
        }
        if let Some(transparent) = res.resident.as_mut().and_then(|model| model.transparent.as_mut()) {
            transparent.prepare(queue, self.depth_axis);
        }
        let mut rows = self.rows.to_vec();
        rows.push([f32::from(res.linear), 0.0, 0.0, 0.0]);
        let bytes: Vec<_> = rows.into_iter().flatten().flat_map(f32::to_le_bytes).collect();
        queue.write_buffer(&res.uniform, 0, &bytes);
        Vec::new()
    }

    fn paint(&self, _: egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>, resources: &egui_wgpu::CallbackResources) {
        let Some(res) = resources.get::<Resources>() else { return };
        let Some(model) = &res.resident else { return };
        pass.set_bind_group(0, &res.bind, &[]);
        pass.set_vertex_buffer(0, model.buffer.slice(..));
        for run in &model.runs {
            pass.set_pipeline(&res.pipelines[run.shape as usize]);
            pass.draw(run.vertices.clone(), 0..1);
        }
        if let Some(transparent) = &model.transparent {
            pass.set_pipeline(&res.transparent_fill);
            pass.set_index_buffer(transparent.indices.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..transparent.triangles.len() as u32 * 3, 0, 0..1);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn model_shader_passes_wgsl_validation() {
        let module = naga::front::wgsl::parse_str(super::SHADER).unwrap();
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::empty()).validate(&module).unwrap();
    }
}
