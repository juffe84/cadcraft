use std::collections::{HashMap, HashSet};

use cadcraft_doc::Handle;
use cadcraft_engine::snap::SnapHit;
use cadcraft_geom::Vec3;
use egui::{Align2, FontId, Painter, Pos2, Rect, Stroke};
use serde::Deserialize;
use serde_json::json;

use crate::{CadApp, i18n::t, theme::Tokens};

mod chain;
mod segments;

const TRIANGLES: &str = "TIN-SURFACES";

#[derive(Clone, Deserialize)]
struct Point {
    handle: String,
    #[serde(rename = "lineNumber")]
    line_number: Option<u32>,
    vertex: usize,
    position: Vec3,
    whole: bool,
    arc: bool,
    #[serde(rename = "endpointOnly")]
    endpoint_only: bool,
    removable: bool,
    extensible: bool,
    single: bool,
    locked: bool,
}

#[derive(Default, Deserialize)]
struct Snapshot {
    points: Vec<Point>,
    curves: Vec<Curve>,
    conflicts: usize,
    cuts: Vec<String>,
    #[serde(skip)]
    labels: Vec<(Vec3, String)>,
}

impl Snapshot {
    fn cache_labels(&mut self) {
        let mut seen = HashSet::new();
        self.labels = self
            .points
            .iter()
            .filter_map(|point| {
                let p = point.position;
                let key = [p.x, p.y, p.z].map(|v| if v == 0.0 { 0 } else { v.to_bits() });
                seen.insert(key).then(|| (p, format!("{:.4}", p.z)))
            })
            .collect();
    }
}

#[derive(Deserialize)]
struct Curve {
    handle: String,
    segments: Vec<Vec<Vec3>>,
    locked: bool,
    closed: bool,
}

struct Draft {
    segment: Option<segments::Segment>,
    choices: Vec<Point>,
    selected: usize,
    x: String,
    y: String,
    height: String,
    focus: bool,
    extending: bool,
    next: Option<PickMode>,
    guide: Option<PickGuide>,
    error: Option<&'static str>,
}

#[derive(Clone, Copy)]
pub(crate) enum PickMode {
    Position(f64),
    Elevation(Handle),
}

#[derive(Clone, Copy)]
struct PickGuide {
    destination: Option<Vec3>,
    snap: Option<(Vec3, SnapHit)>,
}

impl Draft {
    fn new(segment: Option<segments::Segment>, choices: Vec<Point>) -> Self {
        let mut draft = Self {
            segment,
            choices,
            selected: 0,
            x: String::new(),
            y: String::new(),
            height: String::new(),
            focus: false,
            extending: false,
            next: None,
            guide: None,
            error: None,
        };
        draft.reset_fields();
        draft
    }

    fn point(&self) -> &Point {
        &self.choices[self.selected]
    }

    fn reset_fields(&mut self) {
        let p = self.point().position;
        self.x = p.x.to_string();
        self.y = p.y.to_string();
        self.height = p.z.to_string();
        self.next = None;
        self.guide = None;
        self.error = None;
        self.extending = false;
    }

    fn position(&self) -> Option<Vec3> {
        Some(Vec3::new(coordinate(&self.x)?, coordinate(&self.y)?, coordinate(&self.height)?))
    }
}

fn coordinate(text: &str) -> Option<f64> {
    text.trim().replace(',', ".").parse::<f64>().ok().filter(|n| n.is_finite() && n.abs() <= 1e12)
}

fn field(ui: &mut egui::Ui, name: &'static str, value: &mut String, enabled: bool) -> egui::Response {
    let label = ui.label(t(name));
    let field = ui.add_enabled(enabled, egui::TextEdit::singleline(value).desired_width(150.0)).labelled_by(label.id);
    crate::control::record_widget(ui, &field, t(name), "textbox", None);
    field
}

pub(super) struct SourceControls {
    key: Option<(u64, u64, String)>,
    snapshot: Snapshot,
    show_heights: bool,
    label_size: f32,
    show_lines: bool,
    width: f32,
    selected_point: Option<String>,
    coincident: Vec<(String, usize)>,
    segments: segments::Editor,
    chain: chain::Tools,
    draft: Option<Draft>,
}

