use super::{CadApp, Color32, Rect, Shape, Tokens, Vec2, Xf, pos2, vec2};
use cadcraft_doc::Space;
use cadcraft_doc::point_cloud::{CloudPoint, CloudView, PointCloud};
use cadcraft_engine::PointCloudOrbit;
use cadcraft_geom::{Bounds2, Vec3};
use egui::{Key, PointerButton, Sense};
use serde_json::json;
use std::sync::{Arc, Weak};

pub(super) fn controls(app: &mut CadApp, ui: &mut egui::Ui) {
    let orbit = app.session.state().is_ok_and(|state| state.point_cloud_orbit.is_some());
    let streaming = cloud(app).is_some_and(|cloud| cloud.detail.is_some());
    let status = cloud(app).map(|cloud| view(cloud, cloud.bounds(), app, false)).unwrap_or_default();
    if status.pending {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(125));
    }
    ui.horizontal(|ui| {
        let mut size = app.session.settings.point_cloud_size;
        let mut snap = app.session.settings.point_cloud_snap;
        if ui.add(egui::Slider::new(&mut size, 1.0..=12.0).step_by(0.5).text(crate::i18n::t("Point size"))).changed() {
            let _ = app.run("pointcloud.settings", json!({ "size": size }));
        }
        if ui.add_enabled(!orbit, egui::Checkbox::new(&mut snap, crate::i18n::t("Snap to cloud points"))).changed() {
            let _ = app.run("pointcloud.settings", json!({ "snap": snap }));
        }
        if ui.selectable_label(orbit, crate::i18n::t("3D Orbit")).clicked() {
            let _ = app.run("3dorbit", json!({ "enabled": !orbit }));
        }
        if streaming {
            ui.menu_button(crate::i18n::t("Detail loading"), |ui| {
                let mut radius = app.session.settings.point_cloud_radius_factor;
                let mut batch = app.session.settings.point_cloud_batch_points / 1_000;
                if ui.add(egui::Slider::new(&mut radius, 0.0..=5.0).logarithmic(true).step_by(0.01).text(crate::i18n::t("Load radius (×)"))).changed()
                {
                    let _ = app.run("pointcloud.settings", json!({ "detailRadius": 0.0, "detailRadiusFactor": radius }));
                }
                if ui
                    .add(
                        egui::Slider::new(&mut batch, 0..=2000)
                            .step_by(10.0)
                            .logarithmic(true)
                            .text(crate::i18n::t("Read batch (k points per worker)")),
                    )
                    .changed()
                {
                    let _ = app.run("pointcloud.settings", json!({ "batchPoints": batch * 1_000 }));
                }
                ui.label(crate::i18n::t("Zero uses automatic loading. At close zoom, 0.10× loads at least 30 drawing units on each side."));
                let absolute = app.session.settings.point_cloud_detail_radius;
                if absolute > 0.0 {
                    ui.label(crate::i18n::t("Saved absolute radius: {radius} drawing units").replace("{radius}", &absolute.to_string()));
                }
                if ui.button(crate::i18n::t("Fast close-up")).clicked() {
                    let _ = app.run("pointcloud.settings", json!({ "detailRadius": 0.0, "detailRadiusFactor": 0.02, "batchPoints": 50_000 }));
                }
                if ui.button(crate::i18n::t("Automatic")).clicked() {
                    let _ = app.run("pointcloud.settings", json!({ "detailRadius": 0.0, "detailRadiusFactor": 0.0, "batchPoints": 0 }));
                }
            });
        }
        if status.failed {
            ui.label(crate::i18n::t("Could not load point cloud detail"));
        } else if status.pending {
            ui.label(crate::i18n::t("Loading point cloud detail…"));
        } else if let Some(count) = status.detail_count {
            let message = if count == 0 {
                crate::i18n::t("No detail points in this area").to_string()
            } else {
                crate::i18n::t("Detail: {count} points").replace("{count}", &count.to_string())
            };
            ui.label(message);
        } else if streaming {
            ui.label(crate::i18n::t("Zoom in to load detail"));
        } else if orbit {
            ui.label(crate::i18n::t("Middle drag to orbit. Right drag to pan. Scroll to zoom at cursor."));
        }
    });
}

fn cloud(app: &CadApp) -> Option<&Arc<PointCloud>> {
    let state = app.session.state().ok()?;
    if state.space != Space::Model || state.block_editor().is_some() {
        return None;
    }
    state.doc.point_cloud.as_ref()
}

