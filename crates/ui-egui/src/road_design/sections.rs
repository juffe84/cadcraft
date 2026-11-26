//! Interactive preview and station-scoped instructions for numbered road sections.
use std::collections::BTreeMap;

use cadcraft_doc::road::{ConstructionLayer, Design, ReferenceLine, SectionExtension, SectionRule, Template};
use cadcraft_geom::{
    Vec2, Vec3,
    terrain::road::{ExtensionSide, ReferencePoint, Section, SectionLayer, SectionReference, section_layer_grade_hit},
};
use egui::{Align2, Pos2, Rect, Stroke, pos2, vec2};
use serde_json::{Value, json};

use crate::{
    CadApp,
    control::record_widget,
    i18n::t,
    icons::{self, Icon},
    theme::Tokens,
};

use super::choose_button;

mod references;
mod segments;
use segments::{Segment, SegmentId, SegmentPick};

#[derive(Default)]
pub(super) struct Editor {
    axis: String,
    station: String,
    cache: Option<Preview>,
    view: SectionView,
    references: references::Settings,
    layer: usize,
    new_layer: Option<String>,
    drafts: BTreeMap<usize, Draft>,
}

struct Preview {
    revision: u64,
    params: Value,
    section: Result<Section, String>,
}

struct SectionView {
    zoom: f32,
    center: egui::Vec2,
}

impl Default for SectionView {
    fn default() -> Self {
        Self { zoom: 1.0, center: egui::Vec2::ZERO }
    }
}

impl SectionView {
    fn zoom_at(&mut self, factor: f32, anchor: egui::Vec2) {
        let next = (self.zoom * factor).clamp(1.0, 32.0);
        self.center += anchor * (self.zoom.recip() - next.recip());
        self.zoom = next;
        self.clamp_center();
    }

    fn clamp_center(&mut self) {
        let limit = egui::Vec2::splat((1.0 - self.zoom.recip()) * 0.5);
        self.center = self.center.clamp(-limit, limit);
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        if navigation_button(ui, Icon::ZoomIn, "Zoom In Cross Section", self.zoom < 32.0) {
            self.zoom_at(1.5, egui::Vec2::ZERO);
        }
        if navigation_button(ui, Icon::ZoomOut, "Zoom Out Cross Section", self.zoom > 1.0) {
            self.zoom_at(1.0 / 1.5, egui::Vec2::ZERO);
        }
        if navigation_button(ui, Icon::ZoomFit, "Fit Cross Section", self.zoom > 1.0) {
            *self = Self::default();
        }
    }

    fn pointer(&mut self, ui: &egui::Ui, response: &egui::Response, inner: Rect) {
        if response.contains_pointer() {
            let (scroll, pointer) = ui.input_mut(|input| {
                let scroll = std::mem::take(&mut input.smooth_scroll_delta.y);
                (scroll, input.pointer.hover_pos())
            });
            if scroll != 0.0
                && let Some(pointer) = pointer
            {
                let delta = pointer - inner.center();
                let anchor = vec2(delta.x / inner.width(), delta.y / inner.height());
                self.zoom_at((scroll.clamp(-300.0, 300.0) * 0.006).exp(), anchor);
            }
        }
        if response.dragged_by(egui::PointerButton::Middle) {
            let delta = response.drag_delta();
            self.center -= vec2(delta.x / inner.width(), delta.y / inner.height()) / self.zoom;
            self.clamp_center();
        }
    }

    fn project(&self, relative: egui::Vec2, inner: Rect) -> Pos2 {
        let delta = (relative - self.center) * self.zoom;
        inner.center() + vec2(delta.x * inner.width(), delta.y * inner.height())
    }
}

#[derive(Default)]
struct Draft {
    focus_number: bool,
    number: String,
    source: String,
    source_layer: Option<String>,
    from_reference: Option<ReferencePoint>,
    all: bool,
    start: String,
    end: String,
    offset: String,
    height: String,
    start_grade: String,
    end_grade: String,
    edges: Vec<String>,
    extension: Option<ExtensionDraft>,
    helper: bool,
    editing: Option<usize>,
}

#[derive(Default)]
struct ExtensionDraft {
    follow_segment: bool,
    grade: String,
    through: String,
    target: [String; 2],
    target_layer: Option<String>,
    whole_layer: bool,
    target_side: Option<ExtensionSide>,
    pick: Option<SegmentPick>,
    helper_pick: Option<HelperPick>,
    error: Option<&'static str>,
}

#[derive(Clone, Copy)]
enum HelperPick {
    First,
    Second { layer: usize, number: u32 },
}

const EXTENSION_POINTS: [&str; 4] = ["Direction Start", "Direction End", "Target Start", "Target End"];

impl Draft {
    fn read(&self) -> Option<SectionRule> {
        let stations = if self.all { None } else { Some([parse_station(&self.start)?, parse_station(&self.end)?]) };
        let mut rule = SectionRule {
            number: self.number.trim().parse().ok()?,
            from: self.source.trim().parse().ok()?,
            from_layer: self.source_layer.clone(),
            from_reference: self.from_reference,
            stations,
            offset: 0.0,
            height: 0.0,
            crossfall: [0.0; 2],
            edges: Vec::new(),
            extension: None,
            helper: self.helper,
        };
        if let Some(extension) = &self.extension {
            let (through, grade) = if extension.follow_segment {
                (Some(extension.through.trim().parse().ok()?), None)
            } else {
                (None, Some(decimal(&extension.grade)?))
            };
            rule.extension = Some(SectionExtension {
                through,
                grade,
                target: if extension.whole_layer {
                    None
                } else {
                    Some([extension.target[0].trim().parse().ok()?, extension.target[1].trim().parse().ok()?])
                },
                target_layer: extension.target_layer.clone(),
                target_side: if extension.whole_layer { Some(extension.target_side?) } else { None },
            });
        } else {
            let start_grade = optional_decimal(&self.start_grade, 0.0)?;
            rule.offset = optional_decimal(&self.offset, 0.0)?;
            rule.height = optional_decimal(&self.height, 0.0)?;
            rule.crossfall = [start_grade, optional_decimal(&self.end_grade, start_grade)?];
            rule.edges.clone_from(&self.edges);
        }
        Some(rule)
    }

    fn from_rule(index: usize, rule: &SectionRule) -> Self {
        let [start, end] = rule.stations.unwrap_or_default().map(|v| v.to_string());
        Self {
            focus_number: true,
            number: rule.number.to_string(),
            source: rule.from.to_string(),
            source_layer: rule.from_layer.clone(),
            from_reference: rule.from_reference,
            all: rule.stations.is_none(),
            start,
            end,
            offset: rule.offset.to_string(),
            height: rule.height.to_string(),
            start_grade: rule.crossfall[0].to_string(),
            end_grade: rule.crossfall[1].to_string(),
            edges: rule.edges.clone(),
            extension: rule.extension.as_ref().map(|extension| ExtensionDraft {
                follow_segment: extension.through.is_some(),
                grade: extension.grade.map_or_else(String::new, |grade| grade.to_string()),
                through: extension.through.map_or_else(String::new, |number| number.to_string()),
                target: extension.target.map(|points| points.map(|number| number.to_string())).unwrap_or_default(),
                target_layer: extension.target_layer.clone(),
                whole_layer: extension.target_side.is_some(),
                target_side: extension.target_side,
                ..Default::default()
            }),
            helper: rule.helper,
            editing: Some(index),
        }
    }

    fn pick_point(&mut self, layer: Option<&str>, number: u32) {
        self.source = number.to_string();
        self.source_layer = layer.map(str::to_owned);
        self.from_reference = None;
        if let Some(extension) = &mut self.extension
            && !extension.follow_segment
        {
            extension.pick = (!extension.whole_layer).then_some(SegmentPick::TargetFromPoint);
            extension.error = None;
        }
    }

    fn pick_reference(&mut self, number: u32, point: ReferencePoint) {
        self.pick_point(None, number);
        self.from_reference = Some(point);
    }

    fn selected_reference(&self) -> Option<(u32, ReferencePoint)> {
        Some((self.source.trim().parse().ok()?, self.from_reference?))
    }

    fn pick_segment(&mut self, picked: Segment, segments: &[Segment], layers: &[SectionLayer], active: usize) {
        let Some(extension) = &mut self.extension else { return };
        let Some(pick) = extension.pick else { return };
        let name = |id: SegmentId| (id.layer != active).then(|| layers[id.layer].name.clone());
        if matches!(pick, SegmentPick::TargetFromPoint) {
            extension.target = picked.id.numbers.map(|number| number.to_string());
            extension.target_layer = name(picked.id);
            extension.pick = None;
            extension.error = None;
            return;
        }
        let SegmentPick::Target(source) = pick else {
            extension.pick = Some(SegmentPick::Target(picked.id));
            extension.error = None;
            return;
        };
        let Some(source) = segments.iter().find(|segment| segment.id == source) else {
            extension.pick = Some(SegmentPick::Source);
            extension.error = Some("The source segment is unavailable at this station. Pick it again.");
            return;
        };
        let Some((direction, _)) = source.continuation_to(picked) else {
            extension.error = Some("The source does not reach that segment beyond either endpoint. Pick another target.");
            return;
        };
        [self.source, extension.through] = direction.map(|number| number.to_string());
        self.source_layer = name(source.id);
        extension.target = picked.id.numbers.map(|number| number.to_string());
        extension.target_layer = name(picked.id);
        extension.pick = None;
        extension.error = None;
    }

