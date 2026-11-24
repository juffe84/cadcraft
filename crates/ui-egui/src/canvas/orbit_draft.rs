//! Spatial pointer input and rubber bands for the model orbit.
use std::sync::Arc;

use cadcraft_doc::{
    Handle,
    point_cloud::{CloudView, PointCloud},
};
use cadcraft_engine::{Session, snap};
use cadcraft_geom::Vec3;
use cadcraft_render::{Kind, Options};
use egui::{Painter, Pos2, Response, Stroke};

use super::orbit_scene::Model;
use super::orbit_snap::Query;
use super::point_cloud::{OrbitFrame, layers};
use super::{CadApp, Tokens, cursor, press};

#[derive(Clone, Copy, Debug)]
pub(crate) struct DraftPoint {
    pub position: Vec3,
    pub snap: Option<snap::SnapHit>,
}

impl DraftPoint {
    pub fn cloud_snap(self) -> bool {
        self.snap.is_some_and(|snap| snap.name == "Point cloud point")
    }
}

type CloudLayers<'a> = Option<(&'a Arc<PointCloud>, &'a CloudView)>;

fn point_at(session: &Session, frame: &OrbitFrame, model: Option<&Model>, clouds: CloudLayers<'_>, at: Pos2, shift: bool) -> Option<DraftPoint> {
    let plane = session.orbit_draft_plane();
    let query = Query::new(session);
    if let Some((position, mode)) = model.and_then(|model| query.find(model, frame, at)) {
        let hit = snap::SnapHit { world_z: Some(position.z), ..snap::SnapHit::plain(mode, position.xy()) };
        return Some(DraftPoint { position: session.project_spatial_snap(position), snap: Some(hit) });
    }
    if plane.cloud_snap
        && session.settings.point_cloud_snap
        && session.snap_override().map_or(session.settings.osmode & snap::mode::OFF == 0, |mode| mode & snap::mode::NOD != 0)
        && let Some((root, snapshot)) = clouds
    {
        let size = session.settings.point_cloud_size;
        let aperture = query.radius.max(size * 0.5);
        if let Some(position) = frame.pick_where(layers(snapshot, root), at, 160_000, aperture, size, |p| query.accepts(p)) {
            let hit = snap::SnapHit::cloud_sample(position);
            return Some(DraftPoint { position: session.project_spatial_snap(position), snap: Some(hit) });
        }
    }
    let reference = session.spatial_draft_reference();
    let position = match reference {
        Some(reference) => frame.on_plane(at, reference, session.ucs().z()),
        None => frame.on_draft_plane(at, plane.height),
    }?;
    Some(DraftPoint { position: session.constrain_orbit_point(position, shift), snap: None })
}

fn point_prompt(session: &Session) -> bool {
    session.current_prompt().is_some_and(|p| p.accept.point && !p.picks_objects())
}

pub(super) fn draw(
    app: &mut CadApp,
    painter: &Painter,
    frame: &OrbitFrame,
    response: &Response,
    model: Option<&Model>,
    clouds: CloudLayers<'_>,
    navigating: bool,
) {
    app.canvas.orbit_cursor = None;
    app.canvas.hover = None;
    app.canvas.hover_at = None;
    if let Some(model) = model {
        model.highlight(painter, frame, &app.session.selection(), Tokens::get().selection, 2.5);
    }
    if navigating {
        return;
    }
    let (steps, hover, shift) = painter.ctx().input(|input| (press::steps(&input.events), input.pointer.hover_pos(), input.modifiers.shift));
    let bar = app.cmd.ime_rect.map(|(bar, _)| bar).filter(|_| app.ui.show_command_line);
    let on_canvas =
        |at: Pos2| frame.rect.contains(at) && bar.is_none_or(|bar| !bar.contains(at)) && painter.ctx().layer_id_at(at) == Some(response.layer_id);
    for step in steps {
        if let press::Step::Down(at, mods) = step
            && on_canvas(at)
        {
            if point_prompt(&app.session) {
                let hit = point_at(&app.session, frame, model, clouds, at, mods.shift);
                let mode = hit.and_then(|hit| hit.snap).map_or(0, |snap| snap.mode);
                if mode == 0 && app.session.snap_override().is_some_and(|requested| requested != 0) {
                    app.session.echo(crate::i18n::t("No object snap found."));
                } else if let Some(hit) = hit {
                    app.canvas.draft_cursor = Some(hit.position);
                    app.canvas.snap = hit.snap;
                    let result = super::place_command_point(app, hit.position.xy(), crate::cmdline::dyn_frame(app));
                    report(&mut app.session, result);
                } else {
                    app.session.echo(crate::i18n::t("The drawing plane is edge-on. Tilt the view or snap to a point."));
                }
            } else if let Some((handle, point)) = picked(&app.session, frame, model, at) {
                let result = if app.session.click_picks() {
                    app.session.pick_cycled(handle, mods.shift)
                } else {
                    app.session.input_object(handle, Some(point.xy()))
                };
                report(&mut app.session, result);
            }
            painter.ctx().request_repaint();
        }
    }
    if response.secondary_clicked() && app.session.prompting() {
        crate::cmdline::submit(app);
        painter.ctx().request_repaint();
    }
    let Some(at) = hover.filter(|at| on_canvas(*at)) else { return };
    if !point_prompt(&app.session) {
        if app.session.previews_selection()
            && let Some(model) = model
            && let Some((handle, _)) = picked(&app.session, frame, Some(model), at)
            && app.session.doc().is_ok_and(|doc| {
                doc.entity(handle).is_some_and(|e| !cadcraft_engine::select::preview_filtered(doc, e, app.session.settings.previewfilter))
            })
        {
            app.canvas.hover = Some(handle);
            model.highlight(painter, frame, &[handle], Tokens::get().hover, 3.0);
        }
        return;
    }
    let Some(mut hit) = point_at(&app.session, frame, model, clouds, at, shift) else { return };
    if let Some(f) = crate::cmdline::dyn_frame(app)
        && let Some(e) = crate::dyninput::parse(&app.cmd.buffer, &f)
        && let Some(point) = crate::dyninput::resolve_xyz(&e, &f, hit.position, false)
    {
        hit = DraftPoint { position: point, snap: None };
    }
    app.canvas.orbit_cursor = Some(hit);
    app.canvas.draft_cursor = Some(hit.position);
    app.session.spatial_cursor = Some(hit.position);
    app.canvas.cursor = Some(hit.position.xy());
    app.session.cursor = hit.position.xy();
    if let Some(snap) = hit.snap {
        app.canvas.snap = Some(snap);
        let target = frame.project(snap.point.to3(snap.world_z.unwrap_or(hit.position.z))).0;
        cursor::draw_snap_marker(painter, target, &snap);
    } else {
        painter.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
    }
    preview(&app.session, painter, frame, hit.position);
    if app.cmd.options.is_none() && app.session.spatial_draft_reference().is_some() {
        super::dyn_boxes::dynamic_input(app, painter, at, false);
    }
}

fn picked(session: &Session, frame: &OrbitFrame, model: Option<&Model>, at: Pos2) -> Option<(Handle, Vec3)> {
    if !session.picking_objects() || (!session.click_picks() && !session.current_prompt().is_some_and(|p| p.picks_objects() && !p.accept.select)) {
        return None;
    }
    model?.pick_object(frame, at, session.settings.pickbox.max(1.0) as f32 * 1.5)
}

fn report(session: &mut Session, result: cadcraft_engine::Result<()>) {
    if let Err(error) = result {
        session.echo(error.to_string());
    }
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