fn sample_budget(rect: Rect) -> usize {
    let pixels = f64::from(rect.width()) * f64::from(rect.height());
    ((pixels / 8.0) as usize).clamp(1, 120_000)
}

fn color(cloud: &PointCloud, point: &CloudPoint, fallback: Color32) -> Color32 {
    if cloud.has_rgb {
        let [r, g, b] = point.rgb.map(|c| if cloud.rgb_is_8_bit { c as u8 } else { (c >> 8) as u8 });
        Color32::from_rgb(r, g, b)
    } else {
        fallback
    }
}

fn view(cloud: &PointCloud, bounds: Bounds2, app: &CadApp, request: bool) -> CloudView {
    cloud.detail.as_ref().map_or_else(CloudView::default, |details| {
        if request { details.0.view_with(bounds, app.session.settings.cloud_detail_options()) } else { details.0.current(bounds) }
    })
}

fn layers<'a>(snapshot: &'a CloudView, root: &'a Arc<PointCloud>) -> impl Iterator<Item = &'a Arc<PointCloud>> {
    let overview = snapshot.overview.as_ref().or_else(|| (snapshot.indexing && snapshot.points.is_some()).then_some(root));
    overview.into_iter().chain(std::iter::once(snapshot.points.as_ref().unwrap_or(root))).filter(|cloud| !cloud.points().is_empty())
}

pub(super) fn paint_plan(app: &CadApp, painter: &egui::Painter, xf: &Xf, request: bool) {
    let Some(cloud) = cloud(app) else { return };
    let bounds = xf.world_bounds();
    let snapshot = view(cloud, bounds, app, request);
    if request && snapshot.pending {
        painter.ctx().request_repaint_after(std::time::Duration::from_millis(125));
    }
    if request && layers(&snapshot, cloud).all(|shown| crate::cloud_gpu::available(app.canvas.gpu, shown)) {
        let (sin, cos) = xf.twist.sin_cos();
        let camera = crate::cloud_gpu::Camera {
            center: Vec3::new(xf.center.x, xf.center.y, 0.0),
            axes: [Vec3::new(cos, -sin, 0.0), Vec3::new(sin, cos, 0.0), Vec3::Z],
            pan: [0.0; 2],
            scale: xf.scale,
            depth: None,
            bounds,
        };
        crate::cloud_gpu::paint(
            painter,
            xf.rect,
            layers(&snapshot, cloud).cloned(),
            &camera,
            app.session.settings.point_cloud_size,
            Tokens::get().canvas_ink,
        );
        return;
    }
    let mut mesh = egui::Mesh::default();
    let size = egui::Vec2::splat(app.session.settings.point_cloud_size);
    let fallback = Tokens::get().canvas_ink;
    for shown in layers(&snapshot, cloud) {
        shown.visible(bounds, sample_budget(xf.rect), |point| {
            let screen = xf.to_screen(point.position.xy());
            if xf.rect.contains(screen) {
                mesh.add_colored_rect(Rect::from_center_size(screen, size), color(shown, point, fallback));
            }
        });
    }
    painter.add(Shape::mesh(mesh));
}

pub(super) fn snap_visible(app: &CadApp, xf: &Xf, raw: Vec2, aperture: f64) -> Option<Vec2> {
    let cloud = cloud(app)?;
    let bounds = xf.world_bounds();
    let snapshot = view(cloud, bounds, app, false);
    let mut nearest = None;
    let mut distance = aperture.max(f64::from(app.session.settings.point_cloud_size) * 0.5 / xf.scale).powi(2);
    let gpu = layers(&snapshot, cloud).all(|shown| crate::cloud_gpu::available(app.canvas.gpu, shown));
    for shown in layers(&snapshot, cloud) {
        let (pick, limit) = if gpu { (Bounds2::new(raw, raw).expand(distance.sqrt()), usize::MAX) } else { (bounds, sample_budget(xf.rect)) };
        shown.visible(pick, limit, |point| {
            let xy = point.position.xy();
            let delta = xy - raw;
            let d = delta.dot(delta);
            if d <= distance && xf.rect.contains(xf.to_screen(xy)) {
                distance = d;
                nearest = Some(xy);
            }
        });
    }
    nearest
}

