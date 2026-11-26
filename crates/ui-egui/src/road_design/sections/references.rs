//! Road reference line settings and pickable pipe points in the cross section.
use cadcraft_doc::{
    Handle,
    road::{Design, MAX_REFERENCE_VERTICES, MAX_REFERENCES, ReferenceLine},
};
use cadcraft_geom::terrain::road::{Pipe, PipeLevel, ReferencePoint, SectionReference};
use egui::{Align2, Painter, Pos2, Rect, Stroke, vec2};

use crate::{CadApp, control::record_widget, i18n::t, theme::Tokens};

use super::{choose_button, decimal, design_rules, field, next_line_number};

#[derive(Default)]
pub(super) struct Settings {
    pipe: Option<PipeForm>,
}

struct PipeForm {
    number: u32,
    diameter: String,
    level: PipeLevel,
    focus: bool,
}

impl Settings {
    pub(super) fn show(&mut self, app: &CadApp, ui: &mut egui::Ui, design: &mut Design, selection: &[String], message: &mut Option<String>) {
        ui.collapsing(t("Reference Lines"), |ui| {
            let label = t("Add Selected Reference Lines");
            let add = ui.add_enabled(!selection.is_empty(), egui::Button::new(label));
            record_widget(ui, &add, label, "button", None);
            if add.clicked() {
                *message = add_selection(app, design, selection).err();
            }
            let mut remove = None;
            let mut as_line = None;
            for reference in &design.references {
                ui.push_id(reference.number, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(reference_label(reference.number)).on_hover_text(&reference.handle);
                        let edit = if reference.pipe.is_some() { "Edit Pipe" } else { "Configure Pipe" };
                        if choose_button(ui, edit) {
                            self.pipe = Some(PipeForm {
                                number: reference.number,
                                diameter: reference.pipe.map_or_else(String::new, |pipe| pipe.diameter.to_string()),
                                level: reference.pipe.map_or(PipeLevel::Invert, |pipe| pipe.level),
                                focus: true,
                            });
                        }
                        if let Some(pipe) = reference.pipe {
                            let level = level_label(pipe.level);
                            ui.label(crate::tf!("Diameter {diameter} · {level}", diameter = pipe.diameter, level = level));
                            if choose_button(ui, "Use as Line") {
                                as_line = Some(reference.number);
                            }
                        }
                        if choose_button(ui, "Remove Reference") {
                            remove = Some(reference.number);
                        }
                    });
                });
            }
            if let Some(number) = remove.or(as_line) {
                if design_rules(design)
                    .any(|rule| rule.from == number && rule.from_reference.is_some_and(|part| remove.is_some() || part != ReferencePoint::Line))
                {
                    *message = Some(t("Remove the instructions that use this reference point first.").to_owned());
                } else if remove.is_some() {
                    design.references.retain(|reference| reference.number != number);
                    self.pipe = self.pipe.take().filter(|form| form.number != number);
                    *message = None;
                } else if let Some(reference) = design.references.iter_mut().find(|reference| reference.number == number) {
                    reference.pipe = None;
                    *message = None;
                }
            }
            self.pipe_form(ui, design, message);
        });
    }

    fn pipe_form(&mut self, ui: &mut egui::Ui, design: &mut Design, message: &mut Option<String>) {
        let Some(form) = &mut self.pipe else { return };
        ui.separator();
        ui.strong(reference_label(form.number));
        let (save, cancel) = ui
            .horizontal_wrapped(|ui| {
                let diameter = field(ui, "Pipe Diameter", &mut form.diameter);
                if std::mem::take(&mut form.focus) {
                    diameter.request_focus();
                }
                let label = t("Line Height Represents");
                let caption = ui.label(label);
                let level = craft_ui::ComboBox::from_id_salt("pipe-level")
                    .selected_text(level_label(form.level))
                    .show_ui(ui, |ui| {
                        for value in [PipeLevel::Invert, PipeLevel::Crown] {
                            ui.selectable_value(&mut form.level, value, level_label(value));
                        }
                    })
                    .response
                    .labelled_by(caption.id);
                record_widget(ui, &level, label, "combobox", None);
                let save = choose_button(ui, "Apply Pipe");
                (save, choose_button(ui, "Cancel Pipe Edit"))
            })
            .inner;
        if cancel {
            self.pipe = None;
            *message = None;
        } else if save {
            let pipe = Pipe { diameter: decimal(&form.diameter).unwrap_or(0.0), level: form.level };
            if !pipe.valid() {
                *message = Some(t("Enter a positive pipe diameter in drawing units.").into());
                return;
            }
            if let Some(reference) = design.references.iter_mut().find(|reference| reference.number == form.number) {
                reference.pipe = Some(pipe);
            }
            self.pipe = None;
            *message = None;
        }
    }
}

