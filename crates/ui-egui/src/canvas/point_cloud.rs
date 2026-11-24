use super::orbit_scene::{self, Extent, Model};
use super::{CadApp, Color32, Rect, Shape, Tokens, Vec2, Xf, pos2, vec2};
use crate::cloud_gpu::{CloudEdits, PointStyle};
use cadcraft_doc::Space;
use cadcraft_doc::point_cloud::{CloudPoint, CloudRegion, CloudView, PointCloud};
use cadcraft_engine::OrbitView;
use cadcraft_geom::{Bounds2, Vec3};
use egui::{Key, PointerButton, Sense};
use serde_json::json;
use std::sync::{Arc, Weak};

pub(super) fn navigation_bar(app: &mut CadApp, ui: &mut egui::Ui) {
    let orbit = app.session.state().ok().and_then(|state| state.orbit);
    let has_cloud = cloud(app).is_some();
    if !has_cloud && orbit.is_none() {
        return;
    }
    let mut content = ui.available_rect_before_wrap();
    if let Some(toggle) = app.canvas.orbit_button {
        content.max.x = (toggle.left() - 8.0).max(content.left());
    }
    ui.scope_builder(egui::UiBuilder::new().max_rect(content), |ui| {
        if has_cloud {
            controls(app, ui);
        }
        if let Some(orbit) = orbit {
            orbit_controls(app, ui, orbit);
        }
    });
}

pub(super) fn orbit_button_rect(workspace: Rect) -> Option<Rect> {
    let size = vec2(28.0, 28.0);
    let rect = Rect::from_min_size(workspace.right_top() + vec2(-size.x - 8.0, 8.0), size);
    workspace.contains_rect(rect).then_some(rect)
}

pub(super) fn show_orbit_button(app: &mut CadApp, ui: &mut egui::Ui) {
    let Some(rect) = app.canvas.orbit_button else { return };
    let active = app.session.state().is_ok_and(|state| state.orbit.is_some());
    ui.painter().rect_filled(rect, 3.0, Tokens::get().panel);
    let mut child = ui.new_child(egui::UiBuilder::new().id_salt("orbit_button").max_rect(rect).layout(egui::Layout::top_down(egui::Align::Min)));
    let button = crate::icons::button(&mut child, crate::icons::Icon::Orbit, rect.width(), "3D Orbit", active);
    if button.clicked() {
        let _ = app.run("3dorbit", json!({ "enabled": !active }));
    }
}