/// Camera math stays in double precision even for survey coordinates far from the origin.
struct OrbitFrame {
    camera: crate::cloud_gpu::Camera,
    rect: Rect,
    bounds: Bounds2,
}

impl OrbitFrame {
    fn new(orbit: PointCloudOrbit, plan: cadcraft_engine::View, cloud: &PointCloud, rect: Rect) -> Self {
        let extent = cloud.bounds();
        let plan_center = if extent.contains(plan.center) { plan.center } else { extent.center() };
        let z_mid = (cloud.z_range[0] + cloud.z_range[1]) * 0.5;
        let center = orbit.pivot.unwrap_or(Vec3::new(plan_center.x, plan_center.y, z_mid));
        let diagonal = extent.width().hypot(extent.height()).hypot(cloud.z_range[1] - cloud.z_range[0]).max(1e-6);
        let fit = f64::from(rect.width().min(rect.height()).max(1.0)) / (diagonal * 1.05);
        let scale = (f64::from(rect.height().max(1.0)) / plan.height.max(1e-6)).max(fit) * orbit.zoom;
        let (sy, cy) = orbit.yaw.sin_cos();
        let (sin, cos) = orbit.tilt.sin_cos();
        let axes = [Vec3::new(cy, -sy, 0.0), Vec3::new(sy * cos, cy * cos, sin), Vec3::new(-sy * sin, -cy * sin, cos)];
        let x = f64::from(rect.width()) / (2.0 * scale);
        let y = f64::from(rect.height()) / (2.0 * scale);
        let rotated = Bounds2::from_points(extent.corners().map(|p| (p - center.xy()).rotate(orbit.yaw)));
        let (y_min, y_max) = if cos.abs() < 0.001 {
            (rotated.min.y, rotated.max.y)
        } else {
            let mut limits = Bounds2::EMPTY;
            for height in cloud.z_range {
                for edge in [-y, y] {
                    limits.add(Vec2::new(0.0, (edge - orbit.pan.y - (height - center.z) * sin) / cos));
                }
            }
            (limits.min.y.max(rotated.min.y), limits.max.y.min(rotated.max.y))
        };
        let min = Vec2::new((-x - orbit.pan.x).max(rotated.min.x), y_min);
        let max = Vec2::new((x - orbit.pan.x).min(rotated.max.x), y_max);
        let bounds = if min.x <= max.x && min.y <= max.y {
            Bounds2::from_points(Bounds2::new(min, max).corners().map(|p| center.xy() + p.rotate(-orbit.yaw)))
        } else {
            Bounds2::EMPTY
        };
        let depth = diagonal + (center - Vec3::new(plan_center.x, plan_center.y, z_mid)).len();
        let camera = crate::cloud_gpu::Camera { center, axes, pan: [orbit.pan.x, orbit.pan.y], scale, depth: Some(depth), bounds };
        Self { camera, rect, bounds }
    }

    fn project(&self, point: Vec3) -> (egui::Pos2, f64) {
        let point = point - self.camera.center;
        let [x, y, z] = self.camera.axes;
        let [px, py] = self.camera.pan;
        let at = self.rect.center();
        (pos2(at.x + ((x.dot(point) + px) * self.camera.scale) as f32, at.y - ((y.dot(point) + py) * self.camera.scale) as f32), z.dot(point))
    }

    fn ray_origin(&self, at: egui::Pos2) -> Vec3 {
        let [right, up, _] = self.camera.axes;
        let offset = at - self.rect.center();
        let [x, y] = self.camera.pan;
        self.camera.center + right * (f64::from(offset.x) / self.camera.scale - x) - up * (f64::from(offset.y) / self.camera.scale + y)
    }

    /// Locate a screen pixel on the plane at the depth of a visible sample.
    fn on_surface(&self, at: egui::Pos2, point: Vec3) -> Vec3 {
        let front = self.camera.axes[2];
        self.ray_origin(at) + front * front.dot(point - self.camera.center)
    }

    fn detail_bounds(&self, focus: Vec3) -> Bounds2 {
        // Loading is centered on the geometry under the view. The wide footprint belonging
        // to the cloud's entire elevation range is only used for render culling.
        let half = Vec2::new(f64::from(self.rect.width()), f64::from(self.rect.height())) / (2.0 * self.camera.scale);
        let margin = half.x.max(half.y) * (1.0 - self.camera.axes[2].z.abs());
        let span = half + Vec2::new(margin, margin);
        Bounds2::new(focus.xy() - span, focus.xy() + span)
    }