impl Default for SourceControls {
    fn default() -> Self {
        Self {
            key: None,
            snapshot: Snapshot::default(),
            show_heights: true,
            label_size: 11.0,
            show_lines: true,
            width: 2.0,
            selected_point: None,
            coincident: Vec::new(),
            segments: segments::Editor::default(),
            chain: chain::Tools::default(),
            draft: None,
        }
    }
}

impl SourceControls {
    pub(super) fn prepare(&mut self, app: &mut CadApp, key: (u64, u64), group: &str, linked: bool) {
        if !linked {
            self.key = None;
            self.stop();
            return;
        }
        if self.key.as_ref().is_some_and(|(uid, rev, name)| (*uid, *rev) == key && name == group) {
            return;
        }
        if self.key.as_ref().is_some_and(|(uid, _, name)| *uid != key.0 || name != group) {
            self.stop();
        }
        let previous = self.draft.take();
        self.chain.clear_preview();
        self.segments.clear_preview();
        match app
            .run("tin.source.points", json!({"group":group,"segments":true}))
            .and_then(|value| serde_json::from_value::<Snapshot>(value).map_err(|e| e.to_string()))
        {
            Ok(mut snapshot) => {
                snapshot.cache_labels();
                self.segments.validate(&snapshot.curves);
                self.snapshot = snapshot;
                self.sync_draft();
                if let Some(previous) = previous
                    && let Some(draft) = &mut self.draft
                    && let Some(index) = draft.choices.iter().position(|p| p.handle == previous.point().handle && p.vertex == previous.point().vertex)
                {
                    draft.selected = index;
                    draft.reset_fields();
                    // Display and snap settings can redraw the drawing while a point pick is open.
                    if previous.next.is_some() && !draft.point().locked && draft.point().position == previous.point().position {
                        draft.next = previous.next;
                        draft.extending = previous.extending;
                        draft.x = previous.x;
                        draft.y = previous.y;
                        draft.height = previous.height;
                    }
                }
                self.key = Some((key.0, key.1, group.into()));
            }
            Err(error) => {
                app.set_status(error);
                self.key = None;
            }
        }
    }

    pub(super) fn stop(&mut self) {
        self.selected_point = None;
        self.coincident.clear();
        self.segments.stop();
        self.chain.cancel();
        self.draft = None;
    }

    pub(super) fn controls(&mut self, app: &mut CadApp, ui: &mut egui::Ui, group: &str) {
        let Some((_, _, loaded_name)) = &self.key else { return };
        if loaded_name != group {
            return;
        }
        ui.label(crate::tf!("Height conflicts: {count}", count = self.snapshot.conflicts));
        if let Some(visible) = app.session.doc().ok().and_then(|d| d.layer(TRIANGLES)).map(|layer| layer.on && !layer.frozen) {
            let mut visible = visible;
            let toggle = ui.checkbox(&mut visible, t("Show Linked Triangles"));
            crate::control::record_widget(ui, &toggle, t("Show Linked Triangles"), "checkbox", Some(visible));
            if toggle.changed() && app.run("layer.set", json!({"name":TRIANGLES,"on":visible,"frozen":false})).is_ok() && !visible {
                let selected = app.session.selection();
                if let Ok(doc) = app.session.doc() {
                    let kept: Vec<_> =
                        selected.iter().filter(|&&handle| doc.model.get(handle).is_some_and(|e| doc.is_visible(e))).map(|h| h.hex()).collect();
                    if kept.len() != selected.len() {
                        let _ = app.run("select", json!({"handles":kept}));
                    }
                }
            }
        }
        let show_lines = ui.checkbox(&mut self.show_lines, t("Show Source Lines"));
        crate::control::record_widget(ui, &show_lines, t("Show Source Lines"), "checkbox", Some(self.show_lines));
        let width = ui.add_enabled(self.show_lines, egui::Slider::new(&mut self.width, 1.0..=8.0).text(t("Source Line Width")).suffix(" px"));
        crate::control::record_widget(ui, &width, t("Source Line Width"), "slider", None);
        self.elevations_toggle(ui, "Show Source Elevations");
        let paths_enabled = !app.session.prompting() && self.draft.as_ref().is_none_or(|draft| draft.next.is_none());
        let selection = self.segments.selection().cloned();
        ui.add_enabled_ui(paths_enabled, |ui| {
            self.segments.controls(ui, &self.snapshot.curves);
        });
        if self.segments.selection() != selection.as_ref() {
            self.coincident.clear();
        }
        self.sync_draft();
        if let Some(segment) = self.segments.selection() {
            if let Some(curve) = self.snapshot.curves.iter().find(|curve| curve.handle == segment.handle) {
                let height = self.draft.as_ref().map_or(0.0, |draft| draft.point().position.z);
                if self.chain.controls(app, ui, group, curve, height, paths_enabled && !self.segments.inserting()) {
                    return;
                }
            }
            let label = t("Delete Source Line");
            let delete = ui.add_enabled(paths_enabled && !self.segments.inserting(), egui::Button::new(label));
            crate::control::record_widget(ui, &delete, label, "button", None);
            if delete.clicked() && app.run("tin.source.delete", json!({"group":group, "handle":segment.handle})).is_ok() {
                self.stop();
                ui.ctx().request_repaint();
                return;
            }
        } else {
            self.chain.cancel();
        }
        if self.draft.is_none() && !self.segments.inserting() {
            ui.label(t("Click a source point, its elevation label, or a segment."));
        }
        if !self.segments.inserting() {
            self.coincident_points(ui);
            self.fields(app, ui, group);
        }
    }