fn controls(app: &mut CadApp, ui: &mut egui::Ui) {
    let orbit = app.session.state().is_ok_and(|state| state.orbit.is_some());
    let streaming = cloud(app).is_some_and(|cloud| cloud.detail.is_some());
    let status = cloud(app).map(|cloud| view(cloud, cloud.bounds(), app, false)).unwrap_or_default();
    if status.pending {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(125));
    }
    ui.horizontal_wrapped(|ui| {
        let mut size = app.session.settings.point_cloud_size;
        let mut snap = app.session.settings.point_cloud_snap;
        if ui.add(egui::Slider::new(&mut size, 1.0..=12.0).step_by(0.5).text(crate::i18n::t("Point size"))).changed() {
            let _ = app.run("pointcloud.settings", json!({ "size": size }));
        }
        if ui.checkbox(&mut snap, crate::i18n::t("Snap to cloud points")).changed() {
            let _ = app.run("pointcloud.settings", json!({ "snap": snap }));
        }
        let selecting = super::cloud_selection::enabled(app);
        let label = "Select cloud points";
        let choice = ui
            .selectable_label(selecting, crate::i18n::t(label))
            .on_hover_text(crate::i18n::t("Drag a rectangle or click its two corners to select points through the cloud."));
        crate::control::record_widget(ui, &choice, label, "button", Some(selecting));
        if choice.clicked() {
            let _ = app.run("pointcloud.select", json!({ "enabled": !selecting }));
        }
        if app.session.state().is_ok_and(|state| state.cloud_selection.is_some()) {
            let label = "Copy Cloud Points with Base Point...";
            let copy = ui
                .button(crate::i18n::t(label))
                .on_hover_text(crate::i18n::t("Copy the selected points as CAD objects for pasting into another drawing."));
            crate::control::record_widget(ui, &copy, label, "button", None);
            if copy.clicked() {
                app.start("pointcloud.copybase");
                if app.session.prompting()
                    && let Some(id) = app.canvas.orbit_view_id.or(app.canvas.plan_view_id)
                {
                    ui.memory_mut(|memory| memory.request_focus(id));
                }
            }
            let label = "Delete cloud selection";
            let button =
                ui.button(crate::i18n::t(label)).on_hover_text(crate::i18n::t("Remove the selected area in this drawing. You can undo this."));
            crate::control::record_widget(ui, &button, label, "button", None);
            if button.clicked() {
                let _ = app.run("erase.selection", json!({}));
            }
        }
        let label = "Export LAZ...";
        let export = ui.button(crate::i18n::t(label)).on_hover_text(crate::i18n::t("Save the edited point cloud as a full-resolution LAZ file."));
        crate::control::record_widget(ui, &export, label, "button", None);
        if export.clicked() {
            app.start("pointcloud.export");
        }
        if streaming {
            ui.menu_button(crate::i18n::t("Detail loading"), |ui| {
                let mut radius = app.session.settings.point_cloud_radius_factor;
                let mut block_points = app.session.settings.point_cloud_block_points;
                if crate::opening::detail_controls(ui, &mut radius, &mut block_points) {
                    let _ = app.run("pointcloud.settings", json!({ "detailRadiusFactor": radius, "blockPoints": block_points }));
                }
                ui.label(crate::i18n::t("Zero uses automatic loading. At close zoom, 0.10× loads at least 30 drawing units on each side."));
                let absolute = app.session.settings.point_cloud_detail_radius;
                if absolute > 0.0 {
                    ui.label(crate::i18n::t("Saved absolute radius: {radius} drawing units").replace("{radius}", &absolute.to_string()));
                }
                if ui.button(crate::i18n::t("Fast close-up")).clicked() {
                    let _ = app.run("pointcloud.settings", json!({ "detailRadius": 0.0, "detailRadiusFactor": 0.02, "blockPoints": 50_000 }));
                }
                if ui.button(crate::i18n::t("Automatic")).clicked() {
                    let _ =
                        app.run("pointcloud.settings", json!({ "detailRadius": 0.0, "detailRadiusFactor": 0.0, "blockPoints": 0, "batchPoints": 0 }));
                }
            });
        }
        if status.failed {
            ui.label(crate::i18n::t("Could not load point cloud detail"));
        } else if status.pending {
            ui.label(crate::i18n::t("Loading point cloud detail…"));
        } else if let Some(count) = status.detail_count {
            let message = if status.detail_limited {
                crate::i18n::t("Detail memory limit: {count} points").replace("{count}", &count.to_string())
            } else if count == 0 {
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

pub(super) fn removals(session: &cadcraft_engine::Session) -> &[CloudRegion] {
    session.doc().map_or(&[], |doc| doc.point_cloud_removals.as_slice())
}

fn point_style(app: &CadApp, moving: bool) -> PointStyle {
    let tokens = Tokens::get();
    let edits = app.session.state().map_or_else(
        |_| CloudEdits::default(),
        |state| CloudEdits { selected: state.cloud_selection, removed: state.doc.point_cloud_removals.clone(), highlight: tokens.selection },
    );
    PointStyle { size: app.session.settings.point_cloud_size, fallback: tokens.canvas_ink, moving, edits }
}

fn displayed_color(style: &PointStyle, cloud: &PointCloud, point: &CloudPoint) -> Option<Color32> {
    if style.edits.removed.iter().any(|region| region.contains(point.position)) {
        return None;
    }
    if style.edits.selected.is_some_and(|region| region.contains(point.position)) {
        Some(style.edits.highlight)
    } else {
        Some(color(cloud, point, style.fallback))
    }
}

fn view(cloud: &PointCloud, bounds: Bounds2, app: &CadApp, request: bool) -> CloudView {
    cloud.detail.as_ref().map_or_else(CloudView::default, |details| {
        if request { details.0.view_with(bounds, app.session.settings.cloud_detail_options()) } else { details.0.current(bounds) }
    })
}

pub(super) fn layers<'a>(snapshot: &'a CloudView, root: &'a Arc<PointCloud>) -> impl Iterator<Item = &'a Arc<PointCloud>> {
    snapshot.layers(root)
}

pub(super) fn plan_camera(xf: &Xf) -> crate::cloud_gpu::Camera {
    let (sin, cos) = xf.twist.sin_cos();
    crate::cloud_gpu::Camera {
        center: xf.center.to3(0.0),
        axes: [Vec3::new(cos, -sin, 0.0), Vec3::new(sin, cos, 0.0), Vec3::Z],
        pan: [0.0; 2],
        scale: xf.scale,
        depth: None,
        eye: None,
        bounds: xf.world_bounds(),
    }
}

pub(super) fn paint_plan(app: &CadApp, painter: &egui::Painter, xf: &Xf, request: bool) {
    let Some(cloud) = cloud(app) else { return };
    let bounds = xf.world_bounds();
    let snapshot = view(cloud, bounds, app, request);
    if request && snapshot.pending {
        painter.ctx().request_repaint_after(std::time::Duration::from_millis(125));
    }
    let style = point_style(app, false);
    if request && layers(&snapshot, cloud).all(|shown| crate::cloud_gpu::available(app.canvas.gpu, shown)) {
        crate::cloud_gpu::paint(painter, xf.rect, layers(&snapshot, cloud).cloned(), &plan_camera(xf), style);
        return;
    }
    let mut mesh = egui::Mesh::default();
    let size = egui::Vec2::splat(style.size);
    let limit = sample_budget(xf.rect).div_ceil(layers(&snapshot, cloud).count().max(1));
    for shown in layers(&snapshot, cloud) {
        shown.visible(bounds, limit, |point| {
            let screen = xf.to_screen(point.position.xy());
            if xf.rect.contains(screen)
                && let Some(color) = displayed_color(&style, shown, point)
            {
                mesh.add_colored_rect(Rect::from_center_size(screen, size), color);
            }
        });
    }
    painter.add(Shape::mesh(mesh));
}

pub(super) fn snap_visible(app: &CadApp, xf: &Xf, raw: Vec2, aperture: f64) -> Option<Vec3> {
    let cloud = cloud(app)?;
    let bounds = xf.world_bounds();
    let snapshot = view(cloud, bounds, app, false);
    let removed = removals(&app.session);
    let mut nearest = None;
    let mut distance = aperture.max(f64::from(app.session.settings.point_cloud_size) * 0.5 / xf.scale).powi(2);
    let gpu = layers(&snapshot, cloud).all(|shown| crate::cloud_gpu::available(app.canvas.gpu, shown));
    let fallback_limit = sample_budget(xf.rect).div_ceil(layers(&snapshot, cloud).count().max(1));
    for shown in layers(&snapshot, cloud) {
        let (pick, limit) = if gpu { (Bounds2::new(raw, raw).expand(distance.sqrt()), usize::MAX) } else { (bounds, fallback_limit) };
        shown.visible(pick, limit, |point| {
            if removed.iter().any(|region| region.contains(point.position)) {
                return;
            }
            let xy = point.position.xy();
            let delta = xy - raw;
            let d = delta.dot(delta);
            if (d < distance || (d == distance && nearest.is_none_or(|p: Vec3| p.z < point.position.z))) && xf.rect.contains(xf.to_screen(xy)) {
                distance = d;
                nearest = Some(point.position);
            }
        });
    }
    nearest
}

/// Camera math stays in double precision even for survey coordinates far from the origin.
pub(super) struct OrbitFrame {
    pub camera: crate::cloud_gpu::Camera,
    pub rect: Rect,
    bounds: Bounds2,
}

impl OrbitFrame {
    #[cfg(test)]
    fn new(orbit: OrbitView, plan: cadcraft_engine::View, scene: Extent, rect: Rect) -> Self {
        Self::framed(orbit, plan, scene, scene, rect)
    }

    fn framed(orbit: OrbitView, plan: cadcraft_engine::View, reference: Extent, scene: Extent, rect: Rect) -> Self {
        let extent = scene.xy;
        let plan_center =
            if reference.xy.is_empty() || (!orbit.fit && reference.xy.contains(plan.center)) { plan.center } else { reference.xy.center() };
        let z_mid = reference.center().z;
        let center = orbit.pivot.unwrap_or(Vec3::new(plan_center.x, plan_center.y, z_mid));
        let diagonal = if reference.xy.is_empty() { plan.height } else { reference.span() }.max(1e-6);
        let fit = f64::from(rect.width().min(rect.height()).max(1.0)) / (diagonal * if orbit.perspective { 1.55 } else { 1.05 });
        let plan_scale = f64::from(rect.height().max(1.0)) / plan.height.max(1e-6);
        let scale = if orbit.fit { fit } else { plan_scale.max(fit) } * orbit.zoom;
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
            for height in scene.z {
                for edge in [-y, y] {
                    limits.add(Vec2::new(0.0, (edge - orbit.pan.y - (height - center.z) * sin) / cos));
                }
            }
            (limits.min.y.max(rotated.min.y), limits.max.y.min(rotated.max.y))
        };
        let min = Vec2::new((-x - orbit.pan.x).max(rotated.min.x), y_min);
        let max = Vec2::new((x - orbit.pan.x).min(rotated.max.x), y_max);
        let bounds = if orbit.perspective {
            extent
        } else if min.x <= max.x && min.y <= max.y {
            Bounds2::from_points(Bounds2::new(min, max).corners().map(|p| center.xy() + p.rotate(-orbit.yaw)))
        } else {
            Bounds2::EMPTY
        };
        let reference_depth = diagonal + (center - Vec3::new(plan_center.x, plan_center.y, z_mid)).len();
        let depth = reference_depth.max(scene.span() + (center - scene.center()).len());
        let eye = orbit.perspective.then_some(reference_depth * 1.6);
        let camera = crate::cloud_gpu::Camera { center, axes, pan: [orbit.pan.x, orbit.pan.y], scale, depth: Some(depth), eye, bounds };
        Self { camera, rect, bounds }
    }

    pub(super) fn project(&self, point: Vec3) -> (egui::Pos2, f64) {
        let point = point - self.camera.center;
        let [x, y, z] = self.camera.axes;
        let [px, py] = self.camera.pan;
        let at = self.rect.center();
        let depth = z.dot(point);
        let scale = self.camera.scale / self.camera.weight(depth).max(0.01);
        (pos2(at.x + ((x.dot(point) + px) * scale) as f32, at.y - ((y.dot(point) + py) * scale) as f32), depth)
    }

    pub(super) fn interpolate<const N: usize>(&self, points: [Vec3; N], weights: [f64; N]) -> Option<Vec3> {
        let front = self.camera.axes[2];
        let mut sum = 0.0;
        let mut point = Vec3::ZERO;
        for (p, weight) in points.into_iter().zip(weights) {
            let w = self.camera.weight(front.dot(p - self.camera.center));
            if w <= 0.01 {
                return None;
            }
            let weight = weight / w;
            point = point + p * weight;
            sum += weight;
        }
        (sum > 0.0).then(|| point * (1.0 / sum))
    }

    fn ray_origin(&self, at: egui::Pos2) -> Vec3 {
        let [right, up, _] = self.camera.axes;
        let offset = at - self.rect.center();
        let [x, y] = self.camera.pan;
        self.camera.center + right * (f64::from(offset.x) / self.camera.scale - x) - up * (f64::from(offset.y) / self.camera.scale + y)
    }

    fn ray_direction(&self, at: egui::Pos2) -> Vec3 {
        let [right, up, front] = self.camera.axes;
        let offset = at - self.rect.center();
        self.camera.eye.map_or(front, |eye| front - (right * f64::from(offset.x) - up * f64::from(offset.y)) * (1.0 / (eye * self.camera.scale)))
    }

    /// A view ray meeting the horizontal drafting plane at the current elevation.
    pub(super) fn on_draft_plane(&self, at: egui::Pos2, z: f64) -> Option<Vec3> {
        self.on_plane(at, Vec3::new(0.0, 0.0, z), Vec3::Z).map(|p| p.xy().to3(z))
    }

    pub(super) fn on_plane(&self, at: egui::Pos2, reference: Vec3, normal: Vec3) -> Option<Vec3> {
        let origin = self.ray_origin(at);
        let direction = self.ray_direction(at);
        let den = direction.dot(normal);
        if den.abs() <= 1e-9 {
            return None;
        }
        let depth = (reference - origin).dot(normal) / den;
        let p = origin + direction * depth;
        (p.is_finite() && self.camera.weight(depth) > 0.01).then_some(p)
    }

    /// Locate a screen pixel on the plane at the depth of a visible sample.
    fn on_surface(&self, at: egui::Pos2, point: Vec3) -> Vec3 {
        let front = self.camera.axes[2];
        self.ray_origin(at) + self.ray_direction(at) * front.dot(point - self.camera.center)
    }

    fn detail_bounds(&self, surface: Option<Vec3>) -> Bounds2 {
        // Loading is centered on the geometry under the view. The wide footprint belonging
        // to the cloud's entire elevation range is only used for render culling.
        let focus = self.on_surface(self.rect.center(), surface.unwrap_or(self.camera.center));
        let depth = self.camera.axes[2].dot(focus - self.camera.center);
        let half = Vec2::new(f64::from(self.rect.width()), f64::from(self.rect.height())) * (self.camera.weight(depth) / (2.0 * self.camera.scale));
        let margin = half.x.max(half.y) * (1.0 - self.camera.axes[2].z.abs());
        let span = half + Vec2::new(margin, margin);
        Bounds2::new(focus.xy() - span, focus.xy() + span)
    }

    pub(super) fn pick_bounds(&self, heights: [f64; 2], cursor: egui::Pos2, radius: f32) -> Bounds2 {
        let front = self.camera.axes[2];
        if front.z.abs() < 0.001 {
            return self.bounds;
        }
        let corners = Rect::from_center_size(cursor, egui::Vec2::splat(radius * 2.0)).intersect(self.rect);
        let mut bounds = Bounds2::EMPTY;
        for at in [corners.left_top(), corners.right_top(), corners.left_bottom(), corners.right_bottom()] {
            let origin = self.ray_origin(at);
            let direction = self.ray_direction(at);
            if direction.z * front.z <= 1e-6 {
                return self.bounds;
            }
            for z in heights {
                bounds.add((origin + direction * ((z - origin.z) / direction.z)).xy());
            }
        }
        // Leave a margin for roundoff at a pixel or tile edge at survey coordinates.
        bounds.expand(0.001)
    }

    #[cfg(test)]
    fn pick<'a>(&self, clouds: impl Iterator<Item = &'a Arc<PointCloud>>, cursor: egui::Pos2, limit: usize, radius: f32, size: f32) -> Option<Vec3> {
        self.pick_where(clouds, cursor, limit, radius, size, |_| true)
    }

    pub(super) fn pick_where<'a>(
        &self,
        clouds: impl Iterator<Item = &'a Arc<PointCloud>>,
        cursor: egui::Pos2,
        limit: usize,
        radius: f32,
        size: f32,
        accepts: impl Fn(Vec3) -> bool,
    ) -> Option<Vec3> {
        let mut best: Option<SurfaceHit> = None;
        let clouds: Vec<_> = clouds.collect();
        let wide_limit = limit.div_ceil(clouds.len().max(1));
        for cloud in clouds {
            let bounds = self.pick_bounds(cloud.z_range, cursor, radius);
            let limit = if bounds == self.bounds { wide_limit } else { limit };
            cloud.visible(bounds, limit, |p| {
                if !accepts(p.position) {
                    return;
                }
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
    center_surface: Option<Vec3>,
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
    scene: (&Arc<PointCloud>, Option<&Model>),
    frame: &OrbitFrame,
    document: u64,
    dragging: bool,
    hint: Option<FocusHint>,
) -> CloudView {
    let (root, model) = scene;
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
        state = Some(OrbitFocus { document, pose, changed_at: now, at, surface, center_surface: None, sample_source: None });
    }
    let Some(mut state) = state else { return snapshot };
    if dragging || now - state.changed_at < 0.12 {
        if snapshot.block_streaming {
            let _ = view(root, frame.detail_bounds(state.center_surface), app, true);
        }
        app.canvas.cloud_orbit_focus = Some(state);
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(120));
        return snapshot;
    }
    let overview = if snapshot.indexing { root } else { snapshot.overview.as_ref().or(snapshot.points.as_ref()).unwrap_or(root) };
    let samples = Arc::downgrade(overview);
    let at = frame.rect.center();
    if state.sample_source.as_ref().is_none_or(|previous| state.center_surface.is_none() && !Weak::ptr_eq(previous, &samples)) {
        let tilted = frame.camera.axes[2].z.abs() < 1.0 - 1e-6 || frame.camera.eye.is_some();
        if tilted {
            state.center_surface = pick_target(app, Some(root), model, frame, at, FOCUS_RADIUS);
        }
        if state.at == at
            && let Some(surface) = state.center_surface
        {
            state.surface = Some(frame.on_surface(at, surface));
        }
        state.sample_source = Some(samples);
    }
    // The scroll anchor can be under the pointer. Detail follows the center of the
    // view, with the camera target's depth as a fallback between sparse samples.
    let bounds = frame.detail_bounds(state.center_surface);
    app.canvas.cloud_orbit_focus = Some(state);
    let _ = view(root, bounds, app, true);
    // Fetch the composed layers against the actual render footprint, not the load zone.
    view(root, frame.bounds, app, false)
}

fn orbit_controls(app: &mut CadApp, ui: &mut egui::Ui, orbit: OrbitView) {
    ui.horizontal_wrapped(|ui| {
        let fit = ui.button(crate::i18n::t("Fit model"));
        crate::control::record_widget(ui, &fit, "Fit model", "button", None);
        if fit.clicked() {
            let _ = app.run("3dorbit", json!({ "fit": true }));
        }
        ui.menu_button(crate::i18n::t("View"), |ui| {
            for (label, view) in [
                ("Top", "top"),
                ("Bottom", "bottom"),
                ("Front", "front"),
                ("Back", "back"),
                ("Left", "left"),
                ("Right", "right"),
                ("Isometric", "isometric"),
            ] {
                if ui.button(crate::i18n::t(label)).clicked() {
                    let _ = app.run("3dorbit", json!({ "view": view, "perspective": false }));
                    ui.close();
                }
            }
        });
        let mut perspective = orbit.perspective;
        let toggle = ui.checkbox(&mut perspective, crate::i18n::t("Perspective"));
        crate::control::record_widget(ui, &toggle, "Perspective", "checkbox", Some(perspective));
        if toggle.changed() {
            let _ = app.run("3dorbit", json!({ "perspective": perspective }));
        }
        let mut wireframe = orbit.wireframe;
        let toggle = ui.checkbox(&mut wireframe, crate::i18n::t("Wireframe"));
        crate::control::record_widget(ui, &toggle, "Wireframe", "checkbox", Some(wireframe));
        if toggle.changed() {
            let _ = app.run("3dorbit", json!({ "wireframe": wireframe }));
        }
    });
}

fn pick_target(
    app: &CadApp,
    root: Option<&Arc<PointCloud>>,
    model: Option<&Model>,
    frame: &OrbitFrame,
    cursor: egui::Pos2,
    radius: f32,
) -> Option<Vec3> {
    let size = app.session.settings.point_cloud_size;
    let geometry = model.and_then(|model| model.pick(frame, cursor, radius));
    let point = root.and_then(|root| {
        let snapshot = view(root, frame.bounds, app, false);
        let removed = removals(&app.session);
        frame.pick_where(layers(&snapshot, root), cursor, 160_000, radius, size, |p| !removed.iter().any(|region| region.contains(p)))
    });
    match (geometry, point) {
        (Some(a), Some(b)) => {
            let (pa, za) = frame.project(a);
            let (pb, zb) = frame.project(b);
            let db = pb.distance(cursor);
            let da = pa.distance(cursor);
            let offset = (pb - cursor).abs();
            let cloud_covers = offset.x.max(offset.y) <= 0.5 * size;
            Some(if da <= 0.5 && cloud_covers && za != zb {
                if za > zb { a } else { b }
            } else if da <= db {
                a
            } else {
                b
            })
        }
        _ => geometry.or(point),
    }
}

pub(super) fn orbit(app: &mut CadApp, ui: &mut egui::Ui) {
    app.canvas.orbit_view_id = None;
    let Ok(state) = app.session.state() else { return };
    let Some(mut orbit) = state.orbit else { return };
    let plan = state.view();
    let document = state.uid;
    let root = cloud(app).cloned();
    let model = orbit_scene::model(app, ui.ctx(), f64::from(ui.available_height().max(1.0)));
    if let Some(model) = &model {
        if model.list.truncated {
            ui.label(crate::i18n::t("The orbit display limit was reached. Some geometry is omitted."));
        }
        if model.missing != 0 {
            ui.label(crate::i18n::t("Some imported objects do not have geometry CADCraft can display."));
        }
    }
    let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
    app.canvas.rect = Some(rect);
    app.canvas.xf = None;
    app.canvas.cursor = None;
    app.canvas.snap = None;
    app.canvas.orbit_cursor = None;
    app.canvas.draft_cursor = None;
    app.session.spatial_cursor = None;
    let label = if model.is_none() && root.is_some() { "Point cloud 3D view" } else { "Model 3D view" };
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Image, true, crate::i18n::t(label)));
    crate::control::record_widget(ui, &response, label, "image", None);
    let response = response.on_hover_text(crate::i18n::t(
        "Middle drag or arrow keys to rotate. Right drag or Shift+arrows to pan. Scroll or +/− to zoom. Home fits the model. Esc exits.",
    ));
    if response.drag_started() || response.clicked() {
        response.request_focus();
    }
    let focused = response.has_focus();
    let lost_focus = response.lost_focus();
    app.canvas.orbit_view_id = Some(response.id);
    let hovered = response.hovered() && !response.hover_pos().is_some_and(|at| app.canvas.over_orbit_button(at));
    let (arrows, keyboard_pan, scroll, escape, fit, press, pointer) = ui.input(|input| {
        let key = |k| f32::from(focused && input.key_pressed(k));
        let arrows = vec2(key(Key::ArrowRight) - key(Key::ArrowLeft), key(Key::ArrowDown) - key(Key::ArrowUp));
        (
            arrows,
            input.modifiers.shift,
            if hovered { input.smooth_scroll_delta.y } else { 0.0 },
            (focused || lost_focus) && input.key_pressed(Key::Escape),
            focused && input.key_pressed(Key::Home),
            input.pointer.button_pressed(PointerButton::Middle),
            input.pointer.interact_pos(),
        )
    });
    if escape {
        ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape));
        if app.session.prompting() || super::cloud_selection::active(app) {
            crate::cmdline::escape(app);
        } else {
            let _ = app.run("3dorbit", json!({ "enabled": false }));
        }
        return;
    }
    // Egui may already have used Enter to activate the focused canvas widget.
    // It is still a command-line key while the viewport owns focus.
    let enter = ui.ctx().current_pass_index() == 0
        && ui.input(|i| {
            i.raw.events.iter().any(|event| matches!(event, egui::Event::Key { key: Key::Enter, pressed: true, modifiers, .. } if !modifiers.any()))
        });
    if (focused || lost_focus) && app.session.prompting() && app.cmd.options.is_none() && enter {
        ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Enter));
        crate::cmdline::submit(app);
    }
    if fit {
        let _ = app.run("3dorbit", json!({ "fit": true }));
        return;
    }
    let key_zoom = ui.input(|input| {
        if focused && !app.session.prompting() {
            50.0 * (f32::from(input.key_pressed(Key::Plus) || input.key_pressed(Key::Equals)) - f32::from(input.key_pressed(Key::Minus)))
        } else {
            0.0
        }
    });
    let extent = model.as_ref().map_or_else(Extent::default, |model| model.extent);
    let extent = if let Some(root) = &root { extent.union(Extent { xy: root.bounds(), z: root.z_range }) } else { extent };
    // Drawing a new entity must not move the view under the pointer. Reframing
    // only occurs when an orbit starts, changes document, or fits explicitly.
    let framing = app.canvas.orbit_scene.framing(document, orbit.fit_revision, extent);
    let mut frame = OrbitFrame::framed(orbit, plan, framing, extent, rect);
    let mut updated = false;
    let mut hint = None;
    if press
        && let Some(cursor) = pointer.filter(|p| rect.contains(*p) && !app.canvas.over_orbit_button(*p))
        && let Some(pivot) = pick_target(app, root.as_ref(), model.as_ref(), &frame, cursor, 8.0)
    {
        // Changing the pivot must leave the picked point at the same pixel.
        let at = frame.project(pivot).0 - rect.center();
        orbit.pan = Vec2::new(f64::from(at.x), -f64::from(at.y)) / frame.camera.scale;
        orbit.pivot = Some(pivot);
        hint = Some(FocusHint { at: cursor, surface: Some(pivot) });
        updated = true;
    }
    let motion = response.drag_motion();
    let rotation = if response.dragged_by(PointerButton::Middle) { vec2(motion.x, -motion.y) } else { egui::Vec2::ZERO }
        + if keyboard_pan { egui::Vec2::ZERO } else { arrows * 10.0 };
    if rotation != egui::Vec2::ZERO {
        orbit.yaw += f64::from(rotation.x / rect.width().max(1.0)) * 220.0_f64.to_radians();
        orbit.tilt += f64::from(rotation.y / rect.height().max(1.0)) * 220.0_f64.to_radians();
        updated = true;
    }
    let pan = (if response.dragged_by(PointerButton::Secondary) { motion } else { egui::Vec2::ZERO })
        + if keyboard_pan { arrows * 20.0 } else { egui::Vec2::ZERO };
    if pan != egui::Vec2::ZERO {
        orbit.pan += Vec2::new(f64::from(pan.x), -f64::from(pan.y)) / frame.camera.scale;
        hint = Some(FocusHint { at: rect.center(), surface: None });
        updated = true;
    }
    let zoom_at = if scroll != 0.0 {
        pointer.map(|p| (p, scroll))
    } else if key_zoom != 0.0 {
        Some((rect.center(), key_zoom))
    } else {
        None
    };
    if let Some((pointer, steps)) = zoom_at {
        let remembered =
            app.canvas.cloud_orbit_focus.as_ref().filter(|focus| focus.document == document && focus.at == pointer).and_then(|focus| focus.surface);
        let surface = remembered.or_else(|| {
            // Orthographic CAD-only zoom is independent of depth. Avoid picking
            // the entire drawing on every frame of a smooth wheel gesture.
            if root.is_some() || orbit.perspective { pick_target(app, root.as_ref(), model.as_ref(), &frame, pointer, FOCUS_RADIUS) } else { None }
        });
        hint = Some(FocusHint { at: pointer, surface: surface.map(|point| frame.on_surface(pointer, point)) });
        let at = pointer - rect.center();
        let old = orbit.zoom;
        orbit.zoom = (old * (f64::from(steps) * 0.005).exp()).clamp(0.02, 10_000.0);
        let weight = surface.map_or(1.0, |p| frame.camera.weight(frame.project(p).1));
        let adjustment = (old / orbit.zoom - 1.0) * weight / frame.camera.scale;
        orbit.pan += Vec2::new(f64::from(at.x), -f64::from(at.y)) * adjustment;
        updated = true;
    }
    if updated {
        let _ = app.run(
            "3dorbit",
            json!({ "yaw": orbit.yaw, "tilt": orbit.tilt, "zoom": orbit.zoom,
            "pan": [orbit.pan.x, orbit.pan.y], "pivot": orbit.pivot.map(|p| [p.x, p.y, p.z]) }),
        );
        if let Some(applied) = app.session.state().ok().and_then(|state| state.orbit) {
            frame = OrbitFrame::framed(applied, plan, framing, extent, rect);
        }
    }
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, Tokens::get().canvas);
    let dragging = response.dragged_by(PointerButton::Middle) || response.dragged_by(PointerButton::Secondary);
    let can_select = !dragging && app.cmd.options.is_none();
    let cloud_select = super::cloud_selection::input(app, &response, rect, &frame.camera, can_select);
    let snapshot = root.as_ref().map(|root| detail_view(app, ui, (root, model.as_ref()), &frame, document, dragging, hint));
    if let Some(model) = &model {
        model.paint_images(&painter, &frame);
    }
    let shown = root.as_ref().zip(snapshot.as_ref());
    if let Some((root, snapshot)) = shown {
        if snapshot.pending {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(70));
        }
        paint_cloud_in_orbit(app, &painter, &frame, root, snapshot, dragging || updated);
    }
    // The model wins depth ties with the underlying cloud samples used for drawing.
    if let Some(model) = &model {
        model.paint(&painter, &frame, crate::orbit_gpu::available(app.canvas.gpu));
    }
    super::orbit_draft::draw(app, &painter, &frame, &response, model.as_ref(), shown, dragging || cloud_select);
    super::cloud_selection::draw(app, &painter, &response, rect);
}