    fn pick_bounds(&self, cloud: &PointCloud, cursor: egui::Pos2, radius: f32) -> Bounds2 {
        let front = self.camera.axes[2];
        if front.z.abs() < 0.001 {
            return self.bounds;
        }
        let corners = Rect::from_center_size(cursor, egui::Vec2::splat(radius * 2.0)).intersect(self.rect);
        let mut bounds = Bounds2::EMPTY;
        for at in [corners.left_top(), corners.right_top(), corners.left_bottom(), corners.right_bottom()] {
            let origin = self.ray_origin(at);
            for z in cloud.z_range {
                bounds.add((origin + front * ((z - origin.z) / front.z)).xy());
            }
        }
        // Leave a margin for roundoff at a pixel or tile edge at survey coordinates.
        bounds.expand(0.001)
    }

    fn pick<'a>(&self, clouds: impl Iterator<Item = &'a Arc<PointCloud>>, cursor: egui::Pos2, limit: usize, radius: f32, size: f32) -> Option<Vec3> {
        let mut best: Option<SurfaceHit> = None;
        for cloud in clouds {
            cloud.visible(self.pick_bounds(cloud, cursor, radius), limit, |p| {
                let (at, depth) = self.project(p.position);
                let delta = (at - cursor).abs();
                if !self.rect.contains(at) || delta.length_sq() > radius * radius {
                    return;
                }
                let hit = SurfaceHit { point: p.position, distance: delta.length_sq(), depth, covers: delta.x.max(delta.y) <= 0.5 * size };
                if best.is_none_or(|previous| hit.nearer(previous)) {
                    best = Some(hit);
                }
            });
        }
        best.map(|hit| hit.point)
    }
}

#[derive(Clone, Copy)]
struct SurfaceHit {
    point: Vec3,
    distance: f32,
    depth: f64,
    covers: bool,
}

impl SurfaceHit {
    fn nearer(self, other: Self) -> bool {
        // Match the depth test when both point markers cover the actual pixel.
        if self.covers && other.covers {
            self.depth > other.depth
        } else {
            self.covers
                .cmp(&other.covers)
                .then_with(|| other.distance.total_cmp(&self.distance))
                .then_with(|| self.depth.total_cmp(&other.depth))
                .is_gt()
        }
    }
}

#[derive(PartialEq)]
struct FocusPose {
    center: Vec3,
    axes: [Vec3; 3],
    pan: [f64; 2],
    scale: f64,
    rect: Rect,
}

impl From<&OrbitFrame> for FocusPose {
    fn from(frame: &OrbitFrame) -> Self {
        Self { center: frame.camera.center, axes: frame.camera.axes, pan: frame.camera.pan, scale: frame.camera.scale, rect: frame.rect }
    }
}

pub(super) struct OrbitFocus {
    document: u64,
    pose: FocusPose,
    changed_at: f64,
    at: egui::Pos2,
    surface: Option<Vec3>,
    sample_source: Option<Weak<PointCloud>>,
}

struct FocusHint {
    at: egui::Pos2,
    surface: Option<Vec3>,
}

const FOCUS_RADIUS: f32 = 48.0;