    fn activate_point(&mut self, point: &Point) -> bool {
        if point.single {
            self.segments.stop();
            self.selected_point = Some(point.handle.clone());
        } else if !self.segments.select_vertex(&self.snapshot.curves, &point.handle, point.vertex) {
            return false;
        }
        self.sync_draft();
        let Some(draft) = &mut self.draft else { return false };
        if let Some(index) = draft.choices.iter().position(|p| p.handle == point.handle && p.vertex == point.vertex) {
            if draft.selected != index {
                draft.selected = index;
                draft.reset_fields();
            }
            draft.focus = true;
            return true;
        }
        false
    }

    fn coincident_points(&mut self, ui: &mut egui::Ui) {
        if self.coincident.len() < 2 || self.draft.as_ref().is_some_and(|draft| draft.next.is_some()) {
            return;
        }
        let caption = ui.label(t("Points at This Location"));
        let active = self.draft.as_ref().map(|draft| draft.point());
        let label = |p: &Point| format!("Z {:.4} · {} / {}", p.position.z, p.handle, p.vertex + 1);
        let combo =
            craft_ui::ComboBox::from_id_salt("coincident-source-points").selected_text(active.map_or_else(String::new, label)).show_ui(ui, |ui| {
                let mut selected = None;
                for (handle, vertex) in &self.coincident {
                    if let Some(point) = self.snapshot.points.iter().find(|p| &p.handle == handle && p.vertex == *vertex)
                        && ui.selectable_label(active.is_some_and(|a| a.handle == point.handle && a.vertex == point.vertex), label(point)).clicked()
                    {
                        selected = Some(point.clone());
                    }
                }
                selected
            });
        let selector = combo.response.labelled_by(caption.id);
        crate::control::record_widget(ui, &selector, t("Points at This Location"), "combobox", None);
        if let Some(point) = combo.inner.flatten() {
            self.activate_point(&point);
        }
    }

    fn sync_draft(&mut self) {
        if let Some(segment) = self.segments.selection() {
            if self.draft.as_ref().and_then(|draft| draft.segment.as_ref()) == Some(segment) {
                return;
            }
            let point = |index| self.snapshot.points.iter().find(|p| p.handle == segment.handle && p.vertex == index);
            let next = point(segment.index + 1).or_else(|| point(0));
            let choices = point(segment.index).into_iter().chain(next).cloned().collect::<Vec<_>>();
            self.selected_point = None;
            self.draft = (!choices.is_empty()).then(|| Draft::new(Some(segment.clone()), choices));
        } else if let Some(handle) = &self.selected_point {
            if self.draft.as_ref().is_some_and(|draft| draft.segment.is_none() && draft.point().handle == *handle) {
                return;
            }
            self.draft = self.snapshot.points.iter().find(|p| p.handle == *handle && p.single).cloned().map(|p| Draft::new(None, vec![p]));
            if self.draft.is_none() {
                self.selected_point = None;
            }
        } else {
            self.draft = None;
        }
    }