fn paint_cloud_in_orbit(app: &CadApp, painter: &egui::Painter, frame: &OrbitFrame, root: &Arc<PointCloud>, snapshot: &CloudView, moving: bool) {
    let rect = frame.rect;
    let style = point_style(app, moving);
    if layers(snapshot, root).all(|shown| crate::cloud_gpu::available(app.canvas.gpu, shown)) {
        crate::cloud_gpu::paint(painter, rect, layers(snapshot, root).cloned(), &frame.camera, style);
        return;
    }
    let mut projected = Vec::new();
    let limit = sample_budget(rect).div_ceil(layers(snapshot, root).count().max(1));
    for shown in layers(snapshot, root) {
        shown.visible(frame.bounds, limit, |point| {
            let (at, depth) = frame.project(point.position);
            if rect.contains(at)
                && let Some(color) = displayed_color(&style, shown, point)
            {
                projected.push((depth, at, color));
            }
        });
    }
    projected.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
    let mut mesh = egui::Mesh::default();
    for (_, position, color) in projected {
        mesh.add_colored_rect(Rect::from_center_size(position, egui::Vec2::splat(style.size)), color);
    }
    painter.add(Shape::mesh(mesh));
}

#[cfg(test)]
mod tests {
    use super::*;
    use cadcraft_engine::Session;
    use egui_kittest::kittest::{NodeT, Queryable};
    use std::sync::Arc;

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn copy_with_base_point_accepts_xyz_and_snaps_to_cloud_height() {
        cadcraft_engine::cmd::file::set_point_cloud_reader(|input, step, progress| {
            cadcraft_io::point_cloud::read_with_overview_progress(input, step, None, progress).map_err(|error| error.to_string())
        });
        cadcraft_engine::cmd::file::cloud_copy::set_reader(|input, region, removals, limit| {
            cadcraft_io::point_cloud::read_selection(input, region, removals, limit).map_err(|error| error.to_string())
        });
        let source = std::env::temp_dir().join(format!("cadcraft-copy-base-{}.las", std::process::id()));
        // Three authored LAS 1.2 format-0 points; the middle one is on the centre pixel.
        let mut las = vec![0_u8; 227 + 3 * 20];
        las[..4].copy_from_slice(b"LASF");
        las[24..26].copy_from_slice(&[1, 2]);
        las[94..96].copy_from_slice(&227_u16.to_le_bytes());
        las[96..100].copy_from_slice(&227_u32.to_le_bytes());
        las[105..107].copy_from_slice(&20_u16.to_le_bytes());
        las[107..111].copy_from_slice(&3_u32.to_le_bytes());
        for (offset, value) in (131..227).step_by(8).zip([1.0_f64, 1.0, 1.0, 0.0, 0.0, 0.0, 20.0, -20.0, 20.0, -20.0, 32.0, 32.0]) {
            las[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        for (record, xy) in las[227..].as_chunks_mut::<20>().0.iter_mut().zip([-20_i32, 0, 20]) {
            record[..4].copy_from_slice(&xy.to_le_bytes());
            record[4..8].copy_from_slice(&xy.to_le_bytes());
            record[8..12].copy_from_slice(&32_i32.to_le_bytes());
        }
        std::fs::write(&source, las).unwrap();
        for orbit in [false, true] {
            let mut session = Session::empty();
            session.execute("open", &json!({ "path": source })).unwrap();
            session.execute("pointcloud.select", &json!({ "min": [-5, -5], "max": [5, 5] })).unwrap();
            session.settings.osmode = cadcraft_engine::snap::mode::END;
            session.settings.point_cloud_snap = true;
            if orbit {
                session.execute("3dorbit", &json!({ "view": "top", "fit": true })).unwrap();
            }
            let services =
                crate::Services { pick_save: Some(Box::new(|_| panic!("copying points must not show a save dialog"))), ..Default::default() };
            let app = CadApp::new(session, services);
            let mut harness = egui_kittest::Harness::builder().with_size(vec2(900.0, 500.0)).build_ui_state(
                |ui, app: &mut CadApp| {
                    crate::exchange::finish_cloud_copy(app);
                    super::super::show(app, ui);
                },
                app,
            );
            assert!(harness.get_by_label("Copy Cloud Points with Base Point...").rect().is_positive());
            harness.state_mut().start("copybase");
            harness.step();
            assert_eq!(harness.state().session.current_prompt().unwrap().message, "Specify base point (X,Y,Z)");
            let base = if orbit {
                let at = harness.state().canvas.rect.unwrap().center();
                harness.event(egui::Event::PointerMoved(at));
                harness.step();
                assert!(harness.state().canvas.orbit_cursor.unwrap().cloud_snap());
                for pressed in [true, false] {
                    harness.event(egui::Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE });
                    harness.step();
                }
                Vec3::new(0.0, 0.0, 32.0)
            } else {
                harness.event(egui::Event::Text("4,5,32".into()));
                harness.key_press(Key::Enter);
                Vec3::new(4.0, 5.0, 32.0)
            };
            harness.run();
            assert_eq!(harness.state().session.last_point3(), base);
            assert!(harness.state().session.state().unwrap().cloud_selection.is_some());
            let app = harness.state_mut();
            // The copy is bound to the source even if a new drawing is opened before it finishes.
            app.session.new_drawing(true);
            let result = app.cloud_copy_job.as_ref().unwrap().recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            let (send, receive) = std::sync::mpsc::channel();
            send.send(result).unwrap();
            app.cloud_copy_job = Some(receive);
            crate::exchange::poll_cloud_copy(app, &egui::Context::default());
            assert_eq!(app.session.clipboard.len(), 1, "{:?}", app.session.log);
            app.session.execute("pasteorig", &json!({})).unwrap();
            let pasted = app.session.doc().unwrap().model.iter().next().unwrap();
            assert!(matches!(&pasted.kind, cadcraft_doc::EntityKind::Point(point) if point.p == Vec3::new(0.0, 0.0, 32.0)));
        }
        std::fs::remove_file(source).unwrap();
    }

    #[test]
    fn cloud_toolbar_exposes_laz_export_and_dwg_export_can_pick_cad_in_both_views() {
        use std::{cell::RefCell, rc::Rc};

        let extent = Vec3::new(20.0, 20.0, 10.0);
        let samples = [-extent, extent].map(|position| CloudPoint { position, rgb: [255; 3], intensity: 0, classification: 0 });
        for orbit in [false, true] {
            let mut drawing = cadcraft_doc::Drawing {
                point_cloud: Some(Arc::new(PointCloud::new(samples.to_vec(), samples.len() as u64, true, true))),
                ..cadcraft_doc::Drawing::new_metric()
            };
            let line = cadcraft_doc::Line { a: Vec3::new(-20.0, 0.0, -5.0), b: Vec3::new(20.0, 0.0, 5.0) };
            let handle = drawing.add(&Space::Model, Default::default(), cadcraft_doc::EntityKind::Line(line)).unwrap();
            let mut session = Session::empty();
            session.open_drawing(drawing, "Survey", None);
            session.execute("pointcloud.select", &json!({ "enabled": true })).unwrap();
            if orbit {
                session.execute("3dorbit", &json!({ "view": "top", "fit": true })).unwrap();
            }
            let asked = Rc::new(RefCell::new(Vec::new()));
            let picked = asked.clone();
            let services = crate::Services {
                pick_save: Some(Box::new(move |name| {
                    picked.borrow_mut().push(name.to_owned());
                    None
                })),
                ..Default::default()
            };
            let app = CadApp::new(session, services);
            let mut harness = egui_kittest::Harness::builder().with_size(vec2(900.0, 500.0)).build_ui_state(
                |ui, app: &mut CadApp| {
                    crate::exchange::finish_selection_export(app);
                    super::super::show(app, ui);
                },
                app,
            );
            assert!(harness.get_by_label("Export LAZ...").rect().is_positive());

            harness.state_mut().start("export.selection");
            harness.step();
            assert!(harness.state().session.current_prompt().unwrap().accept.select);
            assert!(asked.borrow().is_empty());
            harness.key_press(Key::Escape);
            harness.run();
            assert!(asked.borrow().is_empty());

            harness.state_mut().start("export.selection");
            harness.step();
            let at = harness.state().canvas.rect.unwrap().center();
            harness.event(egui::Event::PointerMoved(at));
            harness.step();
            for pressed in [true, false] {
                harness.event(egui::Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE });
                harness.step();
            }
            assert_eq!(harness.state().session.selection(), [handle]);
            assert!(asked.borrow().is_empty());
            harness.key_press(Key::Enter);
            harness.run();
            assert_eq!(*asked.borrow(), ["Survey-selection.dwg"]);
        }
    }

