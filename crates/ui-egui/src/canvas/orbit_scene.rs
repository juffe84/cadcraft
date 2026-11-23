//! A display list with model elevations, shared by orbit picking and rendering.
use std::sync::Arc;

use cadcraft_doc::Space;
use cadcraft_geom::{Bounds2, Vec3};
use cadcraft_render::{DPrim, DisplayList, Kind, Options};
use egui::{Color32, Painter, Pos2, Stroke};

use super::point_cloud::OrbitFrame;
use super::{CadApp, ListKey, Tokens, rebuild};

#[derive(Clone, Copy)]
pub(super) struct Extent {
    pub xy: Bounds2,
    pub z: [f64; 2],
}

impl Default for Extent {
    fn default() -> Self {
        Self { xy: Bounds2::EMPTY, z: [f64::INFINITY, f64::NEG_INFINITY] }
    }
}

impl Extent {
    pub fn union(self, other: Self) -> Self {
        Self { xy: self.xy.union(&other.xy), z: [self.z[0].min(other.z[0]), self.z[1].max(other.z[1])] }
    }
    pub fn center(self) -> Vec3 {
        let xy = self.xy.center();
        let z = if self.z[0] <= self.z[1] { (self.z[0] + self.z[1]) * 0.5 } else { 0.0 };
        Vec3::new(xy.x, xy.y, z)
    }
    pub fn span(self) -> f64 {
        if self.xy.is_empty() { 1.0 } else { self.xy.width().hypot(self.xy.height()).hypot((self.z[1] - self.z[0]).max(0.0)).max(1e-6) }
    }
}

#[derive(Clone)]
pub(super) struct Model {
    pub list: Arc<DisplayList>,
    pub extent: Extent,
    pub missing: usize,
}

impl Model {
    fn new(list: DisplayList, doc: &cadcraft_doc::Drawing) -> Self {
        let mut extent = Extent::default();
        for prim in &list.prims {
            let count = if matches!(prim.kind, Kind::Infinite { .. }) { 1 } else { usize::MAX };
            for point in list.spatial_points(prim).take(count) {
                if point.is_finite() {
                    extent.xy.add(point.xy());
                    extent.z[0] = extent.z[0].min(point.z);
                    extent.z[1] = extent.z[1].max(point.z);
                }
            }
        }
        let missing = doc
            .model
            .iter()
            .filter(|e| {
                matches!(&e.kind, cadcraft_doc::EntityKind::Unknown(entity) if !entity.is_ole_frame())
                    && e.common.visible
                    && !doc.is_hidden_object(e.handle)
                    && doc.layer(&e.common.layer).is_none_or(|layer| layer.on && !layer.frozen)
            })
            .count();
        Self { list: Arc::new(list), extent, missing }
    }

