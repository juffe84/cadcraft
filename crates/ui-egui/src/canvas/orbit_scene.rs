//! A display list with model elevations, shared by orbit picking and rendering.
use std::ops::Range;
use std::sync::Arc;

use cadcraft_doc::{Handle, Space};
use cadcraft_geom::{Bounds2, Vec3};
use cadcraft_render::{DPrim, DisplayList, Kind, Options, SpatialSnap};
use egui::{Color32, Painter, Pos2, Rect, Stroke};

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
    fn include(&mut self, point: Vec3) {
        if point.is_finite() {
            self.xy.add(point.xy());
            self.z[0] = self.z[0].min(point.z);
            self.z[1] = self.z[1].max(point.z);
        }
    }

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

struct PickRanges {
    extent: Extent,
    groups: Vec<(Extent, Range<usize>)>,
}

impl PickRanges {
    fn build<T>(items: &[T], mut extent: impl FnMut(&T, &mut Extent)) -> Self {
        const CHUNK: usize = 64;
        let mut extent_all = Extent::default();
        let groups = items
            .chunks(CHUNK)
            .enumerate()
            .map(|(chunk, items)| {
                let mut bounds = Extent::default();
                for item in items {
                    extent(item, &mut bounds);
                }
                let start = chunk * CHUNK;
                extent_all = extent_all.union(bounds);
                (bounds, start..start + items.len())
            })
            .collect();
        Self { extent: extent_all, groups }
    }

    fn near<'a>(&'a self, frame: &'a OrbitFrame, at: Pos2, radius: f32) -> impl Iterator<Item = usize> + 'a {
        let xy = frame.pick_bounds(self.extent.z, at, radius);
        let area = Rect::from_center_size(at, egui::Vec2::splat(radius * 2.0));
        self.groups
            .iter()
            .filter_map(move |(bounds, range)| {
                let near = bounds.xy.intersects(&xy) && projected_bounds(frame, *bounds).is_none_or(|projected| projected.intersects(area));
                near.then(|| range.clone())
            })
            .flatten()
    }
}

fn projected_bounds(frame: &OrbitFrame, extent: Extent) -> Option<Rect> {
    let mut rect = Rect::NOTHING;
    for xy in extent.xy.corners() {
        for z in extent.z {
            let (at, depth) = frame.project(xy.to3(z));
            if frame.camera.weight(depth) <= 0.01 {
                return None;
            }
            rect.extend_with(at);
        }
    }
    Some(rect)
}

struct PickData {
    primitives: PickRanges,
    snaps: PickRanges,
    infinite: Vec<usize>,
    by_handle: Vec<(Handle, usize)>,
}

#[derive(Clone)]
pub(super) struct Model {
    pub list: Arc<DisplayList>,
    pick_data: Arc<PickData>,
    pub extent: Extent,
    pub missing: usize,
}

impl Model {
    fn new(list: DisplayList, doc: &cadcraft_doc::Drawing) -> Self {
        let primitives = PickRanges::build(&list.prims, |prim, extent| {
            let count = if matches!(prim.kind, Kind::Infinite { .. }) { 1 } else { usize::MAX };
            for point in list.spatial_points(prim).take(count) {
                extent.include(point);
            }
        });
        let snaps = PickRanges::build(&list.spatial_snaps, |snap, extent| {
            for point in snap.positions() {
                extent.include(point);
            }
        });
        let infinite = list.prims.iter().enumerate().filter(|(_, p)| matches!(p.kind, Kind::Infinite { .. })).map(|(i, _)| i).collect();
        let mut by_handle: Vec<_> = list.prims.iter().enumerate().map(|(i, p)| (p.handle, i)).collect();
        by_handle.sort_unstable();
        let extent = primitives.extent;
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
        let pick_data = Arc::new(PickData { primitives, snaps, infinite, by_handle });
        Self { list: Arc::new(list), pick_data, extent, missing }
    }

    pub fn paint_images(&self, painter: &Painter, frame: &OrbitFrame) {
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
    }

    pub fn paint(&self, painter: &Painter, frame: &OrbitFrame, gpu: bool) {
        if self.list.prims.is_empty() {
            return;
        }
        let background = Tokens::get().canvas;
        let bg = cadcraft_color::Rgb(background.r(), background.g(), background.b());
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
        self.pick_object(frame, cursor, radius).map(|(_, point)| point)
    }