    pub(super) fn cuts(&self) -> &[String] {
        &self.snapshot.cuts
    }

    fn elevations_toggle(&mut self, ui: &mut egui::Ui, label: &str) {
        let label = t(label);
        let elevations = ui.checkbox(&mut self.show_heights, label);
        crate::control::record_widget(ui, &elevations, label, "checkbox", Some(self.show_heights));
    }

    fn fields(&mut self, app: &mut CadApp, ui: &mut egui::Ui, group: &str) {
        let Some(draft) = &mut self.draft else { return };
        let point_label = |i: usize, p: &Point| {
            let endpoint = if i == 0 { t("Start Point") } else { t("End Point") };
            format!("{endpoint}  Z {:.4}", p.position.z)
        };
        if draft.choices.len() > 1 {
            let previous = draft.selected;
            let caption = ui.label(t("Source Point"));
            let selector = craft_ui::ComboBox::from_id_salt("source-point")
                .selected_text(point_label(draft.selected, draft.point()))
                .show_ui(ui, |ui| {
                    for (i, p) in draft.choices.iter().enumerate() {
                        ui.selectable_value(&mut draft.selected, i, point_label(i, p));
                    }
                })
                .response
                .labelled_by(caption.id);
            crate::control::record_widget(ui, &selector, t("Source Point"), "combobox", None);
            if previous != draft.selected {
                draft.reset_fields();
            }
        }
        let point = draft.point().clone();
        if let Some(number) = point.line_number {
            ui.label(crate::tf!("Road Line {number}", number = number));
        }
        let enabled = !point.locked;
        let (x, y, height) = egui::Grid::new("source-point-xyz")
            .show(ui, |ui| {
                let height = field(ui, "New Elevation", &mut draft.height, enabled);
                ui.end_row();
                let x = field(ui, "X", &mut draft.x, enabled);
                ui.end_row();
                let y = field(ui, "Y", &mut draft.y, enabled);
                ui.end_row();
                (x, y, height)
            })
            .inner;
        if std::mem::take(&mut draft.focus) && enabled {
            height.request_focus();
            if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), height.id) {
                let end = egui::text::CCursor::new(draft.height.chars().count());
                state.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), end)));
                state.store(ui.ctx(), height.id);
            }
        }
        if point.locked {
            ui.label(t("The source is on a locked layer."));
        } else if draft.extending {
            ui.label(t("Add a new point beyond this endpoint."));
        } else if point.arc {
            ui.label(t("Editing an arc endpoint converts the arc to a 3D polyline."));
        } else if point.whole {
            ui.label(t("Different vertex elevations convert a 2D polyline to a 3D polyline."));
        }
        let position = draft.position().filter(|&to| !draft.extending || to != point.position);
        let enter = (x.lost_focus() || y.lost_focus() || height.lost_focus()) && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let mut finished = false;
        ui.horizontal_wrapped(|ui| {
            let pending = draft.next.is_some();
            let pick_xy = t("Pick Position");
            let xy = ui.add_enabled(enabled && !pending, egui::Button::new(pick_xy));
            crate::control::record_widget(ui, &xy, pick_xy, "button", None);
            if xy.clicked() {
                draft.next = Some(PickMode::Position(coordinate(&draft.height).unwrap_or(point.position.z)));
                draft.error = None;
            }
            let pick_z = t("Pick Elevation from Line");
            let z = ui.add_enabled(enabled && !pending, egui::Button::new(pick_z));
            crate::control::record_widget(ui, &z, pick_z, "button", None);
            if z.clicked()
                && let Some(handle) = Handle::parse_hex(&point.handle)
            {
                draft.next = Some(PickMode::Elevation(handle));
                draft.error = None;
            }
        });
        if point.extensible && !draft.extending {
            let label = t("Extend Source Line");
            let button = ui.add_enabled(enabled && draft.next.is_none(), egui::Button::new(label));
            crate::control::record_widget(ui, &button, label, "button", None);
            if button.clicked() {
                draft.reset_fields();
                draft.extending = true;
                draft.next = Some(PickMode::Position(point.position.z));
            }
        }
        let prompt = match draft.next {
            Some(PickMode::Position(_)) => Some("Click the new position in the drawing."),
            Some(PickMode::Elevation(_)) => Some("Click another line, polyline, or arc to read its elevation."),
            _ => None,
        };
        if let Some(error) = draft.error.or(prompt) {
            ui.label(t(error));
        }
        ui.horizontal_wrapped(|ui| {
            let apply = ui.add_enabled(enabled && position.is_some(), egui::Button::new(t("Apply")));
            crate::control::record_widget(ui, &apply, t("Apply"), "button", None);
            if enabled
                && (apply.clicked() || enter)
                && let Some(position) = position
            {
                let command = if draft.extending { "tin.source.extend" } else { "tin.source.move" };
                finished = app
                    .run(
                        command,
                        json!({"group":group, "handle":point.handle, "vertex":point.vertex,
                    "position":[position.x,position.y,position.z]}),
                    )
                    .is_ok();
            }
            let remove = ui
                .add_enabled(enabled && point.removable && !draft.extending, egui::Button::new(t("Remove Point")))
                .on_disabled_hover_text(t("A closed polyline needs at least three points."));
            let remove =
                if point.endpoint_only { remove.on_hover_text(t("Removing this endpoint leaves the opposite endpoint as a point.")) } else { remove };
            crate::control::record_widget(ui, &remove, t("Remove Point"), "button", None);
            if remove.clicked() {
                finished = app.run("tin.source.remove", json!({"group":group, "handle":point.handle, "vertex":point.vertex})).is_ok();
            }
            if ui.button(t("Cancel")).clicked() {
                if draft.next.take().is_some() {
                    draft.error = None;
                } else if draft.extending {
                    draft.reset_fields();
                    draft.focus = true;
                } else {
                    finished = true;
                }
            }
        });
        if finished {
            self.coincident.clear();
            draft.reset_fields();
            ui.ctx().request_repaint();
        }
    }
}

