use super::{CadApp, Tokens, press};
use crate::cloud_gpu::Camera;
use cadcraft_doc::Space;
use cadcraft_doc::point_cloud::CloudRegion;
use cadcraft_geom::{Bounds2, Vec2};
use egui::{Id, Key, Painter, Pos2, Rect, Response, Stroke, StrokeKind};
use serde_json::json;

pub(crate) struct Rectangle {
    view: Id,
    document: u64,
    corner: Pos2,
    held: bool,
}

pub(super) fn enabled(app: &CadApp) -> bool {
    app.session.state().is_ok_and(|state| {
        state.space == Space::Model
            && state.block_editor().is_none()
            && state.doc.point_cloud.is_some()
            && state.cloud_selection_mode.unwrap_or_else(|| state.doc.model.is_empty())
    })
}

pub(super) fn input(app: &mut CadApp, response: &Response, viewport: Rect, camera: &Camera, can_start: bool) -> bool {
    if !enabled(app) || app.session.prompting() {
        app.canvas.cloud_rectangle = None;
        return false;
    }
    let ctx = &response.ctx;
    let document = app.session.state().map_or(0, |state| state.uid);
    let mut rectangle = app.canvas.cloud_rectangle.take().filter(|rectangle| rectangle.view == response.id && rectangle.document == document);
    if ctx.input(|i| i.key_pressed(Key::Escape) || i.pointer.middle_down() || i.pointer.secondary_down()) {
        return true;
    }
    let command_bar = app.cmd.ime_rect.map(|(rect, _)| rect).filter(|_| app.ui.show_command_line);
    let orbit_button = app.canvas.orbit_button;
    let hit = |at: Pos2| {
        can_start
            && viewport.contains(at)
            && command_bar.is_none_or(|bar| !bar.contains(at))
            && orbit_button.is_none_or(|button| !button.contains(at))
            && ctx.layer_id_at(at) == Some(response.layer_id)
    };
    for event in ctx.input(|input| press::steps(&input.events)) {
        match event {
            press::Step::Down(at, _) if hit(at) => {
                response.request_focus();
                if let Some(first) = rectangle.take().filter(|first| !first.held) {
                    select(app, viewport, camera, first.corner, at);
                } else {
                    rectangle = Some(Rectangle { view: response.id, document, corner: at, held: true });
                }
            }
            press::Step::Up(at) => {
                if let Some(first) = rectangle.take().filter(|first| first.held) {
                    if first.corner.distance(at) >= press::TOLERANCE {
                        select(app, viewport, camera, first.corner, at);
                    } else {
                        rectangle = Some(Rectangle { held: false, ..first });
                    }
                }
            }
            _ => {}
        }
    }
    app.canvas.cloud_rectangle = rectangle;
    true
}

fn select(app: &mut CadApp, viewport: Rect, camera: &Camera, first: Pos2, second: Pos2) {
    let rect = Rect::from_two_pos(first, second).intersect(viewport);
    if rect.width() < 1.0 || rect.height() < 1.0 {
        return;
    }
    let ndc = |point: Pos2| {
        let delta = point - viewport.center();
        Vec2::new(f64::from(delta.x) * 2.0 / f64::from(viewport.width()), -f64::from(delta.y) * 2.0 / f64::from(viewport.height()))
    };
    let region = CloudRegion::projected(
        camera.center,
        camera.rows_precise(viewport, camera.center),
        Bounds2::new(ndc(rect.left_top()), ndc(rect.right_bottom())),
    );
    let _ = app.run("pointcloud.select", json!({ "region": region }));
}

pub(super) fn active(app: &CadApp) -> bool {
    app.canvas.cloud_rectangle.is_some() || app.session.state().is_ok_and(|state| state.cloud_selection.is_some())
}

pub(super) fn draw(app: &CadApp, painter: &Painter, response: &Response, viewport: Rect) {
    if let Some(rectangle) = app.canvas.cloud_rectangle.as_ref().filter(|rectangle| rectangle.view == response.id)
        && let Some(pointer) = painter.ctx().input(|input| input.pointer.hover_pos())
    {
        let rect = Rect::from_two_pos(rectangle.corner, pointer).intersect(viewport);
        let color = Tokens::get().selection;
        painter.rect_filled(rect, 0.0, color.linear_multiply(0.15));
        painter.rect_stroke(rect, 0.0, Stroke::new(1.0, color), StrokeKind::Inside);
    }
}
