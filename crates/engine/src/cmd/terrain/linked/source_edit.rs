use cadcraft_doc::{EntityKind, Handle, LwPolyline, Point, Polyline3d, Space};
use cadcraft_geom::{PolyVertex, Polyline, Segment, Vec2, Vec3, bulge_to_arc};
use serde_json::{Value, json};

use super::{CHORD_TOLERANCE, FLAG_LAYER, MAX_VERTICES, source_vertex, vertices};
use crate::cmd::{bad, point_value};
use crate::grips::remove_polyline_vertex;
use crate::{Result, Session};

const MOVE: &str = "tin.source.move";
const REMOVE: &str = "tin.source.remove";

fn position(p: &Value, old_z: f64, command: &str) -> Result<Vec3> {
    p.get("position")
        .and_then(|position| {
            let xy = point_value(position)?;
            let z = position.get(2).or_else(|| position.get("z")).map_or(Some(old_z), Value::as_f64)?;
            Some(xy.to3(z))
        })
        .filter(|p| p.is_finite() && p.x.abs().max(p.y.abs()).max(p.z.abs()) <= 1e12)
        .ok_or_else(|| bad(command, "Enter valid coordinates within the drawing range."))
}

pub(super) fn move_point(s: &mut Session, p: &Value) -> Result<Value> {
    let (entity, vertex, from) = source_vertex(s.doc()?, p, MOVE)?;
    let to = position(p, from.z, MOVE)?;
    if to == from {
        return Ok(json!({"changed": false}));
    }
    let handle = entity.handle;
    let mut kind = entity.kind.clone();
    let converted = match &mut kind {
        EntityKind::Point(point) => {
            point.p = to;
            false
        }
        EntityKind::Line(line) => {
            if vertex == 0 {
                line.a = to;
            } else {
                line.b = to;
            }
            false
        }
        EntityKind::Polyline3d(poly) => {
            poly.points[vertex] = to;
            false
        }
        EntityKind::LwPolyline(poly) => {
            poly.vertices[vertex].p = to.xy();
            if to.z == poly.elevation {
                false
            } else {
                kind = EntityKind::Polyline3d(raise_vertex(poly, vertex, to.z, MOVE)?);
                true
            }
        }
        EntityKind::Arc(_) => {
            let path = moved_arc(entity, vertex, from, to)?;
            kind = EntityKind::Polyline3d(Polyline3d { points: path, closed: false });
            true
        }
        _ => return Err(bad(MOVE, "This source does not have editable points.")),
    };
    s.doc_mut()?.model.modify(handle, |entity| {
        entity.kind = kind;
        if converted {
            entity.common.extrusion = Vec3::Z;
        }
    });
    Ok(json!({"changed": true, "converted": converted}))
}

fn moved_arc(entity: &cadcraft_doc::Entity, vertex: usize, from: Vec3, to: Vec3) -> Result<Vec<Vec3>> {
    let mut path = vertices(entity, MAX_VERTICES)?;
    let last = path.len().saturating_sub(1);
    if last == 0 {
        return Err(bad(MOVE, "The arc endpoint no longer exists."));
    }
    let delta = to - from;
    for (i, point) in path.iter_mut().enumerate() {
        let steps = if vertex == 0 { last - i } else { i };
        *point = *point + delta * (steps as f64 / last as f64);
    }
    path[if vertex == 0 { 0 } else { last }] = to;
    Ok(path)
}

fn sample_bulge(a: PolyVertex, b: PolyVertex, samples: &mut Vec<Vec2>) -> bool {
    let Some((arc, ccw)) = bulge_to_arc(a.p, b.p, a.bulge) else { return false };
    samples.clear();
    Segment::Arc { arc, ccw }.tessellate(CHORD_TOLERANCE, samples);
    true
}

fn interpolated_height(start: f64, end: f64, step: usize, segments: usize) -> f64 {
    start + (end - start) * step as f64 / segments as f64
}

