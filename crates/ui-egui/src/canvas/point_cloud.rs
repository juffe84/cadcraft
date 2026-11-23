use super::{CadApp, Color32, Rect, Shape, Tokens, Vec2, Xf, pos2, vec2};
use cadcraft_doc::Space;
use cadcraft_doc::point_cloud::{CloudPoint, CloudView, PointCloud};
use cadcraft_geom::Bounds2;
use egui::{Key, Sense};
use serde_json::json;

pub(super) fn controls(app: &mut CadApp, ui: &mut egui::Ui) {
    let orbit = app.session.state().is_ok_and(|state| state.point_cloud_orbit.is_some());
    let status = cloud(app).map(|cloud| view(cloud, cloud.bounds(), false)).unwrap_or_default();
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
        if status.failed {
            ui.label(crate::i18n::t("Could not load point cloud detail"));
        } else if status.pending {
            ui.label(crate::i18n::t("Loading point cloud detail…"));
        } else if orbit {
            ui.label(crate::i18n::t("Drag to rotate. Scroll to zoom."));
        }
    });
}

fn cloud(app: &CadApp) -> Option<&PointCloud> {
    let state = app.session.state().ok()?;
    if state.space != Space::Model || state.block_editor().is_some() {
        return None;
    }
    state.doc.point_cloud.as_deref()
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

fn view(cloud: &PointCloud, bounds: Bounds2, request: bool) -> CloudView {
    cloud.detail.as_ref().map_or_else(CloudView::default, |details| if request { details.0.view(bounds) } else { details.0.current(bounds) })
}

pub(super) fn paint_plan(app: &CadApp, painter: &egui::Painter, xf: &Xf, request: bool) {
    let Some(cloud) = cloud(app) else { return };
    let bounds = xf.world_bounds();
    let snapshot = view(cloud, bounds, request);
    if request && snapshot.pending {
        painter.ctx().request_repaint_after(std::time::Duration::from_millis(125));
    }
    let shown = snapshot.points.as_deref().unwrap_or(cloud);
    let mut mesh = egui::Mesh::default();
    let size = egui::Vec2::splat(app.session.settings.point_cloud_size);
    let fallback = Tokens::get().canvas_ink;
    shown.visible(bounds, sample_budget(xf.rect), |point| {
        let screen = xf.to_screen(point.position.xy());
        if xf.rect.contains(screen) {
            mesh.add_colored_rect(Rect::from_center_size(screen, size), color(shown, point, fallback));
        }
    });
    painter.add(Shape::mesh(mesh));
}

pub(super) fn snap_visible(app: &CadApp, xf: &Xf, raw: Vec2, aperture: f64) -> Option<Vec2> {
    let cloud = cloud(app)?;
    let bounds = xf.world_bounds();
    let snapshot = view(cloud, bounds, false);
    let shown = snapshot.points.as_deref().unwrap_or(cloud);
    let mut nearest = None;
    let mut distance = aperture.max(f64::from(app.session.settings.point_cloud_size) * 0.5 / xf.scale).powi(2);
    shown.visible(bounds, sample_budget(xf.rect), |point| {
        let xy = point.position.xy();
        let delta = xy - raw;
        let d = delta.dot(delta);
        if d <= distance && xf.rect.contains(xf.to_screen(xy)) {
            distance = d;
            nearest = Some(xy);
        }
    });
    nearest
}

pub(super) fn orbit(app: &mut CadApp, ui: &mut egui::Ui) {
    let Ok(state) = app.session.state() else { return };
    let Some([mut yaw, mut tilt, mut zoom]) = state.point_cloud_orbit else { return };
    let plan = state.view();
    let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Image, true, crate::i18n::t("Point cloud 3D view")));
    if response.drag_started() || response.clicked() {
        response.request_focus();
    }
    let delta = response.drag_motion();
    let focused = response.has_focus();
    let (keys, scroll, escape) = ui.input(|input| {
        let key = |k| f32::from(focused && input.key_pressed(k));
        let keys = vec2(key(Key::ArrowRight) - key(Key::ArrowLeft), key(Key::ArrowDown) - key(Key::ArrowUp));
        (keys, if response.hovered() { input.smooth_scroll_delta.y } else { 0.0 }, focused && input.key_pressed(Key::Escape))
    });
    if escape {
        let _ = app.run("3dorbit", json!({ "enabled": false }));
        return;
    }
    let delta = delta + keys * 10.0;
    if delta != egui::Vec2::ZERO || scroll != 0.0 {
        let changed = app.run(
            "3dorbit",
            json!({ "yaw": yaw + f64::from(delta.x) * 0.008, "tilt": tilt + f64::from(delta.y) * 0.008,
                "zoom": zoom * (f64::from(scroll) * 0.005).exp() }),
        );
        if let Ok(value) = changed {
            yaw = value["yaw"].as_f64().unwrap_or(yaw);
            tilt = value["tilt"].as_f64().unwrap_or(tilt);
            zoom = value["zoom"].as_f64().unwrap_or(zoom);
        }
    }
    let Some(cloud) = cloud(app) else { return };
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, Tokens::get().canvas);
    let extent = cloud.bounds();
    let center = if extent.contains(plan.center) { plan.center } else { extent.center() };
    let z_mid = (cloud.z_range[0] + cloud.z_range[1]) * 0.5;
    let z_half = cloud.z_range[1] - z_mid;
    let diagonal = extent.width().hypot(extent.height()).hypot(2.0 * z_half).max(1e-6);
    let fit = f64::from(rect.width().min(rect.height()).max(1.0)) / (diagonal * 1.05);
    let scale = (f64::from(rect.height()) / plan.height.max(1e-6)).max(fit) * zoom;
    let (sin, cos) = tilt.sin_cos();
    // Unproject the screen rectangle at both ends of the cloud's elevation range. This
    // bounds every XY point that can be visible, including at an oblique view angle.
    let x = f64::from(rect.width()) / (2.0 * scale);
    let rotated = Bounds2::from_points(extent.corners().map(|p| (p - center).rotate(yaw)));
    let (y_min, y_max) = if cos.abs() < 0.001 {
        (rotated.min.y, rotated.max.y)
    } else {
        let y = (f64::from(rect.height()) / (2.0 * scale) + z_half * sin.abs()) / cos.abs();
        ((-y).max(rotated.min.y), y.min(rotated.max.y))
    };
    let min = Vec2::new((-x).max(rotated.min.x), y_min);
    let max = Vec2::new(x.min(rotated.max.x), y_max);
    let bounds = if min.x <= max.x && min.y <= max.y {
        Bounds2::from_points(Bounds2::new(min, max).corners().map(|p| center + p.rotate(-yaw)))
    } else {
        Bounds2::EMPTY
    };
    let snapshot = view(cloud, bounds, true);
    if snapshot.pending {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(125));
    }
    let shown = snapshot.points.as_deref().unwrap_or(cloud);
    let mut projected = Vec::new();
    let origin = rect.center();
    let fallback = Tokens::get().canvas_ink;
    shown.visible(bounds, sample_budget(rect), |point| {
        let xy = (point.position.xy() - center).rotate(yaw);
        let z = point.position.z - z_mid;
        let at = pos2(origin.x + (xy.x * scale) as f32, origin.y - ((xy.y * cos + z * sin) * scale) as f32);
        if rect.contains(at) {
            projected.push((z * cos - xy.y * sin, at, color(shown, point, fallback)));
        }
    });
    projected.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
    let size = egui::Vec2::splat(app.session.settings.point_cloud_size);
    let mut mesh = egui::Mesh::default();
    for (_, position, color) in projected {
        mesh.add_colored_rect(Rect::from_center_size(position, size), color);
    }
    painter.add(Shape::mesh(mesh));
}

#[cfg(test)]
mod tests {
    use super::*;
    use cadcraft_engine::Session;
    use cadcraft_geom::Vec3;
    use egui_kittest::kittest::{NodeT, Queryable};
    use std::sync::Arc;

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
        assert!(harness.state().session.state().unwrap().point_cloud_orbit.is_some());
        harness.get_by_label("3D Orbit").click();
        harness.run();
        assert!(harness.state().session.state().unwrap().point_cloud_orbit.is_none());
    }
}
