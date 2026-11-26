use cadcraft_geom::Vec3;
use serde_json::{Value, json};

use super::{Curve, coordinate, field};
use crate::{CadApp, i18n::t};

const HEIGHT: &str = "tin.source.chain.elevation";
const COPY: &str = "tin.source.chain.copy";
const PREVIEW: &str = "tin.source.chain.preview";

#[derive(Default)]
pub(super) struct Tools {
    handle: String,
    form: Option<Form>,
    focus: bool,
    preview: Option<(Value, Vec<Vec3>)>,
    error: Option<String>,
}

enum Form {
    Elevation(String),
    Copy(CopyForm),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Destination {
    Distance,
    Slope,
    Elevation,
    Terrain,
}

impl Destination {
    const OPTIONS: [Self; 4] = [Self::Distance, Self::Slope, Self::Elevation, Self::Terrain];

    fn label(self) -> &'static str {
        match self {
            Self::Distance => "Horizontal Offset",
            Self::Slope => "Slope and Elevation Change",
            Self::Elevation => "Slope to Target Elevation",
            Self::Terrain => "Slope to Target Surface",
        }
    }
}

struct CopyForm {
    destination: Destination,
    side: &'static str,
    rise: String,
    distance: String,
    slope: String,
    elevation: String,
    surface: String,
    uphill: bool,
    round: bool,
    attach: bool,
}

impl CopyForm {
    fn new(closed: bool, height: f64) -> Self {
        Self {
            destination: Destination::Slope,
            side: if closed { "outside" } else { "left" },
            rise: "-1".into(),
            distance: "1".into(),
            slope: "1:2".into(),
            elevation: (height - 1.0).to_string(),
            surface: String::new(),
            uphill: false,
            round: true,
            attach: true,
        }
    }

    fn params(&self, group: &str, handle: &str) -> Option<Value> {
        let mut params = json!({"group":group,"handle":handle,"side":self.side,"round":self.round,"attach":self.attach});
        if self.destination != Destination::Distance {
            let (v, h) = self.slope.trim().split_once(':')?;
            let (v, h) = (coordinate(v)?, coordinate(h)?);
            if v <= 0.0 || h <= 0.0 {
                return None;
            }
            params["slope"] = json!([v, h]);
        }
        match self.destination {
            Destination::Distance | Destination::Slope => {
                params["rise"] = json!(coordinate(&self.rise)?);
                if self.destination == Destination::Distance {
                    params["distance"] = json!(coordinate(&self.distance)?);
                }
            }
            Destination::Elevation => params["elevation"] = json!(coordinate(&self.elevation)?),
            Destination::Terrain => {
                if self.surface.is_empty() {
                    return None;
                }
                params["target"] = json!(self.surface);
                params["uphill"] = json!(self.uphill);
            }
        }
        Some(params)
    }

    fn inputs(&mut self, ui: &mut egui::Ui, app: &CadApp, group: &str, closed: bool) {
        let caption = ui.label(t("Copy Method"));
        let select = craft_ui::ComboBox::from_id_salt("chain-copy-method")
            .selected_text(t(self.destination.label()))
            .show_ui(ui, |ui| {
                for option in Destination::OPTIONS {
                    ui.selectable_value(&mut self.destination, option, t(option.label()));
                }
            })
            .response
            .labelled_by(caption.id);
        crate::control::record_widget(ui, &select, t("Copy Method"), "combobox", None);
        let choices = if closed { [("outside", "Outside"), ("inside", "Inside")] } else { [("left", "Left"), ("right", "Right")] };
        ui.horizontal(|ui| {
            ui.label(t("Copy Side"));
            for (key, label) in choices {
                ui.radio_value(&mut self.side, key, t(label));
            }
        });
        if self.destination != Destination::Distance {
            field(ui, "Slope (Vertical:Horizontal)", &mut self.slope, true);
        }
        match self.destination {
            Destination::Distance | Destination::Slope => {
                field(ui, "Elevation Change", &mut self.rise, true);
                if self.destination == Destination::Distance {
                    field(ui, "Horizontal Distance", &mut self.distance, true);
                } else if let Some(distance) =
                    self.params(group, "").and_then(|p| Some(p["rise"].as_f64()?.abs() * p["slope"][1].as_f64()? / p["slope"][0].as_f64()?))
                {
                    ui.label(crate::tf!("Horizontal distance: {distance}", distance = format!("{distance:.3}")));
                }
            }
            Destination::Elevation => {
                field(ui, "Target Elevation", &mut self.elevation, true);
            }
            Destination::Terrain => self.terrain(ui, app, group),
        }
        ui.checkbox(&mut self.round, t("Round Outside Corners"));
        ui.checkbox(&mut self.attach, t("Add Copy to Source Surface"));
    }