    fn selected_segments(&self, segments: &[Segment], layers: &[SectionLayer], active: usize, target: Option<Segment>) -> Option<[Segment; 2]> {
        let extension = self.extension.as_ref()?;
        if !extension.follow_segment {
            return None;
        }
        let first = self.source.trim().parse::<u32>().ok()?;
        let second = extension.through.trim().parse::<u32>().ok()?;
        let source = find_segment(segments, layers, active, self.source_layer.as_deref(), [first, second])?.oriented(first);
        Some([source, target?])
    }

    fn target_segment(&self, layers: &[SectionLayer], active: usize, project: impl Fn(f64, f64) -> Pos2) -> Option<Segment> {
        let extension = self.extension.as_ref().filter(|extension| !extension.whole_layer)?;
        let target = [extension.target[0].trim().parse().ok()?, extension.target[1].trim().parse().ok()?];
        let layer = match extension.target_layer.as_deref() {
            Some(name) => layers.iter().position(|layer| layer.name.eq_ignore_ascii_case(name))?,
            None => active,
        };
        section_segment(layers, layer, target, project)
    }

    fn pick_helper(
        &mut self,
        picked: (usize, u32),
        layers: &[SectionLayer],
        segments: &[Segment],
        active: usize,
        project: impl Fn(f64, f64) -> Pos2,
    ) {
        let Some(extension) = &mut self.extension else { return };
        let (picked_layer, picked_number) = picked;
        let Some(HelperPick::Second { layer, number }) = extension.helper_pick else {
            extension.helper_pick = Some(HelperPick::Second { layer: picked_layer, number: picked_number });
            extension.error = None;
            return;
        };
        if picked_layer != layer || picked_number == number {
            extension.error = Some("Choose a different helper point on the same layer.");
            return;
        }
        let Some(target) = section_segment(layers, layer, [number, picked_number], project) else {
            extension.helper_pick = Some(HelperPick::First);
            extension.error = Some("The first helper point is unavailable at this station. Pick it again.");
            return;
        };
        self.pick_segment(target, segments, layers, active);
        if let Some(extension) = &mut self.extension
            && matches!(extension.pick, None | Some(SegmentPick::Source))
        {
            extension.helper_pick = None;
        }
    }

    fn source_position(&self, layers: &[SectionLayer], active: usize, references: &[SectionReference]) -> Option<Vec2> {
        let number = self.source.trim().parse::<u32>().ok()?;
        if let Some(part) = self.from_reference {
            return references.iter().find(|reference| reference.number == number)?.point(part);
        }
        let layer = match &self.source_layer {
            Some(name) => layers.iter().find(|layer| &layer.name == name)?,
            None => layers.get(active)?,
        };
        Some(layer.nodes.iter().find(|point| point.number == number)?.point())
    }

    fn grade_guide(&self, layers: &[SectionLayer], active: usize, references: &[SectionReference], target: Segment) -> Option<(Segment, Vec2, Vec2)> {
        let extension = self.extension.as_ref().filter(|extension| !extension.follow_segment)?;
        let start = self.source_position(layers, active, references)?;
        let grade = decimal(&extension.grade)?;
        Some((target, start, target.hit_at_grade(start, grade)?))
    }

    fn layer_guide(&self, layers: &[SectionLayer], active: usize, references: &[SectionReference], segments: &[Segment]) -> Option<(Vec2, Vec2)> {
        let extension = self.extension.as_ref().filter(|extension| extension.whole_layer)?;
        let layer = layers.iter().position(|layer| layer.name.eq_ignore_ascii_case(extension.target_layer.as_deref().unwrap_or_default()))?;
        let start = self.source_position(layers, active, references)?;
        let targets = segments.iter().copied().filter(|segment| segment.id.layer == layer).map(Segment::points);
        Some((start, section_layer_grade_hit(start, decimal(&extension.grade)?, extension.target_side?, targets)?))
    }

    fn segment_pick(&self) -> Option<SegmentPick> {
        self.extension.as_ref()?.pick
    }

    fn points_are_selectable(&self) -> bool {
        self.extension.as_ref().is_none_or(|extension| !extension.follow_segment && extension.pick.is_none() && extension.helper_pick.is_none())
    }

    fn helper_pick(&self) -> Option<HelperPick> {
        self.extension.as_ref()?.helper_pick
    }

    fn references(&self, layer: Option<&str>, number: u32) -> bool {
        let matches = |input: &str| input.trim().parse::<u32>().ok() == Some(number);
        if self.from_reference.is_none()
            && self.source_layer.as_deref() == layer
            && (matches(&self.source) || self.extension.as_ref().is_some_and(|extension| extension.follow_segment && matches(&extension.through)))
        {
            return true;
        }
        self.extension.as_ref().is_some_and(|extension| {
            !extension.whole_layer && extension.target_layer.as_deref() == layer && extension.target.iter().any(|number| matches(number))
        })
    }
}

fn section_segment(layers: &[SectionLayer], layer: usize, [first, last]: [u32; 2], project: impl Fn(f64, f64) -> Pos2) -> Option<Segment> {
    let nodes = &layers.get(layer)?.nodes;
    let find = |number| nodes.iter().copied().find(|node| node.number == number);
    Some(Segment::from_nodes(layer, [find(first)?, find(last)?], project))
}

fn find_segment(segments: &[Segment], layers: &[SectionLayer], active: usize, layer: Option<&str>, numbers: [u32; 2]) -> Option<Segment> {
    segments.iter().copied().find(|segment| {
        let name = (segment.id.layer != active).then(|| layers[segment.id.layer].name.as_str());
        name == layer && segment.id.matches(numbers)
    })
}