fn current_overlay(app: &CadApp) -> bool {
    app.session.state().ok().zip(app.surface_editor.source.key.as_ref()).is_some_and(|(doc, (uid, _, name))| {
        doc.uid == *uid
            && doc.space == cadcraft_doc::Space::Model
            && doc.block_editor().is_none()
            && (app.ui.dialog.as_deref() != Some(super::DIALOG) || app.surface_editor.selected == *name)
    })
}

pub(crate) fn refresh_overlay(app: &mut CadApp) {
    let Ok(doc) = app.session.state() else { return };
    let key = (doc.uid, doc.revision);
    let state = &app.surface_editor;
    if app.ui.dialog.as_deref() == Some(super::DIALOG) || state.source.key.as_ref().is_some_and(|(uid, rev, _)| (*uid, *rev) == key) {
        return;
    }
    let previous = state.source.key.as_ref().filter(|(uid, _, _)| *uid == key.0).map(|(_, _, name)| name.as_str());
    let mut linked = doc.doc.groups.iter().filter(|group| {
        group.linked_terrain_id().is_some()
            && (group.members.is_empty() || group.members.iter().any(|&handle| doc.doc.model.get(handle).is_some_and(|entity| entity.common.visible)))
    });
    let Some(first) = linked.next() else {
        app.surface_editor.source.key = None;
        return;
    };
    let group = std::iter::once(first).chain(linked).find(|group| Some(group.name.as_str()) == previous).unwrap_or(first).name.clone();
    let mut source = std::mem::take(&mut app.surface_editor.source);
    source.prepare(app, key, &group, true);
    app.surface_editor.source = source;
}

pub(crate) fn toolbar(app: &mut CadApp, ui: &mut egui::Ui) {
    if current_overlay(app) {
        let source = &mut app.surface_editor.source;
        ui.horizontal_wrapped(|ui| {
            source.elevations_toggle(ui, "Source Elevations");
            let label = t("Elevation Label Size");
            let size =
                ui.add_enabled(source.show_heights, egui::Slider::new(&mut source.label_size, 8.0..=26.0).step_by(1.0).suffix(" px").text(label));
            crate::control::record_widget(ui, &size, label, "slider", None);
        });
    }
}

pub(crate) fn picking(app: &CadApp) -> bool {
    current_overlay(app) && app.ui.dialog.as_deref() == Some(super::DIALOG) && !app.session.prompting()
}

