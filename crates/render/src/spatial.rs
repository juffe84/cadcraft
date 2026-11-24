//! Spatial primitives for the model orbit. Planar artwork is drawn by the normal builder at its elevation.
use super::{Builder, Ctx, Entity, EntityKind, Ink, SpatialSnap, Vec2, Vec3};
use cadcraft_geom::{Mat4, Polyline, ccw_sweep};
use std::f64::consts::{PI, TAU};

pub(crate) fn elevation(kind: &EntityKind) -> f64 {
    match kind {
        EntityKind::Point(point) => point.p.z,
        EntityKind::Line(line) => line.a.z,
        EntityKind::Circle(circle) => circle.center.z,
        EntityKind::Arc(arc) => arc.center.z,
        EntityKind::LwPolyline(line) => line.elevation,
        EntityKind::Text(text) => text.align_pt.unwrap_or(text.insert).z,
        EntityKind::MText(text) => text.insert.z,
        EntityKind::AttDef(attrib) => attrib.text.insert.z,
        EntityKind::Hatch(hatch) => hatch.elevation,
        EntityKind::Dimension(dimension) if dimension.block.is_none() => dimension.defpt.z,
        EntityKind::MLine(line) => line.vertices.first().map_or(0.0, |v| v.p.z),
        EntityKind::Table(table) => table.insert.z,
        EntityKind::Tolerance(tolerance) => tolerance.insert.z,
        _ => 0.0,
    }
}

pub(crate) fn snaps(builder: &mut Builder, ctx: &Ctx, e: &Entity) {
    use SpatialSnap::*;
    let world = |p: Vec3| lifted(ctx.xf.apply(p.xy()), ctx.z_offset + ctx.z_scale * p.z);
    let mut add = |snap: SpatialSnap| {
        if snap.positions().iter().all(|p| p.is_finite()) && (!builder.snaps_only || builder.charge(1)) {
            builder.list.spatial_snaps.push(snap);
        }
    };
    let line = |a, b| Segment { ends: [world(a), world(b)], range: [0.0, 1.0] };
    match &e.kind {
        EntityKind::Line(l) => add(line(l.a, l.b)),
        EntityKind::Polyline3d(p) => {
            for pair in p.points.windows(2) {
                add(line(pair[0], pair[1]));
            }
            if p.closed
                && let Some((last, first)) = p.points.last().zip(p.points.first())
            {
                add(line(*last, *first));
            }
        }
        EntityKind::LwPolyline(p) => {
            let at = |q| lifted(q, p.elevation);
            for (a, b) in p.vertices.iter().zip(p.vertices.iter().cycle().skip(1)).take(p.vertices.len().saturating_sub(usize::from(!p.closed))) {
                if let Some((arc, _)) = cadcraft_geom::bulge_to_arc(a.p, b.p, a.bulge) {
                    add(End(world(at(a.p))));
                    add(End(world(at(b.p))));
                    add(Mid(world(at(arc.mid_point()))));
                    add(Center(world(at(arc.center))));
                } else {
                    add(line(at(a.p), at(b.p)));
                }
            }
        }
        EntityKind::Point(p) => add(Node(world(p.p))),
        EntityKind::Text(t) => add(Insertion(world(lifted(t.justify_point(), elevation(&e.kind))))),
        EntityKind::MText(t) => add(Insertion(world(t.insert))),
        EntityKind::Insert(p) => add(Insertion(world(p.insert))),
        EntityKind::Face3d(f) => {
            for (a, b) in f.visible_edges() {
                add(line(a, b));
            }
        }
        EntityKind::Circle(c) => {
            let (m, _) = circle_axes(e.common.extrusion);
            add(Center(world(c.center)));
            for angle in [0.0, PI * 0.5, PI, PI * 1.5] {
                add(Quadrant(world(circle_point(c.center, c.radius, m, angle))));
            }
        }
        EntityKind::Arc(a) => {
            let (m, reversed) = circle_axes(e.common.extrusion);
            let (start, end) = if reversed { (PI - a.end, PI - a.start) } else { (a.start, a.end) };
            let sweep = ccw_sweep(start, end);
            let at = |angle| world(circle_point(a.center, a.radius, m, angle));
            add(Center(world(a.center)));
            add(End(at(start)));
            add(End(at(end)));
            add(Mid(at(start + sweep * 0.5)));
            for angle in [0.0, PI * 0.5, PI, PI * 1.5] {
                if (angle - start).rem_euclid(TAU) <= sweep {
                    add(Quadrant(at(angle)));
                }
            }
        }
        _ => {}
    }
}

fn circle_axes(extrusion: Vec3) -> (Mat4, bool) {
    let normal = extrusion.normalized();
    if normal.x.abs().max(normal.y.abs()) > 1e-10 { (Mat4::ocs(normal), normal.z < 0.0) } else { (Mat4::ocs(Vec3::Z), false) }
}

fn circle_point(center: Vec3, radius: f64, axes: Mat4, angle: f64) -> Vec3 {
    center + axes.apply(Vec3::new(radius * angle.cos(), radius * angle.sin(), 0.0))
}