fn detail_view(
    app: &mut CadApp,
    ui: &mut egui::Ui,
    root: &Arc<PointCloud>,
    frame: &OrbitFrame,
    document: u64,
    dragging: bool,
    hint: Option<FocusHint>,
) -> CloudView {
    let snapshot = view(root, frame.bounds, app, false);
    if root.detail.is_none() {
        return snapshot;
    }
    let now = ui.input(|input| input.time);
    let pose = FocusPose::from(frame);
    let mut state = app.canvas.cloud_orbit_focus.take().filter(|state| state.document == document);
    if hint.is_some() || state.as_ref().is_none_or(|state| state.pose != pose) {
        let at = hint
            .as_ref()
            .map(|hint| hint.at)
            .or_else(|| state.as_ref().map(|state| state.at))
            .filter(|at| frame.rect.contains(*at))
            .unwrap_or_else(|| frame.rect.center());
        let surface = hint
            .and_then(|hint| hint.surface)
            .or_else(|| state.as_ref().and_then(|state| state.surface).filter(|point| frame.project(*point).0.distance(at) <= FOCUS_RADIUS));
        state = Some(OrbitFocus { document, pose, changed_at: now, at, surface, sample_source: None });
    }
    let Some(mut state) = state else { return snapshot };
    if dragging || now - state.changed_at < 0.12 {
        app.canvas.cloud_orbit_focus = Some(state);
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(120));
        return snapshot;
    }
    let overview = if snapshot.indexing { root } else { snapshot.overview.as_ref().or(snapshot.points.as_ref()).unwrap_or(root) };
    let samples = Arc::downgrade(overview);
    if state.sample_source.as_ref().is_none_or(|previous| state.surface.is_none() && !Weak::ptr_eq(previous, &samples)) {
        let size = app.session.settings.point_cloud_size;
        if let Some(point) = frame.pick(layers(&snapshot, root), state.at, 160_000, FOCUS_RADIUS, size) {
            state.surface = Some(frame.on_surface(state.at, point));
        }
        state.sample_source = Some(samples);
    }
    let bounds = state.surface.map_or(Bounds2::EMPTY, |point| frame.detail_bounds(frame.on_surface(state.at, point)));
    app.canvas.cloud_orbit_focus = Some(state);
    let _ = view(root, bounds, app, true);
    // Fetch the composed layers against the actual render footprint, not the load zone.
    view(root, frame.bounds, app, false)
}

pub(super) fn orbit(app: &mut CadApp, ui: &mut egui::Ui) {
    let Ok(state) = app.session.state() else { return };
    let Some(mut orbit) = state.point_cloud_orbit else { return };
    let plan = state.view();
    let document = state.uid;
    let Some(root) = cloud(app).cloned() else { return };
    let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Image, true, crate::i18n::t("Point cloud 3D view")));
    if response.drag_started() || response.clicked() {
        response.request_focus();
    }
    let focused = response.has_focus();
    let hovered = response.hovered();
    let (keys, scroll, escape, press, pointer) = ui.input(|input| {
        let key = |k| f32::from(focused && input.key_pressed(k));
        let keys = vec2(key(Key::ArrowRight) - key(Key::ArrowLeft), key(Key::ArrowDown) - key(Key::ArrowUp));
        (
            keys,
            if hovered { input.smooth_scroll_delta.y } else { 0.0 },
            focused && input.key_pressed(Key::Escape),
            input.pointer.button_pressed(PointerButton::Middle),
            input.pointer.interact_pos(),
        )
    });
    if escape {
        let _ = app.run("3dorbit", json!({ "enabled": false }));
        return;
    }
    let mut frame = OrbitFrame::new(orbit, plan, &root, rect);
    let mut updated = false;
    let mut hint = None;
    if press && let Some(cursor) = pointer.filter(|p| rect.contains(*p)) {
        let snapshot = view(&root, frame.bounds, app, false);
        let gpu = layers(&snapshot, &root).all(|shown| crate::cloud_gpu::available(app.canvas.gpu, shown));
        let limit = if gpu { usize::MAX } else { sample_budget(rect) };
        let size = app.session.settings.point_cloud_size;
        if let Some(pivot) = frame.pick(layers(&snapshot, &root), cursor, limit, size.max(6.0), size) {
            let delta = pivot - frame.camera.center;
            orbit.pan += Vec2::new(frame.camera.axes[0].dot(delta), frame.camera.axes[1].dot(delta));
            orbit.pivot = Some(pivot);
            hint = Some(FocusHint { at: cursor, surface: Some(pivot) });
            updated = true;
        }
    }
    let motion = response.drag_motion();
    let rotation = if response.dragged_by(PointerButton::Middle) { vec2(motion.x, -motion.y) } else { egui::Vec2::ZERO } + keys * 10.0;
    if rotation != egui::Vec2::ZERO {
        orbit.yaw += f64::from(rotation.x / rect.width().max(1.0)) * 220.0_f64.to_radians();
        orbit.tilt += f64::from(rotation.y / rect.height().max(1.0)) * 220.0_f64.to_radians();
        updated = true;
    }
    if response.dragged_by(PointerButton::Secondary) && motion != egui::Vec2::ZERO {
        orbit.pan += Vec2::new(f64::from(motion.x), -f64::from(motion.y)) / frame.camera.scale;
        hint = Some(FocusHint { at: rect.center(), surface: None });
        updated = true;
    }
    if scroll != 0.0
        && let Some(pointer) = pointer
    {
        let remembered =
            app.canvas.cloud_orbit_focus.as_ref().filter(|focus| focus.document == document && focus.at == pointer).and_then(|focus| focus.surface);
        let surface = remembered.or_else(|| {
            let snapshot = view(&root, frame.bounds, app, false);
            frame.pick(layers(&snapshot, &root), pointer, 160_000, FOCUS_RADIUS, app.session.settings.point_cloud_size)
        });
        hint = Some(FocusHint { at: pointer, surface: surface.map(|point| frame.on_surface(pointer, point)) });
        let at = pointer - rect.center();
        let old = orbit.zoom;
        orbit.zoom = (old * (f64::from(scroll) * 0.005).exp()).clamp(0.02, 10_000.0);
        let adjustment = (old / orbit.zoom - 1.0) / frame.camera.scale;
        orbit.pan += Vec2::new(f64::from(at.x), -f64::from(at.y)) * adjustment;
        updated = true;
    }
    if updated {
        let _ = app.run(
            "3dorbit",
            json!({ "yaw": orbit.yaw, "tilt": orbit.tilt, "zoom": orbit.zoom,
            "pan": [orbit.pan.x, orbit.pan.y], "pivot": orbit.pivot.map(|p| [p.x, p.y, p.z]) }),
        );
        if let Some(applied) = app.session.state().ok().and_then(|state| state.point_cloud_orbit) {
            frame = OrbitFrame::new(applied, plan, &root, rect);
        }
    }
    let dragging = response.dragged_by(PointerButton::Middle) || response.dragged_by(PointerButton::Secondary);
    let snapshot = detail_view(app, ui, &root, &frame, document, dragging, hint);
    if snapshot.pending {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(70));
    }
    let fallback = Tokens::get().canvas_ink;
    let size = app.session.settings.point_cloud_size;
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, Tokens::get().canvas);
    if layers(&snapshot, &root).all(|shown| crate::cloud_gpu::available(app.canvas.gpu, shown)) {
        crate::cloud_gpu::paint(&painter, rect, layers(&snapshot, &root).cloned(), &frame.camera, size, fallback);
        return;
    }
    let mut projected = Vec::new();
    for shown in layers(&snapshot, &root) {
        shown.visible(frame.bounds, sample_budget(rect), |point| {
            let (at, depth) = frame.project(point.position);
            if rect.contains(at) {
                projected.push((depth, at, color(shown, point, fallback)));
            }
        });
    }
    projected.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
    let mut mesh = egui::Mesh::default();
    for (_, position, color) in projected {
        mesh.add_colored_rect(Rect::from_center_size(position, egui::Vec2::splat(size)), color);
    }
    painter.add(Shape::mesh(mesh));
}

