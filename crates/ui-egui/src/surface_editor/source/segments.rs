use cadcraft_engine::snap::SnapHit;
use cadcraft_geom::Vec3;
use egui::{Painter, Pos2, Stroke};
use serde_json::{Value, json};

use super::Curve;
use crate::{
    i18n::t,
    icons::{self, Icon},
    theme::Tokens,
};

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Segment {
    pub handle: String,
    pub index: usize,
}

enum Mode {
    Select(Option<Segment>),
    Insert(Segment),
}

#[derive(Clone, Copy)]
struct Hit {
    point: Vec3,
    fraction: f64,
    distance: f32,
}

pub(super) struct Editor {
    mode: Mode,
    hovered: Option<Segment>,
    placement: Option<(Vec3, Option<SnapHit>)>,
    snap: bool,
}

impl Default for Editor {
    fn default() -> Self {
        Self { mode: Mode::Select(None), hovered: None, placement: None, snap: true }
    }
}

impl Editor {
    pub(super) fn select_vertex(&mut self, curves: &[Curve], handle: &str, vertex: usize) -> bool {
        let Some(curve) = curves.iter().find(|curve| curve.handle == handle && !curve.locked && !curve.segments.is_empty()) else { return false };
        let count = curve.segments.len();
        if !self.selection().is_some_and(|s| {
            s.handle == handle && (s.index == vertex || s.index + 1 == vertex || (curve.closed && vertex == 0 && s.index + 1 == count))
        }) {
            self.mode = Mode::Select(Some(Segment { handle: handle.into(), index: vertex.min(count - 1) }));
        }
        self.clear_preview();
        true
    }

    pub(super) fn inserting(&self) -> bool {
        matches!(self.mode, Mode::Insert(_))
    }

    pub(super) fn snap_enabled(&self) -> bool {
        self.inserting() && self.snap
    }

    pub(super) fn stop(&mut self) {
        self.mode = Mode::Select(None);
        self.clear_preview();
    }

    pub(super) fn clear_preview(&mut self) {
        self.hovered = None;
        self.placement = None;
    }

    pub(super) fn selection(&self) -> Option<&Segment> {
        match &self.mode {
            Mode::Select(Some(s)) | Mode::Insert(s) => Some(s),
            _ => None,
        }
    }

    pub(super) fn validate(&mut self, curves: &[Curve]) {
        if self.selection().is_some_and(|s| segment_path(curves, s).is_none()) {
            self.mode = Mode::Select(None);
        }
    }

    pub(super) fn cancel(&mut self) {
        if let Mode::Insert(selection) = &self.mode {
            self.mode = Mode::Select(Some(selection.clone()));
            self.clear_preview();
        } else {
            self.stop();
        }
    }

    pub(super) fn controls(&mut self, ui: &mut egui::Ui, curves: &[Curve]) {
        if let Some(mut selection) = self.selection().cloned()
            && let Some(curve) = curves.iter().find(|c| c.handle == selection.handle)
        {
            let count = curve.segments.len();
            let mut move_to = None;
            ui.horizontal(|ui| {
                let previous = ui
                    .add_enabled_ui(selection.index > 0 || curve.closed && count > 1, |ui| {
                        icons::button(ui, Icon::ChevronLeft, 26.0, "Previous Segment", false)
                    })
                    .inner;
                ui.label(crate::tf!("Segment {index} of {count}", index = selection.index + 1, count = count));
                let next = ui
                    .add_enabled_ui(selection.index + 1 < count || curve.closed && count > 1, |ui| {
                        icons::button(ui, Icon::ChevronRight, 26.0, "Next Segment", false)
                    })
                    .inner;
                if previous.clicked() {
                    move_to = Some(selection.index.checked_sub(1).unwrap_or(count - 1));
                } else if next.clicked() {
                    move_to = Some((selection.index + 1) % count);
                }
            });
            if let Some(index) = move_to {
                selection.index = index;
                self.mode = if self.inserting() { Mode::Insert(selection.clone()) } else { Mode::Select(Some(selection.clone())) };
                self.clear_preview();
            }
            let label = if self.inserting() { "Cancel Point Placement" } else { "Insert Source Point" };
            let button = ui.button(t(label));
            crate::control::record_widget(ui, &button, t(label), "button", None);
            if button.clicked() {
                self.mode = if self.inserting() { Mode::Select(Some(selection)) } else { Mode::Insert(selection) };
                self.clear_preview();
            }
            if self.inserting() {
                let label = t("Snap to Existing Points");
                let toggle = ui.checkbox(&mut self.snap, label);
                crate::control::record_widget(ui, &toggle, label, "checkbox", Some(self.snap));
                ui.label(t("Click the position of the new point."));
            }
        }
    }

    pub(super) fn accepts_insertion(&self, curves: &[Curve], point: Vec3) -> bool {
        self.selection()
            .and_then(|active| segment_path(curves, active))
            .is_some_and(|path| path.first().zip(path.last()).is_some_and(|(&a, &b)| interior(point, a, b)))
    }

