use cadcraft_engine::snap::{SnapHit, mode};
use cadcraft_geom::Vec3;
use egui::{Painter, Pos2, Response};

use super::{CadApp, Xf, orbit_draft, orbit_scene::Model, orbit_snap::Query, point_cloud::OrbitFrame};
use crate::surface_editor::{self, PickMode};

pub(super) fn plan(app: &mut CadApp, xf: &Xf, at: Option<Pos2>, clicked: bool, painter: &Painter) {
    let pick = surface_editor::pick_mode(app);
    let base = surface_editor::pick_base(app).unwrap_or(Vec3::ZERO);
    let snap = at.filter(|_| pick.is_some() || surface_editor::snap_segment_placement(app)).and_then(|at| {
        let session = &app.session;
        let state = session.state().ok()?;
        let settings = &session.settings;
        let aperture = settings.aperture / xf.scale;
        let osmode = session.snap_override().unwrap_or(settings.osmode) | if settings.osnaphatch { mode::HATCH } else { 0 };
        let xy = xf.to_world(at);
        let elevation = match pick {
            Some(PickMode::Elevation(exclude)) => Some(exclude),
            _ => None,
        };
        let candidates = cadcraft_engine::pointmod::snap_candidates(&state.doc, &state.edit_space(), xy, aperture, osmode, Some(base.xy()), false);
        let point = candidates.into_iter().find_map(|hit| {
            let point = if let Some(exclude) = elevation {
                session.authored_snap_xyz(hit, aperture, |handle| session.terrain_elevation_reference(handle, exclude))?
            } else if let Some(z) = hit.world_z {
                hit.point.to3(z)
            } else {
                let elevation = if let Some(PickMode::Position(z)) = pick { z } else { base.z };
                session.authored_snap_xyz(hit, aperture, |_| true).unwrap_or_else(|| hit.point.to3(elevation))
            };
            (pick.is_some() || surface_editor::accepts_insertion(app, point)).then_some((point, hit))
        });
        point.or_else(|| {
            if elevation.is_some()
                || !settings.point_cloud_snap
                || osmode & mode::OFF != 0
                || session.snap_override().is_some_and(|bits| bits & mode::NOD == 0)
            {
                return None;
            }
            let point = super::point_cloud::snap_visible(app, xf, xy, aperture)?;
            (pick.is_some() || surface_editor::accepts_insertion(app, point)).then_some((point, SnapHit::cloud_sample(point)))
        })
    });
    if let Some(pick) = pick {
        let target = snap.map(|(p, _)| p).or_else(|| {
            let xy = xf.to_world(at?);
            match pick {
                PickMode::Position(z) => Some(xy.to3(z)),
                PickMode::Elevation(exclude) => app.session.terrain_reference_point(xy, exclude, app.session.settings.aperture / xf.scale),
            }
        });
        surface_editor::preview_pick(app, target, snap, clicked && at.is_some());
    } else {
        surface_editor::segment_input(
            app,
            at,
            |p| Some((xf.to_screen(p.xy()), 1.0)),
            |p, reference| Some(xf.to_world(p).to3(reference.z)),
            snap,
            clicked,
            painter,
        );
    }
}

pub(super) fn orbit(
    app: &mut CadApp,
    response: &Response,
    frame: &OrbitFrame,
    model: Option<&Model>,
    clouds: orbit_draft::CloudLayers<'_>,
    dragging: bool,
    painter: &Painter,
) -> bool {
    if !surface_editor::picking_source(app) {
        return false;
    }
    let clicked = !dragging && response.clicked_by(egui::PointerButton::Primary);
    let at = if clicked { response.interact_pointer_pos() } else { response.hover_pos() };
    let command_bar = app.cmd.ime_rect.map(|(bar, _)| bar).filter(|_| app.ui.show_command_line);
    let at = at.filter(|&at| {
        !dragging
            && frame.rect.contains(at)
            && !app.canvas.over_viewport_tools(at)
            && !surface_editor::covers_canvas_point(app, at)
            && command_bar.is_none_or(|bar| !bar.contains(at))
            && response.ctx.layer_id_at(at) == Some(response.layer_id)
    });
    let pick = surface_editor::pick_mode(app);
    let base = surface_editor::pick_base(app).unwrap_or(Vec3::ZERO);
    let query = Query::new(&app.session).for_source(base);
    let plane = |at, reference: Vec3| frame.on_draft_plane(at, reference.z).or_else(|| frame.on_plane(at, reference, frame.camera.axes[2]));
    let snap = at.filter(|_| pick.is_some() || surface_editor::snap_segment_placement(app)).and_then(|at| {
        let owner = |handle| match pick {
            Some(PickMode::Elevation(exclude)) => app.session.terrain_elevation_reference(handle, exclude),
            _ => true,
        };
        let accepts = |p| pick.is_some() || surface_editor::accepts_insertion(app, p);
        if let Some((point, mode)) = model.and_then(|model| query.find_where(model, frame, at, owner, accepts)) {
            return Some((point, SnapHit { world_z: Some(point.z), ..SnapHit::plain(mode, point.xy()) }));
        }
        if matches!(pick, Some(PickMode::Elevation(_))) {
            return None;
        }
        orbit_draft::cloud_point(&app.session, frame, clouds, at, query, accepts).map(|point| (point, SnapHit::cloud_sample(point)))
    });
    if let Some(pick) = pick {
        let target = snap.map(|(p, _)| p).or_else(|| match pick {
            PickMode::Position(z) => frame.on_draft_plane(at?, z),
            PickMode::Elevation(exclude) => {
                model?.pick_object_where(frame, at?, query.radius, |handle| app.session.terrain_elevation_reference(handle, exclude)).map(|(_, p)| p)
            }
        });
        surface_editor::preview_pick(app, target, snap, clicked && at.is_some());
    } else {
        surface_editor::segment_input(app, at, |p| super::point_cloud::project_source(frame, p), plane, snap, clicked, painter);
    }
    true
}

pub(super) fn surface_move_preview(
    session: &cadcraft_engine::Session,
    point: Vec3,
    painter: &Painter,
    project: impl Fn(Vec3) -> Option<Pos2>,
) -> bool {
    if !session.running.as_ref().is_some_and(|run| run.id == surface_editor::MOVE_SURFACE_VERTEX) {
        return false;
    }
    let stroke = egui::Stroke::new(2.0, crate::theme::Tokens::get().snap);
    for entity in session.preview_3d(point) {
        if let cadcraft_doc::EntityKind::Line(line) = entity.kind {
            surface_editor::dashed_guide(painter, &[line.a, line.b], &project, stroke);
        }
    }
    true
}