    pub fn pick_object(&self, frame: &OrbitFrame, cursor: Pos2, radius: f32) -> Option<(Handle, Vec3)> {
        let mut best: Option<(f32, f64, Handle, Vec3)> = None;
        let indices = self.pick_data.primitives.near(frame, cursor, radius).chain(self.pick_data.infinite.iter().copied());
        for index in indices {
            let p = &self.list.prims[index];
            if p.kind == Kind::Mask {
                continue;
            }
            self.visit_prim(p, |geometry| {
                let candidate = match geometry {
                    Geometry::Point(point) => Some((frame.project(point).0.distance(cursor), point)),
                    Geometry::Line(points) => edge(frame, cursor, points),
                    Geometry::Triangle(points) => triangle(frame, cursor, points),
                };
                let Some((distance, point)) = candidate.filter(|(d, _)| *d <= radius) else { return };
                let depth = frame.project(point).1;
                if frame.camera.weight(depth) > 0.01
                    && best.is_none_or(|(old_d, old_z, _, _)| distance < old_d || (distance == old_d && depth >= old_z))
                {
                    best = Some((distance, depth, p.handle, point));
                }
            });
        }
        best.map(|(_, _, handle, point)| (handle, point))
    }

    pub fn snaps_near<'a>(&'a self, frame: &'a OrbitFrame, at: Pos2, radius: f32) -> impl Iterator<Item = SpatialSnap> + 'a {
        self.pick_data.snaps.near(frame, at, radius).map(|i| self.list.spatial_snaps[i])
    }

    pub fn highlight(&self, painter: &Painter, frame: &OrbitFrame, handles: &[Handle], color: Color32, width: f32) {
        let stroke = Stroke::new(width, color);
        for handle in handles {
            let entries = &self.pick_data.by_handle;
            let start = entries.partition_point(|(h, _)| h < handle);
            for &(_, index) in entries[start..].iter().take_while(|(h, _)| h == handle) {
                self.visit_prim(&self.list.prims[index], |geometry| match geometry {
                    Geometry::Point(p) => {
                        painter.circle_stroke(frame.project(p).0, 3.0, stroke);
                    }
                    Geometry::Line(points) => {
                        painter.line_segment(points.map(|p| frame.project(p).0), stroke);
                    }
                    Geometry::Triangle(_) => {}
                });
            }
        }
    }

    fn visit(&self, mut visit: impl FnMut(Geometry, &DPrim)) {
        for p in &self.list.prims {
            self.visit_prim(p, |geometry| visit(geometry, p));
        }
    }

    fn visit_prim(&self, p: &DPrim, mut visit: impl FnMut(Geometry)) {
        let mut points = self.list.spatial_points(p);
        match p.kind {
            Kind::Point => {
                if let Some(point) = points.next() {
                    visit(Geometry::Point(point));
                }
            }
            Kind::Polyline => {
                if let Some(mut a) = points.next() {
                    for b in points {
                        visit(Geometry::Line([a, b]));
                        a = b;
                    }
                }
            }
            Kind::Infinite { ray } => {
                if let Some((base, dir)) = points.next().zip(points.next()) {
                    visit(Geometry::Line(crate::orbit_gpu::infinite(base, dir, ray, self.extent.center(), self.extent.span())));
                }
            }
            Kind::Tris | Kind::Mask => {
                while let (Some(a), Some(b), Some(c)) = (points.next(), points.next(), points.next()) {
                    visit(Geometry::Triangle([a, b, c]));
                }
            }
            Kind::Image { .. } => {
                if let (Some(a), Some(b), Some(c), Some(d)) = (points.next(), points.next(), points.next(), points.next()) {
                    visit(Geometry::Triangle([a, b, c]));
                    visit(Geometry::Triangle([a, c, d]));
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

pub(super) fn edge(frame: &OrbitFrame, cursor: Pos2, points: [Vec3; 2]) -> Option<(f32, Vec3)> {
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
    frame: Option<(u64, u64, Extent)>,
}

impl Scene {
    pub fn framing(&mut self, document: u64, fit_revision: u64, extent: Extent) -> Extent {
        if let Some((uid, fit, reference)) = self.frame
            && uid == document
            && fit == fit_revision
        {
            return reference;
        }
        self.frame = Some((document, fit_revision, extent));
        extent
    }
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
