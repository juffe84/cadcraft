//! Viewport and View menu controls for the current drawing's terrain style.
use cadcraft_doc::Space;
use cadcraft_render::{TerrainColor, TerrainStyle};
use serde_json::{Value, json};

use crate::{CadApp, i18n::t};

pub(crate) const COMMAND: &str = "ui.terrain.display";
pub(crate) const DIALOG: &str = "terrain_display";

fn popup_id() -> egui::Id {
    egui::Id::unique("terrain-display-viewport")
}

pub(crate) fn viewport_popup_open(ctx: &egui::Context) -> bool {
    egui::Popup::is_id_open(ctx, popup_id())
}

pub(crate) fn button_rect(ui: &egui::Ui, orbit: egui::Rect) -> Option<egui::Rect> {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let width = ui.painter().layout_no_wrap(t("Terrain Display").to_owned(), font, ui.visuals().text_color()).size().x + 24.0;
    let size = egui::vec2(width, orbit.height());
    let rect = egui::Rect::from_min_size(orbit.left_top() - egui::vec2(width + 5.0, 0.0), size);
    ui.available_rect_before_wrap().contains_rect(rect).then_some(rect)
}

pub(crate) fn viewport_button(app: &mut CadApp, ui: &mut egui::Ui, rect: Option<egui::Rect>) {
    let id = popup_id();
    let Some(rect) = rect else {
        egui::Popup::close_id(ui.ctx(), id);
        return;
    };
    let label = t("Terrain Display");
    let active = viewport_popup_open(ui.ctx());
    let button = ui.put(rect, egui::Button::new(label).selected(active));
    crate::control::record_widget(ui, &button, label, "button", Some(active));
    if active && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        egui::Popup::close_id(ui.ctx(), id);
        button.request_focus();
        return;
    }
    let focus = button.clicked() && !active;
    egui::Popup::from_toggle_button_response(&button).id(id).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
        controls(app, ui, focus);
    });
}

pub(crate) fn available(app: &CadApp) -> bool {
    app.session.state().is_ok_and(|state| {
        state.space == Space::Model && state.block_editor().is_none() && state.doc.groups.iter().any(cadcraft_doc::Group::is_terrain_surface)
    })
}

pub(crate) fn open(app: &mut CadApp) -> Result<Value, String> {
    if !available(app) {
        return Err(t("Open or create a terrain surface on the Model tab.").into());
    }
    app.terrain_display_focus = app.ui.dialog.as_deref() != Some(DIALOG);
    app.ui.dialog = Some(DIALOG.into());
    Ok(Value::Null)
}

pub(crate) fn return_to_menu(ctx: &egui::Context, id: egui::Id) {
    ctx.data_mut(|data| data.insert_temp(egui::Id::unique(DIALOG).with("menu_focus"), Some(id)));
}

pub(crate) fn restore_focus(ctx: &egui::Context) {
    let id = egui::Id::unique(DIALOG);
    let key = id.with("focus_after_close");
    let Some(target) = ctx.data(|data| data.get_temp::<Option<egui::Id>>(key)).flatten() else { return };
    // The closing frame still blocks focus in the menu bar.
    match ctx.memory(|memory| memory.top_modal_layer()) {
        None => {
            ctx.data_mut(|data| data.remove_temp::<Option<egui::Id>>(key));
            ctx.memory_mut(|memory| memory.request_focus(target));
        }
        Some(layer) if layer == egui::Modal::default_area(id).layer() => ctx.request_repaint(),
        _ => {}
    }
}

pub(crate) fn dialog(app: &mut CadApp, ctx: &egui::Context, open: &mut bool) {
    if !available(app) {
        *open = false;
        return;
    }
    let focus = std::mem::take(&mut app.terrain_display_focus);
    let id = egui::Id::unique(DIALOG);
    let previous_focus = id.with("previous_focus");
    if focus {
        let previous = ctx.memory(|memory| memory.focused());
        ctx.data_mut(|data| {
            let menu = data.remove_temp::<Option<egui::Id>>(id.with("menu_focus")).flatten();
            data.insert_temp(previous_focus, menu.or(previous));
        });
    }

    let response = crate::modal::instant(ctx, id).show(ctx, |ui| {
        ui.heading(t("Terrain Display"));
        controls(app, ui, focus);
        if ui.button(t("Close")).clicked() {
            ui.close();
        }
    });
    if response.should_close() {
        let previous = ctx.data_mut(|data| data.remove_temp::<Option<egui::Id>>(previous_focus)).flatten();
        ctx.data_mut(|data| data.insert_temp(id.with("focus_after_close"), previous));
        ctx.request_repaint();
        *open = false;
    }
}

