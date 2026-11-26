//! Persistent source-linked terrain and visual conflicts.
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use cadcraft_color::Color;
use cadcraft_doc::terrain::{CHORD_TOLERANCE, SOURCE_PREFIX, group_members as sources, source_arc as terrain_arc, source_vertices};
use cadcraft_doc::{Common, Drawing, Entity, EntityKind, Face3d, Group, Handle, LINKED_TIN_PREFIX, Space, TIN_CONFLICT_LAYER as FLAG_LAYER};
use cadcraft_geom::{Bounds2, Vec2, Vec3, terrain};
use rstar::{
    RTree, RTreeObject,
    primitives::{GeomWithData, Rectangle},
};
use serde_json::{Value, json};

use super::{MAX_VERTICES, model_space};
use crate::cmd::{CommandSpec, bad, str_param, targets};
use crate::{Result, Session};

mod fence;
mod flag;
mod grade;
mod path;
mod source_edit;
pub(crate) use flag::upgrade_legacy_flags;
pub(crate) use path::{DRAW_SOURCE, REMOVE_CROSSED, RESTORE_CROSSED};

const LINK: &str = "tin.link";
pub(super) const FLAG_PREFIX: &str = "CADCraft TIN flags ";
const FACE_LAYER: &str = "TIN-SURFACES";
const Z_TOLERANCE: f64 = 0.0001;

const POINTS: &str = "tin.source.points";
const CHANGE_Z: &str = "tin.source.elevation";

pub(super) fn specs() -> [CommandSpec; 16] {
    [
        CommandSpec::new(LINK, "Build Linked Terrain", create)
            .menu(&["Draw", "Terrain", "Build Linked Terrain"])
            .params("{handles: POINT, LINE, ARC, 2D and 3D polyline handles, name?: surface name} -> {group, triangles, conflicts}")
            .enabled(model_space)
            .interactive(|_| Ok(Box::new(super::super::machines::SelectRun::new(LINK, "TIN.LINK")))),
        CommandSpec::new(POINTS, "List Terrain Source Points", source_points)
            .params("{group: linked terrain name, segments?: bool} -> {points, paths or curves, conflicts}")
            .enabled(model_space)
            .noundo(),
        CommandSpec::new(CHANGE_Z, "Set Terrain Source Elevation", change_z)
            .params("{group: linked terrain name, handle: source handle, vertex: zero-based index, elevation: drawing units}")
            .enabled(model_space),
        CommandSpec::new("tin.source.move", "Move Terrain Source Point", source_edit::move_point)
            .params("{group: linked terrain name, handle: source handle, vertex: zero-based index, position: [x,y] or [x,y,z]}")
            .enabled(model_space),
        CommandSpec::new("tin.source.remove", "Remove Terrain Source Point", source_edit::remove_point)
            .params("{group: linked terrain name, handle: source handle, vertex: zero-based index}")
            .enabled(model_space),
        CommandSpec::new("tin.source.insert", "Insert Terrain Source Point", source_edit::insert_point)
            .params("{group: linked terrain name, handle: source handle, segment: zero-based index, fraction?: along the segment, position?: explicit [x,y,z]}")
            .enabled(model_space),
        CommandSpec::new("tin.source.extend", "Extend Terrain Source Line", source_edit::extend)
            .params("{group: linked terrain name, handle: source handle, vertex: endpoint index, position: [x,y] or [x,y,z]}")
            .enabled(model_space),
        CommandSpec::new("tin.source.delete", "Delete Terrain Source", source_edit::delete_source)
            .params("{group: linked terrain name, handle: source handle}")
            .enabled(model_space),
        grade::elevation_spec(),
        grade::copy_spec(),
        grade::preview_spec(),
        path::draw_spec(),
        path::remove_spec(),
        path::restore_spec(),
        fence::preview_spec(),
        fence::restore_spec(),
    ]
}

fn linked_id<'a>(d: &'a Drawing, p: &Value, cmd: &str) -> Result<&'a str> {
    let name = str_param(p, "group").ok_or_else(|| bad(cmd, "Select a linked terrain surface."))?;
    d.groups
        .iter()
        .find(|g| g.name.eq_ignore_ascii_case(name))
        .and_then(Group::linked_terrain_id)
        .ok_or_else(|| bad(cmd, "Select a linked terrain surface."))
}

fn authored_points(e: &Entity) -> Vec<Vec3> {
    match &e.kind {
        EntityKind::Point(point) => vec![point.p],
        EntityKind::Line(line) => vec![line.a, line.b],
        EntityKind::Polyline3d(poly) => poly.points.clone(),
        EntityKind::LwPolyline(poly) => poly.vertices.iter().map(|v| v.p.to3(poly.elevation)).collect(),
        EntityKind::Arc(arc) => {
            let (curve, axes) = terrain_arc(e, arc);
            [0.0, 1.0].map(|t| arc.center + axes.apply(curve.at(t).to3(0.0))).to_vec()
        }
        _ => Vec::new(),
    }
}

