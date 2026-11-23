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
        if cloud(app).is_some_and(|cloud| cloud.detail.is_some()) {
            ui.menu_button(crate::i18n::t("Detail loading"), |ui| {
                let mut radius = app.session.settings.point_cloud_detail_radius;
                let mut batch = app.session.settings.point_cloud_batch_points / 1_000;
                if ui
                    .add(egui::Slider::new(&mut radius, 0.0..=1000.0).logarithmic(true).text(crate::i18n::t("Detail radius (drawing units)")))
                    .changed()
                {
                    let _ = app.run("pointcloud.settings", json!({ "detailRadius": radius }));
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
                ui.label(crate::i18n::t("Zero uses automatic loading."));
                if ui.button(crate::i18n::t("Fast close-up")).clicked() {
                    let _ = app.run("pointcloud.settings", json!({ "detailRadius": 5.0, "batchPoints": 50_000 }));
                }
                if ui.button(crate::i18n::t("Automatic")).clicked() {
                    let _ = app.run("pointcloud.settings", json!({ "detailRadius": 0.0, "batchPoints": 0 }));
                }
            });
        }
        if status.failed {
            ui.label(crate::i18n::t("Could not load point cloud detail"));
        } else if status.pending {
            ui.label(crate::i18n::t("Loading point cloud detail…"));
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
    snapshot.overview.iter().chain(std::iter::once(snapshot.points.as_ref().unwrap_or(root))).filter(|cloud| !cloud.points().is_empty())
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

    /// The ray through the center of the window, at the depth of a known surface point.
    /// This also tracks a screen-space pan when the exact point is between rendered samples.
    fn center_on_surface(&self, point: Vec3) -> Vec3 {
        let [right, up, front] = self.camera.axes;
        let [x, y] = self.camera.pan;
        self.camera.center - right * x - up * y + front * front.dot(point - self.camera.center)
    }

    fn detail_bounds(&self, focus: Vec3) -> Bounds2 {
        // Loading is centered on the geometry under the view. The wide footprint belonging
        // to the cloud's entire elevation range is only used for render culling.
        let half = Vec2::new(f64::from(self.rect.width()), f64::from(self.rect.height())) / (2.0 * self.camera.scale);
        let margin = half.x.max(half.y) * (1.0 - self.camera.axes[2].z.abs());
        let span = half + Vec2::new(margin, margin);
        Bounds2::new(focus.xy() - span, focus.xy() + span)
    }

    fn pick(&self, shown: &PointCloud, cursor: egui::Pos2, limit: usize, radius: f32) -> Option<Vec3> {
        // Only run when picking a pivot or when the camera settles. Screen distance chooses the pixel, and depth breaks
        // ties so overlapping points turn around the front of the cloud.
        let mut best = (radius * radius, f64::NEG_INFINITY);
        let mut pivot = None;
        shown.visible(self.bounds, limit, |p| {
            let (at, depth) = self.project(p.position);
            let d = at.distance_sq(cursor);
            if self.rect.contains(at) && (d < best.0 || (d == best.0 && depth > best.1)) {
                pivot = Some(p.position);
                best = (d, depth);
            }
        });
        pivot
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
    surface: Vec3,
    sample_source: Weak<PointCloud>,
}

/// Use coarse visible geometry to find the center of interest. Never read the GPU depth buffer
/// back into the CPU or restart a streaming request for every drag event.
fn detail_view(app: &mut CadApp, ui: &egui::Ui, root: &Arc<PointCloud>, frame: &OrbitFrame, document: u64, dragging: bool) -> CloudView {
    let snapshot = view(root, frame.bounds, app, false);
    if root.detail.is_none() {
        return snapshot;
    }
    let now = ui.input(|input| input.time);
    let pose = FocusPose::from(frame);
    let mut state = app.canvas.cloud_orbit_focus.take().filter(|state| state.document == document);
    if state.as_ref().is_none_or(|state| state.pose != pose) {
        let surface = state.as_ref().map_or(frame.camera.center, |state| state.surface);
        state = Some(OrbitFocus { document, pose, changed_at: now, surface, sample_source: Weak::new() });
    }
    let Some(mut state) = state else { return snapshot };
    if dragging || now - state.changed_at < 0.12 {
        app.canvas.cloud_orbit_focus = Some(state);
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(120));
        return snapshot;
    }
    let overview = snapshot.overview.as_ref().or(snapshot.points.as_ref()).unwrap_or(root);
    let samples = Arc::downgrade(overview);
    if !Weak::ptr_eq(&state.sample_source, &samples) {
        // The shader draws all resident vertices. Keep the CPU pick bounded when the
        // camera is looking at a large area and points are densely packed.
        if let Some(point) = frame.pick(overview, frame.rect.center(), 160_000, 24.0) {
            state.surface = point;
        }
        state.sample_source = samples;
    }
    let center = frame.center_on_surface(state.surface);
    let bounds = frame.detail_bounds(center);
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
    if press && let Some(cursor) = pointer.filter(|p| rect.contains(*p)) {
        let snapshot = view(&root, frame.bounds, app, false);
        let gpu = layers(&snapshot, &root).all(|shown| crate::cloud_gpu::available(app.canvas.gpu, shown));
        let limit = if gpu { usize::MAX } else { sample_budget(rect) };
        if let Some(pivot) = layers(&snapshot, &root)
            .filter_map(|cloud| frame.pick(cloud, cursor, limit, app.session.settings.point_cloud_size.max(6.0)))
            .min_by(|a, b| {
                let a = frame.project(*a);
                let b = frame.project(*b);
                a.0.distance_sq(cursor).total_cmp(&b.0.distance_sq(cursor)).then_with(|| b.1.total_cmp(&a.1))
            })
        {
            let delta = pivot - frame.camera.center;
            orbit.pan += Vec2::new(frame.camera.axes[0].dot(delta), frame.camera.axes[1].dot(delta));
            orbit.pivot = Some(pivot);
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
        updated = true;
    }
    if scroll != 0.0
        && let Some(pointer) = pointer
    {
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
    let snapshot = detail_view(app, ui, &root, &frame, document, dragging);
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
        let focus = frame.center_on_surface(target);
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