    pub fn paint(&self, painter: &Painter, frame: &OrbitFrame, gpu: bool) {
        let background = Tokens::get().canvas;
        let bg = cadcraft_color::Rgb(background.r(), background.g(), background.b());
        if self.list.prims.is_empty() {
            return;
        }
        for p in &self.list.prims {
            if let Kind::Image { index } = p.kind
                && let Some(image) = self.list.images.get(index as usize)
            {
                let v: Vec<_> = self.list.spatial_points(p).collect();
                if let [a, b, _, d] = v.as_slice() {
                    super::images::draw_orbit(painter, image, p.alpha(), frame.camera.scale, |q| {
                        let z = image.pixel_of(q).map_or(a.z, |uv| {
                            a.z + (b.z - a.z) * uv.x / f64::from(image.raster.width()) + (d.z - a.z) * uv.y / f64::from(image.raster.height())
                        });
                        frame.project(Vec3::new(q.x, q.y, z)).0
                    });
                }
            }
        }
        if gpu {
            crate::orbit_gpu::paint(painter, frame.rect, self.list.clone(), &frame.camera, self.extent.center(), self.extent.span(), bg);
            return;
        }
        let mut shapes = Vec::new();
        self.visit(|geometry, p| {
            if matches!(p.kind, Kind::Image { .. }) {
                return;
            }
            let rgb = p.display_rgb(bg);
            let color = if p.kind == Kind::Mask { background } else { Color32::from_rgba_unmultiplied(rgb.0, rgb.1, rgb.2, p.alpha()) };
            let (shape, depth) = match geometry {
                Geometry::Point(p) => {
                    let (at, depth) = frame.project(p);
                    (egui::Shape::circle_filled(at, 1.5, color), depth)
                }
                Geometry::Line([a, b]) => {
                    let (a, za) = frame.project(a);
                    let (b, zb) = frame.project(b);
                    (egui::Shape::line_segment([a, b], Stroke::new(1.0, color)), (za + zb) * 0.5)
                }
                Geometry::Triangle([a, b, c]) => {
                    let (a, za) = frame.project(a);
                    let (b, zb) = frame.project(b);
                    let (c, zc) = frame.project(c);
                    (egui::Shape::convex_polygon(vec![a, b, c], color, Stroke::NONE), (za + zb + zc) / 3.0)
                }
            };
            if shape.visual_bounding_rect().intersects(frame.rect) {
                shapes.push((depth, shape));
            }
        });
        shapes.sort_by(|a, b| a.0.total_cmp(&b.0));
        painter.extend(shapes.into_iter().map(|(_, shape)| shape));
    }

    /// Real XYZ under the cursor, with perspective-correct interpolation on edges and faces.
    pub fn pick(&self, frame: &OrbitFrame, cursor: Pos2, radius: f32) -> Option<Vec3> {
        let mut best: Option<(f32, f64, Vec3)> = None;
        self.visit(|geometry, p| {
            if p.kind == Kind::Mask {
                return;
            }
            let candidate = match geometry {
                Geometry::Point(point) => Some((frame.project(point).0.distance(cursor), point)),
                Geometry::Line(points) => edge(frame, cursor, points),
                Geometry::Triangle(points) => triangle(frame, cursor, points),
            };
            let Some((distance, point)) = candidate.filter(|(d, _)| *d <= radius) else { return };
            let depth = frame.project(point).1;
            if best.is_none_or(|(old_d, old_z, _)| distance < old_d || (distance == old_d && depth > old_z)) {
                best = Some((distance, depth, point));
            }
        });
        best.map(|(_, _, point)| point)
    }

    fn visit(&self, mut visit: impl FnMut(Geometry, &DPrim)) {
        for p in &self.list.prims {
            let mut points = self.list.spatial_points(p);
            match p.kind {
                Kind::Point => {
                    if let Some(point) = points.next() {
                        visit(Geometry::Point(point), p);
                    }
                }
                Kind::Polyline => {
                    if let Some(mut a) = points.next() {
                        for b in points {
                            visit(Geometry::Line([a, b]), p);
                            a = b;
                        }
                    }
                }
                Kind::Infinite { ray } => {
                    if let Some((base, dir)) = points.next().zip(points.next()) {
                        visit(Geometry::Line(crate::orbit_gpu::infinite(base, dir, ray, self.extent.center(), self.extent.span())), p);
                    }
                }
                Kind::Tris | Kind::Mask => {
                    while let (Some(a), Some(b), Some(c)) = (points.next(), points.next(), points.next()) {
                        visit(Geometry::Triangle([a, b, c]), p);
                    }
                }
                Kind::Image { .. } => {
                    if let (Some(a), Some(b), Some(c), Some(d)) = (points.next(), points.next(), points.next(), points.next()) {
                        visit(Geometry::Triangle([a, b, c]), p);
                        visit(Geometry::Triangle([a, c, d]), p);
                    }
                }
            }
        }
    }
}