impl Editor {
    pub(super) fn layers(&mut self, ui: &mut egui::Ui, design: &mut Design, message: &mut Option<String>) {
        self.layer = self.layer.min(design.layers.len());
        let adding = ui
            .horizontal_wrapped(|ui| {
                let caption = ui.label(t("Layer"));
                let previous = self.layer;
                let combo = craft_ui::ComboBox::from_id_salt("road-layer")
                    .selected_text(layer_name(self.selected_layer(design)))
                    .show_ui(ui, |ui| {
                        for (index, name) in std::iter::once("Top").chain(design.layers.iter().map(|layer| layer.name.as_str())).enumerate() {
                            ui.selectable_value(&mut self.layer, index, layer_name(name));
                        }
                    })
                    .response
                    .labelled_by(caption.id);
                record_widget(ui, &combo, t("Layer"), "combobox", None);
                if self.layer != previous {
                    *message = None;
                }
                self.new_layer.is_none() && choose_button(ui, "Add Layer")
            })
            .inner;
        if adding {
            self.new_layer = Some(String::new());
        }
        if let Some(name) = &mut self.new_layer {
            let (create, cancel) = ui
                .horizontal_wrapped(|ui| {
                    let input = field(ui, "Layer Name", name);
                    if adding {
                        input.request_focus();
                    }
                    let create = choose_button(ui, "Create Layer") || (input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                    (create, choose_button(ui, "Cancel"))
                })
                .inner;
            if create {
                let name = name.trim();
                if !cadcraft_doc::road::valid_name(name)
                    || name.eq_ignore_ascii_case("Top")
                    || design.layers.iter().any(|layer| layer.name.eq_ignore_ascii_case(name))
                {
                    *message = Some(t("Enter a valid and unique layer name.").into());
                } else if design.layers.len() == cadcraft_doc::road::MAX_LAYERS {
                    *message = Some(t("The road has reached its layer limit.").into());
                } else {
                    design.layers.push(ConstructionLayer {
                        name: name.into(),
                        thickness: 0.0,
                        template: Template { inherit: false, rules: Vec::new() },
                    });
                    self.layer = design.layers.len();
                    self.new_layer = None;
                    *message = None;
                }
            } else if cancel {
                self.new_layer = None;
                *message = None;
            }
        }
    }

    pub(super) fn selected_layer<'a>(&self, design: &'a Design) -> &'a str {
        self.layer.checked_sub(1).and_then(|i| design.layers.get(i)).map_or("Top", |layer| &layer.name)
    }

    pub(super) fn reference_controls(
        &mut self,
        app: &CadApp,
        ui: &mut egui::Ui,
        design: &mut Design,
        selection: &[String],
        message: &mut Option<String>,
    ) {
        self.references.show(app, ui, design, selection, message);
    }

    pub(super) fn graph(&mut self, app: &mut CadApp, ui: &mut egui::Ui, design: &Design) {
        if self.axis != design.axis {
            self.axis.clone_from(&design.axis);
            self.station = design.range[0].to_string();
            self.cache = None;
            self.view = SectionView::default();
        }
        ui.strong(t("Cross Section"));
        if design.axis.is_empty() {
            ui.label(t("Choose a centerline to see the cross section."));
            return;
        }
        let [from, to] = design.range;
        ui.horizontal_wrapped(|ui| {
            field(ui, "Section Station", &mut self.station);
            if from >= to {
                return;
            }
            let station = parse_station(&self.station);
            let mut position = station.unwrap_or(from).clamp(from, to);
            let previous = navigation_button(ui, Icon::ChevronLeft, "Previous Section", position > from);
            let label = t("Section Position");
            let slider = ui.add(egui::Slider::new(&mut position, from..=to).show_value(false).max_decimals(3));
            slider.widget_info(|| egui::WidgetInfo::slider(slider.enabled(), position, label));
            record_widget(ui, &slider, label, "slider", None);
            let next = navigation_button(ui, Icon::ChevronRight, "Next Section", position < to);
            if previous {
                self.station = (position - design.interval).max(from).to_string();
            } else if next {
                self.station = (position + design.interval).min(to).to_string();
            } else if slider.changed() {
                self.station = position.to_string();
            }
            self.view.controls(ui);
        });
        let Some(station) = parse_station(&self.station) else {
            ui.label(t("Enter a station number."));
            return;
        };
        let revision = app.session.state().map_or(0, |s| s.revision);
        let params = json!({"station":station, "design":design});
        if self.cache.as_ref().is_none_or(|p| p.params != params || p.revision != revision) {
            // A draft can be incomplete while typing; query without sending every validation error to command history.
            let section = app
                .session
                .execute("tin.road.section", &params)
                .map_err(|e| e.to_string())
                .and_then(|value| serde_json::from_value(value).map_err(|e| e.to_string()));
            self.cache = Some(Preview { revision, params, section });
            ui.ctx().request_repaint();
        }
        if let Some(cache) = &self.cache {
            match &cache.section {
                Ok(section) => {
                    let active = self.layer;
                    if let Some(visible) = section.layers.get(..=active) {
                        graph(ui, visible, &section.references, active, self.drafts.get_mut(&active), &mut self.view);
                        for layer in visible.iter().filter(|layer| !layer.missed_extensions.is_empty()) {
                            let numbers = layer.missed_extensions.iter().map(u32::to_string).collect::<Vec<_>>().join(", ");
                            let name = layer_name(&layer.name);
                            let status = ui.label(crate::tf!(
                                "Lines {numbers} on {name} do not reach their targets at this station.",
                                numbers = numbers,
                                name = name
                            ));
                            ui.ctx().accesskit_node_builder(status.id, |node| node.set_live(egui::accesskit::Live::Polite));
                        }
                        let numbers = visible[active]
                            .clipped
                            .iter()
                            .filter(|number| !visible[active].missed_extensions.contains(number))
                            .map(u32::to_string)
                            .collect::<Vec<_>>()
                            .join(", ");
                        if !numbers.is_empty() {
                            ui.label(crate::tf!("Source geometry is unavailable at this station for lines {numbers}.", numbers = numbers));
                        }
                    }
                }
                Err(message) => {
                    ui.label(message);
                }
            }
        }
    }

    pub(super) fn marker(&self) -> Option<(f64, [Vec3; 2])> {
        let station = parse_station(&self.station)?;
        let section = self.cache.as_ref()?.section.as_ref().ok()?;
        let [min, max] = section
            .layers
            .iter()
            .take(self.layer + 1)
            .flat_map(|layer| &layer.nodes)
            .filter(|node| !node.helper)
            .fold([0.0_f64; 2], |[min, max], node| [min.min(node.offset), max.max(node.offset)]);
        let margin = ((max - min) * 0.12).max(2.0);
        Some((station, [min - margin, max + margin].map(|offset| section.origin + section.right * offset)))
    }

    pub(super) fn layer_index(&self) -> usize {
        self.layer
    }

    pub(super) fn is_editing(&self) -> bool {
        self.drafts.contains_key(&self.layer)
    }

    pub(super) fn instructions(
        &mut self,
        ui: &mut egui::Ui,
        session: &mut cadcraft_engine::Session,
        design: &mut Design,
        selection: &[String],
        message: &mut Option<String>,
    ) {
        let layer = self.layer;
        let earlier: Vec<_> = std::iter::once("Top").chain(design.layers.iter().map(|layer| layer.name.as_str())).take(layer).collect();
        let template = if layer == 0 { &design.top } else { &design.layers[layer - 1].template };
        let station = parse_station(&self.station).filter(|&at| design.range[0] <= at && at <= design.range[1]);
        // As in the sampler, the last applicable instruction defines each line.
        let active: BTreeMap<_, _> = template
            .rules
            .iter()
            .enumerate()
            .filter(|(_, rule)| station.is_some_and(|at| rule.stations.is_none_or(|[from, to]| from <= at && at <= to)))
            .map(|(index, rule)| (rule.number, index))
            .collect();
        let mut remove = None;
        let mut applied = false;
        let mut draft = self.drafts.get_mut(&layer);
        let mut edit = None;
        for (index, rule) in template.rules.iter().enumerate() {
            ui.push_id(("instruction", layer, index), |ui| {
                let current = active.get(&rule.number) == Some(&index);
                let mut frame = egui::Frame::new().inner_margin(4.0);
                if current {
                    let tokens = Tokens::get();
                    frame = frame.fill(tokens.list_row).stroke(Stroke::new(1.0, tokens.accent)).corner_radius(4.0);
                }
                frame.show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    if let Some(draft) = draft.as_deref_mut().filter(|draft| draft.editing == Some(index)) {
                        if current {
                            ui.strong(t("Active at this station"));
                        }
                        applied = instruction_row(ui, draft, &earlier, design.range, &self.station, selection, &design.references);
                    } else {
                        ui.horizontal_wrapped(|ui| {
                            let range = rule.stations.map_or_else(|| t("Whole Road").to_owned(), |[from, to]| format!("{from:.3} – {to:.3}"));
                            ui.label(instruction_label(rule, &range));
                            if current {
                                ui.strong(t("Active at this station"));
                            }
                            if draft.is_none() {
                                if choose_button(ui, "Edit Instruction") {
                                    edit = Some(Draft::from_rule(index, rule));
                                } else if choose_button(ui, "Remove Instruction") {
                                    remove = Some(index);
                                }
                            }
                        });
                    }
                });
                ui.separator();
            });
        }
        if let Some(draft) = draft.filter(|draft| draft.editing.is_none()) {
            applied = ui
                .push_id(("new-instruction", layer), |ui| {
                    instruction_row(ui, draft, &earlier, design.range, &self.station, selection, &design.references)
                })
                .inner;
        }
        if let Some(index) = remove {
            template_mut(design, layer).rules.remove(index);
            *message = None;
        }
        if applied {
            self.apply_instruction(session, design, message);
        }
        if let Some(draft) = edit {
            self.drafts.insert(layer, draft);
        }
        if self.is_editing() {
            if choose_button(ui, "Cancel Instruction") {
                self.drafts.remove(&layer);
                *message = None;
            }
        } else if choose_button(ui, "Add Instruction") {
            let next = next_line_number(design);
            self.drafts.insert(
                layer,
                Draft {
                    number: next.to_string(),
                    source: design.centerline_number.to_string(),
                    source_layer: (layer > 0).then(|| "Top".into()),
                    all: true,
                    focus_number: true,
                    ..Default::default()
                },
            );
            *message = None;
        }
    }

    fn apply_instruction(&mut self, session: &mut cadcraft_engine::Session, design: &mut Design, message: &mut Option<String>) {
        let layer = self.layer;
        let Some(draft) = self.drafts.get(&layer) else { return };
        let Some(rule) = draft.read().filter(|rule| {
            rule.number > 0
                && (layer > 0 || rule.number != design.centerline_number)
                && (rule.from_layer.is_some() || rule.number != rule.from)
                && !design.references.iter().any(|reference| reference.number == rule.number)
                && rule.crossfall.iter().all(|grade| grade.abs() <= 100.0)
                && rule.stations.is_none_or(|[a, b]| a < b && a >= design.range[0] && b <= design.range[1])
        }) else {
            let label = if draft.extension.is_some() {
                "Check the extension points, grade and station range."
            } else {
                "Check the line numbers, dimensions and station range."
            };
            *message = Some(t(label).into());
            return;
        };
        if rule.extension.is_some() || rule.from_reference.is_some() || !rule.edges.is_empty() {
            let mut preview = design.clone();
            save_rule(template_mut(&mut preview, layer), draft.editing, rule.clone());
            let [from, to] = rule.stations.unwrap_or(design.range);
            let station = parse_station(&self.station).unwrap_or(from).clamp(from, to);
            if let Err(error) = session.execute("tin.road.section", &json!({"design":preview, "station":station})) {
                *message = Some(error.to_string());
                return;
            }
        }
        if let Some([from, _]) = rule.stations {
            self.station = from.to_string();
        }
        save_rule(template_mut(design, layer), draft.editing, rule);
        self.drafts.remove(&layer);
        *message = None;
    }
}

fn navigation_button(ui: &mut egui::Ui, icon: Icon, name: &'static str, enabled: bool) -> bool {
    ui.add_enabled_ui(enabled, |ui| icons::button(ui, icon, 24.0, name, false)).inner.clicked()
}

fn template_mut(design: &mut Design, layer: usize) -> &mut Template {
    if layer == 0 { &mut design.top } else { &mut design.layers[layer - 1].template }
}

