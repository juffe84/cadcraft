//! Point-cloud vertices stay on the GPU when the camera moves.
use cadcraft_doc::point_cloud::PointCloud;
use cadcraft_geom::{Bounds2, Vec3};
use egui::{Color32, Rect};
use egui_wgpu::wgpu;
use egui_wgpu::wgpu::util::DeviceExt;
use std::collections::VecDeque;
use std::ops::Range;
use std::sync::{Arc, Weak};

const POINT_BYTES: usize = 16;
const CLOUD_BUFFER_BUDGET: u64 = 512 << 20;

const SHADER: &str = r#"
struct View {
    x: vec4f,
    y: vec4f,
    z: vec4f,
    w: vec4f,
    marker: vec4f,
};
@group(0) @binding(0) var<uniform> view: View;
struct Output {
    @builtin(position) pos: vec4f,
    @location(0) color: vec4f,
};
@vertex fn vs(@location(0) position: vec3f, @location(1) color: vec4f, @builtin(vertex_index) vertex: u32) -> Output {
    let u = vec4f(position, 1.0);
    let corner = array<vec2f, 4>(vec2f(-1.0,-1.0), vec2f(1.0,-1.0), vec2f(-1.0,1.0), vec2f(1.0,1.0));
    var out: Output;
    let w = dot(view.w, u);
    out.pos = vec4f(vec2f(dot(view.x, u), dot(view.y, u)) + corner[vertex] * view.marker.xy * w, dot(view.z, u), w);
    out.color = color;
    return out;
}
@fragment fn fs(in: Output) -> @location(0) vec4f {
    if view.marker.z < 0.5 {
        return in.color;
    }
    let c = in.color.rgb;
    return vec4f(select(c / 12.92, pow((c + 0.055) / 1.055, vec3f(2.4)), c >= vec3f(0.04045)), in.color.a);
}
"#;

struct Resources {
    plan: wgpu::RenderPipeline,
    orbit: wgpu::RenderPipeline,
    views: [(wgpu::BindGroup, wgpu::Buffer); 2],
    resident: VecDeque<Resident>,
    linear: bool,
}

struct Resident {
    cloud: Weak<PointCloud>,
    color: Color32,
    buffer: wgpu::Buffer,
    blocks: Vec<(Bounds2, Range<u32>)>,
}

pub fn install(rs: &egui_wgpu::RenderState, target: crate::gpu::GpuTarget) {
    let Some(format) = target.stencil.filter(|format| format.has_depth_aspect()) else { return };
    let device = &rs.device;
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("cloud"), source: wgpu::ShaderSource::Wgsl(SHADER.into()) });
    let bindings = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("cloud"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        }],
    });
    let views = std::array::from_fn(|_| {
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("cloud view"),
            size: 80,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("cloud"),
            layout: &bindings,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() }],
        });
        (bind, uniform)
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("cloud"),
        bind_group_layouts: &[Some(&bindings)],
        immediate_size: 0,
    });
    let pipeline = |with_depth| {
        const ATTRS: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![0 => Float32x3, 1 => Unorm8x4];
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("cloud"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout { array_stride: 16, step_mode: wgpu::VertexStepMode::Instance, attributes: &ATTRS })],
            },
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
            depth_stencil: Some(wgpu::DepthStencilState {
                format,
                depth_write_enabled: Some(with_depth),
                depth_compare: Some(if with_depth { wgpu::CompareFunction::LessEqual } else { wgpu::CompareFunction::Always }),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState { format: target.format, blend: None, write_mask: wgpu::ColorWrites::ALL })],
            }),
            multiview_mask: None,
            cache: None,
        })
    };
    rs.renderer.write().callback_resources.insert(Resources {
        plan: pipeline(false),
        orbit: pipeline(true),
        views,
        resident: VecDeque::new(),
        linear: target.format.is_srgb(),
    });
}

pub fn available(target: Option<crate::gpu::GpuTarget>, points: &PointCloud) -> bool {
    fits_device(target, points.points().len())
}

