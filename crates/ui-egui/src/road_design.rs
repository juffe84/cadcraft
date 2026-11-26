//! Road surface form. All drawing changes go through registered commands.
use cadcraft_doc::road::{Design, Template, WidthControl};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{CadApp, Pick, control::record_widget, i18n::t};

mod live;
mod sections;
pub(crate) use live::paint_preview;

pub(crate) const COMMAND: &str = "ui.terrain.road";
pub(crate) const DIALOG: &str = "road_design";

#[derive(Clone, Deserialize)]
struct Axis {
    name: String,
    handle: String,
    range: [f64; 2],
}

#[derive(Default)]
pub(crate) struct State {
    document: Option<u64>,
    axes: Vec<Axis>,
    roads: Vec<String>,
    design: Option<Design>,
    message: Option<String>,
    focus: bool,
    section: sections::Editor,
    live: live::Preview,
}

fn new_design(name: String) -> Design {
    Design {
        name,
        axis: String::new(),
        range: [0.0, 0.0],
        interval: 1.0,
        centerline_number: 1,
        left: WidthControl { width: 0.0, edges: Vec::new() },
        right: WidthControl { width: 0.0, edges: Vec::new() },
        crossfall: Vec::new(),
        top: Template { inherit: false, rules: Vec::new() },
        layers: Vec::new(),
        references: Vec::new(),
    }
}

fn set_axis(design: &mut Design, axis: &Axis) {
    design.axis.clone_from(&axis.handle);
    design.range = axis.range;
}

fn fresh_design(app: &CadApp, axes: &[Axis]) -> Design {
    let doc = app.session.doc().ok();
    let name = std::iter::once(t("Road").to_owned())
        .chain((2..).map(|n| crate::tf!("Road {n}", n = n)))
        .find(|name| {
            doc.is_none_or(|doc| {
                let top = format!("{name} - Top");
                let strings = format!("{top} - Lines");
                !doc.groups.iter().any(|group| [&*name, &top, &strings].iter().any(|n| group.name.eq_ignore_ascii_case(n)))
            })
        })
        .unwrap_or_else(|| t("Road").into());
    let mut design = new_design(name);
    if let Some(axis) = axes.first() {
        set_axis(&mut design, axis);
    }
    design
}

pub(crate) fn available(app: &CadApp) -> bool {
    app.session.state().is_ok_and(|d| d.space == cadcraft_doc::Space::Model && d.block_editor().is_none())
}

pub(crate) fn open(app: &mut CadApp) -> Result<Value, String> {
    if !available(app) {
        return Err(t("Open the Model tab to design a road.").into());
    }
    app.road_design.focus = true;
    app.ui.dialog = Some(DIALOG.into());
    Ok(Value::Null)
}

fn reload(app: &mut CadApp, state: &mut State) -> Result<(), String> {
    let value = app.run("tin.road.axis", json!({}))?;
    state.axes = serde_json::from_value(value["axes"].clone()).map_err(|e| e.to_string())?;
    let value = app.run("tin.road.list", json!({}))?;
    state.roads = serde_json::from_value(value["roads"].clone()).map_err(|e| e.to_string())?;
    Ok(())
}

fn load(app: &mut CadApp, name: &str) -> Result<Design, String> {
    app.run("tin.road.get", json!({"name":name})).and_then(|value| serde_json::from_value(value).map_err(|e| e.to_string()))
}

fn choose_button(ui: &mut egui::Ui, label: &'static str) -> bool {
    enabled_button(ui, label, true)
}

fn enabled_button(ui: &mut egui::Ui, label: &'static str, enabled: bool) -> bool {
    let button = ui.add_enabled(enabled, egui::Button::new(t(label)));
    record_widget(ui, &button, t(label), "button", None);
    button.clicked()
}

fn set_instruction_status(app: &mut CadApp, result: Result<(), String>, success: &'static str) -> Result<Value, String> {
    let message = result.as_ref().err().cloned().unwrap_or_else(|| t(success).into());
    app.road_design.message = Some(message);
    result.map(|()| Value::Null)
}