pub(crate) fn pick_mode(app: &CadApp) -> Option<PickMode> {
    if !picking(app) {
        return None;
    }
    app.surface_editor.source.draft.as_ref()?.next
}

pub(crate) fn pick_base(app: &CadApp) -> Option<Vec3> {
    app.surface_editor.source.draft.as_ref().map(|draft| draft.point().position)
}

pub(crate) fn point_pick_active(app: &CadApp) -> bool {
    picking(app) && (pick_mode(app).is_some() || app.surface_editor.source.segments.inserting())
}

pub(crate) fn snap_segment_placement(app: &CadApp) -> bool {
    app.surface_editor.source.segments.snap_enabled()
}

pub(crate) fn accepts_insertion(app: &CadApp, position: Vec3) -> bool {
    let source = &app.surface_editor.source;
    source.segments.accepts_insertion(&source.snapshot.curves, position)
}

pub(crate) fn segment_input(
    app: &mut CadApp,
    at: Option<Pos2>,
    project: impl Fn(Vec3) -> Option<(Pos2, f64)>,
    position: impl Fn(Pos2, Vec3) -> Option<Vec3>,
    snap: Option<(Vec3, SnapHit)>,
    clicked: bool,
    painter: &Painter,
) {
    let state = &mut app.surface_editor.source;
    let inserting = state.segments.inserting();
    let pressed = at.filter(|_| clicked && !inserting);
    if let Some(at) = pressed {
        if pick_vertex(state, at, painter, &project) {
            return;
        }
        state.selected_point = None;
        state.coincident.clear();
    }
    if let Some(mut params) = state.segments.input(&state.snapshot.curves, at, &project, position, snap, clicked)
        && let Some((_, _, group)) = &state.key
    {
        params["group"] = json!(group);
        if app.run("tin.source.insert", params).is_ok() {
            app.surface_editor.source.segments.finish_insert();
        }
    } else if pressed.is_some() {
        state.sync_draft();
    }
}

fn pick_vertex(state: &mut SourceControls, at: Pos2, painter: &Painter, project: &impl Fn(Vec3) -> Option<(Pos2, f64)>) -> bool {
    let marker = |p| project(p).map(|(position, _)| position);
    let mut candidates = state.snapshot.points.iter().filter(|p| !p.locked).filter_map(|p| marker(p.position).map(|screen| (p, screen)));
    let near = candidates.clone().filter(|(_, p)| p.distance(at) <= 12.0).min_by(|a, b| a.1.distance(at).total_cmp(&b.1.distance(at)));
    let label = if state.show_heights && near.is_none_or(|(_, position)| position.distance(at) > 4.0) {
        height_labels(&state.snapshot, painter, state.label_size, &marker).find(|label| label.bounds.expand(2.0).contains(at))
    } else {
        None
    };
    let best = label.and_then(|label| candidates.find(|(point, _)| point.position == label.world)).or(near);
    let Some((point, screen)) = best else { return false };
    let point = point.clone();
    state.coincident = state
        .snapshot
        .points
        .iter()
        .filter(|p| !p.locked && marker(p.position).is_some_and(|p| p.distance(screen) <= 3.0))
        .map(|p| (p.handle.clone(), p.vertex))
        .collect();
    state.activate_point(&point)
}

pub(crate) fn cancel_pick(app: &mut CadApp) -> bool {
    if !picking(app) {
        return false;
    }
    let state = &mut app.surface_editor.source;
    if let Some(draft) = &mut state.draft
        && draft.next.take().is_some()
    {
        draft.error = None;
    } else if state.segments.inserting() {
        state.segments.cancel();
    } else if state.chain.cancel() {
        // Closing a chain form leaves its source selected.
    } else {
        state.stop();
    }
    true
}

