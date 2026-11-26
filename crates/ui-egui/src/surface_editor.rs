//! A modeless editor that keeps the chosen surface while the user works on the canvas.
use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::{CadApp, i18n::t};

mod cuts;
mod source;
pub(crate) use cuts::visible_triangles as removed_triangles;
pub(crate) use source::{
    PickMode, accepts_insertion, cancel_pick as cancel_source_pick, dashed_guide, paint as paint_sources, pick_base, pick_mode,
    picking as picking_source, point_pick_active, preview_pick, refresh_overlay, segment_input, snap_segment_placement, toolbar,
};

pub(crate) const COMMAND: &str = "ui.terrain.editor";
pub(crate) const DIALOG: &str = "surface_editor";
pub(crate) const DRAW_SOURCE: &str = "tin.source.draw";
pub(crate) const REMOVE_CROSSED: &str = "tin.removecrossed";
pub(crate) const RESTORE_CROSSED: &str = "tin.restorecrossed";
pub(crate) const MOVE_SURFACE_VERTEX: &str = "tin.movevertex";

const TOOLS: &[(&str, &str)] = &[
    ("tin.elevation", "Set Vertex Elevation"),
    (MOVE_SURFACE_VERTEX, "Move Surface Vertex"),
    ("tin.swapedge", "Swap Triangle Edge"),
    ("tin.breakline", "Apply 3D Breakline"),
    ("tin.clip", "Clip Surface to Boundary"),
    ("tin.cuthole", "Cut Hole in Surface"),
    ("tin.removetriangles", "Remove Surface Triangles"),
];

#[derive(Default)]
pub(crate) struct State {
    model: Option<(u64, u64)>,
    surfaces: Vec<String>,
    visibility: BTreeMap<String, bool>,
    selected: String,
    requested: Option<String>,
    road_error: Option<String>,
    focus_surface: bool,
    source: source::SourceControls,
    cuts: cuts::State,
    window_rect: Option<egui::Rect>,
}

pub(crate) fn open(app: &mut CadApp) -> Result<Value, String> {
    if !crate::terrain_display::available(app) {
        return Err(t("Open or create a terrain surface on the Model tab.").into());
    }
    app.surface_editor.focus_surface = app.ui.dialog.as_deref() != Some(DIALOG);
    app.ui.dialog = Some(DIALOG.into());
    Ok(Value::Null)
}

pub(crate) fn open_surface(app: &mut CadApp, name: &str) -> Result<Value, String> {
    let result = open(app)?;
    app.surface_editor.requested = Some(name.into());
    Ok(result)
}