fn source_points(s: &mut Session, p: &Value) -> Result<Value> {
    let d = s.doc()?;
    let id = linked_id(d, p, POINTS)?;
    let mut points = Vec::new();
    let mut paths = Vec::new();
    let mut curves = Vec::new();
    let include_segments = p.get("segments").and_then(Value::as_bool).unwrap_or(false);
    let mut budget = MAX_VERTICES;
    for &handle in sources(d, &format!("{SOURCE_PREFIX}{id}")) {
        let Some(e) = d.model.get(handle).filter(|e| d.is_visible(e)) else { continue };
        let whole = matches!(e.kind, EntityKind::LwPolyline(_) | EntityKind::Arc(_));
        let arc = matches!(e.kind, EntityKind::Arc(_));
        let removable = source_edit::can_remove(&e.kind);
        let locked = d.layer(&e.common.layer).is_some_and(|layer| layer.locked);
        if include_segments {
            if let Some(segments) = source_edit::segments(e, &mut budget)
                && !segments.is_empty()
            {
                let closed = match &e.kind {
                    EntityKind::LwPolyline(p) => p.closed,
                    EntityKind::Polyline3d(p) => p.closed,
                    _ => false,
                };
                curves.push(json!({"handle": handle.hex(), "segments": segments, "locked": locked, "closed": closed}));
            }
        } else if let Ok(path) = vertices(e, budget)
            && path.len() <= budget
            && path.iter().all(|p| p.is_finite())
        {
            budget -= path.len();
            if path.len() > 1 {
                paths.push(path);
            }
        }
        let authored = authored_points(e);
        let count = authored.len();
        let endpoint_only = count == 2 && removable;
        let open_curve = source_edit::is_open_curve(&e.kind);
        let line = cadcraft_doc::road::line_number(&e.common);
        for (vertex, position) in authored.into_iter().enumerate().filter(|(_, p)| p.is_finite()) {
            let extensible = open_curve && (vertex == 0 || vertex + 1 == count);
            let mut point = json!({"handle": handle.hex(), "vertex": vertex, "position": position, "whole": whole, "arc": arc, "endpointOnly": endpoint_only, "removable": removable, "locked": locked, "extensible": extensible, "single": count == 1});
            if let Some(number) = line {
                point["lineNumber"] = json!(number);
            }
            points.push(point);
        }
    }
    let conflicts = sources(d, &format!("{FLAG_PREFIX}{id}")).len();
    let cuts = fence::cut_handles(d, id).iter().map(|h| h.hex()).collect::<Vec<_>>();
    Ok(if include_segments {
        json!({"points": points, "curves": curves, "conflicts": conflicts, "cuts": cuts})
    } else {
        json!({"points": points, "paths": paths, "conflicts": conflicts})
    })
}

fn editable_source<'a>(d: &'a Drawing, p: &Value, cmd: &str) -> Result<&'a Entity> {
    let id = linked_id(d, p, cmd)?;
    let handle = str_param(p, "handle")
        .and_then(Handle::parse_hex)
        .filter(|h| sources(d, &format!("{SOURCE_PREFIX}{id}")).contains(h))
        .ok_or_else(|| bad(cmd, "Choose a source point from the selected surface."))?;
    let e = d.model.get(handle).ok_or_else(|| bad(cmd, "The source was deleted."))?;
    if d.layer(&e.common.layer).is_some_and(|layer| layer.locked) {
        return Err(bad(cmd, "The source is on a locked layer."));
    }
    Ok(e)
}

fn source_vertex<'a>(d: &'a Drawing, p: &Value, cmd: &str) -> Result<(&'a Entity, usize, Vec3)> {
    let e = editable_source(d, p, cmd)?;
    let (vertex, point) = p
        .get("vertex")
        .and_then(Value::as_u64)
        .and_then(|i| usize::try_from(i).ok())
        .and_then(|i| {
            let point = match &e.kind {
                EntityKind::Polyline3d(poly) => poly.points.get(i).copied(),
                EntityKind::LwPolyline(poly) => poly.vertices.get(i).map(|v| v.p.to3(poly.elevation)),
                _ => authored_points(e).get(i).copied(),
            }?;
            Some((i, point))
        })
        .ok_or_else(|| bad(cmd, "The source point no longer exists."))?;
    Ok((e, vertex, point))
}

fn change_z(s: &mut Session, p: &Value) -> Result<Value> {
    let (e, vertex, point) = source_vertex(s.doc()?, p, CHANGE_Z)?;
    let handle = e.handle;
    let elevation = p
        .get("elevation")
        .and_then(Value::as_f64)
        .filter(|z| z.is_finite() && z.abs() <= 1e12)
        .ok_or_else(|| bad(CHANGE_Z, "Enter an elevation within the drawing coordinate range."))?;
    let mut kind = e.kind.clone();
    match &mut kind {
        EntityKind::Point(point) => point.p.z = elevation,
        EntityKind::Line(line) => {
            if vertex == 0 {
                line.a.z = elevation
            } else {
                line.b.z = elevation
            }
        }
        EntityKind::Polyline3d(poly) => poly.points[vertex].z = elevation,
        EntityKind::LwPolyline(poly) => poly.elevation = elevation,
        EntityKind::Arc(arc) => {
            arc.center.z += elevation - point.z;
        }
        _ => return Err(bad(CHANGE_Z, "This source does not have editable points.")),
    }
    let changed = kind != e.kind;
    if changed {
        s.doc_mut()?.model.modify(handle, |e| e.kind = kind);
    }
    Ok(json!({"changed": changed}))
}

