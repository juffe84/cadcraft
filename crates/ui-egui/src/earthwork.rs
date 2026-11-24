//! Choose the surfaces to compare with TIN.VOLUMES.
use serde_json::{Value, json};

use crate::{CadApp, i18n::t};

pub(crate) struct State {
    uid: u64,
    revision: u64,
    names: Vec<String>,
    existing: String,
    design: String,
    result: Option<Result<Value, String>>,
    focus: bool,
}

fn surfaces(app: &mut CadApp) -> Result<Vec<String>, String> {
    let reply = app.run("tin.surfaces", json!({}))?;
    serde_json::from_value(reply["surfaces"].clone()).map_err(|error| error.to_string())
}

pub(crate) fn open(app: &mut CadApp) -> Result<Value, String> {
    let names = surfaces(app).inspect_err(|e| app.set_status(e))?;
    let s = app.session.state().map_err(|error| error.to_string())?;
    app.earthwork =
        Some(State { uid: s.uid, revision: s.revision, names, existing: String::new(), design: String::new(), result: None, focus: true });
    app.ui.dialog = Some("earthwork".into());
    Ok(Value::Null)
}

pub(crate) fn dialog(app: &mut CadApp, ctx: &egui::Context, open: &mut bool) {
    let Some(mut state) = app.earthwork.take() else {
        *open = false;
        return;
    };
    let Ok(drawing) = app.session.state() else {
        *open = false;
        return;
    };
    if drawing.uid != state.uid {
        *open = false;
        return;
    }
    if drawing.revision != state.revision {
        state.revision = drawing.revision;
        match surfaces(app) {
            Ok(names) => {
                state.names = names;
                if !state.names.contains(&state.existing) {
                    state.existing.clear();
                }
                if !state.names.contains(&state.design) {
                    state.design.clear();
                }
                state.result = None;
            }
            Err(error) => {
                app.set_status(error);
                *open = false;
                return;
            }
        }
    }

    let id = egui::Id::unique("cc_terrain_earthwork");
    let mut calculate = false;
    let mut close = false;
    egui::Window::new(t("Measure Terrain Earthwork")).id(id).resizable(false).open(open).show(ctx, |ui| {
        if state.names.len() < 2 {
            ui.label(t("Create at least two terrain surfaces in this drawing."));
        } else {
            let changed = surface_box(ui, "Existing surface", &state.names, &mut state.existing, &state.design, state.focus);
            state.focus = false;
            if surface_box(ui, "Design surface", &state.names, &mut state.design, &state.existing, false) || changed {
                state.result = None;
            }
        }
        match &state.result {
            Some(Ok(result)) => {
                for (field, label) in [("cut", "Cut"), ("fill", "Fill"), ("overlapArea", "Common area")] {
                    if let Some(value) = result[field].as_f64() {
                        ui.horizontal(|ui| {
                            ui.label(t(label));
                            ui.label(format!("{value:.4}"));
                        });
                    }
                }
            }
            Some(Err(error)) => {
                ui.label(error);
            }
            None => {}
        }
        ui.horizontal(|ui| {
            let ready = !state.existing.is_empty() && !state.design.is_empty();
            let button = ui.add_enabled(ready, egui::Button::new(t("Calculate")));
            crate::control::record_widget(ui, &button, "Calculate", "button", None);
            calculate = button.clicked();
            close = ui.button(t("Close")).clicked();
        });
    });

    if calculate {
        state.result = Some(app.run("tin.volumes", json!({ "existing": state.existing, "design": state.design })));
    }
    if close || ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
        *open = false;
    }
    if *open {
        app.earthwork = Some(state);
    }
}

fn surface_box(ui: &mut egui::Ui, label: &'static str, choices: &[String], selected: &mut String, excluded: &str, focus: bool) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let caption = ui.label(t(label));
        let shown = if selected.is_empty() { t("(select)") } else { selected.as_str() };
        let response = craft_ui::ComboBox::from_id_salt(label)
            .selected_text(shown)
            .width(200.0)
            .show_ui(ui, |ui| {
                for name in choices.iter().filter(|name| name.as_str() != excluded) {
                    changed |= ui.selectable_value(selected, name.clone(), name).changed();
                }
            })
            .response
            .labelled_by(caption.id);
        crate::control::record_widget(ui, &response, label, "combobox", None);
        if focus {
            response.request_focus();
        }
    });
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::{Queryable, by};

    #[test]
    fn chooses_unnamed_terrain_groups_and_displays_fill() {
        let mut session = cadcraft_engine::Session::new();
        for elevation in [0, 2] {
            session.execute("tin", &json!({"points": [[0,0,elevation], [10,0,elevation], [0,10,elevation]]})).unwrap();
        }
        session.execute("point", &json!({"at": [20,20,0]})).unwrap();
        let point = session.doc().unwrap().model.iter().last().unwrap().handle.hex();
        session.execute("group", &json!({"name":"Unrelated", "handles": [point]})).unwrap();
        let mut app = CadApp::new(session, Default::default());
        app.start("tin.volumes");
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(700.0, 450.0))
            .build_ui_state(|ui, app: &mut CadApp| crate::dialogs::show(app, ui.ctx()), app);
        assert_eq!(harness.state().earthwork.as_ref().unwrap().names, ["*A1", "*A2"]);
        for (i, name) in ["*A1", "*A2"].into_iter().enumerate() {
            harness.query_all(by().role(egui::accesskit::Role::ComboBox)).nth(i).unwrap().click();
            harness.run();
            harness.get_by_label(name).click();
            harness.run();
        }
        harness.get_by_label("Calculate").click();
        harness.run();
        let state = harness.state().earthwork.as_ref().unwrap();
        let result = state.result.as_ref().unwrap().as_ref().unwrap();
        assert_eq!(result["fill"], 100.0);
        assert_eq!(result["cut"], 0.0);
        assert!(harness.query_by_label("100.0000").is_some());
    }
}