fn design_rules(design: &Design) -> impl Iterator<Item = &SectionRule> {
    std::iter::once(&design.top).chain(design.layers.iter().map(|layer| &layer.template)).flat_map(|template| &template.rules)
}

fn next_line_number(design: &Design) -> u32 {
    let reserved = design.centerline_number + if design.top.inherit { 2 } else { 0 };
    design_rules(design)
        .map(|rule| rule.number)
        .chain(design.references.iter().map(|reference| reference.number))
        .fold(reserved, u32::max)
        .saturating_add(1)
}

fn save_rule(template: &mut Template, editing: Option<usize>, rule: SectionRule) {
    if let Some(index) = editing {
        template.rules.remove(index);
    }
    template.rules.push(rule);
}

fn instruction_label(rule: &SectionRule, range: &str) -> String {
    let n = rule.number;
    let label = if rule.helper { crate::tf!("Helper Point {n}", n = n) } else { crate::tf!("Line {n}", n = n) };
    if let Some(extension) = &rule.extension {
        if let (Some(layer), Some(side)) = (&extension.target_layer, extension.target_side) {
            let grade = extension.grade.unwrap_or_default();
            let side = side_label(side);
            return crate::tf!(
                "{label} · Extend {from} to {layer} at {grade}% · {side} · {range}",
                label = label,
                from = rule.from,
                layer = layer_name(layer),
                grade = grade,
                side = side,
                range = range
            );
        }
        let [first, last] = extension.target.unwrap_or_default();
        if let Some(through) = extension.through {
            crate::tf!(
                "{label} · Extend {from} through {through} to {first}–{last} · {range}",
                label = label,
                from = rule.from,
                through = through,
                first = first,
                last = last,
                range = range
            )
        } else {
            let grade = extension.grade.unwrap_or_default();
            crate::tf!(
                "{label} · Extend {from} to {first}–{last} at {grade}% · {range}",
                label = label,
                from = rule.from,
                first = first,
                last = last,
                grade = grade,
                range = range
            )
        }
    } else if let Some(point) = rule.from_reference {
        let point = references::part_label(point);
        crate::tf!("{label} from reference {from} · {point} · {range}", label = label, from = rule.from, point = point, range = range)
    } else {
        crate::tf!("{label} from {from} · {range}", label = label, from = rule.from, range = range)
    }
}

fn instruction_row(
    ui: &mut egui::Ui,
    draft: &mut Draft,
    earlier: &[&str],
    range: [f64; 2],
    station: &str,
    selection: &[String],
    lines: &[ReferenceLine],
) -> bool {
    ui.horizontal_wrapped(|ui| {
        let number = field(ui, "Line Number", &mut draft.number);
        if std::mem::take(&mut draft.focus_number) {
            number.request_focus();
        }
        let label = t("Helper Point");
        let helper = ui
            .checkbox(&mut draft.helper, label)
            .on_hover_text(t("Keep this point available to other instructions and leave it out of the surface."));
        record_widget(ui, &helper, label, "checkbox", Some(draft.helper));
        let mut extend = draft.extension.is_some();
        let label = t("Extend to Segment");
        let mode = ui.checkbox(&mut extend, label);
        record_widget(ui, &mode, label, "checkbox", Some(extend));
        if mode.changed() {
            draft.extension = extend.then(|| ExtensionDraft { target_layer: earlier.first().map(|name| (*name).to_owned()), ..Default::default() });
            if extend {
                draft.source.clear();
                draft.source_layer = None;
                draft.from_reference = None;
            }
        }
    });
    if draft.extension.is_some() {
        extension_fields(ui, draft, earlier, lines);
    } else {
        offset_fields(ui, draft, earlier, selection, lines);
    }
    ui.horizontal_wrapped(|ui| {
        let all = ui.checkbox(&mut draft.all, t("Whole Road"));
        record_widget(ui, &all, t("Whole Road"), "checkbox", Some(draft.all));
        if !draft.all {
            if draft.start.is_empty() {
                draft.start = parse_station(station).unwrap_or(range[0]).to_string();
            }
            if draft.end.is_empty() {
                draft.end = range[1].to_string();
            }
            field(ui, "From Station", &mut draft.start);
            field(ui, "To Station", &mut draft.end);
        }
    });
    choose_button(ui, "Apply Section Instruction")
}

fn extension_fields(ui: &mut egui::Ui, draft: &mut Draft, earlier: &[&str], lines: &[ReferenceLine]) {
    let Some(extension) = &mut draft.extension else { return };
    ui.horizontal_wrapped(|ui| {
        let label = t("Follow Source Segment");
        let method = ui.checkbox(&mut extension.follow_segment, label);
        record_widget(ui, &method, label, "checkbox", Some(extension.follow_segment));
        if method.changed() {
            extension.pick = extension.follow_segment.then_some(SegmentPick::Source);
            extension.helper_pick = None;
            extension.error = None;
            if extension.follow_segment {
                draft.from_reference = None;
                extension.whole_layer = false;
            }
        }
        if !extension.follow_segment {
            field(ui, "Extension Grade (%)", &mut extension.grade)
                .on_hover_text(t("Positive rises away from the source. Negative falls away from it."));
        }
    });
    if !extension.follow_segment {
        source_fields(ui, draft, earlier, lines);
    }
    let Some(extension) = &mut draft.extension else { return };
    if !extension.follow_segment && !earlier.is_empty() {
        let label = t("Target Whole Layer");
        let mode = ui.checkbox(&mut extension.whole_layer, label);
        record_widget(ui, &mode, label, "checkbox", Some(extension.whole_layer));
        if mode.changed() {
            extension.pick = None;
            extension.helper_pick = None;
            extension.error = None;
            if extension.whole_layer && extension.target_layer.is_none() {
                extension.target_layer = earlier.last().map(|name| (*name).to_owned());
            }
        }
    }
    if extension.whole_layer {
        ui.horizontal_wrapped(|ui| {
            extension_layer_fields(ui, extension, earlier);
        });
        return;
    }
    ui.horizontal_wrapped(|ui| {
        if let Some(helper) = extension.helper_pick {
            if choose_button(ui, "Stop Picking Helper Points") {
                extension.helper_pick = None;
                extension.error = None;
            } else {
                let message = match helper {
                    HelperPick::First => "Click the first helper point number in the cross section.",
                    HelperPick::Second { .. } => "Click the second helper point number on the same layer.",
                };
                let status = ui.label(t(extension.error.unwrap_or(message)));
                ui.ctx().accesskit_node_builder(status.id, |node| node.set_live(egui::accesskit::Live::Polite));
            }
            return;
        }
        let can_pick_helpers = if extension.follow_segment {
            matches!(extension.pick, Some(SegmentPick::Target(_)))
        } else {
            draft.source.trim().parse::<u32>().is_ok()
        };
        if can_pick_helpers && choose_button(ui, "Pick Two Helper Points") {
            extension.helper_pick = Some(HelperPick::First);
            if !extension.follow_segment {
                extension.pick = Some(SegmentPick::TargetFromPoint);
            }
            extension.error = None;
            return;
        }
        if let Some(pick) = extension.pick {
            if choose_button(ui, "Stop Picking") {
                extension.pick = None;
                extension.error = None;
            } else {
                let message = match pick {
                    SegmentPick::Source => t("Click the segment to extend.").to_owned(),
                    SegmentPick::Target(source) => {
                        let [first, last] = source.numbers;
                        crate::tf!("Click the target segment for {first}–{last}.", first = first, last = last)
                    }
                    SegmentPick::TargetFromPoint => t("Click the target segment.").to_owned(),
                };
                let status = ui.label(extension.error.map_or(message.as_str(), t));
                ui.ctx().accesskit_node_builder(status.id, |node| node.set_live(egui::accesskit::Live::Polite));
            }
        } else if extension.follow_segment {
            if choose_button(ui, "Pick Segments") {
                extension.pick = Some(SegmentPick::Source);
                extension.error = None;
            }
        } else if !draft.source.trim().is_empty() {
            if choose_button(ui, "Pick Target Segment") {
                extension.pick = Some(SegmentPick::TargetFromPoint);
            }
        } else {
            ui.label(t("Click a starting point in the cross section."));
        }
        if extension.pick.is_none() && extension.target.iter().all(|number| !number.is_empty()) {
            ui.label(crate::tf!(
                "Target {layer} · {first}–{last}",
                layer = extension.target_layer.as_deref().map_or(t("This Layer"), layer_name),
                first = extension.target[0],
                last = extension.target[1]
            ));
        }
    });
    ui.collapsing(t("Point Numbers"), |ui| {
        if extension.follow_segment {
            ui.horizontal_wrapped(|ui| {
                layer_field(ui, "Direction Layer", &mut draft.source_layer, earlier);
                field(ui, EXTENSION_POINTS[0], &mut draft.source);
                field(ui, EXTENSION_POINTS[1], &mut extension.through);
            });
        }
        ui.horizontal_wrapped(|ui| {
            layer_field(ui, "Target Layer", &mut extension.target_layer, earlier);
            field(ui, EXTENSION_POINTS[2], &mut extension.target[0]);
            field(ui, EXTENSION_POINTS[3], &mut extension.target[1]);
        });
    });
}

