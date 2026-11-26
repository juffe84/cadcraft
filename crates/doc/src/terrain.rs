//! Source geometry shared by the linked terrain editor and terrain export.
use cadcraft_geom::{Arc as CurveArc, Mat4, Polyline, Vec2, Vec3};

use crate::{Arc, Drawing, Entity, EntityKind, Handle};

pub const SOURCE_PREFIX: &str = "CADCraft TIN sources ";
pub const CHORD_TOLERANCE: f64 = 0.01;

pub fn group_members<'a>(drawing: &'a Drawing, description: &str) -> &'a [Handle] {
    drawing.groups.iter().find(|g| g.description == description).map_or(&[], |g| g.members.as_slice())
}

pub fn source_arc(entity: &Entity, arc: &Arc) -> (CurveArc, Mat4) {
    let normal = entity.common.extrusion.normalized();
    let (start, end) = if normal.z < 0.0 { (std::f64::consts::PI - arc.end, std::f64::consts::PI - arc.start) } else { (arc.start, arc.end) };
    (CurveArc::new(Vec2::ZERO, arc.radius, start, end), Mat4::ocs(normal))
}

pub fn source_vertices(entity: &Entity, budget: usize) -> Result<Vec<Vec3>, &'static str> {
    let points = match &entity.kind {
        EntityKind::Point(p) => vec![p.p],
        EntityKind::Line(p) => vec![p.a, p.b],
        EntityKind::Polyline3d(p) => {
            let close = p.closed && p.points.first() != p.points.last();
            if p.points.len().saturating_add(usize::from(close)) > budget {
                return Err("The polyline exceeds the terrain vertex limit.");
            }
            let mut out = p.points.clone();
            if close && let Some(&first) = p.points.first() {
                out.push(first);
            }
            out
        }
        EntityKind::Arc(a) => {
            let (curve, axes) = source_arc(entity, a);
            let mut points = Vec::new();
            curve.tessellate(CHORD_TOLERANCE, &mut points);
            points.into_iter().map(|p| a.center + axes.apply(p.to3(0.0))).collect()
        }
        EntityKind::LwPolyline(p) if p.vertices.len() <= budget && entity.common.extrusion == Vec3::Z => {
            let mut limit = budget;
            let line = Polyline { vertices: p.vertices.clone(), closed: p.closed };
            let poly = line.tessellate_within(CHORD_TOLERANCE, &mut limit);
            if limit == 0 {
                return Err("The tessellated polyline exceeds the terrain vertex limit.");
            }
            poly.into_iter().map(|v| v.to3(p.elevation)).collect()
        }
        _ => return Err("Use points, lines, arcs or supported polylines as terrain sources."),
    };
    if points.len() > budget {
        return Err("The source exceeds the terrain vertex limit.");
    }
    Ok(points)
}
