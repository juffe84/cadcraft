//! Cross-section segments, their slope labels and selection in the section view.
use cadcraft_geom::{
    Bounds2, Vec2,
    terrain::road::{SectionLayer, SectionNode, section_extension_hit},
};
use egui::{Align2, Color32, Painter, Pos2, Rect, Stroke, pos2, vec2};

use crate::{control::record_widget, i18n::t, theme::Tokens};

use super::layer_name;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct SegmentId {
    pub layer: usize,
    pub numbers: [u32; 2],
}

impl SegmentId {
    pub(super) fn matches(self, numbers: [u32; 2]) -> bool {
        self.numbers == numbers || self.numbers == [numbers[1], numbers[0]]
    }
}

#[derive(Clone, Copy)]
pub(super) enum SegmentPick {
    Source,
    Target(SegmentId),
    TargetFromPoint,
}

impl SegmentPick {
    fn accepts(self, segment: Segment) -> bool {
        match self {
            Self::Source | Self::TargetFromPoint => true,
            Self::Target(source) => segment.id != source,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Segment {
    pub id: SegmentId,
    nodes: [SectionNode; 2],
    screen: [Pos2; 2],
}

impl Segment {
    pub(super) fn from_nodes(layer: usize, nodes: [SectionNode; 2], project: impl Fn(f64, f64) -> Pos2) -> Self {
        Self {
            id: SegmentId { layer, numbers: nodes.map(|node| node.number) },
            nodes,
            screen: nodes.map(|node| project(node.offset, node.elevation)),
        }
    }

    pub(super) fn collect(layers: &[SectionLayer], project: impl Fn(f64, f64) -> Pos2) -> Vec<Self> {
        layers
            .iter()
            .enumerate()
            .flat_map(|(layer, section)| {
                let project = &project;
                let surface = section.nodes.iter().copied().filter(|node| !node.helper);
                surface.clone().zip(surface.skip(1)).map(move |(left, right)| Self::from_nodes(layer, [left, right], project))
            })
            .collect()
    }

    pub(super) fn oriented(mut self, first: u32) -> Self {
        if self.id.numbers[1] == first {
            self.id.numbers.reverse();
            self.nodes.reverse();
            self.screen.reverse();
        }
        self
    }

    pub(super) fn points(self) -> [Vec2; 2] {
        self.nodes.map(|node| Vec2::new(node.offset, node.elevation))
    }

    pub(super) fn hit(self, target: Self) -> Option<Vec2> {
        section_extension_hit(self.points(), target.points())
    }

    pub(super) fn hit_at_grade(self, start: Vec2, grade: f64) -> Option<Vec2> {
        cadcraft_geom::terrain::road::section_grade_hit(start, grade, self.points())
    }

    pub(super) fn continuation_to(self, target: Self) -> Option<([u32; 2], Vec2)> {
        let [start, end] = self.points();
        let target = target.points();
        if let Some(hit) = section_extension_hit([start, end], target) {
            return Some((self.id.numbers, hit));
        }
        let [first, last] = self.id.numbers;
        section_extension_hit([end, start], target).map(|hit| ([last, first], hit))
    }

    fn grade(self) -> (String, [u32; 2]) {
        let [mut start, mut end] = self.nodes;
        if start.offset.abs() > end.offset.abs() {
            std::mem::swap(&mut start, &mut end);
        }
        let run = (end.offset - start.offset).abs();
        let rise = end.elevation - start.elevation;
        let label = if run > cadcraft_geom::EPS {
            let percent = 100.0 * rise / run;
            let percent = if percent.abs() < 0.05 { 0.0 } else { percent };
            format!("{percent:.1}%")
        } else if rise.abs() > cadcraft_geom::EPS {
            t("Vertical").to_owned()
        } else {
            t("Coincident").to_owned()
        };
        (label, [start.number, end.number])
    }

    pub(super) fn description(self, layers: &[SectionLayer]) -> String {
        let (grade, [start, end]) = self.grade();
        crate::tf!("{layer} · {start} to {end} · {grade}", layer = layer_name(&layers[self.id.layer].name), start = start, end = end, grade = grade)
    }

    pub(super) fn nearest(segments: &[Self], pointer: Pos2, pick: SegmentPick, active: usize) -> Option<Self> {
        segments
            .iter()
            .copied()
            .filter(|segment| pick.accepts(*segment))
            .filter_map(|segment| {
                let [first, last] = segment.screen;
                let delta = last - first;
                let length_squared = delta.length_sq();
                if length_squared < 1.0 {
                    return None;
                }
                let fraction = ((pointer - first).dot(delta) / length_squared).clamp(0.0, 1.0);
                let distance = pointer.distance(first + delta * fraction);
                let preferred_layer = match pick {
                    SegmentPick::Source => segment.id.layer == active,
                    SegmentPick::Target(_) | SegmentPick::TargetFromPoint => segment.id.layer != active,
                };
                (distance <= 9.0).then_some((distance + if preferred_layer { 0.0 } else { 1.0 }, segment))
            })
            .min_by(|(a, _), (b, _)| a.total_cmp(b))
            .map(|(_, segment)| segment)
    }

    pub(super) fn choice(ui: &mut egui::Ui, layers: &[SectionLayer], segments: &[Self], pick: SegmentPick) -> Option<Self> {
        let caption = match pick {
            SegmentPick::Source => t("Source Segment"),
            SegmentPick::Target(_) | SegmentPick::TargetFromPoint => t("Target Segment"),
        };
        let mut chosen = None;
        ui.horizontal_wrapped(|ui| {
            let label = ui.label(caption);
            let response = craft_ui::ComboBox::from_id_salt("road-section-segment")
                .selected_text(t("Select a segment"))
                .show_ui(ui, |ui| {
                    for segment in segments.iter().copied().filter(|segment| pick.accepts(*segment)) {
                        if ui.selectable_label(false, segment.description(layers)).clicked() {
                            chosen = Some(segment);
                        }
                    }
                })
                .response
                .labelled_by(label.id);
            record_widget(ui, &response, caption, "combobox", None);
        });
        chosen
    }

    pub(super) fn paint(self, painter: &Painter, active: usize, selected: bool, hovered: bool) {
        let tokens = Tokens::get();
        let color = if hovered || selected {
            tokens.hover
        } else if self.id.layer == active {
            tokens.accent
        } else {
            tokens.text_dim
        };
        let width = if hovered || selected {
            4.0
        } else if self.id.layer == active {
            2.4
        } else {
            1.2
        };
        painter.line_segment(self.screen, Stroke::new(width, color));
    }

    pub(super) fn paint_helper_target(self, painter: &Painter) {
        painter.extend(egui::Shape::dashed_line(&self.screen, Stroke::new(2.0, Tokens::get().icon_point), 4.0, 4.0));
    }

    pub(super) fn uses_helper(self) -> bool {
        self.nodes.iter().any(|node| node.helper)
    }

    pub(super) fn paint_grades(painter: &Painter, rect: Rect, segments: &[Self], active: usize) {
        let tokens = Tokens::get();
        let bounds = Bounds2 { min: screen_vec(rect.min), max: screen_vec(rect.max) };
        let mut placed = Vec::new();
        for &segment in segments.iter().rev() {
            let [start, end] = segment.screen.map(screen_vec);
            let Some((left, right)) = cadcraft_render::clip::clip_segment(start, end, &bounds) else { continue };
            let [left, right] = [left, right].map(|point| pos2(point.x as f32, point.y as f32));
            let (text, _) = segment.grade();
            let offset = if segment.id.layer == active && active > 0 { 12.0 } else { -12.0 };
            let galley = painter.layout_no_wrap(text, crate::theme::small(), tokens.text);
            for fraction in [0.5, 0.32, 0.68] {
                let at = left.lerp(right, fraction) + vec2(0.0, offset);
                let label = Align2::CENTER_CENTER.anchor_size(at, galley.size());
                let padded = label.expand2(vec2(3.0, 1.0));
                if rect.contains_rect(padded) && placed.iter().all(|other: &Rect| !other.intersects(padded)) {
                    painter.rect_filled(padded, 2.0, tokens.canvas);
                    painter.galley(label.min, galley, Color32::PLACEHOLDER);
                    placed.push(padded);
                    break;
                }
            }
        }
    }

    pub(super) fn paint_continuation(self, painter: &Painter, hit: Vec2, project: impl Fn(f64, f64) -> Pos2) {
        paint_extension(painter, self.screen[1], project(hit.x, hit.y));
    }
}

pub(super) fn paint_extension(painter: &Painter, start: Pos2, hit: Pos2) {
    let color = Tokens::get().hover;
    painter.extend(egui::Shape::dashed_line(&[start, hit], Stroke::new(2.0, color), 5.0, 3.0));
    painter.circle_stroke(hit, 5.0, Stroke::new(2.0, color));
}

fn screen_vec(point: Pos2) -> Vec2 {
    Vec2::new(f64::from(point.x), f64::from(point.y))
}