fn start_tool(app: &mut CadApp, id: &str, selected: &str) -> Result<(), String> {
    app.cmd.buffer.clear();
    app.session.start(id).map_err(|error| error.to_string())?;
    // Send the editor's surface name through the same prompt used at the command line.
    if matches!(id, DRAW_SOURCE | REMOVE_CROSSED | RESTORE_CROSSED) {
        return app.session.input(cadcraft_engine::Input::Text(selected.into())).map_err(|error| error.to_string());
    }
    if app.session.current_prompt().is_some_and(|p| p.keywords.iter().any(|k| k == "Name")) {
        app.session.input(cadcraft_engine::Input::Keyword("Name".into())).map_err(|error| error.to_string())?;
        app.session.input(cadcraft_engine::Input::Text(selected.into())).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn export_menu(ui: &mut egui::Ui) -> Option<(Option<i64>, bool)> {
    let label = t("Export LandXML 1.2…");
    let menu = ui.menu_button(label, |ui| {
        if ui
            .button(t("Survey Compatibility (Metres)"))
            .on_hover_text(t("Use a 1 mm grid for the exported terrain and split intersecting source lines."))
            .clicked()
        {
            ui.close();
            return Some((Some(6), true));
        }
        ui.separator();
        for (label, unit) in [("Use Drawing Units", None), ("Metres", Some(6)), ("Millimetres", Some(4)), ("Feet", Some(2))] {
            if ui.button(t(label)).clicked() {
                ui.close();
                return Some((unit, false));
            }
        }
        None
    });
    crate::control::record_widget(ui, &menu.response, label, "button", None);
    menu.inner.flatten()
}

fn visibility_menu(ui: &mut egui::Ui, state: &State) -> Option<Value> {
    let label = t("Surface Visibility");
    let menu = ui.menu_button(label, |ui| {
        let label = t("Show Only Selected Surface");
        let only = ui.button(label);
        crate::control::record_widget(ui, &only, label, "button", None);
        if only.clicked() {
            ui.close();
            return Some(json!({"group":state.selected, "only":true}));
        }
        ui.separator();
        let mut change = None;
        for name in &state.surfaces {
            let mut visible = state.visibility.get(name).copied().unwrap_or(true);
            let checkbox = ui.checkbox(&mut visible, name);
            crate::control::record_widget(ui, &checkbox, name, "checkbox", Some(visible));
            if checkbox.changed() {
                change = Some(json!({"group":name, "visible":visible}));
            }
        }
        change
    });
    crate::control::record_widget(ui, &menu.response, label, "button", None);
    menu.inner.flatten()
}

pub(crate) fn selected_surface(app: &CadApp) -> Option<&cadcraft_doc::Group> {
    if app.ui.dialog.as_deref() != Some(DIALOG) {
        return None;
    }
    app.session.doc().ok()?.groups.iter().find(|group| group.name == app.surface_editor.selected)
}

pub(crate) fn covers_canvas_point(app: &CadApp, at: egui::Pos2) -> bool {
    window_rect(app).is_some_and(|rect| rect.contains(at))
}

pub(crate) fn window_rect(app: &CadApp) -> Option<egui::Rect> {
    app.surface_editor.window_rect.filter(|_| app.ui.dialog.as_deref() == Some(DIALOG))
}

pub(crate) fn dialog(app: &mut CadApp, ctx: &egui::Context, open: &mut bool) {
    if !crate::terrain_display::available(app) {
        *open = false;
        return;
    }
    let Ok(doc) = app.session.state() else { return };
    let key = (doc.uid, doc.revision);
    let mut state = std::mem::take(&mut app.surface_editor);
    if state.model != Some(key) {
        match app.run("tin.surfaces", json!({"editable": true, "linked": true, "road": true, "visibility": true})) {
            Ok(result) => {
                state.surfaces =
                    result.get("surfaces").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
                state.visibility = serde_json::from_value(result["visibility"].clone()).unwrap_or_default();
                if state.model.is_none_or(|(uid, _)| uid != key.0) || !state.surfaces.contains(&state.selected) {
                    state.selected = state.surfaces.first().cloned().unwrap_or_default();
                    state.road_error = None;
                }
                state.model = Some(key);
            }
            Err(error) => {
                app.set_status(error);
                *open = false;
                return;
            }
        }
    }
    if let Some(name) = state.requested.take().filter(|name| state.surfaces.contains(name)) {
        state.selected = name;
        state.road_error = None;
    }
    let active_fence = app.session.running.as_ref().is_some_and(|run| matches!(run.id.as_str(), REMOVE_CROSSED | RESTORE_CROSSED));
    let restoring = app.session.running.as_ref().is_some_and(|run| run.id == RESTORE_CROSSED);
    let active_path = active_fence || app.session.running.as_ref().is_some_and(|run| run.id == DRAW_SOURCE);
    let active_tool = active_path || app.session.running.as_ref().is_some_and(|run| TOOLS.iter().any(|(id, _)| run.id == *id));
    let mut start = None;
    let mut cancel = false;
    let mut finish = false;
    let mut clear = false;
    let mut export = None;
    let mut editable_copy = false;
    let mut visibility = None;
    let mut bounds = ctx.content_rect();
    if let Some(bottom) = app.canvas.viewport_tools_bottom() {
        bounds.min.y = (bottom + 6.0).min(bounds.max.y);
    }
    let window = egui::Window::new(t("Surface Editor"))
        .id(egui::Id::unique(DIALOG))
        .default_pos(bounds.right_top() + egui::vec2(-315.0, 0.0))
        .constrain_to(bounds)
        .default_width(270.0)
        .max_width(300.0)
        .default_height((bounds.height() - 32.0).max(0.0))
        .max_height((bounds.height() - 32.0).max(0.0))
        .resizable(false)
        .vscroll(true)
        .open(open)
        .show(ctx, |ui| {
            ui.set_max_width(300.0);
            if state.surfaces.is_empty() {
                ui.label(t("No editable terrain surfaces in this drawing."));
                return;
            }
            let caption = ui.label(t("Terrain surface"));
            let selector = ui
                .add_enabled_ui(!active_tool, |ui| {
                    craft_ui::ComboBox::from_id_salt("surface").selected_text(&state.selected).width(235.0).show_ui(ui, |ui| {
                        for name in &state.surfaces {
                            if ui.selectable_value(&mut state.selected, name.clone(), name).changed() {
                                state.road_error = None;
                            }
                        }
                    })
                })
                .inner
                .response
                .labelled_by(caption.id);
            crate::control::record_widget(ui, &selector, "Terrain surface", "combobox", None);
            if std::mem::take(&mut state.focus_surface) {
                selector.request_focus();
            }
            if !active_tool {
                ui.horizontal_wrapped(|ui| {
                    visibility = visibility_menu(ui, &state);
                    export = export_menu(ui);
                });
            }
            let group = app.session.doc().ok().and_then(|doc| doc.groups.iter().find(|g| g.name == state.selected));
            if group.is_some_and(|g| g.description.starts_with(cadcraft_doc::road::SURFACE_PREFIX)) {
                state.source.prepare(app, key, &state.selected, false);
                let label = t("Open Editable Copy");
                let button = ui.add_enabled(!app.session.prompting(), egui::Button::new(label));
                crate::control::record_widget(ui, &button, label, "button", None);
                editable_copy = button.clicked();
                if let Some(error) = &state.road_error {
                    ui.label(error);
                }
                return;
            }
            let linked = group.is_some_and(|g| g.linked_terrain_id().is_some());
            let visible = state.visibility.get(&state.selected).copied().unwrap_or(true);
            state.source.prepare(app, key, &state.selected, linked && visible);
            if !visible && !active_tool {
                ui.label(t("This surface is hidden."));
                return;
            }
            if linked && !active_path {
                state.source.controls(app, ui, &state.selected);
                state.cuts.controls(app, ui, &state.selected, state.source.cuts(), key);
            } else if linked && restoring && state.cuts.show_all(app, &state.selected, key) {
                ui.ctx().request_repaint();
            }
            for &(id, label) in TOOLS.iter().filter(|_| !linked) {
                let button = ui.add_enabled(!active_tool, egui::Button::new(t(label)));
                if button.clicked() {
                    button.surrender_focus();
                    start = Some(id);
                }
            }
            if !active_tool {
                ui.horizontal_wrapped(|ui| {
                    if linked {
                        let label = t("Draw Source Line");
                        let button = ui.add_enabled(!app.session.prompting(), egui::Button::new(label));
                        crate::control::record_widget(ui, &button, label, "button", None);
                        if button.clicked() {
                            start = Some(DRAW_SOURCE);
                        }
                    }
                    let label = t("Remove Triangles Along Line");
                    let button = ui.add_enabled(!app.session.prompting(), egui::Button::new(label));
                    crate::control::record_widget(ui, &button, label, "button", None);
                    if button.clicked() {
                        start = Some(REMOVE_CROSSED);
                    }
                    if linked && !state.source.cuts().is_empty() {
                        let label = t("Restore Triangles Along Line");
                        let button = ui.add_enabled(!app.session.prompting(), egui::Button::new(label));
                        crate::control::record_widget(ui, &button, label, "button", None);
                        if button.clicked() {
                            start = Some(RESTORE_CROSSED);
                        }
                    }
                });
            }
            if active_tool && let Some(prompt) = app.session.current_prompt() {
                ui.separator();
                ui.label(t(&prompt.message));
                ui.horizontal_wrapped(|ui| {
                    if active_fence {
                        finish = ui.add_enabled(prompt.base.is_some(), egui::Button::new(t("Apply Line"))).clicked();
                        clear = ui.add_enabled(prompt.base.is_some(), egui::Button::new(t("Discard Line"))).clicked();
                    } else if active_path {
                        finish = ui.button(t("Finish")).clicked();
                    }
                    let label = if active_fence { "Stop" } else { "Cancel command" };
                    cancel = ui.button(t(label)).clicked();
                });
            }
        });
    state.window_rect = window.map(|window| window.response.rect);
    if editable_copy {
        state.road_error = match app.run("tin.road.editable", json!({"surface":state.selected, "hideOriginal":true})) {
            Ok(result) => {
                state.requested = result["group"].as_str().map(str::to_owned);
                state.focus_surface = true;
                ctx.request_repaint();
                None
            }
            Err(error) => Some(error),
        };
    }
    if let Some(params) = visibility {
        match app.run("tin.surface.visibility", params) {
            Ok(result) => {
                if let Some(message) = result["message"].as_str() {
                    app.set_status(t(message));
                }
                ctx.request_repaint();
            }
            Err(error) => app.set_status(error),
        }
    }
    if let Some((unit, survey)) = export
        && let Err(error) = crate::exchange::export_terrain(app, state.selected.clone(), unit, survey)
    {
        app.set_status(error);
    }
    if let Some(id) = start {
        state.source.stop();
        if let Err(error) = start_tool(app, id, &state.selected) {
            app.set_status(error);
        }
    }
    if finish && let Err(error) = app.session.input(cadcraft_engine::Input::Enter) {
        app.set_status(error.to_string());
    }
    if clear {
        let _ = app.session.input(cadcraft_engine::Input::Keyword("Clear".into()));
    }
    if cancel {
        app.session.cancel();
        state.focus_surface = true;
    }
    if !*open {
        state.source.stop();
        if active_fence {
            app.session.cancel();
        }
    }
    app.surface_editor = state;
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable;

    #[test]
    fn sets_a_chain_elevation_and_previews_a_grade_to_a_different_surface() {
        use cadcraft_doc::{EntityKind, Handle};
        use cadcraft_geom::Vec2;
        use egui::{Event, Modifiers, PointerButton, Shape};

        let mut session = cadcraft_engine::Session::new();
        let line = session.execute("3dpoly", &json!({"points":[[0,0,10],[8,0,11],[8,6,10],[0,6,10]],"closed":true})).unwrap();
        let handle = Handle::parse_hex(line["handle"].as_str().unwrap()).unwrap();
        session.execute("tin.link", &json!({"name":"Footing","handles":[line["handle"]]})).unwrap();
        session.execute("tin", &json!({"name":"Ground","standalone":true,"points":[[-5,-5,8],[15,-5,8],[15,15,8],[-5,15,8]]})).unwrap();
        session.zoom_extents().unwrap();
        let mut app = CadApp::new(session, Default::default());
        app.start(COMMAND);
        let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(1100.0, 740.0)).build_ui_state(
            |ui, app: &mut CadApp| {
                crate::canvas::show(app, ui);
                crate::dialogs::show(app, ui.ctx());
            },
            app,
        );
        h.run_steps(3);
        let at = h.state().canvas.xf.unwrap().to_screen(Vec2::new(4.0, 0.0));
        h.event(Event::PointerMoved(at));
        h.run();
        for pressed in [true, false] {
            h.event(Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE });
            h.run();
        }
        h.run_steps(3);
        h.get_by_label("Set Chain Elevation…").click();
        h.run_steps(3);
        assert!(h.get_by_label("Chain Elevation").is_focused());
        h.event(Event::Text("10".into()));
        h.run();
        h.key_press(egui::Key::Enter);
        h.run_steps(3);
        let source = h.state().session.doc().unwrap().model.get(handle).unwrap();
        assert!(matches!(&source.kind,EntityKind::Polyline3d(poly) if poly.points.iter().all(|p|p.z==10.0)));

        h.get_by_label("Copy Chain…").click();
        h.run_steps(3);
        h.get_by_role_and_label(egui::Role::ComboBox, "Copy Method").click();
        h.run_steps(3);
        h.get_by_label("Slope to Target Surface").click();
        h.run_steps(3);
        let count = h.state().session.doc().unwrap().entity_count();
        h.get_by_label("Preview Copy").click();
        h.run_steps(3);
        assert_eq!(h.state().session.doc().unwrap().entity_count(), count);
        let color = crate::theme::Tokens::get().compare_area;
        assert!(
            h.output().shapes.iter().any(|shape| matches!(shape.shape,Shape::LineSegment {stroke,..} if stroke.color==color && stroke.width==3.0))
        );
        h.get_by_label("Create Copy").click();
        h.run_steps(3);
        assert!(h.state().session.doc().unwrap().model.iter().any(|entity|matches!(&entity.kind,EntityKind::Polyline3d(poly) if entity.handle!=handle && poly.closed && poly.points.len()>12 && poly.points.iter().all(|p|(p.z-8.0).abs()<1e-7))));
    }

    #[test]
    fn survey_landxml_menu_opens_a_save_dialog_for_the_current_terrain() {
        use std::{cell::RefCell, rc::Rc};

        let mut session = cadcraft_engine::Session::default();
        session.execute("tin", &json!({"name":"Site","standalone":true,"points":[[1,1,0],[5,1,0],[1,5,0]]})).unwrap();
        let suggested = Rc::new(RefCell::new(String::new()));
        let saved = Rc::clone(&suggested);
        let services = crate::Services {
            pick_save: Some(Box::new(move |name| {
                *saved.borrow_mut() = name.to_owned();
                None
            })),
            ..Default::default()
        };
        let mut app = CadApp::new(session, services);
        app.start(COMMAND);
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1000.0, 600.0))
            .build_ui_state(|ui, app: &mut CadApp| crate::dialogs::show(app, ui.ctx()), app);
        h.run_steps(3);
        h.get_by_label("Export LandXML 1.2…").click();
        h.run_steps(3);
        h.get_by_label("Survey Compatibility (Metres)").click();
        h.run_steps(3);
        assert!(suggested.borrow().ends_with("-terrain-survey.xml"));
    }

    #[test]
    fn clicking_a_linked_source_endpoint_and_correcting_its_height_removes_the_flag() {
        for orbit in [false, true] {
            correct_height_on_canvas(orbit);
        }
    }

    fn correct_height_on_canvas(orbit: bool) {
        use egui::{Event, Modifiers, PointerButton};

        let mut session = cadcraft_engine::Session::default();
        for points in [[[0, 0, 1], [10, 10, 1]], [[0, 10, 3], [10, 0, 1]]] {
            session.execute("line", &json!({"points":points})).unwrap();
        }
        let handles: Vec<_> = session.doc().unwrap().model.iter().map(|e| e.handle.hex()).collect();
        let linked = session.execute("tin.link", &json!({"handles":handles,"name":"Linked"})).unwrap();
        session.zoom_extents().unwrap();
        if orbit {
            session.execute("3dorbit", &json!({"view":"isometric","perspective":true,"pivot":[0,10,3]})).unwrap();
        } else {
            session.execute("select", &json!({"handles":[linked["handles"][0]]})).unwrap();
            assert!(!session.selection().is_empty());
        }
        let mut app = CadApp::new(session, Default::default());
        app.start(COMMAND);
        let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(1000.0, 600.0)).build_ui_state(
            |ui, app: &mut CadApp| {
                crate::canvas::show(app, ui);
                crate::dialogs::show(app, ui.ctx());
            },
            app,
        );
        h.run_steps(3);
        assert!(h.query_by_label("Height conflicts: 1").is_some());
        if !orbit {
            h.get_by_label("Show Linked Triangles").click();
            h.run_steps(3);
            assert!(h.state().session.selection().is_empty());
        }
        assert_eq!(h.state().session.doc().unwrap().layer("TIN-SURFACES").unwrap().on, orbit);
        let at = h
            .output()
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "3.0000" => Some(text.pos + 0.5 * text.galley.size()),
                _ => None,
            })
            .unwrap();
        h.event(Event::PointerMoved(at));
        h.run();
        for pressed in [true, false] {
            h.event(Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE });
            h.run();
        }
        h.run_steps(2);
        assert!(h.get_by_label("New Elevation").is_focused());
        h.event(Event::Text("1".into()));
        h.run();
        h.key_press(egui::Key::Enter);
        h.run_steps(3);
        assert!(h.query_by_label("Height conflicts: 0").is_some(), "orbit: {orbit}, log: {:?}", h.state().session.log);
        assert!(h.state().session.selection().is_empty());
    }

    #[test]
    fn labels_coincident_points_and_edits_a_vertex_in_plan_and_orbit() {
        for orbit in [false, true] {
            edit_vertex_on_canvas(orbit);
        }
    }

    fn edit_vertex_on_canvas(orbit: bool) {
        use cadcraft_doc::{EntityKind, Handle};
        use cadcraft_geom::Vec3;
        use egui::{Event, Modifiers, PointerButton, Shape};

        let mut session = cadcraft_engine::Session::default();
        session.settings.osmode |= cadcraft_engine::snap::mode::MID;
        let poly = session.execute("3dpoly", &json!({"points":[[-9,-5,2.1],[-8,-3,3.2],[-4,4,4.3]]})).unwrap();
        let support = session.execute("line", &json!({"points":[[-9,-5,5.4],[9,-6,6.5]]})).unwrap();
        let duplicate = session.execute("point", &json!({"at":[-9,-5,2.1]})).unwrap();
        session.execute("line", &json!({"points":[[-9,8,7],[-3,8,9]]})).unwrap();
        let destination = Vec3::new(-9.0, 0.0, 11.0);
        session.execute("line", &json!({"points":[destination,[-10,2,12]]})).unwrap();
        session.execute("tin.link", &json!({"handles":[poly["handle"],support["handles"][0],duplicate["handles"][0]],"name":"Linked"})).unwrap();
        session.zoom_extents().unwrap();
        let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(1100.0, 630.0)).build_ui_state(
            |ui, app: &mut CadApp| {
                crate::menus::shortcuts(app, ui.ctx());
                crate::canvas::show(app, ui);
                crate::dialogs::show(app, ui.ctx());
            },
            CadApp::new(session, Default::default()),
        );
        h.run_steps(3);
        assert!(h.query_by_label("Source Elevations").is_some());
        for label in ["2.1000", "3.2000", "4.3000", "5.4000", "6.5000"] {
            assert_eq!(
                h.output().shapes.iter().filter(|shape| matches!(&shape.shape, Shape::Text(text) if text.galley.text() == label)).count(),
                1,
                "{label}"
            );
        }
        let text_height = |h: &egui_kittest::Harness<'_, CadApp>| {
            h.output()
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    Shape::Text(text) if text.galley.text() == "2.1000" => Some(text.galley.size().y),
                    _ => None,
                })
                .unwrap()
        };
        let normal_height = text_height(&h);
        h.get_by_role_and_label(egui::Role::Slider, "Elevation Label Size").set_value_accesskit(19.0);
        h.run_steps(3);
        assert!(text_height(&h) > normal_height);
        if orbit {
            h.state_mut().session.execute("3dorbit", &json!({"view":"isometric","perspective":true})).unwrap();
        }
        h.state_mut().start(COMMAND);
        h.run_steps(3);
        let screen = |h: &mut egui_kittest::Harness<'_, CadApp>, p: Vec3| {
            if orbit {
                h.state_mut().session.execute("3dorbit", &json!({"pivot":[p.x,p.y,p.z]})).unwrap();
                h.run_steps(2);
                h.state().canvas.rect.unwrap().center()
            } else {
                h.state().canvas.xf.unwrap().to_screen(p.xy())
            }
        };
        let click_screen = |h: &mut egui_kittest::Harness<'_, CadApp>, pos| {
            for pressed in [true, false] {
                h.event(Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE });
                h.step();
            }
            h.run_steps(2);
        };
        let click_at = |h: &mut egui_kittest::Harness<'_, CadApp>, p| {
            let pos = screen(h, p);
            h.event(Event::PointerMoved(pos));
            h.step();
            click_screen(h, pos);
        };
        click_at(&mut h, Vec3::new(-8.0, -3.0, 3.2));
        h.get_by_label("Pick Elevation from Line").click();
        h.run_steps(3);
        assert!(matches!(source::pick_mode(h.state()), Some(PickMode::Elevation(_))), "sampling did not start (orbit {orbit})");
        let pos = screen(&mut h, Vec3::new(-6.0, 8.0, 8.0)) + egui::vec2(5.0, 0.0);
        h.event(Event::PointerMoved(pos));
        h.run_steps(2);
        let midpoint_marker = |h: &egui_kittest::Harness<'_, CadApp>| {
            h.output().shapes.iter().any(|shape| matches!(&shape.shape, Shape::Text(text) if text.galley.text() == "Midpoint"))
        };
        assert!(midpoint_marker(&h), "missing midpoint marker (orbit {orbit})");
        h.key_press(egui::Key::F3);
        h.run_steps(2);
        assert!(!midpoint_marker(&h));
        h.key_press(egui::Key::F3);
        h.run_steps(2);
        assert!(midpoint_marker(&h));
        let snap_color = crate::theme::Tokens::get().snap;
        let dashes = h.output().shapes.iter().filter(|shape| {
            matches!(shape.shape, Shape::LineSegment { points, stroke } if stroke.color == snap_color && (points[0].distance(points[1]) - 7.0).abs() < 0.1)
        });
        assert!(dashes.count() > 2, "missing dashed preview (orbit {orbit})");
        click_screen(&mut h, pos);
        assert!(
            source::pick_mode(h.state()).is_none(),
            "the click did not sample an elevation (orbit {orbit}, no line message {})",
            h.query_by_label("No other line under the cursor. Click a line, polyline, or arc.").is_some()
        );
        h.get_by_label("Apply").click();
        h.run_steps(3);
        let handle = Handle::parse_hex(poly["handle"].as_str().unwrap()).unwrap();
        let read = |app: &CadApp| {
            let EntityKind::Polyline3d(poly) = &app.session.doc().unwrap().model.get(handle).unwrap().kind else { panic!("expected a polyline") };
            poly.points.clone()
        };
        let raised = read(h.state());
        assert_eq!(raised[1], Vec3::new(-8.0, -3.0, 8.0), "elevation must preserve XY (orbit {orbit})");
        h.get_by_label("Pick Position").click();
        h.run_steps(3);
        let pos = screen(&mut h, destination) + egui::vec2(3.0, 0.0);
        h.event(Event::PointerMoved(pos));
        h.run_steps(2);
        if !orbit {
            let xf = h.state().canvas.xf.unwrap();
            let target = xf.to_screen(destination.xy());
            let neighbors = [raised[0], raised[2]].map(|p| xf.to_screen(p.xy()));
            let on_segment = |q: egui::Pos2, a: egui::Pos2, b: egui::Pos2| {
                let direction = b - a;
                let t = (q - a).dot(direction) / direction.length_sq();
                (-0.001..=1.001).contains(&t) && q.distance(a + t * direction) < 0.1
            };
            let dashes: Vec<_> = h
                .output()
                .shapes
                .iter()
                .filter_map(|shape| match shape.shape {
                    Shape::LineSegment { points, stroke } if stroke.color == snap_color && (points[0].distance(points[1]) - 7.0).abs() < 0.1 => {
                        Some(points)
                    }
                    _ => None,
                })
                .collect();
            for neighbor in neighbors {
                assert!(dashes.iter().any(|dash| dash.iter().all(|&p| on_segment(p, target, neighbor))));
            }
            let old = xf.to_screen(raised[1].xy());
            assert!(!dashes.iter().any(|dash| dash.iter().all(|&p| on_segment(p, old, target))));
        }
        click_screen(&mut h, pos);
        assert!(source::pick_mode(h.state()).is_none(), "the click did not place the vertex (orbit {orbit})");
        h.get_by_label("Apply").click();
        h.run_steps(3);
        let moved = read(h.state());
        assert_eq!(moved[1], destination, "position must use the snapped XYZ (orbit {orbit})");
        click_at(&mut h, moved[1]);
        h.get_by_label("Remove Point").click();
        h.run_steps(3);
        assert_eq!(read(h.state()), vec![moved[0], moved[2]]);
    }

    #[test]
    fn uses_insertion_extension_and_crossing_removal_in_plan_and_orbit() {
        for orbit in [false, true] {
            edit_source_lines_on_canvas(orbit);
        }
    }

    fn edit_source_lines_on_canvas(orbit: bool) {
        use cadcraft_doc::{EntityKind, Handle};
        use cadcraft_geom::{Vec2, Vec3};
        use egui::{Event, Modifiers, PointerButton, Shape};

        let mut session = cadcraft_engine::Session::default();
        let south = session.execute("line", &json!({"points":[[-9,0,1],[9,0,1]]})).unwrap();
        let north = session.execute("line", &json!({"points":[[-9,8,1],[9,8,1]]})).unwrap();
        let point = session.execute("point", &json!({"at":[-11,3,2]})).unwrap();
        session.execute("tin.link", &json!({"handles":[south["handles"][0],north["handles"][0],point["handles"][0]],"name":"Linked"})).unwrap();
        session.execute("line", &json!({"points":[[-2,-2,3],[-1,-2,4]]})).unwrap();
        session.zoom_extents().unwrap();
        if orbit {
            session.execute("3dorbit", &json!({"view":"isometric","perspective":true})).unwrap();
        }
        let mut app = CadApp::new(session, Default::default());
        app.start(COMMAND);
        let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(1100.0, 630.0)).build_ui_state(
            |ui, app: &mut CadApp| {
                crate::canvas::show(app, ui);
                crate::dialogs::show(app, ui.ctx());
            },
            app,
        );
        h.run_steps(3);
        let click = |h: &mut egui_kittest::Harness<'_, CadApp>, x, y, z| {
            let pos = if orbit {
                h.state_mut().session.execute("3dorbit", &json!({"pivot":[x,y,z]})).unwrap();
                h.run_steps(2);
                h.state().canvas.rect.unwrap().center()
            } else {
                h.state().canvas.xf.unwrap().to_screen(Vec2::new(x, y))
            };
            h.event(Event::PointerMoved(pos));
            h.step();
            for pressed in [true, false] {
                h.event(Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE });
                h.step();
            }
            h.run_steps(2);
        };
        click(&mut h, -6.0, 0.0, 1.0);
        assert!(h.query_by_label("Segment 1 of 1").is_some());
        let hot = crate::theme::Tokens::get().grip_hot;
        assert!(
            h.output()
                .shapes
                .iter()
                .any(|shape| { matches!(shape.shape, Shape::LineSegment { stroke, .. } if stroke.color == hot && stroke.width >= 5.0) })
        );
        h.get_by_label("Insert Source Point").click();
        h.run_steps(3);
        h.get_by_label("Snap to Existing Points").click();
        h.run_steps(3);
        click(&mut h, -6.0, -1.0, 1.0);
        h.get_by_label("Next Segment").click();
        h.run_steps(3);
        assert!(h.query_by_label("Segment 2 of 2").is_some(), "orbit: {orbit}");
        h.get_by_label("Insert Source Point").click();
        h.run_steps(3);
        h.get_by_label("Snap to Existing Points").click();
        h.run_steps(3);
        click(&mut h, -2.0, -2.0, 3.0);
        h.get_by_label("Previous Segment").click();
        h.run_steps(3);
        assert!(h.query_by_label("Segment 1 of 3").is_some(), "orbit: {orbit}");
        click(&mut h, -9.0, 0.0, 1.0);
        let snap = crate::theme::Tokens::get().snap;
        assert!(
            h.output()
                .shapes
                .iter()
                .any(|shape| { matches!(shape.shape, Shape::LineSegment { stroke, .. } if stroke.color == snap && stroke.width >= 5.0) })
        );
        h.get_by_label("Extend Source Line").click();
        h.run_steps(3);
        click(&mut h, -8.0, -1.0, 1.0);
        h.get_by_label("Apply").click();
        h.run_steps(3);
        let handle = Handle::parse_hex(south["handles"][0].as_str().unwrap()).unwrap();
        let EntityKind::Polyline3d(poly) = &h.state().session.doc().unwrap().model.get(handle).unwrap().kind else {
            panic!("expected an extended chain")
        };
        assert_eq!(poly.points.len(), 5, "orbit: {orbit}, points: {:?}, log: {:?}", poly.points, h.state().session.log);
        assert!((poly.points[0].y + 1.0).abs() < 0.01 && (poly.points[2].x + 6.0).abs() < 0.01);
        assert!((poly.points[2].y + 1.0).abs() < 0.01);
        assert_eq!(poly.points[3], Vec3::new(-2.0, -2.0, 3.0), "orbit: {orbit}");

        h.get_by_label("Draw Source Line").click();
        h.run_steps(3);
        for p in ["#-9,6,1", "#9,6,1"] {
            h.state_mut().session.cmdline(p).unwrap();
        }
        h.run_steps(3);
        h.get_by_label("Finish").click();
        h.run_steps(3);
        assert!(!h.state().session.prompting());
        let face_count = |app: &CadApp| app.session.doc().unwrap().groups.iter().find(|g| g.name == "Linked").unwrap().members.len();
        let before = face_count(h.state());
        h.get_by_label("Remove Triangles Along Line").click();
        h.run_steps(3);
        let apply_line = |h: &mut egui_kittest::Harness<'_, CadApp>, y| {
            click(h, -8.0, y, 1.0);
            click(h, 3.0, y, 1.0);
            h.get_by_label("Apply Line").click();
            h.run_steps(3);
            assert!(h.state().session.prompting());
        };
        apply_line(&mut h, 2.0);
        let after_first = face_count(h.state());
        assert!(after_first < before);
        apply_line(&mut h, 7.0);
        let cut = before - face_count(h.state());
        assert!(face_count(h.state()) < after_first, "orbit: {orbit}");
        h.get_by_label("Stop").click();
        h.run_steps(3);
        assert!(!h.state().session.prompting());
        h.get_by_label("Show Removed Triangles").click();
        h.run_steps(3);
        assert!(h.query_by_label(&format!("Removed triangles: {cut}")).is_some());
        h.get_by_label("Restore Triangles Along Line").click();
        h.run_steps(3);
        apply_line(&mut h, 2.0);
        assert!((before - cut..before).contains(&face_count(h.state())), "orbit: {orbit}");
        apply_line(&mut h, 7.0);
        assert_eq!(face_count(h.state()), before);
        h.get_by_label("Stop").click();
        h.run_steps(3);

        let has_entity = |app: &CadApp, value: &Value| {
            let handle = Handle::parse_hex(value.as_str().unwrap()).unwrap();
            app.session.doc().unwrap().model.get(handle).is_some()
        };
        click(&mut h, -6.0, 8.0, 1.0);
        h.get_by_label("Delete Source Line").click();
        h.run_steps(3);
        assert!(!has_entity(h.state(), &north["handles"][0]));
        click(&mut h, -11.0, 3.0, 2.0);
        assert!(h.get_by_label("New Elevation").is_focused());
        h.get_by_label("Remove Point").click();
        h.run_steps(3);
        assert!(!has_entity(h.state(), &point["handles"][0]));
    }

    #[test]
    fn edits_the_surface_selected_in_the_panel_when_two_surfaces_overlap() {
        let mut session = cadcraft_engine::Session::default();
        for (name, height) in [("Existing", 1), ("Design", 2)] {
            session.execute("tin", &json!({"name": name, "points": [[0, 0, height], [10, 0, height], [0, 10, height]]})).unwrap();
        }
        let mut app = CadApp::new(session, Default::default());
        app.start(COMMAND);
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 500.0))
            .build_ui_state(|ui, app: &mut CadApp| crate::dialogs::show(app, ui.ctx()), app);
        h.run_steps(3);
        assert!(h.get_by_role_and_label(egui::Role::ComboBox, "Terrain surface").is_focused());
        h.get_by_role_and_label(egui::Role::ComboBox, "Terrain surface").click();
        h.run_steps(3);
        h.get_by_label("Existing").click();
        h.run_steps(3);
        h.get_by_label("Surface Visibility").click();
        h.run_steps(3);
        h.get_by_role_and_label(egui::Role::CheckBox, "Design").click();
        h.run_steps(3);
        assert_eq!(h.state().surface_editor.visibility.get("Design"), Some(&false));
        assert_eq!(h.state().surface_editor.visibility.get("Existing"), Some(&true));
        h.key_press(egui::Key::Escape);
        h.run_steps(3);
        h.get_by_label("Set Vertex Elevation").click();
        h.run_steps(3);
        let app = h.state_mut();
        assert!(app.session.current_prompt().unwrap().message.contains("vertex"));
        app.session.cmdline("0,0").unwrap();
        app.session.cmdline("3.5").unwrap();
        let drawing = app.session.doc().unwrap();
        for (name, expected) in [("Existing", 3.5), ("Design", 2.0)] {
            let g = drawing.groups.iter().find(|g| g.name == name).unwrap();
            let cadcraft_doc::EntityKind::Face3d(face) = &drawing.model.get(g.members[0]).unwrap().kind else { panic!("expected triangle") };
            assert_eq!(face.corners.iter().find(|p| p.xy() == cadcraft_geom::Vec2::ZERO).unwrap().z, expected);
        }
    }
}