pub(crate) fn draw(b: &mut Builder, ctx: &Ctx, e: &Entity, color: Ink, lw: f32, tol: f64) -> bool {
    let normal = e.common.extrusion.normalized();
    let thickness = normal * e.common.thickness;
    let tilted = normal.x.abs().max(normal.y.abs()) > 1e-10;
    let fill = b.opts.fill && ctx.d.header.i64("FILLMODE", 1) != 0;
    match &e.kind {
        EntityKind::Line(line) if line.a.z != line.b.z || thickness != Vec3::ZERO => {
            path(b, ctx, color, lw, &[line.a, line.b], thickness, fill);
        }
        EntityKind::Polyline3d(line) => {
            let mut points = line.points.clone();
            if line.closed
                && let Some(first) = points.first().copied()
            {
                points.push(first);
            }
            path(b, ctx, color, lw, &points, thickness, fill);
        }
        EntityKind::Face3d(face) => draw_face(b, ctx, color, lw, face),
        EntityKind::PolyfaceMesh(mesh) => {
            for face in &mesh.faces {
                if !b.expand() {
                    break;
                }
                if let Some(face) = mesh.face3d(face) {
                    draw_face(b, ctx, color, lw, &face);
                }
            }
        }
        EntityKind::Solid(solid) | EntityKind::Trace(solid) => {
            quad(b, ctx, color, lw, [solid.corners[0], solid.corners[1], solid.corners[3], solid.corners[2]], fill);
        }
        EntityKind::Circle(circle) if tilted || thickness != Vec3::ZERO => {
            let m = Mat4::ocs(normal);
            let points = arc_points(0.0, TAU, circle.radius, tol, |t| circle_point(circle.center, circle.radius, m, t));
            path(b, ctx, color, lw, &points, thickness, fill);
        }
        EntityKind::Arc(arc) if tilted || thickness != Vec3::ZERO => {
            let m = Mat4::ocs(normal);
            // The document stores arc angles in the plan convention for a negative OCS Z.
            let (start, end) = if normal.z < 0.0 { (PI - arc.end, PI - arc.start) } else { (arc.start, arc.end) };
            let points = arc_points(start, end, arc.radius, tol, |t| circle_point(arc.center, arc.radius, m, t));
            path(b, ctx, color, lw, &points, thickness, fill);
        }
        EntityKind::Ellipse(ellipse) => {
            let normal = if normal == Vec3::ZERO {
                Vec3::Z
            } else if normal.z < 0.0 {
                -normal
            } else {
                normal
            };
            let minor = normal.cross(ellipse.major).normalized() * (ellipse.major.len() * ellipse.ratio);
            let points =
                arc_points(ellipse.start, ellipse.end, ellipse.major.len(), tol, |t| ellipse.center + ellipse.major * t.cos() + minor * t.sin());
            path(b, ctx, color, lw, &points, thickness, fill);
        }
        EntityKind::LwPolyline(line) if thickness != Vec3::ZERO => {
            let poly = Polyline { vertices: line.vertices.clone(), closed: line.closed };
            let points: Vec<_> = poly.tessellate(tol).into_iter().map(|p| lifted(p, line.elevation)).collect();
            path(b, ctx, color, lw, &points, thickness, fill);
        }
        EntityKind::Helix(helix) => {
            let helix = helix.clone().sanitized();
            let points =
                arc_points(0.0, TAU * helix.turns, helix.base_radius.max(helix.top_radius), tol, |t| helix.point_at(t / (TAU * helix.turns)));
            b.polyline3(ctx, color, lw, &points);
        }
        EntityKind::Ray(ray) | EntityKind::XLine(ray) => {
            b.infinite3(ctx, color, ray.base, ray.dir, matches!(e.kind, EntityKind::Ray(_)));
        }
        _ => return false,
    }
    true
}

pub(crate) fn lifted(p: Vec2, z: f64) -> Vec3 {
    Vec3::new(p.x, p.y, z)
}

fn arc_points(start: f64, end: f64, radius: f64, tol: f64, point: impl Fn(f64) -> Vec3) -> Vec<Vec3> {
    // A helix can make several turns; planar arcs use the document's CCW sweep convention.
    let sweep = if end - start > TAU { end - start } else { ccw_sweep(start, end) };
    let step = (2.0 * (1.0 - tol / radius.max(1e-12)).clamp(-1.0, 1.0).acos()).clamp(1e-5, PI / 8.0);
    let count = ((sweep / step).ceil() as usize).clamp(1, 65_536);
    (0..=count).map(|i| point(start + sweep * (i as f64 / count as f64))).collect()
}

fn quad(b: &mut Builder, ctx: &Ctx, color: Ink, lw: f32, [a, c, d, f]: [Vec3; 4], fill: bool) {
    if fill {
        b.tris3(ctx, color, &[a, c, d, a, d, f]);
    } else {
        b.polyline3(ctx, color, lw, &[a, c, d, f, a]);
    }
}

fn path(b: &mut Builder, ctx: &Ctx, color: Ink, lw: f32, points: &[Vec3], thickness: Vec3, fill: bool) {
    b.polyline3(ctx, color, lw, points);
    if thickness == Vec3::ZERO {
        return;
    }
    let top: Vec<_> = points.iter().map(|p| *p + thickness).collect();
    b.polyline3(ctx, color, lw, &top);
    for (base, top) in points.windows(2).zip(top.windows(2)) {
        quad(b, ctx, color, lw, [base[0], base[1], top[1], top[0]], fill);
    }
}

fn draw_face(b: &mut Builder, ctx: &Ctx, color: Ink, lw: f32, face: &cadcraft_doc::Face3d) {
    let [a, c, d, f] = face.corners;
    if super::terrain::draw(b, ctx, color, lw, face) {
        return;
    }
    if b.opts.fill {
        b.tris3(ctx, color, &[a, c, d, a, d, f]);
    }
    for (a, c) in face.visible_edges() {
        b.polyline3(ctx, color, lw, &[a, c]);
    }
}