fn add_selection(app: &CadApp, design: &mut Design, selection: &[String]) -> Result<(), String> {
    let doc = app.session.doc().map_err(|error| error.to_string())?;
    let mut handles: Vec<String> = Vec::new();
    let mut remaining = MAX_REFERENCE_VERTICES;
    for handle in design.references.iter().map(|reference| &reference.handle).chain(selection) {
        if handles.contains(handle) {
            continue;
        }
        let Some(entity) = Handle::parse_hex(handle).and_then(|handle| doc.model.get(handle)) else { continue };
        if !matches!(entity.kind, cadcraft_doc::EntityKind::Line(_) | cadcraft_doc::EntityKind::Polyline3d(_)) {
            continue;
        }
        let path = cadcraft_doc::terrain::source_vertices(entity, remaining).map_err(t)?;
        if !path.windows(2).any(|pair| pair[0].xy() != pair[1].xy()) {
            return Err(t("Each reference line needs a horizontal extent.").into());
        }
        remaining -= path.len();
        handles.push(handle.clone());
    }
    handles.retain(|handle| !design.references.iter().any(|reference| reference.handle == *handle));
    if handles.is_empty() {
        return Err(t("Select new lines or 3D polylines in the drawing.").into());
    }
    if design.references.len() + handles.len() > MAX_REFERENCES {
        return Err(t("The road exceeds the limit on reference lines.").into());
    }
    let first = next_line_number(design);
    let count = u32::try_from(handles.len()).map_err(|error| error.to_string())?;
    if first == u32::MAX || first.checked_add(count - 1).is_none() {
        return Err(t("There are no available line numbers.").into());
    }
    design.references.extend(handles.into_iter().enumerate().map(|(index, handle)| ReferenceLine {
        number: first + index as u32,
        handle,
        pipe: None,
    }));
    Ok(())
}

pub(super) fn reference_label(number: u32) -> String {
    crate::tf!("Reference {number}", number = number)
}

pub(super) fn part_label(part: ReferencePoint) -> &'static str {
    t(match part {
        ReferencePoint::Line => "Line Point",
        ReferencePoint::LeftEdge => "Pipe Left Edge",
        ReferencePoint::RightEdge => "Pipe Right Edge",
        ReferencePoint::Crown => "Pipe Crown",
        ReferencePoint::Invert => "Pipe Invert",
    })
}

fn level_label(level: PipeLevel) -> &'static str {
    part_label(match level {
        PipeLevel::Invert => ReferencePoint::Invert,
        PipeLevel::Crown => ReferencePoint::Crown,
    })
}