enum Geometry {
    Point(Vec3),
    Line([Vec3; 2]),
    Triangle([Vec3; 3]),
}

fn edge(frame: &OrbitFrame, cursor: Pos2, points: [Vec3; 2]) -> Option<(f32, Vec3)> {
    let [a, b] = points.map(|point| frame.project(point).0);
    let line = b - a;
    let distance = line.length_sq();
    let t = if distance > 0.0 { ((cursor - a).dot(line) / distance).clamp(0.0, 1.0) } else { 0.0 };
    let on = (a + line * t).distance(cursor);
    frame.interpolate(points, [1.0 - f64::from(t), f64::from(t)]).map(|p| (on, p))
}

fn triangle(frame: &OrbitFrame, cursor: Pos2, points: [Vec3; 3]) -> Option<(f32, Vec3)> {
    let [a, b, c] = points.map(|point| frame.project(point).0);
    let (u, v, q) = (b - a, c - a, cursor - a);
    let cross = |a: egui::Vec2, b: egui::Vec2| f64::from(a.x) * f64::from(b.y) - f64::from(a.y) * f64::from(b.x);
    let area = cross(u, v);
    if area.abs() <= 1e-10 {
        return None;
    }
    let (x, y) = (cross(q, v) / area, cross(u, q) / area);
    let weights = [1.0 - x - y, x, y];
    if weights.iter().any(|w| *w < -1e-8) {
        return None;
    }
    frame.interpolate(points, weights).map(|p| (0.0, p))
}

#[derive(Default)]
pub(super) struct Scene {
    ready: Option<(ListKey, Model)>,
    job: Option<rebuild::Job>,
}

pub(super) fn model(app: &mut CadApp, ctx: &egui::Context, height: f64) -> Option<Model> {
    let state = app.session.state().ok()?;
    let orbit = state.orbit?;
    if state.doc.model.is_empty() {
        return None;
    }
    let pixels = (state.view().height / (height * orbit.zoom)).max(1e-7);
    let band = pixels.log2().floor() as i32;
    let transparent = app.session.settings.transparency_display;
    let key = (state.uid, state.revision, band, 0, orbit.wireframe, transparent, Arc::as_ptr(&state.doc) as usize, super::list::font_generation());
    let scene = &mut app.canvas.orbit_scene;
    if scene.ready.as_ref().is_some_and(|(k, _)| k == &key) {
        return scene.ready.as_ref().map(|(_, model)| model.clone());
    }
    if let Some(job) = &scene.job
        && job.key == key
    {
        match job.poll() {
            rebuild::Poll::Pending => return scene.ready.as_ref().filter(|(k, _)| k.0 == key.0 && k.1 == key.1).map(|(_, m)| m.clone()),
            rebuild::Poll::Done(built) => {
                let model = Model::new(built.list, &state.doc);
                scene.ready = Some((key, model.clone()));
                scene.job = None;
                return Some(model);
            }
            rebuild::Poll::Lost => {}
        }
        scene.job = None;
    }
    let opts = Options {
        spatial: true,
        fill: !orbit.wireframe,
        on_screen: true,
        transparency: transparent,
        tolerance: 2.0_f64.powi(band) * 0.5,
        min_dash: 2.0_f64.powi(band) * 2.0,
        ..Default::default()
    };
    // The first list establishes the fit bounds. Later zoom bands can tessellate on a worker.
    if let Some((k, model)) = &scene.ready
        && k.0 == key.0
        && k.1 == key.1
    {
        scene.job = rebuild::spawn(rebuild::Request { doc: state.doc.clone(), space: Space::Model, opts: opts.clone(), mesh: None }, key, None, ctx);
        if scene.job.is_some() {
            return Some(model.clone());
        }
    }
    let list = cadcraft_render::build(&state.doc, &Space::Model, &opts);
    let model = Model::new(list, &state.doc);
    scene.ready = Some((key, model.clone()));
    Some(model)
}