pub(crate) fn preview_pick(app: &mut CadApp, target: Option<Vec3>, snap: Option<(Vec3, SnapHit)>, clicked: bool) {
    let Some(draft) = &mut app.surface_editor.source.draft else { return };
    let Some(mode) = draft.next else { return };
    let destination = target.and_then(|point| match mode {
        PickMode::Position(_) => Some(point),
        PickMode::Elevation(_) => Some(Vec3::new(coordinate(&draft.x)?, coordinate(&draft.y)?, point.z)),
    });
    draft.guide = Some(PickGuide { destination, snap });
    if !clicked {
        return;
    }
    if let Some(point) = target {
        match mode {
            PickMode::Position(_) => {
                draft.x = point.x.to_string();
                draft.y = point.y.to_string();
                draft.height = point.z.to_string();
            }
            PickMode::Elevation(_) => {
                draft.height = point.z.to_string();
                draft.focus = true;
            }
        }
        draft.next = None;
        draft.guide = None;
        draft.error = None;
    } else {
        draft.error = Some(match mode {
            PickMode::Position(_) => "The view cannot pick a position at this elevation. Rotate the view or enter X and Y.",
            PickMode::Elevation(_) => "No other line under the cursor. Click a line, polyline, or arc.",
        });
    }
}

pub(crate) fn dashed_guide(painter: &Painter, path: &[Vec3], project: &impl Fn(Vec3) -> Option<Pos2>, stroke: Stroke) {
    const DASH: f64 = 7.0;
    const PERIOD: f64 = 11.0;
    let mut phase = 0.0;
    for edge in path.windows(2) {
        let Some((a, b)) = project(edge[0]).zip(project(edge[1])) else {
            phase = 0.0;
            continue;
        };
        if let Some([start, end]) = super::cuts::clip_line([a, b], painter.clip_rect()) {
            let length = f64::from(start.distance(end));
            let mut offset = -(phase + f64::from(a.distance(start))).rem_euclid(PERIOD);
            while offset < length {
                let from = offset.max(0.0);
                let to = (offset + DASH).min(length);
                if from < to {
                    let at = |distance| start + (end - start) * (distance / length) as f32;
                    painter.line_segment([at(from), at(to)], stroke);
                }
                offset += PERIOD;
            }
        }
        phase = (phase + f64::from(a.distance(b))).rem_euclid(PERIOD);
    }
}

pub(crate) fn paint(app: &CadApp, painter: &Painter, project: impl Fn(Vec3) -> Option<Pos2>) {
    if !current_overlay(app) {
        return;
    }
    let state = &app.surface_editor.source;
    let colors = Tokens::get();
    let clip = painter.clip_rect();
    let in_editor = app.ui.dialog.as_deref() == Some(super::DIALOG);
    let draw_path = |path: &[Vec3], stroke: Stroke| {
        for edge in path.windows(2) {
            if let Some((a, b)) = project(edge[0]).zip(project(edge[1]))
                && clip.intersects(Rect::from_two_pos(a, b).expand(stroke.width))
            {
                painter.line_segment([a, b], stroke);
            }
        }
    };
    if in_editor {
        super::cuts::paint(app, painter, &project);
        if let Some(path) = state.chain.preview() {
            dashed_guide(painter, path, &project, Stroke::new(3.0, colors.compare_area));
        }
    }
    if in_editor && state.show_lines {
        let stroke = Stroke::new(state.width, colors.hover.gamma_multiply(0.78));
        for path in state.snapshot.curves.iter().flat_map(|curve| &curve.segments) {
            draw_path(path, stroke);
        }
    }
    if in_editor {
        let width = (state.width + 2.5).max(5.0);
        state.segments.paint(&state.snapshot.curves, painter, width, draw_path, &project);
    }
    if in_editor {
        for point in state.snapshot.points.iter().filter(|point| point.single) {
            if let Some(at) = project(point.position).filter(|p| clip.contains(*p)) {
                painter.circle_filled(at, 2.8, colors.hover);
            }
        }
    }
    if state.show_heights {
        for HeightLabel { at, bounds, galley, .. } in height_labels(&state.snapshot, painter, state.label_size, &project) {
            painter.circle_filled(at, 1.4, colors.canvas_ink);
            if !bounds.expand(8.0).contains(at) {
                painter.line_segment([at, bounds.clamp(at)], Stroke::new(0.7, colors.canvas_ink));
            }
            painter.rect_filled(bounds.expand(1.0), 2.0, colors.canvas);
            painter.galley(bounds.min, galley, colors.canvas_ink);
        }
    }
    if in_editor {
        if let Some(draft) = &state.draft {
            let point = draft.point();
            let from = point.position;
            let guide = if draft.next.is_some() { draft.guide } else { Some(PickGuide { destination: draft.position(), snap: None }) };
            if let Some(guide) = guide {
                if let Some(to) = guide.destination.filter(|&to| to != from) {
                    let stroke = Stroke::new(2.0, colors.snap);
                    if draft.extending {
                        dashed_guide(painter, &[from, to], &project, stroke);
                    } else if point.single {
                        if let Some(at) = project(to).filter(|p| clip.contains(*p)) {
                            painter.circle_stroke(at, 5.0, stroke);
                        }
                    } else if let Some(handle) = Handle::parse_hex(&point.handle) {
                        for path in app.session.terrain_source_move_paths(handle, point.vertex, from, to) {
                            dashed_guide(painter, &path, &project, Stroke::new(4.0, colors.canvas));
                            dashed_guide(painter, &path, &project, stroke);
                        }
                    }
                }
                if let Some((position, hit)) = guide.snap
                    && let Some(screen) = project(position).filter(|p| clip.contains(*p))
                {
                    crate::canvas::draw_snap_marker(painter, screen, &hit);
                }
            }
        }
        let active = state.draft.as_ref().and_then(|d| project(d.point().position));
        if let Some(at) = active.filter(|p| clip.contains(*p)) {
            let size = 6.0;
            let corners = [egui::vec2(0.0, -size), egui::vec2(size, 0.0), egui::vec2(0.0, size), egui::vec2(-size, 0.0)];
            painter.add(egui::Shape::convex_polygon(corners.map(|d| at + d).to_vec(), colors.canvas, Stroke::new(2.0, colors.snap)));
        }
    }
}