pub(super) fn source(ui: &mut egui::Ui, source: &mut String, part: &mut ReferencePoint, lines: &[ReferenceLine]) {
    let number = source.trim().parse::<u32>().ok();
    let mut selected = lines.iter().find(|line| Some(line.number) == number);
    let label = t("Reference Line");
    let caption = ui.label(label);
    let response = craft_ui::ComboBox::from_id_salt("source-reference-line")
        .selected_text(selected.map_or_else(|| t("Choose a Reference").into(), |line| reference_label(line.number)))
        .show_ui(ui, |ui| {
            for line in lines {
                if ui.selectable_label(Some(line.number) == number, reference_label(line.number)).clicked() {
                    *source = line.number.to_string();
                    selected = Some(line);
                    if line.pipe.is_none() {
                        *part = ReferencePoint::Line;
                    }
                }
            }
        })
        .response
        .labelled_by(caption.id);
    record_widget(ui, &response, label, "combobox", None);
    if selected.is_some_and(|line| line.pipe.is_some()) {
        let label = t("Reference Point");
        let caption = ui.label(label);
        let response = craft_ui::ComboBox::from_id_salt("source-reference-point")
            .selected_text(part_label(*part))
            .show_ui(ui, |ui| {
                for value in std::iter::once(ReferencePoint::Line).chain(ReferencePoint::PIPE_POINTS) {
                    ui.selectable_value(part, value, part_label(value));
                }
            })
            .response
            .labelled_by(caption.id);
        record_widget(ui, &response, label, "combobox", None);
    }
}

pub(super) fn describe(reference: SectionReference) -> String {
    let label = reference_label(reference.number);
    if let Some(pipe) = reference.pipe { crate::tf!("{label} · Pipe diameter {diameter}", label = label, diameter = pipe.diameter) } else { label }
}

pub(super) fn paint(
    ui: &mut egui::Ui,
    painter: &Painter,
    references: &[SectionReference],
    selectable: bool,
    selected: Option<(u32, ReferencePoint)>,
    project: impl Fn(f64, f64) -> Pos2,
) -> Option<(u32, ReferencePoint)> {
    let tokens = Tokens::get();
    let clip = painter.clip_rect();
    let mut picked = None;
    for &reference in references {
        let color = tokens.icon_point;
        let mut parts: &[ReferencePoint] = &[ReferencePoint::Line];
        let mut handle_size = 24.0;
        if let Some((center, radius)) = reference.circle() {
            let at = project(center.x, center.y);
            let edge = project(center.x + radius, center.y);
            let pixels = at.distance(edge);
            painter.circle_stroke(at, pixels, Stroke::new(1.6, color));
            handle_size = (pixels * 1.25).clamp(2.0, 24.0);
            parts = &ReferencePoint::PIPE_POINTS;
        }
        let number_at = reference.point(ReferencePoint::Crown).unwrap_or(reference.line);
        let number_at = project(number_at.x, number_at.y);
        if clip.contains(number_at) {
            painter.text(number_at + vec2(0.0, -11.0), Align2::CENTER_BOTTOM, reference.number.to_string(), crate::theme::small(), color);
        }
        for &part in parts {
            let Some(point) = reference.point(part) else { continue };
            let at = project(point.x, point.y);
            if !clip.contains(at) {
                continue;
            }
            let active = selected == Some((reference.number, part));
            let mut highlight = active;
            if selectable {
                let hit = Rect::from_center_size(at, egui::Vec2::splat(handle_size)).intersect(clip);
                let button = ui.interact(hit, ui.scope_id().with(("road-reference-point", reference.number, part as u8)), egui::Sense::click());
                let name = crate::tf!(
                    "{reference} · {point} · Offset {offset} · Elevation {height}",
                    reference = reference_label(reference.number),
                    point = part_label(part),
                    offset = format!("{:.3}", point.x),
                    height = format!("{:.3}", point.y)
                );
                button.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, active, &name));
                record_widget(ui, &button, &name, "button", Some(active));
                highlight |= button.hovered() || button.has_focus();
                if button.on_hover_text(name).clicked() {
                    picked = Some((reference.number, part));
                }
            }
            painter.circle_filled(at, if highlight { 4.5 } else { 2.7 }, if highlight { tokens.hover } else { color });
        }
    }
    picked
}