    #[test]
    fn point_cloud_rectangle_and_delete_work_in_plan_and_orbit() {
        let center = Vec3::ZERO;
        let outside = Vec3::new(25.0, 20.0, 5.0);
        let points = [center, outside, -outside].map(|position| CloudPoint { position, rgb: [255; 3], intensity: 0, classification: 0 });
        for orbit in [false, true] {
            let drawing = cadcraft_doc::Drawing {
                point_cloud: Some(Arc::new(PointCloud::new(points.to_vec(), 3, true, true))),
                ..cadcraft_doc::Drawing::new_metric()
            };
            let mut session = Session::empty();
            session.open_drawing(drawing, "Cloud", None);
            if orbit {
                session.execute("3dorbit", &json!({ "enabled": true, "fit": true })).unwrap();
            }
            let app = CadApp::new(session, Default::default());
            let mut harness = egui_kittest::Harness::builder()
                .with_size(vec2(900.0, 500.0))
                .build_ui_state(|ui, app: &mut CadApp| super::super::show(app, ui), app);
            let middle = harness.state().canvas.rect.unwrap().center();
            let first = middle - vec2(12.0, 13.0);
            let last = middle + vec2(12.0, 13.0);
            let button = |pos, pressed| egui::Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
            let events = if orbit {
                vec![egui::Event::PointerMoved(first), button(first, true), egui::Event::PointerMoved(last), button(last, false)]
            } else {
                vec![
                    egui::Event::PointerMoved(first),
                    button(first, true),
                    button(first, false),
                    egui::Event::PointerMoved(last),
                    button(last, true),
                    button(last, false),
                ]
            };
            for event in events {
                harness.event(event);
                harness.step();
            }
            let state = harness.state().session.state().unwrap();
            let selected = state.cloud_selection.expect("the two corners select a cloud region");
            assert!(selected.contains(center));
            assert!(!selected.contains(outside));
            harness.key_press(Key::Delete);
            harness.run();
            let session = &mut harness.state_mut().session;
            assert_eq!(session.doc().unwrap().point_cloud_removals.as_slice(), [selected]);
            assert_eq!(session.execute("pointcloud.snap", &json!({ "point": [0,0], "aperture": 1 })).unwrap()["point"], serde_json::Value::Null);
            session.undo().unwrap();
            assert!(session.doc().unwrap().point_cloud_removals.is_empty());
            assert_eq!(session.execute("pointcloud.snap", &json!({ "point": [0,0], "aperture": 1 })).unwrap()["point"], json!([0.0, 0.0, 0.0]));
        }
    }