fn side_label(side: ExtensionSide) -> &'static str {
    match side {
        ExtensionSide::Left => t("Left"),
        ExtensionSide::Right => t("Right"),
    }
}

fn extension_layer_fields(ui: &mut egui::Ui, extension: &mut ExtensionDraft, earlier: &[&str]) {
    let caption = ui.label(t("Target Layer"));
    let layer = craft_ui::ComboBox::from_id_salt("extension-target-layer")
        .selected_text(extension.target_layer.as_deref().map_or(t("Choose a layer"), layer_name))
        .show_ui(ui, |ui| {
            for &name in earlier {
                ui.selectable_value(&mut extension.target_layer, Some(name.to_owned()), layer_name(name));
            }
        })
        .response
        .labelled_by(caption.id);
    record_widget(ui, &layer, t("Target Layer"), "combobox", None);
    let caption = ui.label(t("Extension Side"));
    let side = craft_ui::ComboBox::from_id_salt("extension-side")
        .selected_text(extension.target_side.map_or(t("Choose a side"), side_label))
        .show_ui(ui, |ui| {
            for side in [ExtensionSide::Left, ExtensionSide::Right] {
                ui.selectable_value(&mut extension.target_side, Some(side), side_label(side));
            }
        })
        .response
        .labelled_by(caption.id);
    record_widget(ui, &side, t("Extension Side"), "combobox", None);
}

fn offset_fields(ui: &mut egui::Ui, draft: &mut Draft, earlier: &[&str], selection: &[String], lines: &[ReferenceLine]) {
    source_fields(ui, draft, earlier, lines);
    ui.horizontal_wrapped(|ui| {
        if draft.edges.is_empty() {
            field(ui, "Offset (− Left, + Right)", &mut draft.offset);
        } else {
            field(ui, "Edge Side (Optional)", &mut draft.offset)
                .on_hover_text(t("Leave blank to use the selected edge's side. Negative is left and positive is right."));
        }
        field(ui, "Height Difference", &mut draft.height);
        field(ui, "Start Crossfall (%)", &mut draft.start_grade);
        field(ui, "End Crossfall (%)", &mut draft.end_grade);
    });
    ui.horizontal_wrapped(|ui| {
        let label = t("Use Selected Edge Lines");
        let button = ui.add_enabled(!selection.is_empty(), egui::Button::new(label));
        record_widget(ui, &button, label, "button", None);
        if button.clicked() {
            draft.edges = selection.to_vec();
        }
        if !draft.edges.is_empty() {
            ui.label(crate::tf!("{n} edge objects", n = draft.edges.len()));
            if choose_button(ui, "Use Fixed Offset") {
                draft.edges.clear();
            }
        }
    });
}

fn source_fields(ui: &mut egui::Ui, draft: &mut Draft, earlier: &[&str], lines: &[ReferenceLine]) {
    ui.horizontal_wrapped(|ui| {
        if let Some(line) = lines.first() {
            let mut from_reference = draft.from_reference.is_some();
            let label = t("Use Reference Point");
            let button = ui.checkbox(&mut from_reference, label);
            record_widget(ui, &button, label, "checkbox", Some(from_reference));
            if button.changed() {
                if from_reference {
                    draft.pick_reference(line.number, ReferencePoint::Line);
                } else {
                    draft.from_reference = None;
                    draft.source.clear();
                }
            }
        }
        if let Some(part) = &mut draft.from_reference {
            references::source(ui, &mut draft.source, part, lines);
        } else {
            field(ui, "From Point", &mut draft.source);
            layer_field(ui, "From Layer", &mut draft.source_layer, earlier);
        }
    });
}

fn layer_field(ui: &mut egui::Ui, name: &'static str, layer: &mut Option<String>, earlier: &[&str]) {
    if earlier.is_empty() {
        return;
    }
    let label = t(name);
    let caption = ui.label(label);
    let response = craft_ui::ComboBox::from_id_salt(name)
        .selected_text(layer.as_deref().map_or(t("This Layer"), layer_name))
        .show_ui(ui, |ui| {
            ui.selectable_value(layer, None, t("This Layer"));
            for &name in earlier {
                ui.selectable_value(layer, Some(name.to_owned()), layer_name(name));
            }
        })
        .response
        .labelled_by(caption.id);
    record_widget(ui, &response, label, "combobox", None);
}

fn layer_name(name: &str) -> &str {
    if name == "Top" { t("Top") } else { name }
}

fn field(ui: &mut egui::Ui, name: &'static str, value: &mut String) -> egui::Response {
    let caption = ui.label(t(name));
    let field = ui.add(egui::TextEdit::singleline(value).desired_width(74.0)).labelled_by(caption.id);
    record_widget(ui, &field, t(name), "textbox", None);
    field
}

fn decimal(text: &str) -> Option<f64> {
    text.trim().replace(',', ".").parse::<f64>().ok().filter(|v| v.is_finite())
}

fn optional_decimal(text: &str, default: f64) -> Option<f64> {
    if text.trim().is_empty() { Some(default) } else { decimal(text) }
}

fn parse_station(text: &str) -> Option<f64> {
    if let Some((km, metres)) = text.split_once('+') {
        Some(decimal(km)? * 1000.0 + decimal(metres)?).filter(|v| v.is_finite())
    } else {
        decimal(text)
    }
}

fn graph(
    ui: &mut egui::Ui,
    layers: &[SectionLayer],
    references: &[SectionReference],
    active: usize,
    mut draft: Option<&mut Draft>,
    view: &mut SectionView,
) {
    let tokens = Tokens::get();
    let pick = draft.as_deref().and_then(Draft::segment_pick);
    let picking_helpers = draft.as_deref().and_then(Draft::helper_pick).is_some();
    let segment_pick = pick.filter(|_| !picking_helpers);
    let sense = if segment_pick.is_some() { egui::Sense::click_and_drag() } else { egui::Sense::drag() };
    let available = ui.available_height();
    let height = if available.is_finite() { (available * 0.55).max(96.0) } else { 162.0 };
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width().max(230.0), height), sense);
    let points = || layers.iter().flat_map(|l| &l.nodes);
    let inner = rect.shrink2(vec2(29.0, 26.0));
    view.pointer(ui, &response, inner);
    let ref_bounds = references.iter().flat_map(|reference| {
        reference.circle().map_or([reference.line; 2], |(center, radius)| {
            let delta = Vec2::new(radius, radius);
            [center - delta, center + delta]
        })
    });
    let bounds = cadcraft_geom::Bounds2::from_points(points().map(|node| Vec2::new(node.offset, node.elevation)).chain(ref_bounds));
    let center = bounds.center();
    let mut span_x = bounds.width().max(0.01);
    let mut span_y = bounds.height().max(span_x * 0.07);
    if references.iter().any(|reference| reference.pipe.is_some()) {
        // A uniform section scale keeps the pipe and its four points aligned.
        let scale = (span_x / f64::from(inner.width())).max(span_y / f64::from(inner.height()));
        span_x = scale * f64::from(inner.width());
        span_y = scale * f64::from(inner.height());
    }
    let project = |x: f64, z: f64| -> Pos2 { view.project(vec2(((x - center.x) / span_x) as f32, -((z - center.y) / span_y) as f32), inner) };
    let segments = Segment::collect(layers, project);
    let painter = ui.painter_at(rect);
    let mut description = String::new();
    for layer in layers {
        if !layer.nodes.is_empty() {
            let layer_title = layer_name(&layer.name);
            let numbers = layer.nodes.iter().map(|node| node.number.to_string()).collect::<Vec<_>>().join(", ");
            description.push_str(&crate::tf!("{name} · Points {numbers}. ", name = layer_title, numbers = numbers));
        }
    }
    for segment in &segments {
        description.push_str(&segment.description(layers));
        description.push_str(". ");
    }
    for &reference in references {
        description.push_str(&references::describe(reference));
        description.push_str(". ");
    }
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Image, true, format!("{} {description}", t("Cross Section"))));
    painter.rect_filled(rect, 4.0, tokens.canvas);
    painter.rect_stroke(rect, 4.0, Stroke::new(1.0, tokens.border), egui::StrokeKind::Inside);
    if points().count() > 1 || !references.is_empty() {
        let origin = project(0.0, center.y).x;
        if inner.x_range().contains(origin) {
            painter.line_segment([pos2(origin, inner.top()), pos2(origin, inner.bottom())], Stroke::new(1.0, tokens.grid_major));
        }
        painter.text(rect.left_bottom() + vec2(8.0, -6.0), Align2::LEFT_BOTTOM, t("Left"), crate::theme::small(), tokens.text);
        painter.text(rect.right_bottom() + vec2(-8.0, -6.0), Align2::RIGHT_BOTTOM, t("Right"), crate::theme::small(), tokens.text);
    }
    let hovered = segment_pick.and_then(|pick| {
        response
            .hover_pos()
            .or_else(|| response.interact_pointer_pos().filter(|&at| rect.contains(at)))
            .and_then(|at| Segment::nearest(&segments, at, pick, active))
    });
    let saved_target = draft.as_deref().filter(|_| pick.is_none()).and_then(|draft| draft.target_segment(layers, active, project));
    if let Some(target) = saved_target.filter(|target| target.uses_helper()) {
        target.paint_helper_target(&painter);
    }
    paint_segments(
        &painter,
        &segments,
        active,
        draft.as_deref().and_then(|draft| draft.selected_segments(&segments, layers, active, saved_target)),
        pick,
        hovered,
        project,
    );
    if let Some(draft) = draft.as_deref()
        && let Some(target) = hovered.or(saved_target)
        && let Some((target, source, hit)) = draft.grade_guide(layers, active, references, target)
    {
        if !target.uses_helper() {
            target.paint(&painter, active, true, false);
        }
        segments::paint_extension(&painter, project(source.x, source.y), project(hit.x, hit.y));
    }
    if let Some((source, hit)) = draft.as_deref().and_then(|draft| draft.layer_guide(layers, active, references, &segments)) {
        segments::paint_extension(&painter, project(source.x, source.y), project(hit.x, hit.y));
    }
    Segment::paint_grades(&painter, rect.shrink(5.0), &segments, active);
    if let Some(pick) = segment_pick
        && let Some(draft) = draft.as_deref_mut()
    {
        let chosen = Segment::choice(ui, layers, &segments, pick);
        if let Some(segment) = chosen.or(hovered.filter(|_| response.clicked())) {
            draft.pick_segment(segment, &segments, layers, active);
        }
    }
    let point = paint_points(ui, &painter, layers, active, draft.as_deref(), project);
    let reference = references::paint(
        ui,
        &painter,
        references,
        draft.as_deref().is_some_and(Draft::points_are_selectable),
        draft.as_deref().and_then(Draft::selected_reference),
        project,
    );
    if let Some(draft) = draft {
        if let Some((number, part)) = reference {
            draft.pick_reference(number, part);
        } else if let Some((layer, number)) = point {
            if picking_helpers {
                draft.pick_helper((layer, number), layers, &segments, active, project);
            } else {
                draft.pick_point((layer != active).then(|| layers[layer].name.as_str()), number);
            }
        }
    }
}