struct Mesh {
    triangles: Vec<terrain::Triangle>,
    flags: Vec<Vec3>,
    flag_size: f64,
}

fn create(s: &mut Session, p: &Value) -> Result<Value> {
    build_from_sources(s, p, false)
}

pub(super) fn build_from_sources(s: &mut Session, p: &Value, select_faces: bool) -> Result<Value> {
    let mut sources = targets(s, p)?;
    let mut seen = HashSet::new();
    sources.retain(|&h| seen.insert(h));
    let d = s.doc()?;
    let mesh = build(d, &sources)?;
    install(s, str_param(p, "name"), &sources, &mesh, select_faces)
}

/// Start a linked terrain with an existing mesh and separately owned source lines.
pub(super) fn seed_surface(s: &mut Session, name: &str, sources: Vec<Handle>, triangles: Vec<terrain::Triangle>) -> Result<Value> {
    let mesh = Mesh { triangles, flags: Vec::new(), flag_size: 1.0 };
    install(s, Some(name), &sources, &mesh, false)
}

fn install(s: &mut Session, name: Option<&str>, sources: &[Handle], mesh: &Mesh, select_faces: bool) -> Result<Value> {
    let old = s.state()?.doc.clone();
    let result = (|| {
        let id = s.doc_mut()?.new_handle().hex();
        super::super::groups::create(s, None, &format!("{SOURCE_PREFIX}{id}"), sources, false)?;
        let mut faces = Vec::with_capacity(mesh.triangles.len());
        for &triangle in &mesh.triangles {
            faces.push(add_face(s.doc_mut()?, triangle)?);
        }
        let group = super::super::groups::create(s, name, &format!("{LINKED_TIN_PREFIX}{id}"), &faces, true)?;
        set_flags(s.doc_mut()?, &id, &mesh.flags, mesh.flag_size, true)?;
        let handles = super::hex(&faces);
        s.set_selection(if select_faces { faces } else { Vec::new() });
        Ok(json!({
            "group": group, "triangles": handles.len(), "handles": handles, "conflicts": mesh.flags.len(),
            "message": status(&group, mesh)
        }))
    })();
    if result.is_err() {
        s.state_mut()?.doc = old;
    }
    result
}

fn build(d: &Drawing, sources: &[Handle]) -> Result<Mesh> {
    build_with_plan(d, sources, None)
}

fn build_with_plan(d: &Drawing, sources: &[Handle], plan: Option<Vec<[Vec2; 3]>>) -> Result<Mesh> {
    if sources.is_empty() || sources.len() > MAX_VERTICES {
        return Err(bad(LINK, "Select source points or breakline objects for the surface."));
    }
    let mut remaining = MAX_VERTICES;
    let mut paths = Vec::with_capacity(sources.len());
    for &h in sources {
        let entity = d.model.get(h).ok_or_else(|| bad(LINK, format!("Source {} must be in model space.", h.hex())))?;
        let path = vertices(entity, remaining)?;
        remaining = remaining.checked_sub(path.len()).ok_or_else(|| bad(LINK, "The source geometry exceeds 50,000 vertices."))?;
        if path.is_empty() || path.iter().any(|p| !p.is_finite() || p.x.abs().max(p.y.abs()).max(p.z.abs()) > 1e12) {
            return Err(bad(LINK, format!("Source {} has invalid terrain coordinates.", h.hex())));
        }
        paths.push(path);
    }
    let bounds = Bounds2::from_points(paths.iter().flatten().map(|p| p.xy()));
    let coordinate_scale = bounds.min.x.abs().max(bounds.max.x.abs()).max(bounds.min.y.abs()).max(bounds.max.y.abs());
    let weld = 1e-6_f64.max(16.0 * f64::EPSILON * coordinate_scale);
    let triangles_xy = match plan {
        Some(plan) => plan,
        None => {
            let xy: Vec<_> = paths.iter().map(|path| path.iter().map(|p| p.xy()).collect()).collect();
            terrain::triangulate_linked(&xy, weld, MAX_VERTICES).map_err(|e| bad(LINK, e))?
        }
    };

    let tol = weld * 4.0;
    let entry = |a: Vec3, b: Vec3| {
        let bnd = Bounds2::new(a.xy(), b.xy()).expand(tol);
        GeomWithData::new(Rectangle::from_corners([bnd.min.x, bnd.min.y], [bnd.max.x, bnd.max.y]), [a, b])
    };
    let index = RTree::bulk_load(
        paths
            .iter()
            .flat_map(|path| {
                let singleton = (path.len() == 1).then(|| entry(path[0], path[0]));
                singleton.into_iter().chain(path.windows(2).map(|segment| entry(segment[0], segment[1])))
            })
            .collect(),
    );
    let mut heights = HashMap::new();
    let mut flags = Vec::new();
    let mut triangles = Vec::with_capacity(triangles_xy.len());
    for [a, b, c] in triangles_xy {
        let mut point = |p: Vec2| -> Result<Vec3> {
            let key = [p.x, p.y].map(|x| if x == 0.0 { 0 } else { x.to_bits() });
            if let Some(&z) = heights.get(&key) {
                return Ok(p.to3(z));
            }
            let mut upper = f64::NEG_INFINITY;
            let mut lower = f64::INFINITY;
            let point = Rectangle::from_corners([p.x, p.y], [p.x, p.y]);
            for segment in index.locate_in_envelope_intersecting(point.envelope()) {
                let [a, b] = segment.data;
                let d = (b - a).xy();
                let offset = p - a.xy();
                let t = if d.len2() == 0.0 { 0.0 } else { (offset.dot(d) / d.len2()).clamp(0.0, 1.0) };
                if (offset - d * t).len2() <= tol * tol {
                    let za = a.z + (b.z - a.z) * t;
                    let zb = if d.len2() == 0.0 { b.z } else { za };
                    upper = upper.max(za).max(zb);
                    lower = lower.min(za).min(zb);
                }
            }
            if !upper.is_finite() {
                return Err(bad(LINK, "Could not determine a terrain elevation at a source intersection."));
            }
            if upper - lower > Z_TOLERANCE {
                flags.push(p.to3(upper));
            }
            heights.insert(key, upper);
            Ok(p.to3(upper))
        };
        let triangle = [point(a)?, point(b)?, point(c)?];
        if terrain::signed_area2(triangle) != 0.0 {
            triangles.push(triangle);
        }
    }
    if triangles.is_empty() {
        return Err(bad(LINK, "The linked terrain needs at least three non-collinear source points."));
    }
    flags.sort_unstable_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
    Ok(Mesh { triangles, flags, flag_size: (bounds.width().max(bounds.height()) * 0.004).clamp(0.05, 5.0) })
}