#[cfg(test)]
mod tests {
    use super::*;
    use cadcraft_engine::Session;
    use egui_kittest::kittest::{NodeT, Queryable};
    use std::sync::Arc;

    #[test]
    fn orbit_detail_uses_the_front_surface_at_an_off_center_zoom_anchor() {
        let pose = PointCloudOrbit { pivot: Some(Vec3::ZERO), yaw: 0.5, tilt: 1.1, ..Default::default() };
        let plan = cadcraft_engine::View { center: Vec2::ZERO, height: 80.0, twist: 0.0 };
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 500.0));
        let cloud = |points: &[Vec3]| {
            Arc::new(PointCloud::new(
                points.iter().map(|&position| CloudPoint { position, rgb: [0; 3], intensity: 0, classification: 0 }).collect(),
                points.len() as u64,
                false,
                false,
            ))
        };
        let extent = cloud(&[Vec3::new(-500.0, -500.0, -150.0), Vec3::new(500.0, 500.0, 150.0)]);
        let frame = OrbitFrame::new(pose, plan, &extent, rect);
        let cursor = pos2(620.0, 320.0);
        let front = frame.camera.axes[2];
        let ray = frame.ray_origin(cursor);
        let back = ray - front * 70.0;
        let nearer = ray + front * 70.0 + frame.camera.axes[0] * (1.0 / frame.camera.scale);
        let overview = cloud(&[back]);
        let detail = cloud(&[nearer]);

        let surface = frame.pick([&overview, &detail].into_iter(), cursor, usize::MAX, FOCUS_RADIUS, 4.0).unwrap();
        let focus = frame.on_surface(cursor, surface);
        assert!((focus - nearer).len() < 1.0);
        assert!(frame.detail_bounds(focus).center().dist(back.xy()) > 50.0);
        assert!(frame.project(focus).0.distance(cursor) < 0.01);
    }

    #[test]
    fn orbit_detail_follows_the_panned_surface_instead_of_the_whole_cloud_height() {
        let points = [Vec3::new(-500.0, -900.0, -800.0), Vec3::new(100.0, 700.0, 400.0)].map(|position| CloudPoint {
            position,
            rgb: [0; 3],
            intensity: 0,
            classification: 0,
        });
        let cloud = PointCloud::new(points.to_vec(), 2, false, false);
        let plan = cadcraft_engine::View { center: Vec2::ZERO, height: 60.0, twist: 0.0 };
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 500.0));
        let mut orbit = PointCloudOrbit { pivot: Some(Vec3::ZERO), yaw: 0.5, tilt: 1.3, ..Default::default() };
        let before = OrbitFrame::new(orbit, plan, &cloud, rect);
        let target = Vec3::new(32.0, -78.0, 75.0);
        let [right, up, _] = before.camera.axes;
        orbit.pan = Vec2::new(-right.dot(target), -up.dot(target));
        let frame = OrbitFrame::new(orbit, plan, &cloud, rect);
        assert!(frame.project(target).0.distance(rect.center()) < 0.01);
        let focus = frame.on_surface(rect.center(), target);
        assert!((focus - target).len() < 1e-8);
        assert!(frame.detail_bounds(focus).center().near(target.xy(), 1e-8));
        assert!(frame.bounds.center().dist(target.xy()) > frame.detail_bounds(focus).height());
    }

    #[test]
    fn a_visible_cloud_point_snaps_and_the_orbit_control_returns_to_plan() {
        let xy = Vec2::new(30.0, 45.0);
        let point = CloudPoint { position: Vec3::new(xy.x, xy.y, 10.0), rgb: [0; 3], intensity: 0, classification: 0 };
        let drawing = cadcraft_doc::Drawing {
            point_cloud: Some(Arc::new(PointCloud::new(vec![point], 1, false, false))),
            ..cadcraft_doc::Drawing::new_metric()
        };
        let mut session = Session::empty();
        session.open_drawing(drawing, "Survey", None);
        let app = CadApp::new(session, Default::default());
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 100.0));
        let xf = Xf { rect, center: xy, scale: 2.0, twist: 0.0 };
        assert_eq!(snap_visible(&app, &xf, xy + Vec2::X, 2.0), Some(xy));
        assert_eq!(snap_visible(&app, &xf, xy + Vec2::X * 4.0, 2.0), None);

        let mut harness =
            egui_kittest::Harness::builder().with_size(vec2(640.0, 300.0)).build_ui_state(|ui, app: &mut CadApp| super::super::show(app, ui), app);
        assert!(harness.get_by_label("Snap to cloud points").accesskit_node().toggled().is_some());
        harness.get_by_label("3D Orbit").click();
        harness.run();
        let orbit = |app: &CadApp| app.session.state().unwrap().point_cloud_orbit.unwrap();
        let before = orbit(harness.state());
        let center = harness.get_by_label("Point cloud 3D view").rect().center();
        let button = |pos, button, pressed| egui::Event::PointerButton { pos, button, pressed, modifiers: egui::Modifiers::NONE };
        harness.event(egui::Event::PointerMoved(center));
        harness.event(button(center, PointerButton::Middle, true));
        harness.step();
        let at = center + vec2(34.0, 20.0);
        harness.event(egui::Event::PointerMoved(at));
        harness.step();
        let rotated = orbit(harness.state());
        assert_ne!(rotated.yaw, before.yaw);
        assert!(rotated.tilt < before.tilt, "dragging down tilts the image down");
        harness.event(button(at, PointerButton::Middle, false));
        harness.step();

        harness.event(button(at, PointerButton::Secondary, true));
        harness.step();
        let end = at + vec2(40.0, -22.0);
        harness.event(egui::Event::PointerMoved(end));
        harness.step();
        let panned = orbit(harness.state());
        assert_eq!(panned.yaw, rotated.yaw);
        assert_ne!(panned.pan, rotated.pan);
        harness.event(button(end, PointerButton::Secondary, false));
        harness.step();
        harness.get_by_label("3D Orbit").click();
        harness.run();
        assert!(harness.state().session.state().unwrap().point_cloud_orbit.is_none());
    }
}