/// Preserve curved segments as chords, interpolating each segment between its authored heights.
fn raise_vertex(poly: &LwPolyline, vertex: usize, elevation: f64, command: &str) -> Result<Polyline3d> {
    let n = poly.vertices.len();
    let height = |i| if i == vertex { elevation } else { poly.elevation };
    let mut points = Vec::with_capacity(n);
    let mut samples = Vec::new();
    for (i, a) in poly.vertices.iter().enumerate() {
        points.push(a.p.to3(height(i)));
        let next = i + 1;
        if next == n && !poly.closed {
            break;
        }
        let j = next % n;
        if sample_bulge(*a, poly.vertices[j], &mut samples) {
            let segments = samples.len().saturating_sub(1);
            if points.len() + segments.saturating_sub(1) > MAX_VERTICES {
                return Err(bad(command, "The tessellated polyline exceeds the linked terrain vertex limit."));
            }
            for (k, point) in samples.iter().enumerate().take(segments).skip(1) {
                let z = interpolated_height(height(i), height(j), k, segments);
                points.push(point.to3(z));
            }
        }
    }
    if points.len() + usize::from(poly.closed) > MAX_VERTICES {
        return Err(bad(command, "The tessellated polyline exceeds the linked terrain vertex limit."));
    }
    Ok(Polyline3d { points, closed: poly.closed })
}

fn adjacent_segments(vertex: usize, count: usize, closed: bool) -> impl Iterator<Item = (usize, usize)> {
    let valid = vertex < count && count > 1;
    let previous = vertex.checked_sub(1).or_else(|| closed.then(|| count.saturating_sub(1)));
    let next = (vertex.saturating_add(1) < count).then(|| vertex + 1).or_else(|| closed.then_some(0));
    [previous.map(|index| (index, vertex)), next.map(|index| (vertex, index))].into_iter().flatten().filter(move |_| valid)
}

pub(super) fn can_remove(kind: &EntityKind) -> bool {
    match kind {
        EntityKind::Point(_) | EntityKind::Line(_) | EntityKind::Arc(_) => true,
        EntityKind::Polyline3d(p) => p.points.len() == 1 || !p.closed || p.points.len() > 3,
        EntityKind::LwPolyline(p) => p.vertices.len() == 1 || !p.closed || p.vertices.len() > 3,
        _ => false,
    }
}

pub(super) fn delete_source(s: &mut Session, p: &Value) -> Result<Value> {
    let handle = super::editable_source(s.doc()?, p, "tin.source.delete")?.handle;
    s.doc_mut()?.remove_entity(handle);
    Ok(json!({"changed": true}))
}

pub(super) fn remove_point(s: &mut Session, p: &Value) -> Result<Value> {
    let (entity, vertex, _) = source_vertex(s.doc()?, p, REMOVE)?;
    let handle = entity.handle;
    if !can_remove(&entity.kind) {
        return Err(bad(REMOVE, "A closed polyline needs at least three points."));
    }
    let endpoints = match &entity.kind {
        EntityKind::Point(_) => 1,
        EntityKind::Line(_) | EntityKind::Arc(_) => 2,
        EntityKind::Polyline3d(p) => p.points.len(),
        EntityKind::LwPolyline(p) => p.vertices.len(),
        _ => 0,
    };
    if endpoints == 1 {
        s.doc_mut()?.remove_entity(handle);
        return Ok(json!({"changed": true, "deleted": true}));
    }
    let point_only = endpoints == 2;
    let kind = if point_only {
        let points = super::authored_points(entity);
        let p = *points.get(usize::from(vertex == 0)).ok_or_else(|| bad(REMOVE, "The opposite endpoint no longer exists."))?;
        EntityKind::Point(Point { p, angle: 0.0 })
    } else {
        match &entity.kind {
            EntityKind::Polyline3d(poly) => {
                let mut poly = poly.clone();
                poly.points.remove(vertex);
                EntityKind::Polyline3d(poly)
            }
            EntityKind::LwPolyline(poly) => {
                let mut poly = poly.clone();
                remove_polyline_vertex(&mut poly, vertex).ok_or_else(|| bad(REMOVE, "The polyline point could not be removed."))?;
                EntityKind::LwPolyline(poly)
            }
            _ => return Err(bad(REMOVE, "Select a source point.")),
        }
    };
    s.doc_mut()?.model.modify(handle, |entity| {
        entity.kind = kind;
        if point_only {
            entity.common.extrusion = Vec3::Z;
        }
    });
    Ok(json!({"changed": true, "pointOnly": point_only}))
}