fn controls(app: &mut CadApp, ui: &mut egui::Ui, focus: bool) {
    let Ok(state) = app.session.state() else { return };
    let before = state.terrain_style;
    let from_plan = state.orbit.is_none();
    let filled = state.orbit.is_some_and(|orbit| !orbit.wireframe);
    let mut style = before;
    color_controls(ui, &mut style, focus);
    if let Some(legend) = app.canvas.terrain_legend(state.uid).filter(|legend| filled && legend.color == style.color) {
        ui.horizontal(|ui| {
            let unit = if legend.color == TerrainColor::Slope { "°" } else { "" };
            ui.label(format!("{:.2}{unit}", legend.range[0]));
            for &cadcraft_color::Rgb(r, g, b) in &legend.swatches {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(9.0, 12.0), egui::Sense::hover());
                ui.painter().rect_filled(rect, 0.0, egui::Color32::from_rgb(r, g, b));
            }
            if legend.range[0] != legend.range[1] {
                ui.label(format!("{:.2}{unit}", legend.range[1]));
            }
        });
    }
    let transparency = ui.add(egui::Slider::new(&mut style.transparency, 0..=90).suffix(" %").text(t("Transparency")));
    crate::control::record_widget(ui, &transparency, t("Transparency"), "slider", None);
    let edges = ui.checkbox(&mut style.edges, t("Triangle edges"));
    crate::control::record_widget(ui, &edges, t("Triangle edges"), "checkbox", Some(style.edges));
    if style != before {
        let mut params = json!({
            "terrainColor": style.color.name(),
            "terrainBands": style.bands,
            "terrainTransparency": style.transparency,
            "terrainEdges": style.edges,
            "wireframe": false,
        });
        if from_plan {
            params["view"] = json!("top");
        }
        if let Err(error) = app.run("3dorbit", params) {
            app.set_status(error);
        }
    }
}

fn color_controls(ui: &mut egui::Ui, style: &mut TerrainStyle, focus: bool) {
    for (label, mode) in
        [("Drawing colors", TerrainColor::Drawing), ("Elevation bands", TerrainColor::Elevation), ("Slope bands", TerrainColor::Slope)]
    {
        let active = style.color == mode;
        let response = ui.radio_value(&mut style.color, mode, t(label));
        crate::control::record_widget(ui, &response, label, "radio", Some(style.color == mode));
        if active && focus {
            response.request_focus();
        }
    }
    let bands = ui.add_enabled(style.color != TerrainColor::Drawing, egui::Slider::new(&mut style.bands, 2..=16).text(t("Number of bands")));
    crate::control::record_widget(ui, &bands, t("Number of bands"), "slider", None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable;

    #[test]
    fn viewport_popup_keeps_the_surface_editor_and_applies_changes_while_open() {
        let mut harness = egui_kittest::Harness::builder().with_size(egui::vec2(1000.0, 520.0)).build_ui_state(
            |ui, app: &mut CadApp| {
                crate::canvas::show(app, ui);
                crate::dialogs::show(app, ui.ctx());
            },
            CadApp::new(cadcraft_engine::Session::default(), Default::default()),
        );
        harness.run_steps(3);
        assert!(harness.query_by_label("Terrain Display").is_none());
        let app = harness.state_mut();
        app.run("tin", json!({"name":"Surface","points":[[0,0,0],[10,0,0],[0,10,2]]})).unwrap();
        app.start(crate::surface_editor::COMMAND);
        harness.run_steps(3);
        harness.get_by_label("Terrain Display").click();
        harness.run_steps(3);
        assert!(harness.get_by_label("Drawing colors").is_focused());
        harness.get_by_label("Slope bands").click();
        harness.run_steps(3);
        assert_eq!(harness.state().session.state().unwrap().terrain_style.color, TerrainColor::Slope);
        assert_eq!(harness.state().ui.dialog.as_deref(), Some(crate::surface_editor::DIALOG));
        assert!(harness.query_by_label("Triangle edges").is_some());
        harness.key_press(egui::Key::Escape);
        harness.run_steps(3);
        assert!(harness.query_by_label("Triangle edges").is_none());
        assert_eq!(harness.state().ui.dialog.as_deref(), Some(crate::surface_editor::DIALOG));
        assert!(harness.state().session.state().unwrap().orbit.is_some());
        assert!(harness.get_by_label("Terrain Display").is_focused());
    }

    #[test]
    fn view_menu_opens_the_controls_in_plan_and_applies_color_from_above() {
        let mut session = cadcraft_engine::Session::default();
        session.execute("tin", &json!({"name": "Surface", "points": [[0, 0, 0], [10, 0, 0], [0, 10, 2]]})).unwrap();
        let mut harness = egui_kittest::Harness::builder().with_size(egui::vec2(900.0, 400.0)).build_ui_state(
            |ui, app: &mut CadApp| {
                crate::menus::menu_bar(app, ui);
                crate::dialogs::show(app, ui.ctx());
            },
            CadApp::new(session, Default::default()),
        );
        harness.get_by_label("View").click();
        harness.run_steps(3);
        harness.get_by_label("Terrain Display...").click();
        harness.run_steps(3);
        assert!(harness.state().session.state().unwrap().orbit.is_none());
        assert!(harness.get_by_label("Drawing colors").is_focused());
        harness.key_press(egui::Key::Tab);
        harness.run();
        harness.key_press(egui::Key::Tab);
        harness.run();
        assert!(harness.get_by_label("Slope bands").is_focused());
        harness.key_press(egui::Key::Space);
        harness.run_steps(3);
        let state = harness.state().session.state().unwrap();
        assert_eq!(state.terrain_style.color, TerrainColor::Slope);
        let orbit = state.orbit.unwrap();
        assert_eq!(orbit.tilt, 0.0);
        assert!(!orbit.wireframe);
        harness.key_press(egui::Key::Escape);
        harness.run_steps(3);
        assert!(harness.state().ui.dialog.is_none());
        assert!(harness.get_by_label("View").is_focused());
        assert!(harness.state().session.state().unwrap().orbit.is_some());
    }
}