struct HeightLabel {
    world: Vec3,
    at: Pos2,
    bounds: Rect,
    galley: std::sync::Arc<egui::Galley>,
}

fn height_labels<'a>(
    snapshot: &'a Snapshot,
    painter: &'a Painter,
    size: f32,
    project: impl Fn(Vec3) -> Option<Pos2> + 'a,
) -> impl Iterator<Item = HeightLabel> + 'a {
    let mut layout = LabelLayout::default();
    let font = FontId::monospace(size);
    let clip = painter.clip_rect();
    let ink = Tokens::get().canvas_ink;
    snapshot.labels.iter().filter_map(move |&(world, ref text)| {
        let at = project(world).filter(|p| clip.contains(*p))?;
        let galley = painter.layout_no_wrap(text.clone(), font.clone(), ink);
        let bounds = layout.place(at, galley.size(), clip);
        Some(HeightLabel { world, at, bounds, galley })
    })
}

#[derive(Default)]
struct LabelLayout {
    occupied: HashSet<(i32, i32)>,
    coincident: HashMap<(i32, i32), usize>,
}

impl LabelLayout {
    fn place(&mut self, point: Pos2, size: egui::Vec2, clip: Rect) -> Rect {
        let anchor = (point.x.round() as i32, point.y.round() as i32);
        let order = self.coincident.entry(anchor).or_default();
        let row = (*order / 2).min(20) as f32 * (size.y + 2.0);
        let side = order.is_multiple_of(2);
        *order += 1;
        let right = 4.0;
        let left = -size.x - 4.0;
        let (first, second) = if side { (right, left) } else { (left, right) };
        let above = -size.y - 3.0 - row;
        let below = 3.0 + row;
        let candidate = |dx, dy| Rect::from_min_size(point + egui::vec2(dx, dy), size);
        let choices = [candidate(first, above), candidate(second, above), candidate(first, below), candidate(second, below)];
        let bounds = choices
            .iter()
            .copied()
            .find(|&rect| clip.contains_rect(rect) && Self::cells(rect).all(|p| !self.occupied.contains(&p)))
            .or_else(|| choices.into_iter().find(|&r| clip.contains_rect(r)))
            .unwrap_or_else(|| Align2::LEFT_TOP.anchor_size(clip.clamp(choices[0].min), size));
        self.occupied.extend(Self::cells(bounds));
        bounds
    }

    fn cells(rect: Rect) -> impl Iterator<Item = (i32, i32)> {
        let cell = |c: f32| (c / 12.0).floor() as i32;
        (cell(rect.min.y)..=cell(rect.max.y)).flat_map(move |y| (cell(rect.min.x)..=cell(rect.max.x)).map(move |x| (x, y)))
    }
}