    fn terrain(&mut self, ui: &mut egui::Ui, app: &CadApp, group: &str) {
        let Ok(doc) = app.session.doc() else {
            return;
        };
        let names: Vec<_> =
            doc.groups.iter().filter(|g| g.is_terrain_surface() && !g.members.is_empty() && g.name != group).map(|g| &g.name).collect();
        if !names.iter().any(|name| **name == self.surface) {
            self.surface = names.first().map_or_else(String::new, |name| (*name).clone());
        }
        let caption = ui.label(t("Target Surface"));
        let selector = craft_ui::ComboBox::from_id_salt("chain-copy-terrain")
            .selected_text(&self.surface)
            .show_ui(ui, |ui| {
                for name in names {
                    ui.selectable_value(&mut self.surface, name.clone(), name);
                }
            })
            .response
            .labelled_by(caption.id);
        crate::control::record_widget(ui, &selector, t("Target Surface"), "combobox", None);
        if self.surface.is_empty() {
            ui.label(t("Create another terrain surface to use as a target."));
        }
        ui.horizontal(|ui| {
            ui.label(t("Slope Direction"));
            ui.radio_value(&mut self.uphill, false, t("Down"));
            ui.radio_value(&mut self.uphill, true, t("Up"));
        });
    }
}

impl Tools {
    pub(super) fn cancel(&mut self) -> bool {
        let open = self.form.take().is_some();
        self.clear_preview();
        self.error = None;
        open
    }

    pub(super) fn clear_preview(&mut self) {
        self.preview = None;
    }

    pub(super) fn preview(&self) -> Option<&[Vec3]> {
        self.preview.as_ref().map(|(_, points)| points.as_slice())
    }

    pub(super) fn controls(&mut self, app: &mut CadApp, ui: &mut egui::Ui, group: &str, curve: &Curve, height: f64, enabled: bool) -> bool {
        if self.handle != curve.handle {
            self.cancel();
            self.handle.clone_from(&curve.handle);
        }
        let ends = curve.segments.first().and_then(|p| p.first()).zip(curve.segments.last().and_then(|p| p.last()));
        let closed = curve.closed || ends.is_some_and(|(first, last)| first == last);
        ui.horizontal_wrapped(|ui| {
            if button(ui, "Set Chain Elevation…", enabled) {
                self.cancel();
                self.focus = true;
                self.form = Some(Form::Elevation(height.to_string()));
            }
            if button(ui, "Copy Chain…", enabled) {
                self.cancel();
                self.form = Some(Form::Copy(CopyForm::new(closed, height)));
            }
        });
        let Some(form) = &mut self.form else {
            return false;
        };
        let mut changed = false;
        ui.separator();
        match form {
            Form::Elevation(value) => {
                let edit = field(ui, "Chain Elevation", value, enabled);
                if std::mem::take(&mut self.focus) {
                    edit.request_focus();
                    if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), edit.id) {
                        let stop = egui::text::CCursor::new(value.chars().count());
                        state.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), stop)));
                        state.store(ui.ctx(), edit.id);
                    }
                }
                let val = coordinate(value).filter(|_| enabled);
                let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (button(ui, "Apply Chain Elevation", val.is_some()) || enter)
                    && let Some(elevation) = val
                {
                    changed = save(app, HEIGHT, json!({"group":group,"handle":self.handle,"elevation":elevation}), &mut self.error);
                }
            }
            Form::Copy(form) => {
                form.inputs(ui, app, group, closed);
                let params = form.params(group, &self.handle).filter(|_| enabled);
                if self.preview.as_ref().is_some_and(|(active, _)| Some(active) != params.as_ref()) {
                    self.preview = None;
                }
                ui.horizontal_wrapped(|ui| {
                    if button(ui, "Preview Copy", params.is_some())
                        && let Some(params) = &params
                    {
                        self.error = None;
                        match preview(app, params) {
                            Ok(points) => self.preview = Some((params.clone(), points)),
                            Err(error) => {
                                self.preview = None;
                                self.error = Some(error);
                            }
                        }
                    }
                    if button(ui, "Create Copy", params.is_some())
                        && let Some(params) = params
                    {
                        changed = save(app, COPY, params, &mut self.error);
                    }
                });
            }
        }
        if let Some(error) = &self.error {
            ui.label(error);
        }
        if button(ui, "Close Chain Tools", true) || changed {
            self.cancel();
        }
        true
    }
}

fn button(ui: &mut egui::Ui, label: &'static str, enabled: bool) -> bool {
    let label = t(label);
    let control = ui.add_enabled(enabled, egui::Button::new(label));
    crate::control::record_widget(ui, &control, label, "button", None);
    control.clicked()
}

fn preview(app: &mut CadApp, params: &Value) -> Result<Vec<Vec3>, String> {
    let mut result = app.run(PREVIEW, params.clone())?;
    let mut points: Vec<Vec3> = serde_json::from_value(result["points"].take()).map_err(|e| e.to_string())?;
    if result["closed"].as_bool() == Some(true)
        && let Some(&first) = points.first()
    {
        points.push(first);
    }
    Ok(points)
}

fn save(app: &mut CadApp, id: &str, params: Value, error: &mut Option<String>) -> bool {
    match app.run(id, params) {
        Ok(_) => {
            app.set_status(t(if id == HEIGHT { "Updated the source chain elevation." } else { "Created a copy of the source chain." }));
            true
        }
        Err(message) => {
            *error = Some(message);
            false
        }
    }
}