fn save_instructions(app: &mut CadApp, design: Design) {
    let name = format!("{}.road.json", design.name);
    app.pick_then(Pick::Save(name), move |app, path| {
        let result = serde_json::to_value(design)
            .map_err(|error| error.to_string())
            .and_then(|design| app.run("tin.road.instructions.save", json!({"path":path, "design":design})))
            .map(|_| ());
        set_instruction_status(app, result, "Road instructions saved.")
    });
}

fn load_instructions(app: &mut CadApp) {
    app.pick_then(Pick::RoadInstructions, |app, path| {
        let result = app
            .run("tin.road.instructions.load", json!({"path":path}))
            .and_then(|value| serde_json::from_value::<Design>(value).map_err(|error| error.to_string()))
            .map(|design| {
                app.road_design.design = Some(design);
                app.road_design.section = sections::Editor::default();
            });
        set_instruction_status(app, result, "Road instructions loaded.")
    });
}

pub(crate) fn dialog(app: &mut CadApp, ctx: &egui::Context, open: &mut bool) {
    if !available(app) {
        *open = false;
        return;
    }
    let doc = app.session.state().ok().map(|state| state.uid);
    let mut state = std::mem::take(&mut app.road_design);
    let focus = std::mem::take(&mut state.focus);
    if state.document != doc {
        state = State { document: doc, ..Default::default() };
    }
    if state.design.is_none() || focus {
        if let Err(error) = reload(app, &mut state) {
            state.message = Some(error);
        }
        if state.design.is_none() {
            state.design = Some(match state.roads.first() {
                Some(name) => load(app, name).unwrap_or_else(|error| {
                    state.message = Some(error);
                    fresh_design(app, &state.axes)
                }),
                None => fresh_design(app, &state.axes),
            });
        }
    }

    let selection: Vec<_> = app.session.selection().iter().map(|handle| handle.hex()).collect();
    let mut load_name = None;
    let mut create = false;
    let mut build = false;
    let mut edit_surface = None;
    let mut save_file = false;
    let mut load_file = false;
    let bounds = ctx.content_rect();
    let compact = bounds.height() < 400.0;
    egui::Window::new(t("Road Design"))
        .id(egui::Id::unique(DIALOG))
        .open(open)
        .default_size(egui::vec2(580.0, 515.0))
        .min_size(egui::vec2(340.0_f32.min(bounds.width()), 350.0_f32.min(bounds.height())))
        .constrain_to(bounds)
        .resizable(true)
        .vscroll(compact)
        .show(ctx, |ui| {
            if !compact {
                ui.set_min_height(ui.available_height());
            }
            let Some(design) = &mut state.design else { return };
            if !state.roads.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    let caption = ui.label(t("Saved Road"));
                    let response = craft_ui::ComboBox::from_id_salt("saved-road")
                        .selected_text(&design.name)
                        .show_ui(ui, |ui| {
                            for name in &state.roads {
                                if ui.selectable_label(*name == design.name, name).clicked() {
                                    load_name = Some(name.clone());
                                }
                            }
                        })
                        .response
                        .labelled_by(caption.id);
                    record_widget(ui, &response, t("Saved Road"), "combobox", None);
                    create = choose_button(ui, "New Road");
                });
            }
            if state.axes.len() > 1 || design.axis.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    if !state.axes.is_empty() {
                        let caption = ui.label(t("Centerline"));
                        let active = state.axes.iter().find(|axis| axis.handle == design.axis);
                        let combo = craft_ui::ComboBox::from_id_salt("road-axis")
                            .selected_text(active.map_or(t("Choose a centerline"), |axis| &axis.name))
                            .show_ui(ui, |ui| {
                                let mut picked = None;
                                for axis in &state.axes {
                                    if ui.selectable_label(axis.handle == design.axis, &axis.name).clicked() {
                                        picked = Some(axis);
                                    }
                                }
                                picked
                            });
                        record_widget(ui, &combo.response.labelled_by(caption.id), t("Centerline"), "combobox", None);
                        if let Some(axis) = combo.inner.flatten() {
                            set_axis(design, axis);
                        }
                    }
                    if choose_button(ui, "Use Selected Centerline") {
                        state.message = match selection.as_slice() {
                            [handle] => match app
                                .run("tin.road.axis", json!({"handle":handle}))
                                .and_then(|value| serde_json::from_value::<Axis>(value).map_err(|e| e.to_string()))
                            {
                                Ok(axis) => {
                                    set_axis(design, &axis);
                                    None
                                }
                                Err(message) => Some(message),
                            },
                            _ => Some(t("Select one open line or polyline on the canvas.").into()),
                        };
                    }
                });
            }
            if !design.axis.is_empty() {
                state.section.layers(ui, design, &mut state.message);
            }
            ui.horizontal_wrapped(|ui| {
                let label = t("Section Interval");
                let caption = ui.label(label);
                let response =
                    ui.add(egui::DragValue::new(&mut design.interval).speed(0.1).max_decimals(4).range(0.0001..=f64::MAX)).labelled_by(caption.id);
                record_widget(ui, &response, label, "slider", None);
                if !app.services.no_file_system {
                    if app.services.pick_save.is_some() {
                        save_file = enabled_button(ui, "Save Instructions…", !design.axis.is_empty());
                    }
                    if app.services.pick_road_instructions.is_some() {
                        load_file = choose_button(ui, "Load Instructions…");
                    }
                }
            });
            state.section.graph(app, ui, design);
            if let Some(status) = state.live.status() {
                let response = ui.label(status);
                ui.ctx().accesskit_node_builder(response.id, |node| node.set_live(egui::accesskit::Live::Polite));
            }
            if !design.axis.is_empty() {
                ui.separator();
                let mut instructions = |ui: &mut egui::Ui| {
                    state.section.reference_controls(app, ui, design, &selection, &mut state.message);
                    state.section.instructions(ui, &mut app.session, design, &selection, &mut state.message);
                };
                if compact {
                    instructions(ui);
                } else {
                    let footer = 2.0 * (ui.spacing().interact_size.y + ui.spacing().item_spacing.y);
                    egui::ScrollArea::vertical()
                        .min_scrolled_height(0.0)
                        .max_height((ui.available_height() - footer).max(42.0))
                        .show(ui, instructions);
                }
                ui.horizontal_wrapped(|ui| {
                    if design.top.inherit || !design.top.rules.is_empty() {
                        build = enabled_button(ui, "Build Road Surfaces", !state.section.is_editing());
                    }
                    let surface = format!("{} - {}", design.name, state.section.selected_layer(design));
                    if app.session.doc().is_ok_and(|doc| {
                        doc.groups.iter().any(|group| group.name == surface && group.description.starts_with(cadcraft_doc::road::SURFACE_PREFIX))
                    }) && choose_button(ui, "Open in Surface Editor")
                    {
                        edit_surface = Some(surface);
                    }
                });
            }
            if let Some(message) = &state.message {
                let status = ui.label(message);
                ui.ctx().accesskit_node_builder(status.id, |node| node.set_live(egui::accesskit::Live::Polite));
            }
        });

    if let Some(name) = load_name {
        match load(app, &name) {
            Ok(design) => {
                state.design = Some(design);
                state.section = sections::Editor::default();
                state.message = None;
            }
            Err(error) => state.message = Some(error),
        }
    }
    if create {
        state.design = Some(fresh_design(app, &state.axes));
        state.section = sections::Editor::default();
        state.message = None;
    }
    if let Some(name) = edit_surface {
        state.message = crate::surface_editor::open_surface(app, &name).err();
    }
    if build && let Some(design) = &mut state.design {
        design.name = design.name.trim().to_owned();
        design.crossfall.sort_by(|a, b| a.station.total_cmp(&b.station));
        state.message = match serde_json::to_value(&design).map_err(|e| e.to_string()).and_then(|value| app.run("tin.road.build", value)) {
            Ok(_) => {
                let _ = reload(app, &mut state);
                Some(t("Road surfaces updated.").into())
            }
            Err(error) => Some(error),
        };
    }
    if *open
        && let Some(design) = state.design.as_ref().filter(|design| !design.axis.is_empty())
        && let Ok(drawing) = app.session.state()
    {
        state.live.update(ctx, drawing.doc.clone(), drawing.revision, design);
    }
    app.road_design = state;
    if save_file && let Some(design) = app.road_design.design.clone() {
        save_instructions(app, design);
    } else if load_file {
        load_instructions(app);
    }
}