pub(super) fn vertices(entity: &Entity, budget: usize) -> Result<Vec<Vec3>> {
    source_vertices(entity, budget).map_err(|message| bad(LINK, format!("{}: {message}", entity.handle.hex())))
}

fn face_kind([a, b, c]: terrain::Triangle) -> EntityKind {
    EntityKind::Face3d(Face3d { corners: [a, b, c, c], hidden_edges: 0 })
}

fn add_face(d: &mut Drawing, triangle: terrain::Triangle) -> Result<Handle> {
    Ok(d.add(&Space::Model, Common { layer: FACE_LAYER.into(), ..Common::default() }, face_kind(triangle))?)
}

fn set_flags(d: &mut Drawing, id: &str, points: &[Vec3], size: f64, visible: bool) -> Result<()> {
    let tag = format!("{FLAG_PREFIX}{id}");
    let previous = d.groups.iter().position(|g| g.description == tag).map(|i| d.groups.remove(i));
    if let Some(previous) = previous {
        for h in previous.members {
            d.model.remove(h);
        }
    }
    if points.is_empty() {
        return Ok(());
    }
    let mut members = Vec::with_capacity(points.len());
    for &p in points {
        let kind = EntityKind::PolyfaceMesh(flag::mesh(p, size));
        let common = Common { color: Color::Index(1), layer: FLAG_LAYER.into(), visible, ..Common::default() };
        members.push(d.add(&Space::Model, common, kind)?);
    }
    d.groups.push(Group { name: format!("*TINFLAGS{id}"), description: tag, selectable: false, members });
    Ok(())
}

fn apply_mesh(d: &mut Drawing, id: &str, at: usize, mesh: &Mesh) -> Result<()> {
    let old = std::mem::take(&mut d.groups[at].members);
    let first_face = old.iter().find_map(|&handle| d.model.get(handle));
    let visible = super::visibility::road_sources_visible(d, id).unwrap_or_else(|| first_face.is_none_or(|face| face.common.visible));
    let layer = first_face.map_or_else(|| FACE_LAYER.to_owned(), |face| face.common.layer.clone());
    let common = Common { layer, visible, ..Common::default() };
    let mut members = Vec::with_capacity(mesh.triangles.len());
    for (i, &triangle) in mesh.triangles.iter().enumerate() {
        if let Some(&h) = old.get(i).filter(|&&h| d.model.get(h).is_some()) {
            d.model.modify(h, |e| e.kind = face_kind(triangle));
            members.push(h);
        } else {
            members.push(d.add(&Space::Model, common.clone(), face_kind(triangle))?);
        }
    }
    for &h in old.iter().skip(members.len()) {
        d.model.remove(h);
    }
    d.groups[at].members = members;
    set_flags(d, id, &mesh.flags, mesh.flag_size, visible)
}

fn status(group: &str, mesh: &Mesh) -> String {
    if mesh.flags.is_empty() {
        format!("Linked terrain {group}: {} triangles.", mesh.triangles.len())
    } else {
        format!(
            "Linked terrain {group}: {} triangles and {} height conflict flags. The higher elevation was used.",
            mesh.triangles.len(),
            mesh.flags.len()
        )
    }
}

fn geometry_changed(a: &Drawing, b: &Drawing, h: Handle) -> bool {
    match (a.model.get(h), b.model.get(h)) {
        (Some(a), Some(b)) if !Arc::ptr_eq(a, b) => a.kind != b.kind || a.common.extrusion != b.common.extrusion,
        (Some(_), Some(_)) | (None, None) => false,
        _ => true,
    }
}

