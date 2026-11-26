//! Transient road surfaces in the model viewport.
use std::sync::Arc;

use cadcraft_doc::{Drawing, road::Design};
use cadcraft_engine::cmd::{RoadPreviewLayer, preview_road_surfaces};
use cadcraft_geom::Vec3;
use egui::{Align2, Painter, Pos2, Rect, Shape, Stroke};
use serde_json::Value;

use crate::{CadApp, i18n::t, theme::Tokens};

type ResultMesh = Result<Vec<RoadPreviewLayer>, String>;
type Input = (u64, Value);

#[derive(Default)]
pub(super) struct Preview {
    latest: Option<(Input, ResultMesh)>,
    #[cfg(not(target_arch = "wasm32"))]
    pending: Option<(Input, std::sync::mpsc::Receiver<ResultMesh>)>,
}

impl Preview {
    pub(super) fn update(&mut self, ctx: &egui::Context, drawing: Arc<Drawing>, revision: u64, design: &Design) {
        let Ok(params) = serde_json::to_value(design) else { return };
        let input = (revision, params);
        #[cfg(not(target_arch = "wasm32"))]
        {
            use std::sync::mpsc::TryRecvError;

            if let Some((key, answer)) = &self.pending {
                let result = match answer.try_recv() {
                    Ok(result) => result,
                    Err(TryRecvError::Disconnected) => Err(t("Road preview calculation stopped.").into()),
                    Err(TryRecvError::Empty) => return,
                };
                self.latest = Some((key.clone(), result));
                self.pending = None;
                ctx.request_repaint();
            }
        }
        if self.latest.as_ref().is_some_and(|(key, _)| *key == input) {
            return;
        }
        let design = design.clone();
        let calculate = move || preview_road_surfaces(&drawing, design).map_err(|error| error.to_string());
        self.latest = None;
        #[cfg(not(target_arch = "wasm32"))]
        {
            let repaint = ctx.clone();
            let (send, receive) = std::sync::mpsc::channel();
            match std::thread::Builder::new().name("cadcraft-road-preview".into()).spawn(move || {
                let _ = send.send(calculate());
                repaint.request_repaint();
            }) {
                Ok(_) => self.pending = Some((input, receive)),
                Err(error) => self.latest = Some((input, Err(error.to_string()))),
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.latest = Some((input, calculate()));
        }
        ctx.request_repaint();
    }

    pub(super) fn status(&self) -> Option<&str> {
        match self.latest.as_ref().map(|(_, result)| result) {
            Some(Ok(layers)) if layers.iter().any(|layer| layer.incomplete) => {
                Some(t("Road surface preview has gaps where sections could not be joined."))
            }
            Some(Err(error)) => Some(error),
            _ => None,
        }
    }

    fn layers(&self) -> &[RoadPreviewLayer] {
        match self.latest.as_ref() {
            Some((_, Ok(layers))) => layers,
            _ => &[],
        }
    }
}

pub(crate) fn paint_preview(app: &CadApp, painter: &Painter, project: impl Fn(Vec3) -> Option<Pos2>) {
    let state = &app.road_design;
    if app.ui.dialog.as_deref() != Some(super::DIALOG)
        || !super::available(app)
        || app.session.state().ok().map(|drawing| drawing.uid) != state.document
    {
        return;
    }
    let tokens = Tokens::get();
    let active = state.section.layer_index();
    let mut fill = egui::Mesh::default();
    let mut edges = Vec::new();
    let clip = painter.clip_rect();
    for (index, layer) in state.live.layers().iter().enumerate() {
        let color = if index == active { tokens.accent } else { tokens.text_dim };
        let edge = Stroke::new(if index == active { 1.2 } else { 0.8 }, color);
        for triangle in &layer.triangles {
            let [Some(a), Some(b), Some(c)] = triangle.map(&project) else { continue };
            let bounds = Rect::from_points(&[a, b, c]);
            if !bounds.intersects(clip) {
                continue;
            }
            if index == active {
                let base = fill.vertices.len() as u32;
                for point in [a, b, c] {
                    fill.colored_vertex(point, color.linear_multiply(0.18));
                }
                fill.add_triangle(base, base + 1, base + 2);
            }
            if bounds.width().max(bounds.height()) >= 2.0 {
                triangle_edges(&mut edges, [a, b, c], edge);
            }
        }
    }
    if !fill.is_empty() {
        painter.add(fill);
    }
    painter.extend(edges);
    if let Some((station, line)) = state.section.marker()
        && let [Some(a), Some(b)] = line.map(project)
    {
        painter.line_segment([a, b], Stroke::new(5.0, tokens.canvas));
        painter.line_segment([a, b], Stroke::new(2.5, tokens.hover));
        let station = format!("{station:.3}");
        painter.text(b, Align2::CENTER_BOTTOM, station, crate::theme::small(), tokens.text);
    }
}

fn triangle_edges(edges: &mut Vec<Shape>, [a, b, c]: [Pos2; 3], stroke: Stroke) {
    // The miter on a closed stroke can spike far past an almost edge-on triangle.
    edges.extend([[a, b], [b, c], [c, a]].map(|ends| Shape::line_segment(ends, stroke)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_on_triangle_strokes_stay_close_to_the_projected_triangle() {
        let context = egui::Context::default();
        let points = [egui::pos2(100.0, 100.0), egui::pos2(190.0, 100.0), egui::pos2(130.0, 100.1)];
        let bounds = Rect::from_points(&points).expand(3.0);
        let mut output = context.run_ui(egui::RawInput::default(), |ui| {
            let mut edges = Vec::new();
            triangle_edges(&mut edges, points, Stroke::new(1.2, egui::Color32::WHITE));
            ui.painter().extend(edges);
        });
        output.textures_delta.clear();
        let meshes = context.tessellate(output.shapes, output.pixels_per_point);
        let vertices: Vec<_> = meshes
            .iter()
            .filter_map(|mesh| match &mesh.primitive {
                egui::epaint::Primitive::Mesh(mesh) => Some(&mesh.vertices),
                _ => None,
            })
            .flatten()
            .collect();
        assert!(!vertices.is_empty());
        assert!(vertices.iter().all(|vertex| bounds.contains(vertex.pos)));
    }
}