impl Session {
    /// Preview only the parts of a source curve that change when an authored vertex moves.
    pub fn terrain_source_move_paths(&self, handle: Handle, vertex: usize, from: Vec3, to: Vec3) -> Vec<Vec<Vec3>> {
        let Some(entity) = self.doc().ok().and_then(|doc| doc.model.get(handle)) else { return Vec::new() };
        match &entity.kind {
            EntityKind::Line(line) => vec![if vertex == 0 { vec![to, line.b] } else { vec![line.a, to] }],
            EntityKind::Arc(_) => moved_arc(entity, vertex, from, to).into_iter().collect(),
            EntityKind::Polyline3d(poly) => {
                let point = |index| if index == vertex { to } else { poly.points[index] };
                adjacent_segments(vertex, poly.points.len(), poly.closed).map(|(a, b)| vec![point(a), point(b)]).collect()
            }
            EntityKind::LwPolyline(poly) => {
                let point = |index| {
                    let mut p: PolyVertex = poly.vertices[index];
                    if index == vertex {
                        p.p = to.xy();
                        (p, to.z)
                    } else {
                        (p, poly.elevation)
                    }
                };
                adjacent_segments(vertex, poly.vertices.len(), poly.closed)
                    .map(|(a, b)| {
                        let (a, start) = point(a);
                        let (b, end) = point(b);
                        let mut samples = Vec::new();
                        if sample_bulge(a, b, &mut samples) && samples.len() > 1 {
                            let segments = samples.len() - 1;
                            let interior =
                                samples.iter().enumerate().take(segments).skip(1).map(|(i, p)| p.to3(interpolated_height(start, end, i, segments)));
                            std::iter::once(a.p.to3(start)).chain(interior).chain(std::iter::once(b.p.to3(end))).collect()
                        } else {
                            vec![a.p.to3(start), b.p.to3(end)]
                        }
                    })
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// Whether a visible model-space curve other than the edited object can supply an elevation.
    pub fn terrain_elevation_reference(&self, handle: Handle, exclude: Handle) -> bool {
        handle != exclude
            && self.doc().is_ok_and(|doc| {
                doc.model.get(handle).is_some_and(|e| {
                    doc.is_visible(e)
                        && !e.common.layer.eq_ignore_ascii_case(FLAG_LAYER)
                        && matches!(e.kind, EntityKind::Line(_) | EntityKind::Arc(_) | EntityKind::LwPolyline(_) | EntityKind::Polyline3d(_))
                })
            })
    }

    /// Closest point on a different curve under a plan-view elevation pick.
    pub fn terrain_reference_point(&self, at: Vec2, exclude: Handle, aperture: f64) -> Option<Vec3> {
        let doc = self.doc().ok()?;
        let distance = |p: Vec3| (p.xy() - at).len2();
        let nearest = |a: &Vec3, b: &Vec3| distance(*a).total_cmp(&distance(*b));
        crate::select::pick_all_in(doc, &Space::Model, self.viewport_frozen(), at, aperture)
            .into_iter()
            .filter(|&handle| self.terrain_elevation_reference(handle, exclude))
            .filter_map(|handle| {
                let entity = doc.model.get(handle)?;
                if let EntityKind::LwPolyline(poly) = &entity.kind
                    && entity.common.extrusion == Vec3::Z
                {
                    return Polyline { vertices: poly.vertices.clone(), closed: poly.closed }.closest(at).map(|p| p.to3(poly.elevation));
                }
                let path = vertices(entity, MAX_VERTICES).ok()?;
                path.windows(2)
                    .filter_map(|edge| {
                        let delta = edge[1] - edge[0];
                        let length_sq = delta.xy().len2();
                        if length_sq == 0.0 && delta.z != 0.0 {
                            return None;
                        }
                        let t = if length_sq == 0.0 { 0.0 } else { ((at - edge[0].xy()).dot(delta.xy()) / length_sq).clamp(0.0, 1.0) };
                        Some(edge[0] + delta * t)
                    })
                    .min_by(nearest)
            })
            .filter(|&p| distance(p) <= aperture * aperture)
            .min_by(nearest)
    }
}

/// One sampled path per authored segment, including the closing segment of closed polylines.
pub(super) fn segments(e: &cadcraft_doc::Entity, budget: &mut usize) -> Option<Vec<Vec<Vec3>>> {
    let mut result = Vec::new();
    let mut remaining = *budget;
    let mut push = |path: Vec<Vec3>| {
        if path.len() < 2 || !path.iter().all(|p| p.is_finite()) {
            return None;
        }
        remaining = remaining.checked_sub(path.len() - 1)?;
        result.push(path);
        Some(())
    };
    match &e.kind {
        EntityKind::Line(line) => push(vec![line.a, line.b])?,
        EntityKind::Arc(_) => push(vertices(e, MAX_VERTICES).ok()?)?,
        EntityKind::Polyline3d(poly) => {
            for edge in poly.points.windows(2) {
                push(edge.to_vec())?;
            }
            if poly.closed
                && let Some((&a, &b)) = poly.points.last().zip(poly.points.first())
                && a != b
            {
                push(vec![a, b])?;
            }
        }
        EntityKind::LwPolyline(poly) => {
            let n = poly.vertices.len();
            let count = if poly.closed { n } else { n.saturating_sub(1) };
            for i in 0..count {
                let a = poly.vertices[i];
                let b = poly.vertices[(i + 1) % n];
                let mut xy = Vec::new();
                let path = if sample_bulge(a, b, &mut xy) {
                    xy.into_iter().map(|p| p.to3(poly.elevation)).collect()
                } else {
                    vec![a.p.to3(poly.elevation), b.p.to3(poly.elevation)]
                };
                push(path)?;
            }
        }
        _ => {}
    }
    *budget = remaining;
    Some(result)
}

const INSERT: &str = "tin.source.insert";

pub(super) fn insert_point(s: &mut Session, p: &Value) -> Result<Value> {
    let e = super::editable_source(s.doc()?, p, INSERT)?;
    let handle = e.handle;
    let index =
        p.get("segment").and_then(Value::as_u64).and_then(|i| usize::try_from(i).ok()).ok_or_else(|| bad(INSERT, "Specify the source segment."))?;
    let explicit = p
        .get("position")
        .map(|value| {
            value.as_array().filter(|a| a.len() == 3).ok_or_else(|| bad(INSERT, "Specify the position as [x,y,z]."))?;
            position(p, 0.0, INSERT)
        })
        .transpose()?;
    let t = match p.get("fraction") {
        Some(f) => f.as_f64().filter(|&x| (0.0..=1.0).contains(&x) && (explicit.is_some() || (x > 1e-8 && x < 1.0 - 1e-8))),
        None => explicit.map(|_| 0.5),
    }
    .ok_or_else(|| bad(INSERT, "Pick a point between the ends of a source segment."))?;
    let mut kind = e.kind.clone();
    let missing = || bad(INSERT, "The source segment no longer exists.");
    let between = |a: Vec3, b: Vec3| -> Result<Vec3> {
        let p = explicit.unwrap_or_else(|| a + (b - a) * t);
        if p == a || p == b || (explicit.is_some() && (p.xy() == a.xy() || p.xy() == b.xy())) {
            Err(bad(INSERT, "Pick a position different from the segment endpoints."))
        } else {
            Ok(p)
        }
    };
    let (vertex, position, converted) = match &mut kind {
        EntityKind::Line(line) if index == 0 => {
            let pos = between(line.a, line.b)?;
            kind = EntityKind::Polyline3d(Polyline3d { points: vec![line.a, pos, line.b], closed: false });
            (1, pos, true)
        }
        EntityKind::Polyline3d(poly) => {
            let n = poly.points.len();
            let i = index.checked_add(1).filter(|&i| i < n || (poly.closed && n > 1 && i == n)).ok_or_else(missing)?;
            if n >= MAX_VERTICES - usize::from(poly.closed) {
                return Err(bad(INSERT, "The source exceeds the linked terrain vertex limit."));
            }
            let pos = between(poly.points[index], poly.points[i % n])?;
            poly.points.insert(i, pos);
            (i, pos, false)
        }
        EntityKind::LwPolyline(poly) => {
            let n = poly.vertices.len();
            let i = index.checked_add(1).filter(|&i| i < n || (poly.closed && n > 1 && i == n)).ok_or_else(missing)?;
            if n >= MAX_VERTICES - usize::from(poly.closed) {
                return Err(bad(INSERT, "The source exceeds the linked terrain vertex limit."));
            }
            let a = poly.vertices[index];
            let b = poly.vertices[i % n];
            let curved = bulge_to_arc(a.p, b.p, a.bulge).and_then(|(arc, ccw)| {
                let mut fraction = t;
                if let Some(point) = explicit {
                    let xy = point.xy();
                    let rounding = 8.0 * f64::EPSILON * xy.x.abs().max(xy.y.abs()).max(arc.radius).max(1.0);
                    if arc.closest(xy).dist(xy) > rounding {
                        return None;
                    }
                    let angle = arc.center.angle_to(xy);
                    let u = cadcraft_geom::ccw_sweep(arc.start, angle) / arc.sweep();
                    fraction = if ccw { u } else { 1.0 - u };
                    if !(1e-8..=1.0 - 1e-8).contains(&fraction) {
                        return None;
                    }
                }
                let sweep = if ccw { arc.sweep() } else { -arc.sweep() };
                let xy = arc.at(if ccw { fraction } else { 1.0 - fraction });
                Some((explicit.unwrap_or_else(|| xy.to3(poly.elevation)), sweep, fraction))
            });
            let (pos, first, second, width_fraction) = if let Some((point, sweep, f)) = curved {
                (point, cadcraft_geom::arc_to_bulge(sweep * f), cadcraft_geom::arc_to_bulge(sweep * (1.0 - f)), f)
            } else {
                (between(a.p.to3(poly.elevation), b.p.to3(poly.elevation))?, 0.0, 0.0, t)
            };
            if pos.xy() == a.p || pos.xy() == b.p {
                return Err(bad(INSERT, "Pick a position different from the segment endpoints."));
            }
            let width = a.start_width + (a.end_width - a.start_width) * width_fraction;
            poly.vertices[index].bulge = first;
            poly.vertices[index].end_width = width;
            poly.vertices.insert(i, cadcraft_geom::PolyVertex { p: pos.xy(), bulge: second, start_width: width, end_width: a.end_width });
            if pos.z == poly.elevation {
                (i, pos, false)
            } else {
                let spatial = raise_vertex(poly, i, pos.z, INSERT)?;
                let index = spatial.points.iter().position(|&p| p == pos).ok_or_else(missing)?;
                kind = EntityKind::Polyline3d(spatial);
                (index, pos, true)
            }
        }
        EntityKind::Arc(arc) if index == 0 => {
            let (curve, axes) = super::terrain_arc(e, arc);
            let at = |t| arc.center + axes.apply(curve.at(t).to3(0.0));
            let pos = explicit.unwrap_or_else(|| at(t));
            if pos.xy() == at(0.0).xy() || pos.xy() == at(1.0).xy() {
                return Err(bad(INSERT, "Pick a distinct point between the ends of the arc."));
            }
            let mut path = vertices(e, MAX_VERTICES)?;
            let edges = path.len().saturating_sub(1);
            let edge = ((t * edges as f64).floor() as usize).min(edges.saturating_sub(1));
            let i = if path.get(edge) == Some(&pos) {
                edge
            } else if path.get(edge + 1) == Some(&pos) {
                edge + 1
            } else {
                path.insert(edge + 1, pos);
                edge + 1
            };
            kind = EntityKind::Polyline3d(Polyline3d { points: path, closed: false });
            (i, pos, true)
        }
        _ => return Err(missing()),
    };
    s.doc_mut()?.model.modify(handle, |e| {
        e.kind = kind;
        if converted {
            e.common.extrusion = Vec3::Z;
        }
    });
    Ok(json!({"handle": handle.hex(), "vertex": vertex, "position": position, "converted": converted}))
}

const EXTEND: &str = "tin.source.extend";

pub(super) fn is_open_curve(kind: &EntityKind) -> bool {
    match kind {
        EntityKind::Line(_) | EntityKind::Arc(_) => true,
        EntityKind::LwPolyline(poly) => !poly.closed && poly.vertices.len() >= 2,
        EntityKind::Polyline3d(poly) => !poly.closed && poly.points.len() >= 2,
        _ => false,
    }
}

pub(super) fn extend(s: &mut Session, p: &Value) -> Result<Value> {
    let (entity, vertex, from) = source_vertex(s.doc()?, p, EXTEND)?;
    let handle = entity.handle;
    let count = match &entity.kind {
        EntityKind::LwPolyline(poly) => poly.vertices.len(),
        EntityKind::Polyline3d(poly) => poly.points.len(),
        _ => 2,
    };
    if !is_open_curve(&entity.kind) || (vertex != 0 && vertex + 1 != count) {
        return Err(bad(EXTEND, "Choose the first or last point of an open source line."));
    }
    let to = position(p, from.z, EXTEND)?;
    if to == from {
        return Err(bad(EXTEND, "Pick a position different from the endpoint."));
    }
    let prepend = vertex == 0;
    let mut kind = entity.kind.clone();
    let (index, converted) = match &mut kind {
        EntityKind::LwPolyline(poly) => {
            let index = if prepend { 0 } else { poly.vertices.len() };
            if poly.vertices.len() >= MAX_VERTICES {
                return Err(bad(EXTEND, "The source exceeds the linked terrain vertex limit."));
            }
            if !prepend && let Some(last) = poly.vertices.last_mut() {
                last.bulge = 0.0;
            }
            poly.vertices.insert(index, cadcraft_geom::PolyVertex::new(to.xy()));
            if to.z == poly.elevation {
                (index, false)
            } else {
                let spatial = raise_vertex(poly, index, to.z, EXTEND)?;
                let index = if prepend { 0 } else { spatial.points.len() - 1 };
                kind = EntityKind::Polyline3d(spatial);
                (index, true)
            }
        }
        _ => {
            let mut points = vertices(entity, MAX_VERTICES)?;
            if points.len() >= MAX_VERTICES {
                return Err(bad(EXTEND, "The source exceeds the linked terrain vertex limit."));
            }
            let index = if prepend { 0 } else { points.len() };
            points.insert(index, to);
            kind = EntityKind::Polyline3d(Polyline3d { points, closed: false });
            (index, !matches!(entity.kind, EntityKind::Polyline3d(_)))
        }
    };
    s.doc_mut()?.model.modify(handle, |entity| {
        entity.kind = kind;
        if converted {
            entity.common.extrusion = Vec3::Z;
        }
    });
    Ok(json!({"handle": handle.hex(), "vertex": index, "position": to, "converted": converted}))
}