fn fits_device(target: Option<crate::gpu::GpuTarget>, point_count: usize) -> bool {
    let bytes = (point_count as u64).saturating_mul(POINT_BYTES as u64);
    bytes != 0
        && target.is_some_and(|target| {
            bytes <= target.max_buffer_size.min(CLOUD_BUFFER_BUDGET) && target.stencil.is_some_and(|format| format.has_depth_aspect())
        })
}

pub struct Camera {
    pub center: Vec3,
    pub axes: [Vec3; 3],
    pub pan: [f64; 2],
    pub scale: f64,
    pub depth: Option<f64>,
    pub eye: Option<f64>,
    pub bounds: Bounds2,
}

impl Camera {
    pub fn weight(&self, depth: f64) -> f64 {
        self.eye.map_or(1.0, |eye| 1.0 - depth / eye)
    }

    pub fn rows(&self, rect: Rect, origin: Vec3) -> [[f32; 4]; 4] {
        let delta = origin - self.center;
        let [x, y, z] = self.axes;
        let row = |axis: Vec3, offset: f64, factor: f64, bias: f64| {
            [(axis.x * factor) as f32, (axis.y * factor) as f32, (axis.z * factor) as f32, ((axis.dot(delta) + offset) * factor + bias) as f32]
        };
        let (depth, w) = if let Some(eye) = self.eye {
            (row(z, 0.0, -1.0 / eye, 0.99), row(z, 0.0, -1.0 / eye, 1.0))
        } else {
            (self.depth.map_or([0.0, 0.0, 0.0, 0.5], |depth| row(z, 0.0, -0.49 / depth, 0.5)), [0.0, 0.0, 0.0, 1.0])
        };
        [
            row(x, self.pan[0], 2.0 * self.scale / f64::from(rect.width().max(1.0)), 0.0),
            row(y, self.pan[1], 2.0 * self.scale / f64::from(rect.height().max(1.0)), 0.0),
            depth,
            w,
        ]
    }
}

pub fn paint(painter: &egui::Painter, rect: Rect, clouds: impl IntoIterator<Item = Arc<PointCloud>>, camera: &Camera, size: f32, fallback: Color32) {
    let layers = clouds.into_iter().map(|cloud| Layer::new(rect, cloud, camera, size)).collect();
    painter.add(egui_wgpu::Callback::new_paint_callback(rect, Callback { layers, fallback, orbit: camera.depth.is_some(), bounds: camera.bounds }));
}

struct Layer {
    cloud: Arc<PointCloud>,
    rows: [[f32; 4]; 5],
}

impl Layer {
    fn new(rect: Rect, cloud: Arc<PointCloud>, camera: &Camera, size: f32) -> Self {
        let w = rect.width().max(1.0);
        let h = rect.height().max(1.0);
        let [x, y, z, weight] = camera.rows(rect, origin(&cloud));
        let rows = [x, y, z, weight, [size / w, size / h, 0.0, 0.0]];
        Self { cloud, rows }
    }
}

fn origin(cloud: &PointCloud) -> Vec3 {
    let xy = cloud.bounds().center();
    Vec3::new(xy.x, xy.y, (cloud.z_range[0] + cloud.z_range[1]) * 0.5)
}