    #[test]
    fn drawing_an_orbit_line_by_clicking_cloud_samples_keeps_their_original_xyz() {
        let a = Vec3::new(-10.0, -5.0, 20.0);
        let b = Vec3::new(14.0, 6.0, 45.0);
        // Give the sample being traced room above the floating command bar.
        let samples = [a, b, Vec3::new(2.0, -42.0, 32.5), Vec3::new(2.0, 43.0, 32.5)];
        let root = Arc::new(PointCloud::new(
            samples.map(|position| CloudPoint { position, rgb: [0; 3], intensity: 0, classification: 0 }).to_vec(),
            4,
            false,
            false,
        ));
        let drawing = cadcraft_doc::Drawing { point_cloud: Some(root.clone()), ..cadcraft_doc::Drawing::new_metric() };
        let mut session = Session::empty();
        session.open_drawing(drawing, "Cloud", None);
        session.settings.osmode = cadcraft_engine::snap::mode::END;
        session.execute("3dorbit", &json!({"view": "isometric", "fit": true})).unwrap();
        let plan = session.state().unwrap().view();
        let pose = session.state().unwrap().orbit.unwrap();
        let app = CadApp::new(session, Default::default());
        let mut harness =
            egui_kittest::Harness::builder().with_size(vec2(900.0, 500.0)).build_ui_state(|ui, app: &mut CadApp| super::super::show(app, ui), app);
        let key = |key| egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
        harness.event(egui::Event::Text("LINE".into()));
        harness.event(key(Key::Enter));
        harness.run();
        assert_eq!(harness.state().session.running.as_ref().map(|running| running.id.as_str()), Some("line"));
        let rect = harness.get_by_label("Point cloud 3D view").rect();
        let frame = OrbitFrame::new(pose, plan, Extent { xy: root.bounds(), z: root.z_range }, rect);
        for point in [a, b] {
            let at = frame.project(point).0;
            harness.event(egui::Event::PointerMoved(at));
            harness.run();
            let hit = harness.state().canvas.orbit_cursor.unwrap();
            assert!(hit.cloud_snap());
            assert_eq!(hit.position, point);
            for pressed in [true, false] {
                harness.event(egui::Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE });
                harness.step();
            }
        }
        harness.event(key(Key::Enter));
        harness.event(egui::Event::Key { key: Key::Enter, physical_key: None, pressed: false, repeat: false, modifiers: egui::Modifiers::NONE });
        harness.run();
        assert!(harness.state().session.running.is_none());
        let drawing = harness.state().session.doc().unwrap();
        assert!(matches!(&drawing.model.iter().last().unwrap().kind, cadcraft_doc::EntityKind::Line(line) if line.a == a && line.b == b));
    }

    #[test]
    fn picks_and_draws_from_cad_lines_in_a_perspective_orbit_without_a_cloud() {
        use cadcraft_doc::{Common, Drawing, EntityKind, Line};
        use cadcraft_engine::snap::mode;

        let mut drawing = Drawing::new_metric();
        let a = Vec3::new(-10.0, -5.0, 20.0);
        let b = Vec3::new(14.0, 6.0, 45.0);
        let original = drawing.add(&Space::Model, Common::default(), EntityKind::Line(Line { a, b })).unwrap();
        // Leave enough room to hit the endpoints above the floating command line.
        let support = [Vec3::new(2.0, -42.0, 32.5), Vec3::new(2.0, 43.0, 32.5)];
        drawing.add(&Space::Model, Common::default(), EntityKind::Line(Line { a: support[0], b: support[1] })).unwrap();
        let mut session = Session::empty();
        session.open_drawing(drawing, "Model", None);
        session.settings.osmode = mode::END | mode::NEA;
        session.execute("3dorbit", &json!({"view": "isometric", "fit": true, "perspective": true})).unwrap();
        let plan = session.state().unwrap().view();
        let pose = session.state().unwrap().orbit.unwrap();
        let app = CadApp::new(session, Default::default());
        let mut harness =
            egui_kittest::Harness::builder().with_size(vec2(900.0, 500.0)).build_ui_state(|ui, app: &mut CadApp| super::super::show(app, ui), app);
        let rect = harness.get_by_label("Model 3D view").rect();
        let frame =
            OrbitFrame::new(pose, plan, Extent { xy: Bounds2::from_points([a, b, support[0], support[1]].map(Vec3::xy)), z: [a.z, b.z] }, rect);
        let click = |at, pressed, modifiers| egui::Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers };
        let midpoint = a + (b - a) * 0.5;
        let at = frame.project(a + (b - a) * 0.25).0;
        for modifiers in [egui::Modifiers::NONE, egui::Modifiers::SHIFT] {
            harness.event(egui::Event::PointerMoved(at));
            harness.event(click(at, true, modifiers));
            harness.step();
            assert_eq!(harness.state().session.selection().contains(&original), !modifiers.shift);
            harness.event(click(at, false, modifiers));
            harness.step();
        }

        harness.state_mut().session.start("line").unwrap();
        let near = a + (b - a) * 0.77;
        for (expected, snap) in [(a, mode::END), (midpoint, mode::MID), (near, mode::NEA)] {
            let session = &mut harness.state_mut().session;
            if snap == mode::MID {
                session.settings.osmode |= mode::OFF;
                session.cmdline("_mid").unwrap();
                assert_eq!(session.snap_override(), Some(mode::MID));
            } else {
                session.settings.osmode &= !mode::OFF;
            }
            let at = frame.project(expected).0;
            harness.event(egui::Event::PointerMoved(at));
            harness.step();
            let hit = harness.state().canvas.orbit_cursor.unwrap();
            assert_eq!(hit.snap.map(|hit| hit.mode), Some(snap), "point at {expected:?}");
            assert!((hit.position - expected).len() < 1e-5);
            let xyz = crate::cmdline::dyn_frame(harness.state()).unwrap().live_xyz(hit.position);
            assert!((xyz - expected).len() < 1e-5);
            for pressed in [true, false] {
                harness.event(click(at, pressed, egui::Modifiers::NONE));
                harness.step();
            }
        }
        let drawing = harness.state().session.doc().unwrap();
        let created: Vec<_> = drawing
            .model
            .iter()
            .skip(2)
            .filter_map(|e| match &e.kind {
                EntityKind::Line(line) => Some(line),
                _ => None,
            })
            .collect();
        assert_eq!(created.len(), 2);
        assert_eq!((created[0].a, created[0].b, created[1].a), (a, midpoint, midpoint));
        assert!((created[1].b - near).len() < 1e-5);
    }

    #[test]
    fn adding_geometry_to_an_empty_orbit_keeps_the_work_plane_under_the_cursor() {
        let mut drawing = cadcraft_doc::Drawing::new_metric();
        drawing.header.set_f64("ELEVATION", 3.0);
        let mut session = Session::empty();
        session.open_drawing(drawing, "Model", None);
        session.execute("3dorbit", &json!({"view": "isometric", "fit": true, "perspective": true})).unwrap();
        let plan = session.state().unwrap().view();
        let pose = session.state().unwrap().orbit.unwrap();
        session.start("line").unwrap();
        let app = CadApp::new(session, Default::default());
        let mut harness =
            egui_kittest::Harness::builder().with_size(vec2(900.0, 500.0)).build_ui_state(|ui, app: &mut CadApp| super::super::show(app, ui), app);
        let rect = harness.get_by_label("Model 3D view").rect();
        let frame = OrbitFrame::new(pose, plan, Extent::default(), rect);
        let positions = [vec2(-35.0, -12.0), vec2(10.0, 7.0), vec2(38.0, -20.0)].map(|offset| rect.center() + offset);
        for at in positions {
            harness.event(egui::Event::PointerMoved(at));
            harness.step();
            for pressed in [true, false] {
                harness.event(egui::Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE });
                harness.step();
            }
        }
        let b = frame.on_draft_plane(positions[1], 3.0).unwrap();
        let c = frame.on_draft_plane(positions[2], 3.0).unwrap();
        let drawing = harness.state().session.doc().unwrap();
        assert!(
            matches!(&drawing.model.iter().last().unwrap().kind, cadcraft_doc::EntityKind::Line(line) if (line.a - b).len() < 1e-8 && (line.b - c).len() < 1e-8)
        );
    }

    #[test]
    fn model_orbit_opens_without_a_cloud_and_can_exit_from_keyboard() {
        use cadcraft_doc::{Common, Drawing, EntityKind, Line};
        let mut drawing = Drawing::new_metric();
        drawing.add(&Space::Model, Common::default(), EntityKind::Line(Line { a: Vec3::ZERO, b: Vec3::new(0.0, 0.0, 20.0) })).unwrap();
        let mut session = Session::empty();
        session.open_drawing(drawing, "Model", None);
        let plan = session.state().unwrap().view();
        session.execute("3dorbit", &json!({"view": "front", "fit": true})).unwrap();
        let app = CadApp::new(session, Default::default());
        let mut harness =
            egui_kittest::Harness::builder().with_size(vec2(800.0, 400.0)).build_ui_state(|ui, app: &mut CadApp| super::super::show(app, ui), app);
        harness.get_by_label("Model 3D view").click();
        harness.run();
        let key = |key| egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
        harness.event(key(Key::ArrowRight));
        harness.run();
        assert!(harness.state().session.state().unwrap().orbit.unwrap().yaw > 0.0);
        harness.event(key(Key::Escape));
        harness.run();
        let state = harness.state().session.state().unwrap();
        assert!(state.orbit.is_none());
        assert_eq!(state.view(), plan);
    }

    #[test]
    fn orbit_toolbar_and_middle_drag_keep_a_live_3dpolyline() {
        let mut session = Session::empty();
        session.open_drawing(cadcraft_doc::Drawing::new_metric(), "Model", None);
        session.cmdline("3dpoly 0,0,2 8,4,5").unwrap();
        let app = CadApp::new(session, Default::default());
        let mut harness = egui_kittest::Harness::builder().with_size(vec2(1260.0, 660.0)).build_ui_state(
            |ui, app: &mut CadApp| {
                crate::chrome::title_and_toolbar(app, ui);
                super::super::show(app, ui);
            },
            app,
        );
        let toggle = harness.get_by_label("3D Orbit");
        let plan = harness.state().canvas.rect.unwrap();
        assert!(toggle.rect().top() <= plan.top() + 9.0 && (toggle.rect().right() + 8.0 - plan.right()).abs() < 1.0);
        let toggle_position = toggle.rect().min;
        toggle.click();
        harness.run();
        let before = harness.state().session.state().unwrap().orbit.unwrap();
        let at = harness.get_by_label("Model 3D view").rect().center() - vec2(70.0, 60.0);
        let to = at + vec2(65.0, -35.0);
        let middle = |pos, pressed| egui::Event::PointerButton { pos, button: PointerButton::Middle, pressed, modifiers: egui::Modifiers::NONE };
        for event in [egui::Event::PointerMoved(at), middle(at, true), egui::Event::PointerMoved(to), middle(to, false)] {
            harness.event(event);
            harness.step();
        }
        let after = harness.state().session.state().unwrap().orbit.unwrap();
        assert_ne!(before.yaw, after.yaw);
        let orbit_toggle = harness.get_by_label("3D Orbit");
        let button = orbit_toggle.rect();
        let canvas = harness.get_by_label("Model 3D view").rect();
        assert!((button.min - toggle_position).length() < 1.0 && (button.right() + 8.0 - canvas.right()).abs() < 1.0);
        orbit_toggle.click();
        harness.run();
        assert!(harness.state().session.state().unwrap().orbit.is_none());
        harness.get_by_label("Orbit").click();
        harness.run();
        let session = &mut harness.state_mut().session;
        assert!(session.state().unwrap().orbit.is_some());
        session.cmdline("@0,0,3").unwrap();
        session.cmdline("'3dorbit").unwrap();
        assert!(session.state().unwrap().orbit.is_none());
        session.cmdline("@2,0,0 ").unwrap();
        let drawing = session.doc().unwrap();
        let points = [Vec3::new(0.0, 0.0, 2.0), Vec3::new(8.0, 4.0, 5.0), Vec3::new(8.0, 4.0, 8.0), Vec3::new(10.0, 4.0, 8.0)];
        assert!(matches!(&drawing.model.iter().last().unwrap().kind, cadcraft_doc::EntityKind::Polyline3d(poly) if poly.points == points));
    }

    #[test]
    fn orbit_picks_the_front_surface_at_an_off_center_zoom_anchor() {
        let pose = OrbitView { pivot: Some(Vec3::ZERO), yaw: 0.5, tilt: 1.1, ..Default::default() };
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
        let frame = OrbitFrame::new(pose, plan, Extent { xy: extent.bounds(), z: extent.z_range }, rect);
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
        assert!(focus.xy().dist(back.xy()) > 50.0);
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
        let mut orbit = OrbitView { pivot: Some(Vec3::ZERO), yaw: 0.5, tilt: 1.3, ..Default::default() };
        let before = OrbitFrame::new(orbit, plan, Extent { xy: cloud.bounds(), z: cloud.z_range }, rect);
        let target = Vec3::new(32.0, -78.0, 75.0);
        let [right, up, _] = before.camera.axes;
        orbit.pan = Vec2::new(-right.dot(target), -up.dot(target));
        let frame = OrbitFrame::new(orbit, plan, Extent { xy: cloud.bounds(), z: cloud.z_range }, rect);
        assert!(frame.project(target).0.distance(rect.center()) < 0.01);
        let focus = frame.on_surface(rect.center(), target);
        assert!((focus - target).len() < 1e-8);
        let detail = frame.detail_bounds(Some(target));
        assert!(detail.center().near(target.xy(), 1e-8));
        assert!(frame.bounds.center().dist(target.xy()) > detail.height());
        let fallback = frame.detail_bounds(None);
        assert!(frame.project(fallback.center().to3(frame.ray_origin(rect.center()).z)).0.distance(rect.center()) < 0.01);
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
        assert_eq!(snap_visible(&app, &xf, xy + Vec2::X, 2.0), Some(point.position));
        assert_eq!(snap_visible(&app, &xf, xy + Vec2::X * 4.0, 2.0), None);

        let mut harness =
            egui_kittest::Harness::builder().with_size(vec2(640.0, 300.0)).build_ui_state(|ui, app: &mut CadApp| super::super::show(app, ui), app);
        assert!(harness.get_by_label("Snap to cloud points").accesskit_node().toggled().is_some());
        harness.get_by_label("3D Orbit").click();
        harness.run();
        let orbit = |app: &CadApp| app.session.state().unwrap().orbit.unwrap();
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
        assert!(harness.state().session.state().unwrap().orbit.is_none());
    }
}
