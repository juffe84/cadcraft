//! Spatial pointer input and rubber bands for the model orbit.
use std::sync::Arc;

use cadcraft_doc::point_cloud::{CloudView, PointCloud};
use cadcraft_engine::{Input, Session, snap};
use cadcraft_geom::Vec3;
use cadcraft_render::{Kind, Options};
use egui::{Painter, Pos2, Response, Stroke};

use super::point_cloud::{OrbitFrame, layers};
use super::{CadApp, Tokens, cursor, press};

#[derive(Clone, Copy, Debug)]
pub(crate) struct DraftPoint {
    pub position: Vec3,
    pub snapped: bool,
}

type CloudLayers<'a> = Option<(&'a Arc<PointCloud>, &'a CloudView)>;

fn point_at(session: &Session, frame: &OrbitFrame, clouds: CloudLayers<'_>, at: Pos2) -> Option<DraftPoint> {
    let plane = session.orbit_draft_plane();
    if plane.cloud_snap
        && session.settings.point_cloud_snap
        && session.settings.osmode & snap::mode::OFF == 0
        && session.snap_override().is_none_or(|mode| mode & snap::mode::NOD != 0)
        && let Some((root, snapshot)) = clouds
    {
        let size = session.settings.point_cloud_size;
        let aperture = (session.settings.aperture as f32).max(size * 0.5);
        let close = 1e-7 + plane.height.abs() * f64::EPSILON * 8.0;
        if let Some(position) =
            frame.pick_where(layers(snapshot, root), at, 160_000, aperture, size, |p| !plane.locked || (p.z - plane.height).abs() <= close)
        {
            return Some(DraftPoint { position, snapped: true });
        }
    }
    frame.on_draft_plane(at, plane.height).map(|position| DraftPoint { position, snapped: false })
}

fn point_prompt(session: &Session) -> bool {
    session.current_prompt().is_some_and(|p| p.accept.point && !p.picks_objects())
}

pub(super) fn draw(app: &mut CadApp, painter: &Painter, frame: &OrbitFrame, response: &Response, clouds: CloudLayers<'_>, navigating: bool) {
    app.canvas.orbit_cursor = None;
    if navigating {
        return;
    }
    let (steps, hover) = painter.ctx().input(|input| (press::steps(&input.events), input.pointer.hover_pos()));
    let bar = app.cmd.ime_rect.map(|(bar, _)| bar).filter(|_| app.ui.show_command_line);
    let on_canvas =
        |at: Pos2| frame.rect.contains(at) && bar.is_none_or(|bar| !bar.contains(at)) && painter.ctx().layer_id_at(at) == Some(response.layer_id);
    for step in steps {
        if let press::Step::Down(at, _) = step
            && on_canvas(at)
            && point_prompt(&app.session)
        {
            match point_at(&app.session, frame, clouds, at) {
                Some(hit) => {
                    if let Err(error) = app.session.input_point3(hit.position) {
                        app.session.echo(error.to_string());
                    }
                }
                None => app.session.echo(crate::i18n::t("The drawing plane is edge-on. Tilt the view or snap to a point.")),
            }
            painter.ctx().request_repaint();
        }
    }
    if response.secondary_clicked() && app.session.prompting() {
        if let Err(error) = app.session.input(Input::Enter) {
            app.session.echo(error.to_string());
        }
        painter.ctx().request_repaint();
    }
    if !point_prompt(&app.session) {
        return;
    }
    let Some(at) = hover.filter(|at| on_canvas(*at)) else { return };
    let Some(hit) = point_at(&app.session, frame, clouds, at) else { return };
    app.canvas.orbit_cursor = Some(hit);
    app.canvas.cursor = Some(hit.position.xy());
    app.session.cursor = hit.position.xy();
    let target = frame.project(hit.position).0;
    if hit.snapped {
        let snap = snap::SnapHit::point_cloud(hit.position.xy());
        app.canvas.snap = Some(snap);
        cursor::draw_snap_marker(painter, target, &snap);
    } else {
        painter.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
    }
    preview(&app.session, painter, frame, hit.position);
}

fn preview(session: &Session, painter: &Painter, frame: &OrbitFrame, point: Vec3) {
    let Ok(drawing) = session.doc() else { return };
    let preview = session.preview_3d(point);
    if preview.is_empty() {
        return;
    }
    let opts = Options { spatial: true, fill: false, linetypes: false, tolerance: 0.5 / frame.camera.scale, ..Default::default() };
    let list = cadcraft_render::build_entities(drawing, &preview, &opts);
    let stroke = Stroke::new(1.5, Tokens::get().snap);
    for prim in &list.prims {
        let mut points = list.spatial_points(prim);
        match prim.kind {
            Kind::Polyline => {
                let projected: Vec<_> = points.map(|p| frame.project(p).0).collect();
                if projected.len() >= 2 {
                    painter.line(projected, stroke);
                }
            }
            Kind::Point => {
                if let Some(p) = points.next() {
                    painter.circle_filled(frame.project(p).0, 2.0, stroke.color);
                }
            }
            Kind::Infinite { ray } => {
                if let Some((base, direction)) = points.next().zip(points.next()) {
                    let span = f64::from(frame.rect.width().max(frame.rect.height())) / frame.camera.scale;
                    let line = crate::orbit_gpu::infinite(base, direction, ray, frame.camera.center, span);
                    painter.line_segment(line.map(|p| frame.project(p).0), stroke);
                }
            }
            Kind::Tris | Kind::Mask | Kind::Image { .. } => {}
        }
    }
}