struct Callback {
    layers: Vec<Layer>,
    fallback: Color32,
    orbit: bool,
    bounds: Bounds2,
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
        for (layer, (_, uniform)) in self.layers.iter().zip(&res.views) {
            let cloud = &layer.cloud;
            let weak = Arc::downgrade(cloud);
            if let Some(index) = res.resident.iter().position(|old| Weak::ptr_eq(&old.cloud, &weak) && old.color == self.fallback) {
                if let Some(cached) = res.resident.remove(index) {
                    res.resident.push_back(cached);
                }
            } else {
                let origin = origin(cloud);
                let mut vertices = Vec::with_capacity(cloud.points().len() * POINT_BYTES);
                let mut blocks = Vec::new();
                const BLOCK: usize = 8_192;
                for (index, points) in cloud.points().chunks(BLOCK).enumerate() {
                    let bounds = Bounds2::from_points(points.iter().map(|p| p.position.xy()));
                    let first = index * BLOCK;
                    blocks.push((bounds, first as u32..(first + points.len()) as u32));
                    for point in points {
                        let p = point.position - origin;
                        for value in [p.x, p.y, p.z] {
                            vertices.extend_from_slice(&(value as f32).to_le_bytes());
                        }
                        let color = if cloud.has_rgb {
                            let [r, g, b] = point.rgb.map(|value| if cloud.rgb_is_8_bit { value as u8 } else { (value >> 8) as u8 });
                            [r, g, b, 255]
                        } else {
                            self.fallback.to_array()
                        };
                        vertices.extend_from_slice(&color);
                    }
                }
                while res.resident.len() >= 3
                    || res.resident.iter().map(|entry| entry.buffer.size()).sum::<u64>() + vertices.len() as u64 > CLOUD_BUFFER_BUDGET
                {
                    // Keep the cache bounded even when an explicitly sampled overview is large.
                    if res.resident.pop_front().is_none() {
                        break;
                    }
                }
                let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("cloud points"),
                    contents: &vertices,
                    usage: wgpu::BufferUsages::VERTEX,
                });
                res.resident.push_back(Resident { cloud: weak, color: self.fallback, buffer, blocks });
            }
            let mut rows = layer.rows;
            rows[4][2] = f32::from(res.linear);
            let bytes: Vec<_> = rows.into_iter().flatten().flat_map(f32::to_le_bytes).collect();
            queue.write_buffer(uniform, 0, &bytes);
        }
        Vec::new()
    }

    fn paint(&self, _: egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>, resources: &egui_wgpu::CallbackResources) {
        let Some(res) = resources.get::<Resources>() else { return };
        pass.set_pipeline(if self.orbit { &res.orbit } else { &res.plan });
        for (layer, (bind, _)) in self.layers.iter().zip(&res.views) {
            let weak = Arc::downgrade(&layer.cloud);
            let Some(points) = res.resident.iter().find(|entry| Weak::ptr_eq(&entry.cloud, &weak) && entry.color == self.fallback) else { continue };
            pass.set_bind_group(0, bind, &[]);
            pass.set_vertex_buffer(0, points.buffer.slice(..));
            let mut span: Option<Range<u32>> = None;
            for (extent, range) in &points.blocks {
                if !extent.intersects(&self.bounds) {
                    continue;
                }
                if let Some(old) = &mut span
                    && old.end == range.start
                {
                    old.end = range.end;
                } else if let Some(old) = span.replace(range.clone()) {
                    pass.draw(0..4, old);
                }
            }
            if let Some(span) = span {
                pass.draw(0..4, span);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sampled_overview_can_exceed_a_million_points_on_a_capable_device() {
        use super::*;

        let samples = 3_800_000;
        let target = crate::gpu::GpuTarget {
            format: wgpu::TextureFormat::Bgra8Unorm,
            stencil: Some(wgpu::TextureFormat::Depth24PlusStencil8),
            max_buffer_size: 128 << 20,
        };
        assert!(fits_device(Some(target), samples));
        let small_device = crate::gpu::GpuTarget { max_buffer_size: 32 << 20, ..target };
        assert!(!fits_device(Some(small_device), samples));
    }

    #[test]
    fn point_shader_passes_wgsl_validation() {
        let module = naga::front::wgsl::parse_str(super::SHADER).unwrap();
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::empty()).validate(&module).unwrap();
    }

    #[test]
    fn orbit_uses_the_same_depth_for_model_faces_and_cloud_points() {
        use super::*;
        use cadcraft_doc::point_cloud::CloudPoint;
        use egui_wgpu::{CallbackTrait, ScreenDescriptor};
        const SIZE: u32 = 64;
        const DEPTH: wgpu::TextureFormat = wgpu::TextureFormat::Depth24PlusStencil8;
        let rs = egui_kittest::wgpu::create_render_state(egui_kittest::wgpu::default_wgpu_setup(), Default::default());
        let faults = Arc::new(crate::gpu::GpuFaults::default());
        crate::gpu::install(&rs, &faults, Some(DEPTH));
        let points = |z, rgb| CloudPoint { position: Vec3::new(0.0, 0.0, z), rgb, intensity: 0, classification: 0 };
        let cloud = |point| Arc::new(PointCloud::new(vec![point], 1, true, true));
        let near = cloud(points(1.0, [255; 3]));
        let far = cloud(points(-1.0, [0; 3]));
        let bounds = near.bounds();
        let rows = |depth| [[0.02, 0.0, 0.0, 0.0], [0.0, 0.02, 0.0, 0.0], [0.0, 0.0, -0.1, depth], [0.0, 0.0, 0.0, 1.0], [0.1, 0.1, 0.0, 0.0]];
        let callback = Callback {
            layers: vec![Layer { cloud: near, rows: rows(0.4) }, Layer { cloud: far, rows: rows(0.6) }],
            fallback: Color32::WHITE,
            orbit: true,
            bounds,
        };
        let rect = Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::splat(SIZE as f32));
        let camera = Camera {
            center: Vec3::ZERO,
            axes: [Vec3::new(1.0, 0.0, 0.0), Vec3::new(0.0, 1.0, 0.0), Vec3::Z],
            pan: [0.0; 2],
            scale: 1.0,
            depth: Some(5.0),
            eye: None,
            bounds,
        };
        let mut drawing = cadcraft_doc::Drawing::new_metric();
        let points = [Vec3::new(-25.0, -25.0, 0.0), Vec3::new(25.0, -25.0, 0.0), Vec3::new(0.0, 25.0, 0.0), Vec3::new(0.0, 25.0, 0.0)];
        let color = cadcraft_color::Color::True(cadcraft_color::Rgb(0, 255, 0));
        drawing
            .add(
                &cadcraft_doc::Space::Model,
                cadcraft_doc::Common { color, ..Default::default() },
                cadcraft_doc::EntityKind::Face3d(cadcraft_doc::Face3d { corners: points, hidden_edges: 15 }),
            )
            .unwrap();
        let list = Arc::new(cadcraft_render::build(
            &drawing,
            &cadcraft_doc::Space::Model,
            &cadcraft_render::Options { spatial: true, ..Default::default() },
        ));
        let model = crate::orbit_gpu::callback(rect, list, &camera, Vec3::ZERO, 50.0, cadcraft_color::Rgb(0, 0, 0));
        let device = &rs.device;
        let dimensions = wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 };
        let texture = |format, usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: dimensions,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        let color = texture(rs.target_format, wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC);
        let depth = texture(DEPTH, wgpu::TextureUsages::RENDER_ATTACHMENT);
        let (color_view, depth_view) = (color.create_view(&Default::default()), depth.create_view(&Default::default()));
        let mut encoder = device.create_command_encoder(&Default::default());
        let mut renderer = rs.renderer.write();
        let screen = ScreenDescriptor { size_in_pixels: [SIZE, SIZE], pixels_per_point: 1.0 };
        let _ = callback.prepare(device, &rs.queue, &screen, &mut encoder, &mut renderer.callback_resources);
        let _ = model.prepare(device, &rs.queue, &screen, &mut encoder, &mut renderer.callback_resources);
        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &color_view,
                        resolve_target: None,
                        depth_slice: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                    })],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: &depth_view,
                        depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Discard }),
                        stencil_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(0), store: wgpu::StoreOp::Discard }),
                    }),
                    ..Default::default()
                })
                .forget_lifetime();
            let info = || egui::PaintCallbackInfo { viewport: rect, clip_rect: rect, pixels_per_point: 1.0, screen_size_px: [SIZE, SIZE] };
            callback.paint(info(), &mut pass, &renderer.callback_resources);
            model.paint(info(), &mut pass, &renderer.callback_resources);
        }
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: u64::from(SIZE * SIZE * 4),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            color.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(SIZE * 4), rows_per_image: Some(SIZE) },
            },
            dimensions,
        );
        rs.queue.submit([encoder.finish()]);
        readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        device.poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(std::time::Duration::from_secs(10)) }).unwrap();
        let pixels = readback.slice(..).get_mapped_range().unwrap();
        let center = (SIZE as usize / 2) * (SIZE as usize + 1) * 4;
        assert!(pixels[center..center + 3].iter().all(|c| *c > 200));
        let side = center + 8 * 4;
        assert!(pixels[side + 1] > 200 && pixels[side] < 10 && pixels[side + 2] < 10);
        assert!(pixels[0..3].iter().all(|c| *c < 10));
    }
}