    pub(super) fn input(
        &mut self,
        curves: &[Curve],
        at: Option<Pos2>,
        project: impl Fn(Vec3) -> Option<(Pos2, f64)>,
        position: impl Fn(Pos2, Vec3) -> Option<Vec3>,
        snap: Option<(Vec3, SnapHit)>,
        clicked: bool,
    ) -> Option<Value> {
        self.clear_preview();
        let at = at?;
        match &self.mode {
            Mode::Select(_) => {
                let active = nearest_segment(curves, at, &project);
                self.hovered = active.clone();
                if clicked {
                    self.mode = Mode::Select(active);
                }
                None
            }
            Mode::Insert(active) => {
                let path = segment_path(curves, active)?;
                let (first, last) = path.first().zip(path.last())?;
                let nearest =
                    hit(path, at, &project).unwrap_or(Hit { point: *first + (*last - *first) * 0.5, fraction: 0.5, distance: f32::INFINITY });
                let on_segment = snap.is_none() && nearest.distance <= 0.25 && nearest.fraction > 1e-8 && nearest.fraction < 1.0 - 1e-8;
                let point = snap
                    .map(|(point, _)| point)
                    .or_else(|| if on_segment { Some(nearest.point) } else { position(at, nearest.point) })
                    .filter(|&p| interior(p, *first, *last))?;
                self.placement = Some((point, snap.map(|(_, hit)| hit)));
                if !clicked {
                    return None;
                }
                let mut params = json!({"handle": active.handle, "segment": active.index, "fraction": nearest.fraction});
                if !on_segment {
                    params["position"] = json!([point.x, point.y, point.z]);
                }
                Some(params)
            }
        }
    }

    pub(super) fn finish_insert(&mut self) {
        if let Mode::Insert(s) = &self.mode {
            self.mode = Mode::Select(Some(s.clone()));
        }
        self.clear_preview();
    }

    pub(super) fn paint(
        &self,
        curves: &[Curve],
        painter: &Painter,
        width: f32,
        draw: impl Fn(&[Vec3], Stroke),
        project: &impl Fn(Vec3) -> Option<Pos2>,
    ) {
        let colors = Tokens::get();
        if let Some(s) = &self.hovered
            && let Some(path) = segment_path(curves, s)
        {
            draw(path, Stroke::new(width, colors.hover));
        }
        if let Some(active) = self.selection()
            && let Some(path) = segment_path(curves, active)
        {
            if let Some(curve) = curves.iter().find(|c| c.handle == active.handle) {
                for segment in &curve.segments {
                    draw(segment, Stroke::new(width + 2.5, colors.canvas));
                    draw(segment, Stroke::new(width, colors.snap));
                }
            }
            draw(path, Stroke::new(width + 3.0, colors.canvas));
            draw(path, Stroke::new(width, colors.grip_hot));
            for end in path.first().into_iter().chain(path.last()).filter_map(|&p| project(p)) {
                let marker = egui::Rect::from_center_size(end, egui::vec2(7.0, 7.0));
                painter.rect_filled(marker.expand(1.0), 0.0, colors.canvas);
                painter.rect_filled(marker, 0.0, colors.grip_hot);
            }
            if let Some((point, snapped)) = self.placement
                && let Some(at) = project(point)
            {
                if let Some((&a, &b)) = path.first().zip(path.last()) {
                    super::dashed_guide(painter, &[a, point, b], project, Stroke::new(2.0, colors.snap));
                }
                if let Some(hit) = snapped {
                    crate::canvas::draw_snap_marker(painter, at, &hit);
                } else {
                    painter.circle_stroke(at, 5.0, Stroke::new(2.0, colors.snap));
                }
            }
        }
    }
}

fn interior(point: Vec3, a: Vec3, b: Vec3) -> bool {
    point.xy().dist(a.xy()) > 1e-8 && point.xy().dist(b.xy()) > 1e-8
}

pub(super) fn segment_path<'a>(curves: &'a [Curve], s: &Segment) -> Option<&'a [Vec3]> {
    let curve = curves.iter().find(|c| c.handle == s.handle && !c.locked)?;
    Some(curve.segments.get(s.index)?.as_slice())
}

fn nearest_segment(curves: &[Curve], at: Pos2, project: &impl Fn(Vec3) -> Option<(Pos2, f64)>) -> Option<Segment> {
    curves
        .iter()
        .filter(|curve| !curve.locked)
        .flat_map(|curve| {
            curve.segments.iter().enumerate().filter_map(move |(i, path)| {
                let hit = hit(path, at, project)?;
                (hit.distance <= 9.0).then_some((hit.distance, curve, i))
            })
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, c, index)| Segment { handle: c.handle.clone(), index })
}

fn hit(path: &[Vec3], at: Pos2, project: &impl Fn(Vec3) -> Option<(Pos2, f64)>) -> Option<Hit> {
    path.windows(2)
        .enumerate()
        .filter_map(|(i, edge)| {
            let (a, wa) = project(edge[0])?;
            let (b, wb) = project(edge[1])?;
            let delta = b - a;
            let len_sq = delta.length_sq();
            if len_sq <= f32::EPSILON {
                return None;
            }
            let t = ((at - a).dot(delta) / len_sq).clamp(0.0, 1.0);
            let distance = at.distance(a + delta * t);
            let t = f64::from(t);
            let right = t / wb;
            let u = right / ((1.0 - t) / wa + right);
            let fraction = (i as f64 + u) / (path.len() - 1) as f64;
            Some(Hit { distance, point: edge[0] + (edge[1] - edge[0]) * u, fraction })
        })
        .min_by(|a, b| a.distance.total_cmp(&b.distance))
}