fn paint_segments(
    painter: &egui::Painter,
    segments: &[Segment],
    active: usize,
    selected: Option<[Segment; 2]>,
    pick: Option<SegmentPick>,
    hovered: Option<Segment>,
    project: impl Fn(f64, f64) -> Pos2,
) {
    let source_id = match pick {
        Some(SegmentPick::Target(source)) => Some(source),
        _ => None,
    };
    for &segment in segments {
        let selected = source_id == Some(segment.id)
            || (pick.is_none()
                && selected.is_some_and(|pair| pair.iter().any(|item| item.id.layer == segment.id.layer && item.id.matches(segment.id.numbers))));
        segment.paint(painter, active, selected, hovered.is_some_and(|hover| hover.id == segment.id));
    }
    if let Some(source) = source_id.and_then(|id| segments.iter().copied().find(|segment| segment.id == id))
        && let Some(target) = hovered
        && let Some((direction, hit)) = source.continuation_to(target)
    {
        source.oriented(direction[0]).paint_continuation(painter, hit, project);
    } else if pick.is_none()
        && let Some([source, target]) = selected
        && let Some(hit) = source.hit(target)
    {
        source.paint_continuation(painter, hit, project);
    }
}

fn paint_points(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    layers: &[SectionLayer],
    active: usize,
    draft: Option<&Draft>,
    project: impl Fn(f64, f64) -> Pos2,
) -> Option<(usize, u32)> {
    let tokens = Tokens::get();
    let rect = painter.clip_rect();
    let mut clicked = None;
    for (index, layer) in layers.iter().enumerate() {
        let color = if index == active { tokens.accent } else { tokens.text_dim };
        for (node_index, node) in layer.nodes.iter().enumerate() {
            let at = project(node.offset, node.elevation);
            if !rect.contains(at) {
                continue;
            }
            if node.helper {
                let corners = [vec2(-4.5, 0.0), vec2(0.0, -4.5), vec2(4.5, 0.0), vec2(0.0, 4.5)].map(|offset| at + offset);
                painter.add(egui::Shape::closed_line(corners.to_vec(), Stroke::new(1.5, tokens.icon_point)));
            } else {
                painter.circle_filled(at, 3.4, color);
            }
            let offset = if index == active && active > 0 { 15.0 } else { -15.0 };
            let label = node.number.to_string();
            let width = 24.0_f32.max(label.len() as f32 * 7.0 + 12.0);
            let center = rect.shrink2(vec2(width * 0.5, 12.0)).clamp(at + vec2(0.0, offset));
            let selected = draft.is_some_and(|draft| {
                if let Some(helper) = draft.helper_pick() {
                    matches!(helper, HelperPick::Second { layer, number } if (layer, number) == (index, node.number))
                } else {
                    draft.references((index != active).then_some(layer.name.as_str()), node.number)
                }
            });
            let selectable = draft.is_some_and(|draft| draft.points_are_selectable() || (node.helper && draft.helper_pick().is_some()));
            if !selectable {
                if selected {
                    painter.circle_stroke(at, 6.0, Stroke::new(1.5, tokens.hover));
                }
                painter.text(center, Align2::CENTER_CENTER, label, crate::theme::small(), tokens.text);
                continue;
            }
            let tooltip = crate::tf!(
                "{name} · {kind} {n} · Offset {offset} · Elevation {height}",
                name = layer_name(&layer.name),
                kind = if node.helper { t("Helper Point") } else { t("Point") },
                n = node.number,
                offset = format!("{:.3}", node.offset),
                height = format!("{:.3}", node.elevation)
            );
            let marker = Rect::from_center_size(center, vec2(width, 24.0));
            let button = ui
                .push_id(("section-point", index, node_index), |ui| {
                    ui.put(marker, egui::Button::new(&label).wrap_mode(egui::TextWrapMode::Extend).selected(selected))
                })
                .inner;
            button.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, button.enabled(), selected, format!("{label} · {tooltip}")));
            record_widget(ui, &button, &tooltip, "button", Some(selected));
            if button.on_hover_text(tooltip).clicked() {
                clicked = Some((index, node.number));
            }
        }
    }
    clicked
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::{
        Harness,
        kittest::{NodeT, Queryable},
    };

    fn road_harness() -> Harness<'static, CadApp> {
        let xml = br#"<LandXML><Alignments><Alignment name="Test" staStart="1000"><CoordGeom>
          <Line><Start>0 0 10</Start><End>0 100 11</End></Line>
        </CoordGeom></Alignment></Alignments></LandXML>"#;
        let mut session = cadcraft_engine::Session::default();
        session.open_drawing(cadcraft_io::read(xml, "road.landxml").unwrap(), "Road", None);
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1300.0, 1040.0))
            .build_ui_state(|ui, app: &mut CadApp| crate::dialogs::show(app, ui.ctx()), CadApp::new(session, Default::default()));
        h.state_mut().start(crate::road_design::COMMAND);
        h.run_steps(4);
        h
    }

    #[test]
    fn dragging_the_road_window_corner_enlarges_the_cross_section() {
        let mut h = road_harness();
        let window = h.get_by_role_and_label(egui::Role::Window, "Road Design").rect();
        let section = h.get_by_role(egui::Role::Image).rect();
        let from = window.right_bottom() - vec2(2.0, 2.0);
        let to = from + vec2(130.0, 160.0);
        let button =
            |pos, pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        h.event(egui::Event::PointerMoved(from));
        h.run_steps(2);
        h.event(button(from, true));
        h.run_steps(2);
        for step in 1..=8 {
            h.event(egui::Event::PointerMoved(from.lerp(to, step as f32 / 8.0)));
            h.run_steps(1);
        }
        h.event(button(to, false));
        h.run_steps(4);
        let resized = h.get_by_role(egui::Role::Image).rect();
        assert!(resized.width() > section.width() + 40.0, "{section:?} → {resized:?}");
        assert!(resized.height() > section.height() + 40.0, "{section:?} → {resized:?}");
    }

    #[test]
    fn road_mesh_and_station_marker_are_drawn_in_model_coordinates() {
        let mut h = road_harness();
        let app = h.state_mut();
        let design = app.road_design.design.as_mut().unwrap();
        design.top.rules = serde_json::from_value(json!([{"number":2,"from":1,"offset":-4}])).unwrap();
        let revision = app.session.state().unwrap().revision;
        let context = egui::Context::default();
        let paint = |app: &CadApp| {
            let rect = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 400.0));
            let mut output = context.run_ui(egui::RawInput { screen_rect: Some(rect), ..Default::default() }, |ui| {
                let painter = ui.ctx().layer_painter(egui::LayerId::background());
                crate::road_design::paint_preview(app, &painter, |point| Some(pos2(100.0 + point.x as f32 * 5.0, 200.0 - point.y as f32 * 5.0)));
            });
            output.textures_delta.clear();
            output.shapes
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let shapes = loop {
            h.run_steps(1);
            let shapes = paint(h.state());
            if shapes.iter().any(|shape| matches!(shape.shape, egui::Shape::Mesh(_))) {
                break shapes;
            }
            assert!(std::time::Instant::now() < deadline, "the model preview should contain triangles");
            std::thread::yield_now();
        };
        let marker = |shapes: &[egui::epaint::ClippedShape]| {
            shapes
                .iter()
                .find_map(|shape| match shape.shape {
                    egui::Shape::LineSegment { points, stroke } if stroke.color == Tokens::get().hover => Some(points),
                    _ => None,
                })
                .unwrap()
        };
        let before = marker(&shapes);
        assert!((before[0].x - before[1].x).abs() < f32::EPSILON);
        assert!((before[0].y - before[1].y).abs() > 10.0);
        h.get_by_label("Next Section").click_accesskit();
        h.run_steps(3);
        let after = marker(&paint(h.state()));
        assert!(after[0].x > before[0].x);
        assert_eq!(h.state().session.state().unwrap().revision, revision);
    }

    #[test]
    fn saves_and_loads_applied_instructions_in_the_road_dialog_before_building() {
        let mut h = road_harness();
        let path = std::env::temp_dir().join(format!("cadcraft-road-dialog-{}.road.json", std::process::id()));
        let app = h.state_mut();
        let picked = path.to_string_lossy().into_owned();
        let destination = picked.clone();
        app.services.pick_save = Some(Box::new(move |_| Some(destination.clone())));
        app.services.pick_road_instructions = Some(Box::new(move || Some(picked.clone())));
        let design = app.road_design.design.as_mut().unwrap();
        design.top.rules = serde_json::from_value(json!([{"number":2,"from":1,"offset":-4}])).unwrap();
        h.run_steps(3);
        h.get_by_label("Save Instructions…").click_accesskit();
        h.run_steps(3);
        assert!(h.query_by_label("Road instructions saved.").is_some());
        h.state_mut().road_design.design.as_mut().unwrap().top.rules.clear();
        h.get_by_label("Load Instructions…").click_accesskit();
        h.run_steps(3);
        std::fs::remove_file(path).unwrap();
        assert!(h.query_by_label("Road instructions loaded.").is_some());
        let design = h.state().road_design.design.as_ref().unwrap();
        assert_eq!(design.top.rules[0].number, 2);
    }

    #[test]
    fn zooms_the_section_with_the_wheel_and_fits_it_again() {
        let mut h = road_harness();
        let chart = h.get_by_role(egui::Role::Image).rect();
        h.event(egui::Event::PointerMoved(chart.center()));
        h.run_steps(2);
        h.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: vec2(0.0, 120.0),
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Unknown,
            modifiers: egui::Modifiers::NONE,
        });
        h.run_steps(12);
        assert!(h.state().road_design.section.view.zoom > 1.0);
        h.get_by_label("Fit Cross Section").click_accesskit();
        h.run_steps(16);
        assert_eq!(h.state().road_design.section.view.zoom, 1.0);
    }

    #[test]
    fn adds_a_3d_reference_configures_a_pipe_and_picks_its_invert_for_a_rule() {
        let mut h = road_harness();
        let session = &mut h.state_mut().session;
        let line = session.execute("3dpoly", &json!({"points":[[0,-3,6],[100,-3,7]]})).unwrap();
        session.execute("select", &json!({"handles":[line["handle"]]})).unwrap();
        h.run_steps(3);
        h.get_by_label("Reference Lines").click();
        h.run_steps(6);
        h.get_by_label("Add Selected Reference Lines").click_accesskit();
        h.run_steps(3);
        h.get_by_label("Configure Pipe").click_accesskit();
        h.run_steps(3);
        assert!(h.get_by_label("Pipe Diameter").is_focused());
        h.event(egui::Event::Text("1,2".into()));
        h.run_steps(3);
        h.get_by_role_and_label(egui::Role::ComboBox, "Line Height Represents").click();
        h.run_steps(3);
        h.get_by_label("Pipe Crown").click();
        h.run_steps(3);
        h.get_by_label("Apply Pipe").click_accesskit();
        h.run_steps(3);
        assert!(h.output().shapes.iter().any(|shape| matches!(&shape.shape,
            egui::Shape::Circle(circle) if circle.stroke.color == Tokens::get().icon_point && circle.radius > 0.0)));

        h.get_by_label("Add Instruction").click_accesskit();
        h.run_steps(3);
        h.get_by_label("Reference 2 · Pipe Invert · Offset 3.000 · Elevation 4.800").click_accesskit();
        h.run_steps(3);
        h.get_by_label("Apply Section Instruction").click_accesskit();
        h.run_steps(3);
        let road = &h.state().road_design;
        let rule = &road.design.as_ref().unwrap().top.rules[0];
        assert_eq!((rule.from, rule.from_reference), (2, Some(ReferencePoint::Invert)));
        let section = road.section.cache.as_ref().unwrap().section.as_ref().unwrap();
        let node = section.layers[0].nodes.iter().find(|node| node.number == rule.number).unwrap();
        assert!((node.elevation - 4.8).abs() < 1e-9);
    }

    #[test]
    fn picks_a_direction_and_target_on_different_layers() {
        let mut h = road_harness();
        let road = &mut h.state_mut().road_design;
        let design = road.design.as_mut().unwrap();
        design.top.rules = serde_json::from_value(json!([
            {"number":2,"from":1,"offset":-4},
            {"number":4,"from":2,"offset":-2,"height":-1}
        ]))
        .unwrap();
        design.layers = serde_json::from_value(json!([{"name":"Base","thickness":0,"template":{"inherit":false,"rules":[
            {"number":8,"from":1,"fromLayer":"Top","height":-0.15},
            {"number":9,"from":2,"fromLayer":"Top","height":-0.15}
        ]}}]))
        .unwrap();
        road.section.layer = 1;
        h.run_steps(3);
        h.get_by_label("Add Instruction").click_accesskit();
        h.run_steps(3);
        h.get_by_label("Extend to Segment").click_accesskit();
        h.run_steps(3);
        h.get_by_label("Follow Source Segment").click_accesskit();
        h.run_steps(3);
        let chart = h.get_by_role(egui::Role::Image);
        let node = chart.accesskit_node();
        let description = node.label().unwrap();
        assert!(description.contains("Base · 8 to 9 · 0.0%"));
        assert!(description.contains("Top · 2 to 4 · -50.0%"));
        click_section_segment(&mut h, -2.0, 9.85);
        assert!(h.query_by_role_and_label(egui::Role::ComboBox, "Target Segment").is_some());
        click_section_segment(&mut h, -2.0, 10.0);
        assert!(h.query_by_label("The source does not reach that segment beyond either endpoint. Pick another target.").is_some());
        click_section_segment(&mut h, -5.0, 9.5);
        h.get_by_label("Apply Section Instruction").click_accesskit();
        h.run_steps(3);
        let road = &h.state().road_design;
        let rule = road.design.as_ref().unwrap().layers[0].template.rules.last().unwrap();
        assert_eq!(rule.from, 8);
        let extension = rule.extension.as_ref().unwrap();
        let mut target = extension.target.unwrap();
        target.sort_unstable();
        assert_eq!((extension.through, target, extension.target_layer.as_deref()), (Some(9), [2, 4], Some("Top")));
    }

    #[test]
    fn picks_a_numbered_helper_then_extends_at_a_typed_grade_to_the_target() {
        let mut h = road_harness();
        let road = &mut h.state_mut().road_design;
        let design = road.design.as_mut().unwrap();
        design.top.rules = serde_json::from_value(json!([{"number":2,"from":1,"offset":-4},{"number":4,"from":2,"offset":-2,"height":-1}])).unwrap();
        design.layers = serde_json::from_value(json!([{"name":"Base","thickness":0,"template":{"inherit":false,"rules":[]}}])).unwrap();
        road.section.layer = 1;
        h.run_steps(3);
        h.get_by_label("Add Instruction").click_accesskit();
        h.run_steps(3);
        h.get_by_label("Helper Point").click_accesskit();
        h.state_mut().road_design.section.drafts.get_mut(&1).unwrap().height = "-0,3".into();
        h.run_steps(3);
        h.get_by_label("Apply Section Instruction").click_accesskit();
        h.run_steps(3);
        h.get_by_label("Add Instruction").click_accesskit();
        h.run_steps(3);
        h.get_by_label("Extend to Segment").click_accesskit();
        h.run_steps(3);
        h.get_by_label("Extension Grade (%)").click();
        h.event(egui::Event::Text("-3".into()));
        h.run_steps(3);
        h.get_by_label("5 · Base · Helper Point 5 · Offset 0.000 · Elevation 9.700").click_accesskit();
        h.run_steps(3);
        click_section_segment(&mut h, -5.0, 9.5);
        h.get_by_label("Apply Section Instruction").click_accesskit();
        h.run_steps(3);

        let road = &h.state().road_design;
        let rules = &road.design.as_ref().unwrap().layers[0].template.rules;
        assert!(rules[0].helper);
        let extension = rules[1].extension.as_ref().unwrap();
        assert_eq!((rules[1].from, extension.grade), (5, Some(-3.0)));
        let preview = road.section.cache.as_ref().unwrap().section.as_ref().unwrap();
        let result = preview.layers[1].nodes.iter().find(|node| node.number == rules[1].number).unwrap();
        assert!((result.elevation - (9.7 - 0.03 * result.offset.abs())).abs() < 1e-9);
    }

    #[test]
    fn follows_the_existing_slope_to_the_segment_between_two_helpers() {
        let mut h = road_harness();
        let road = &mut h.state_mut().road_design;
        let design = road.design.as_mut().unwrap();
        design.top.rules = serde_json::from_value(json!([
            {"number":2,"from":1,"offset":-4},
            {"number":4,"from":2,"offset":-2,"height":-1},
            {"number":10,"from":1,"offset":-4.5,"height":-0.1,"helper":true},
            {"number":11,"from":1,"offset":-5.5,"height":-0.9,"helper":true}
        ]))
        .unwrap();
        design.layers = serde_json::from_value(json!([{"name":"Base","thickness":0,"template":{"inherit":false,"rules":[
            {"number":8,"from":1,"fromLayer":"Top","height":-0.15},
            {"number":9,"from":2,"fromLayer":"Top","height":-0.35}
        ]}}]))
        .unwrap();
        road.section.layer = 1;
        h.run_steps(3);
        for label in ["Add Instruction", "Extend to Segment", "Follow Source Segment"] {
            h.get_by_label(label).click_accesskit();
            h.run_steps(3);
        }
        click_section_segment(&mut h, -2.0, 9.75);
        h.get_by_label("Pick Two Helper Points").click_accesskit();
        h.run_steps(3);
        let first = "10 · Top · Helper Point 10 · Offset -4.500 · Elevation 9.900";
        h.get_by_label(first).click_accesskit();
        h.run_steps(3);
        h.get_by_label(first).click_accesskit();
        h.run_steps(3);
        assert!(h.query_by_label("Choose a different helper point on the same layer.").is_some());
        h.get_by_label("11 · Top · Helper Point 11 · Offset -5.500 · Elevation 9.100").click_accesskit();
        h.run_steps(3);
        h.get_by_label("Apply Section Instruction").click_accesskit();
        h.run_steps(3);
        let road = &h.state().road_design;
        let rule = road.design.as_ref().unwrap().layers[0].template.rules.last().unwrap();
        let extension = rule.extension.as_ref().unwrap();
        assert_eq!((rule.from, extension.through, extension.target), (8, Some(9), Some([10, 11])));
        let section = road.section.cache.as_ref().unwrap().section.as_ref().unwrap();
        let node = section.layers[1].nodes.iter().find(|node| node.number == rule.number).unwrap();
        assert!((-5.5..-4.5).contains(&node.offset));
        assert!((node.elevation - (9.85 + node.offset * 0.05)).abs() < 1e-9);
        assert!((node.elevation - (9.9 + (node.offset + 4.5) * 0.8)).abs() < 1e-9);
    }

    fn click_section_segment(h: &mut Harness<'_, CadApp>, offset: f32, elevation: f32) {
        let inner = h.get_by_role(egui::Role::Image).rect().shrink2(vec2(29.0, 26.0));
        let position = inner.center() + vec2((offset + 3.0) / 6.0 * inner.width(), -(elevation - 9.5) * inner.height());
        h.event(egui::Event::PointerMoved(position));
        h.run_steps(2);
        for pressed in [true, false] {
            h.event(egui::Event::PointerButton { pos: position, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE });
            h.run_steps(2);
        }
    }

    fn add_scoped_instruction(h: &mut Harness<'_, CadApp>) {
        h.get_by_label("Add Instruction").click_accesskit();
        h.run_steps(3);
        assert_eq!(h.state().road_design.section.drafts[&0].number, "2");
        h.state_mut().road_design.section.drafts.insert(
            0,
            Draft {
                number: "3".into(),
                source: "1".into(),
                all: false,
                start: "1+020".into(),
                end: "1+080".into(),
                offset: "-0,25".into(),
                height: "0,2".into(),
                start_grade: "0".into(),
                end_grade: "0".into(),
                ..Default::default()
            },
        );
        h.run_steps(3);
        h.get_by_label("Apply Section Instruction").click_accesskit();
        h.run_steps(4);
    }

    #[test]
    fn moves_the_cross_section_and_highlights_the_instruction_at_its_station() {
        let mut h = road_harness();
        let section = &h.state().road_design.section;
        assert_eq!(section.station, "1000");
        let nodes = &section.cache.as_ref().unwrap().section.as_ref().unwrap().layers[0].nodes;
        assert_eq!(nodes.iter().map(|node| node.number).collect::<Vec<_>>(), [1]);
        assert_eq!(h.state().road_design.design.as_ref().unwrap().interval, 1.0);
        assert!(h.query_by_label("From Point").is_none());
        h.get_by_label("Section Position").click();
        h.run_steps(3);
        let at = parse_station(&h.state().road_design.section.station).unwrap();
        assert!(1000.0 < at && at < 1100.0);
        add_scoped_instruction(&mut h);
        let section = &h.state().road_design.section;
        assert_eq!(section.station, "1020");
        let nodes = &section.cache.as_ref().unwrap().section.as_ref().unwrap().layers[0].nodes;
        let node = nodes.iter().find(|n| n.number == 3).unwrap();
        let source = nodes.iter().find(|n| n.number == 1).unwrap();
        assert!((node.offset - source.offset + 0.25).abs() < 1e-9);
        assert!((node.elevation - source.elevation - 0.2).abs() < 1e-9);
        assert!(h.query_by_label("Active at this station").is_some());
        h.get_by_label("Previous Section").click_accesskit();
        h.run_steps(3);
        assert_eq!(h.state().road_design.section.station, "1019");
        assert!(h.query_by_label("Active at this station").is_none());
        h.get_by_label("Next Section").click_accesskit();
        h.run_steps(3);
        assert_eq!(h.state().road_design.section.station, "1020");
        assert!(h.query_by_label("Active at this station").is_some());
    }

    #[test]
    fn switches_layer_instructions_and_opens_the_built_road_in_surface_editor() {
        let mut h = road_harness();
        add_scoped_instruction(&mut h);
        h.get_by_label("Add Layer").click_accesskit();
        h.run_steps(3);
        assert!(h.get_by_label("Layer Name").is_focused());
        h.event(egui::Event::Text("Base".into()));
        h.run_steps(3);
        h.get_by_label("Create Layer").click_accesskit();
        h.run_steps(3);
        assert!(h.query_by_label("Edit Instruction").is_none());
        h.get_by_label("Add Instruction").click_accesskit();
        h.run_steps(3);
        let draft = h.state_mut().road_design.section.drafts.get_mut(&1).unwrap();
        draft.offset = "-0.5".into();
        draft.height = "-0.2".into();
        h.get_by_role_and_label(egui::Role::ComboBox, "Layer").click();
        h.run_steps(3);
        h.get_by_label("Top").click();
        h.run_steps(3);
        assert_eq!(h.query_all_by_label("Edit Instruction").count(), 1);
        assert!(h.query_by_label("Apply Section Instruction").is_none());
        h.get_by_role_and_label(egui::Role::ComboBox, "Layer").click();
        h.run_steps(3);
        h.get_by_label("Base").click();
        h.run_steps(3);
        assert_eq!(h.state().road_design.section.drafts[&1].offset, "-0.5");
        h.get_by_label("Apply Section Instruction").click_accesskit();
        h.run_steps(3);
        h.get_by_label("Add Instruction").click_accesskit();
        h.run_steps(3);
        let draft = h.state_mut().road_design.section.drafts.get_mut(&1).unwrap();
        draft.height = "-0.2".into();
        h.run_steps(3);
        h.get_by_label("Apply Section Instruction").click_accesskit();
        h.run_steps(3);
        assert_eq!(h.query_all_by_label("Edit Instruction").count(), 2);
        h.get_by_label("Add Layer").click_accesskit();
        h.run_steps(3);
        h.event(egui::Event::Text("Future".into()));
        h.run_steps(3);
        h.get_by_label("Create Layer").click_accesskit();
        h.run_steps(3);
        assert!(h.query_by_label("Edit Instruction").is_none());
        h.get_by_label("Build Road Surfaces").click_accesskit();
        h.run_steps(3);
        let app = h.state();
        assert_eq!(app.road_design.message.as_deref(), Some("Road surfaces updated."));
        let doc = app.session.doc().unwrap();
        let strings = doc.groups.iter().find(|group| group.name == "Road - Top - Lines").unwrap();
        let axis = strings.members.iter().filter_map(|&h| doc.model.get(h)).find(|e| cadcraft_doc::road::line_number(&e.common) == Some(1)).unwrap();
        assert!(matches!(&axis.kind, cadcraft_doc::EntityKind::Polyline3d(line)
            if line.points.first().unwrap().x == 0.0 && line.points.last().unwrap().x == 100.0));
        assert!(doc.groups.iter().any(|group| group.name == "Road - Base" && group.is_terrain_surface()));
        assert!(!doc.groups.iter().any(|group| group.name == "Road - Future"));
        h.state_mut().start(crate::surface_editor::COMMAND);
        h.run_steps(3);
        assert!(h.query_by_label("No editable terrain surfaces in this drawing.").is_none());
        h.get_by_label("Open Editable Copy").click_accesskit();
        h.run_steps(3);
        let group = crate::surface_editor::selected_surface(h.state()).unwrap();
        assert!(group.linked_terrain_id().is_some());
        assert!(group.name.ends_with(" - Editable"));
    }
}
