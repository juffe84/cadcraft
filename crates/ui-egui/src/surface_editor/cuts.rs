use cadcraft_geom::Vec3;
use egui::{Painter, Pos2, Stroke};
use serde_json::{Value, json};

use crate::{CadApp, i18n::t, theme::Tokens};

#[derive(Default)]
pub(super) struct State {
    visible: bool,
    selected: Option<String>,
    key: Option<(u64, u64, String, Option<String>)>,
    triangles: Vec<[Vec3; 3]>,
}

impl State {
    pub(super) fn controls(&mut self, app: &mut CadApp, ui: &mut egui::Ui, group: &str, cuts: &[String], key: (u64, u64)) {
        if cuts.is_empty() {
            self.selected = None;
            self.key = None;
            self.triangles.clear();
            return;
        }
        if self.key.as_ref().is_some_and(|(uid, _, name, _)| *uid != key.0 || name != group)
            || self.selected.as_ref().is_some_and(|selected| !cuts.contains(selected))
        {
            self.selected = None;
        }
        let label = t("Show Removed Triangles");
        let toggle = ui.checkbox(&mut self.visible, label);
        crate::control::record_widget(ui, &toggle, label, "checkbox", Some(self.visible));
        if !self.visible {
            self.key = None;
            return;
        }

        let caption = ui.label(t("Removal Lines"));
        let cut_label = |i: usize| crate::tf!("Removal Line {number}", number = i + 1);
        let all = t("All Removal Lines");
        let caption_text = self.selected.as_ref().and_then(|s| cuts.iter().position(|c| c == s)).map_or_else(|| all.to_owned(), cut_label);
        let picker = craft_ui::ComboBox::from_id_salt("terrain-cuts")
            .selected_text(caption_text)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.selected, None, all);
                for (i, cut) in cuts.iter().enumerate() {
                    ui.selectable_value(&mut self.selected, Some(cut.clone()), cut_label(i));
                }
            })
            .response
            .labelled_by(caption.id);
        crate::control::record_widget(ui, &picker, t("Removal Lines"), "combobox", None);

        if self.refresh(app, group, key) {
            ui.ctx().request_repaint();
        }
        ui.label(crate::tf!("Removed triangles: {count}", count = self.triangles.len()));
        let label = if self.selected.is_some() { "Restore Selected Removal" } else { "Restore Latest Removal" };
        let restore = ui.add_enabled(!app.session.prompting(), egui::Button::new(t(label)));
        crate::control::record_widget(ui, &restore, t(label), "button", None);
        if restore.clicked() && app.run("tin.cuts.restore", json!({"group":group, "cut":self.selected})).is_ok() {
            self.key = None;
            self.selected = None;
            self.triangles.clear();
            ui.ctx().request_repaint();
        }
    }
    pub(super) fn show_all(&mut self, app: &mut CadApp, group: &str, key: (u64, u64)) -> bool {
        self.visible = true;
        self.selected = None;
        self.refresh(app, group, key)
    }

    fn refresh(&mut self, app: &mut CadApp, group: &str, key: (u64, u64)) -> bool {
        if self.key.as_ref().is_some_and(|(uid, rev, name, selected)| (*uid, *rev) == key && name == group && selected == &self.selected) {
            return false;
        }
        match app.run("tin.cuts.preview", json!({"group":group, "cut":self.selected})).and_then(read_triangles) {
            Ok(triangles) => self.triangles = triangles,
            Err(error) => {
                self.triangles.clear();
                app.set_status(error);
            }
        }
        self.key = Some((key.0, key.1, group.to_owned(), self.selected.clone()));
        true
    }
}

fn read_triangles(mut response: Value) -> Result<Vec<[Vec3; 3]>, String> {
    serde_json::from_value(response["triangles"].take()).map_err(|e| e.to_string())
}

pub(crate) fn visible_triangles(app: &CadApp) -> &[[Vec3; 3]] {
    let state = &app.surface_editor.cuts;
    let Some((uid, rev, group, _)) = &state.key else { return &[] };
    if !state.visible || app.surface_editor.selected != *group || !app.session.state().is_ok_and(|doc| doc.uid == *uid && doc.revision == *rev) {
        return &[];
    }
    &state.triangles
}

pub(super) fn paint(app: &CadApp, painter: &Painter, project: &impl Fn(Vec3) -> Option<Pos2>) {
    let colors = Tokens::get();
    let stroke = Stroke::new(1.5, colors.compare_area);
    let clip = painter.clip_rect();
    for tri in visible_triangles(app) {
        let [Some(a), Some(b), Some(c)] = tri.map(project) else { continue };
        let bounds = egui::Rect::from_points(&[a, b, c]);
        if bounds.intersects(clip) {
            painter.add(egui::Shape::convex_polygon(vec![a, b, c], colors.compare_area.gamma_multiply(0.18), Stroke::NONE));
            for line in [[a, b], [b, c], [c, a]] {
                // Clip to the canvas before laying out dashes at extreme zoom.
                if let Some(line) = clip_line(line, clip) {
                    painter.extend(egui::Shape::dashed_line(&line, stroke, 7.0, 4.0));
                }
            }
        }
    }
}

pub(super) fn clip_line([a, b]: [Pos2; 2], rect: egui::Rect) -> Option<[Pos2; 2]> {
    let delta = b - a;
    let mut lo: f32 = 0.0;
    let mut hi: f32 = 1.0;
    for (value, change, min, max) in [(a.x, delta.x, rect.min.x, rect.max.x), (a.y, delta.y, rect.min.y, rect.max.y)] {
        if change == 0.0 {
            if value < min || value > max {
                return None;
            }
        } else {
            let from = (min - value) / change;
            let to = (max - value) / change;
            lo = lo.max(from.min(to));
            hi = hi.min(from.max(to));
        }
    }
    (lo <= hi).then_some([a + lo * delta, a + hi * delta])
}