fn road_plan(old: &Drawing, current: &Drawing, id: &str, group: &Group, handles: &[Handle]) -> Option<Vec<[Vec2; 3]>> {
    let previous = old.groups.iter().find(|group| group.linked_terrain_id() == Some(id))?;
    if group.members.is_empty()
        || group.members != previous.members
        || group.members.iter().any(|&handle| geometry_changed(old, current, handle))
        || sources(old, &format!("{SOURCE_PREFIX}{id}")) != handles
        || fence::changed(old, current, id)
        || super::visibility::road_sources_visible(current, id).is_none()
    {
        return None;
    }
    for &handle in handles.iter().filter(|&&handle| geometry_changed(old, current, handle)) {
        let before = old.model.get(handle)?;
        let after = current.model.get(handle)?;
        let (EntityKind::Polyline3d(before), EntityKind::Polyline3d(after)) = (&before.kind, &after.kind) else { return None };
        if before.closed != after.closed || before.points.iter().map(|point| point.xy()).ne(after.points.iter().map(|point| point.xy())) {
            return None;
        }
    }
    group.members.iter().map(|&handle| super::triangle(&current.model.get(handle)?.kind).map(|face| face.map(Vec3::xy))).collect()
}

/// Rebuild inside the source command's undo transaction, after other geometry updates.
pub(crate) fn after_command(s: &mut Session, before: Option<&Arc<Drawing>>, uid: Option<u64>, ok: bool) {
    let Some(old) = before.filter(|_| ok) else { return };
    let Ok(state) = s.state() else { return };
    if uid.is_some_and(|uid| uid != state.uid) || state.block_edit.is_some() || Arc::ptr_eq(old, &state.doc) {
        return;
    }
    let current = &state.doc;
    let jobs: Vec<_> = current
        .groups
        .iter()
        .filter_map(|g| {
            let id = g.linked_terrain_id()?;
            let previous = old.groups.iter().find(|previous| previous.linked_terrain_id() == Some(id))?;
            let tag = format!("{SOURCE_PREFIX}{id}");
            let now = sources(current, &tag);
            let then = sources(old, &tag);
            let flags_tag = format!("{FLAG_PREFIX}{id}");
            let flags_changed = sources(current, &flags_tag) != sources(old, &flags_tag)
                || sources(current, &flags_tag).iter().any(|&h| geometry_changed(old, current, h));
            (now != then
                || fence::changed(old, current, id)
                || g.members != previous.members
                || now.iter().chain(&g.members).any(|&h| geometry_changed(old, current, h))
                || flags_changed)
                .then(|| (id.to_owned(), g.name.clone(), now.to_vec()))
        })
        .collect();
    for (id, group, handles) in jobs {
        let Ok(d) = s.doc() else { return };
        let Some(at) = d.groups.iter().position(|g| g.linked_terrain_id() == Some(&id)) else { continue };
        let result = if let Some(plan) = road_plan(old, d, &id, &d.groups[at], &handles) {
            // A height edit can reuse the exact road footprint, including scoped edges and cuts.
            build_with_plan(d, &handles, Some(plan)).or_else(|_| build(d, &handles))
        } else {
            build(d, &handles)
        };
        let mut next = d.clone();
        let (mesh, message) = match result {
            Ok(mut mesh) => {
                fence::mask(d, &id, &mut mesh);
                let message = status(&group, &mesh);
                (mesh, message)
            }
            Err(error) => (
                Mesh { triangles: Vec::new(), flags: Vec::new(), flag_size: 1.0 },
                format!("The linked terrain {group} has no current mesh. {error}"),
            ),
        };
        match apply_mesh(&mut next, &id, at, &mesh) {
            Ok(()) => {
                if handles.is_empty() {
                    next.groups.remove(at);
                }
                if let Ok(d) = s.doc_mut() {
                    *d = next;
                }
                s.echo(message);
            }
            Err(error) => s.echo(format!("Could not update linked terrain {group}. {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(s: &Session) -> Vec<Vec3> {
        let d = s.doc().unwrap();
        d.groups
            .iter()
            .filter(|g| g.description.starts_with(FLAG_PREFIX))
            .flat_map(|g| &g.members)
            .map(|&h| {
                let EntityKind::PolyfaceMesh(flag) = &d.model.get(h).unwrap().kind else { panic!("expected flag mesh") };
                flag.vertices[0]
            })
            .collect()
    }

    fn terrain_points(s: &Session) -> Vec<Vec3> {
        let d = s.doc().unwrap();
        let g = d.groups.iter().find(|g| g.linked_terrain_id().is_some()).unwrap();
        g.members
            .iter()
            .flat_map(|&h| {
                let EntityKind::Face3d(face) = &d.model.get(h).unwrap().kind else { panic!("expected terrain face") };
                face.corners
            })
            .collect()
    }

    #[test]
    fn a_height_conflict_builds_uses_the_higher_source_and_rebuilds_after_edit_and_undo() {
        let mut s = Session::new();
        s.execute("line", &json!({"points": [[0,0,1],[10,10,1]]})).unwrap();
        s.execute("line", &json!({"points": [[0,10,3],[10,0,3]]})).unwrap();
        let handles: Vec<_> = s.doc().unwrap().model.iter().map(|e| e.handle.hex()).collect();
        let built = s.execute(LINK, &json!({"handles": handles, "name":"Site"})).unwrap();
        assert_eq!(built["conflicts"], 1);
        assert!(built["triangles"].as_u64().unwrap() >= 2);
        assert_eq!(flags(&s), [Vec3::new(5.0, 5.0, 3.0)]);
        assert!(terrain_points(&s).contains(&Vec3::new(5.0, 5.0, 3.0)));
        let d = s.doc().unwrap();
        let flag = d.model.iter().find(|e| e.common.layer == FLAG_LAYER).unwrap().as_ref();
        let orbit = cadcraft_render::build_entities(d, [flag], &cadcraft_render::Options { spatial: true, fill: false, ..Default::default() });
        let mut projected_area = Vec3::ZERO;
        for face in orbit.prims.iter().filter(|p| matches!(p.kind, cadcraft_render::Kind::Tris)) {
            let mut points = orbit.spatial_points(face);
            while let (Some(a), Some(b), Some(c)) = (points.next(), points.next(), points.next()) {
                let normal = (b - a).cross(c - a);
                projected_area = projected_area + Vec3::new(normal.x.abs(), normal.y.abs(), normal.z.abs());
            }
        }
        assert!(projected_area.x > 0.0 && projected_area.y > 0.0 && projected_area.z > 0.0);
        let plan = cadcraft_render::build_entities(d, [flag], &cadcraft_render::Options::default());
        assert!(plan.tris.as_chunks::<3>().0.iter().any(|p| (p[1] - p[0]).cross(p[2] - p[0]).abs() > 0.0));

        s.execute("properties.set", &json!({"handle": handles[1], "startZ":1, "endZ":1})).unwrap();
        assert!(flags(&s).is_empty());
        assert!(terrain_points(&s).iter().all(|p| p.z == 1.0));
        s.execute("u", &Value::Null).unwrap();
        assert_eq!(flags(&s), [Vec3::new(5.0, 5.0, 3.0)]);

        s.execute("move", &json!({"handle": handles[0], "delta": [2,0]})).unwrap();
        assert_eq!(flags(&s), [Vec3::new(6.0, 4.0, 3.0)]);
        assert!(terrain_points(&s).contains(&Vec3::new(2.0, 0.0, 1.0)));
        assert!(!terrain_points(&s).contains(&Vec3::new(0.0, 0.0, 1.0)));
    }

    #[test]
    fn opening_an_old_drawing_upgrades_its_wire_flags_in_place() {
        let mut d = Drawing::new_metric();
        let anchor = Vec3::new(12.0, 21.0, 5.0);
        let mast = anchor + Vec3::new(0.0, 2.0, 0.4);
        let path = vec![anchor, mast, anchor + Vec3::new(1.3, 1.56, 0.4), anchor + Vec3::new(0.0, 1.1, 0.4), mast];
        let marker = d
            .add(
                &Space::Model,
                Common { layer: FLAG_LAYER.into(), ..Common::default() },
                EntityKind::Polyline3d(cadcraft_doc::Polyline3d { points: path, closed: false }),
            )
            .unwrap();
        d.groups.push(Group { name: "Legacy Surface".into(), description: format!("{LINKED_TIN_PREFIX}100"), selectable: true, members: Vec::new() });
        d.groups.push(Group { name: "*TINFLAGS100".into(), description: format!("{FLAG_PREFIX}100"), selectable: false, members: vec![marker] });
        let mut s = Session::empty();
        s.open_drawing(d, "Legacy", None);
        assert_eq!(flags(&s), [anchor]);
        let flag = s.doc().unwrap().model.get(marker).unwrap();
        let EntityKind::PolyfaceMesh(mesh) = &flag.kind else { panic!("expected flag mesh") };
        assert!(mesh.vertices.iter().any(|p| p.z > anchor.z));
        assert_eq!(flag.common.layer, FLAG_LAYER);
        assert!(!s.state().unwrap().is_dirty());
    }

    #[test]
    fn a_surface_with_temporarily_insufficient_sources_recovers_when_undo_restores_them() {
        let mut s = Session::new();
        s.execute("line", &json!({"points": [[0,0],[10,10]]})).unwrap();
        s.execute("line", &json!({"points": [[0,10],[10,0]]})).unwrap();
        let handles: Vec<_> = s.doc().unwrap().model.iter().map(|e| e.handle.hex()).collect();
        s.execute(LINK, &json!({"handles": handles})).unwrap();
        let before = terrain_points(&s);
        s.execute("erase", &json!({"handle":handles[0]})).unwrap();
        assert!(terrain_points(&s).is_empty());
        s.execute("u", &Value::Null).unwrap();
        assert_eq!(terrain_points(&s), before);
    }

    #[test]
    fn removing_a_line_endpoint_leaves_the_other_point_and_rebuilds_the_surface() {
        let mut s = Session::new();
        let line = s.execute("line", &json!({"points":[[0,0,1],[4,0,3]]})).unwrap();
        let poly = s.execute("3dpoly", &json!({"points":[[0,4,2],[4,4,4],[2,9,5]],"closed":true})).unwrap();
        s.execute(LINK, &json!({"handles":[line["handles"][0],poly["handle"]],"name":"Site"})).unwrap();
        assert!(s.execute("tin.source.remove", &json!({"group":"Site","handle":poly["handle"],"vertex":1})).is_err());
        let before = terrain_points(&s);
        let kept = Vec3::new(4.0, 0.0, 3.0);
        let deleted = Vec3::new(0.0, 0.0, 1.0);
        assert!(before.contains(&deleted));
        s.execute("tin.source.remove", &json!({"group":"Site","handle":line["handles"][0],"vertex":0})).unwrap();
        let after = terrain_points(&s);
        assert!(after.contains(&kept));
        assert!(!after.contains(&deleted));
        let handle = Handle::parse_hex(line["handles"][0].as_str().unwrap()).unwrap();
        assert!(matches!(&s.doc().unwrap().model.get(handle).unwrap().kind, EntityKind::Point(point) if point.p == kept));
        s.execute("u", &Value::Null).unwrap();
        assert!(matches!(&s.doc().unwrap().model.get(handle).unwrap().kind, EntityKind::Line(line) if line.a == deleted && line.b == kept));
        assert_eq!(terrain_points(&s), before);
    }

    #[test]
    fn source_insert_and_extensions_preserve_the_order_and_interpolated_height() {
        let mut s = Session::new();
        let line = s.execute("line", &json!({"points":[[0,0,2],[8,0,10]]})).unwrap();
        let support = s.execute("line", &json!({"points":[[0,8,2],[8,8,10]]})).unwrap();
        let source = &line["handles"][0];
        s.execute(LINK, &json!({"handles":[source,support["handles"][0]],"name":"Site"})).unwrap();
        s.execute("tin.source.insert", &json!({"group":"Site","handle":source,"segment":0,"fraction":0.25})).unwrap();
        s.execute("tin.source.extend", &json!({"group":"Site","handle":source,"vertex":0,"position":[-2,0]})).unwrap();
        s.execute("tin.source.extend", &json!({"group":"Site","handle":source,"vertex":3,"position":[10,0,12]})).unwrap();
        assert!(s.execute("tin.source.extend", &json!({"group":"Site","handle":source,"vertex":1,"position":[-4,0]})).is_err());
        let h = Handle::parse_hex(source.as_str().unwrap()).unwrap();
        let EntityKind::Polyline3d(poly) = &s.doc().unwrap().model.get(h).unwrap().kind else { panic!("expected a 3D source chain") };
        assert_eq!(poly.points, [(-2., 2.), (0., 2.), (2., 4.), (8., 10.), (10., 12.)].map(|(x, z)| Vec3::new(x, 0., z)));
        assert!(terrain_points(&s).contains(&poly.points[2]));
    }

    #[test]
    fn a_drawn_source_is_undoable_and_a_crossing_removal_survives_rebuilding() {
        use crate::Input;

        let mut s = Session::new();
        let south = s.execute("line", &json!({"points":[[0,0,1],[8,0,1]]})).unwrap();
        let north = s.execute("line", &json!({"points":[[0,8,1],[8,8,1]]})).unwrap();
        s.execute(LINK, &json!({"handles":[south["handles"][0],north["handles"][0]],"name":"Site"})).unwrap();
        let count = s.doc().unwrap().entity_count();
        let start = |s: &mut Session| {
            s.start(DRAW_SOURCE).unwrap();
            s.input(Input::Text("Site".into())).unwrap();
            s.cmdline("#0,6,1").unwrap();
            s.cmdline("#8,6,1").unwrap();
        };
        start(&mut s);
        s.cancel();
        assert_eq!(s.doc().unwrap().entity_count(), count);
        start(&mut s);
        s.input(Input::Enter).unwrap();
        assert!(!s.prompting());
        let source_count = |s: &Session| {
            sources(s.doc().unwrap(), &format!("{SOURCE_PREFIX}{}", linked_id(s.doc().unwrap(), &json!({"group":"Site"}), LINK).unwrap())).len()
        };
        assert_eq!(source_count(&s), 3);
        s.execute("u", &Value::Null).unwrap();
        assert_eq!(source_count(&s), 2);
        let added = s.execute(DRAW_SOURCE, &json!({"group":"Site","points":[[0,6,1],[8,6,1]]})).unwrap();
        let before = terrain_points(&s).len();
        assert!(s.execute(REMOVE_CROSSED, &json!({"group":"Site","points":[[-1,0],[9,0]]})).is_err());
        let removed = s.execute(REMOVE_CROSSED, &json!({"group":"Site","points":[[-1,2],[9,2]]})).unwrap();
        assert!(removed["removed"].as_u64().unwrap() >= 2);
        let remaining = terrain_points(&s);
        assert!(!remaining.is_empty() && remaining.len() < before && remaining.iter().all(|p| p.y >= 6.0));
        s.execute("tin.source.move", &json!({"group":"Site","handle":added["handle"],"vertex":0,"position":[0,6,2]})).unwrap();
        let after = terrain_points(&s);
        assert_eq!(after.len(), remaining.len());
        assert!(after.iter().all(|p| p.y >= 6.0) && after.iter().any(|p| p.z > 1.0));
        s.execute("u", &Value::Null).unwrap();
        s.execute("u", &Value::Null).unwrap();
        assert_eq!(terrain_points(&s).len(), before);
    }

    #[test]
    fn can_preview_and_restore_one_of_two_overlapping_cuts() {
        let mut s = Session::new();
        let mut handles = Vec::new();
        for y in [0, 4, 8] {
            let line = s.execute("line", &json!({"points":[[0,y,1],[8,y,1]]})).unwrap();
            handles.push(line["handles"][0].clone());
        }
        s.execute(LINK, &json!({"handles":handles,"name":"Site"})).unwrap();
        let original = terrain_points(&s).len() / 3;
        let first = s.execute(REMOVE_CROSSED, &json!({"group":"Site","points":[[-1,2],[9,2]]})).unwrap();
        s.execute(REMOVE_CROSSED, &json!({"group":"Site","points":[[4,3.6],[4,8.2]]})).unwrap();
        let id = linked_id(s.doc().unwrap(), &json!({"group":"Site"}), LINK).unwrap();
        let cut = fence::cut_handles(s.doc().unwrap(), id)[0].hex();
        let before = terrain_points(&s).len() / 3;
        let preview = s.execute("tin.cuts.preview", &json!({"group":"Site","cut":cut})).unwrap();
        let recoverable = preview["triangles"].as_array().unwrap().len();
        assert!(recoverable > 0 && recoverable < first["removed"].as_u64().unwrap() as usize);
        assert_eq!(terrain_points(&s).len() / 3, before);
        s.execute("tin.cuts.restore", &json!({"group":"Site","cut":cut})).unwrap();
        let after = terrain_points(&s).len() / 3;
        assert_eq!(after, before + recoverable);
        assert!(after < original);
        s.execute("u", &Value::Null).unwrap();
        assert_eq!(terrain_points(&s).len() / 3, before);
    }

    #[test]
    fn each_removal_line_is_undoable_and_a_restoration_only_reveals_crossed_faces() {
        use crate::Input;

        let mut s = Session::new();
        let handles: Vec<_> = [0, 4, 8, 12].map(|y| s.execute("line", &json!({"points":[[0,y,1],[8,y,1]]})).unwrap()["handles"][0].clone()).into();
        s.execute(LINK, &json!({"handles":handles,"name":"Site"})).unwrap();
        let count = |s: &Session| terrain_points(s).len() / 3;
        let original = count(&s);
        s.start(REMOVE_CROSSED).unwrap();
        s.input(Input::Text("Site".into())).unwrap();
        let mut counts = Vec::new();
        for y in [2, 6] {
            for x in [-1, 9] {
                s.cmdline(&format!("#{x},{y},1")).unwrap();
            }
            s.input(Input::Enter).unwrap();
            assert!(s.prompting());
            counts.push(count(&s));
        }
        assert!(original > counts[0] && counts[0] > counts[1]);
        s.cancel();
        s.execute("u", &Value::Null).unwrap();
        assert_eq!(count(&s), counts[0]);

        let path = json!({"group":"Site","points":[[4,0.3],[4,1.5]]});
        let restored = s.execute(RESTORE_CROSSED, &path).unwrap()["restored"].as_u64().unwrap() as usize;
        assert!(restored > 0 && restored < original - counts[0]);
        assert_eq!(count(&s), counts[0] + restored);
        s.execute(REMOVE_CROSSED, &path).unwrap();
        assert_eq!(count(&s), counts[0]);
        s.execute("u", &Value::Null).unwrap();
        assert_eq!(count(&s), counts[0] + restored);
    }

    #[test]
    fn snapping_to_an_exact_point_on_a_bulged_segment_preserves_the_curve() {
        let mut s = Session::new();
        let source = s.execute("pline", &json!({"vertices":[{"p":[0,0],"bulge":0.5},[10,0]]})).unwrap();
        let support = s.execute("point", &json!({"at":[5,8,0]})).unwrap();
        s.execute(LINK, &json!({"handles":[source["handle"],support["handles"][0]],"name":"Site"})).unwrap();
        let end = Vec2::new(10.0, 0.0);
        let (curve, _) = cadcraft_geom::bulge_to_arc(Vec2::ZERO, end, 0.5).unwrap();
        let target = curve.at(0.35);
        s.execute("tin.source.insert", &json!({"group":"Site","handle":source["handle"],"segment":0,"position":[target.x,target.y,0]})).unwrap();
        let handle = Handle::parse_hex(source["handle"].as_str().unwrap()).unwrap();
        let EntityKind::LwPolyline(out) = &s.doc().unwrap().model.get(handle).unwrap().kind else { panic!("expected a planar source polyline") };
        let rebuilt = cadcraft_geom::Polyline { vertices: out.vertices.clone(), closed: false };
        assert_eq!(rebuilt.vertices[1].p, target);
        assert!((rebuilt.len() - curve.len()).abs() < 1e-9);
        assert!(rebuilt.segments().iter().all(|s| matches!(s, cadcraft_geom::Segment::Arc { .. })));
    }
}
