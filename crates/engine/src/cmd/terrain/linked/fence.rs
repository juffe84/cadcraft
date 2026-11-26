//! Apply ordered paths that hide and restore terrain triangles in plan.
use cadcraft_doc::{Common, Drawing, EntityKind, Group, Handle, Polyline3d, Space};
use cadcraft_geom::{Vec2, Vec3, terrain::Triangle};
use rstar::{
    RTree, RTreeObject,
    primitives::{GeomWithData, Rectangle},
};
use serde_json::{Value, json};
use std::collections::HashSet;

use super::path::{REMOVE_CROSSED, RESTORE_CROSSED};
use super::{Mesh, SOURCE_PREFIX, build, geometry_changed, linked_id, model_space, sources};
use crate::{
    Result, Session,
    cmd::{CommandSpec, bad, str_param},
};

const CUT_PREFIX: &str = "CADCraft TIN cuts ";
const RESTORE_PREFIX: &str = "CADCraft TIN restores ";
const MAX_EDGES: usize = 50_000;

const PREVIEW: &str = "tin.cuts.preview";
const RESTORE: &str = "tin.cuts.restore";

pub(super) fn preview_spec() -> CommandSpec {
    CommandSpec::new(PREVIEW, "Show Removed Terrain Triangles", preview)
        .params("{group: linked terrain name, cut?: removal line handle} -> {triangles: [[{x,y,z},...], ...]}")
        .enabled(model_space)
        .noundo()
}

pub(super) fn restore_spec() -> CommandSpec {
    CommandSpec::new(RESTORE, "Restore Terrain Triangles", restore)
        .params("{group: linked terrain name, cut?: removal line handle (latest by default)}")
        .enabled(model_space)
}

/// The existing cuts group is the ordered history. Restore paths also belong to
/// a second group that identifies their action. Old files have only cuts.
pub(super) fn cut_handles(d: &Drawing, id: &str) -> Vec<Handle> {
    let restores: HashSet<_> = sources(d, &format!("{RESTORE_PREFIX}{id}")).iter().copied().collect();
    sources(d, &format!("{CUT_PREFIX}{id}")).iter().copied().filter(|h| !restores.contains(h)).collect()
}

fn selected_cut(d: &Drawing, id: &str, p: &Value, command: &str) -> Result<Option<Handle>> {
    if p.get("cut").is_none_or(Value::is_null) {
        return Ok(None);
    }
    str_param(p, "cut")
        .and_then(Handle::parse_hex)
        .filter(|h| cut_handles(d, id).contains(h))
        .map(Some)
        .ok_or_else(|| bad(command, "Choose a removal line from this surface."))
}

fn preview(s: &mut Session, p: &Value) -> Result<Value> {
    let d = s.doc()?;
    let id = linked_id(d, p, PREVIEW)?;
    let selected = selected_cut(d, id, p, PREVIEW)?;
    let history = history_index(d, id, None);
    let triangles = if history.size() == 0 {
        Vec::new()
    } else {
        let without = selected.map(|cut| history_index(d, id, Some(cut)));
        let mesh = build(d, sources(d, &format!("{SOURCE_PREFIX}{id}")))?;
        mesh.triangles.into_iter().filter(|&tri| hidden(&history, tri) && without.as_ref().is_none_or(|index| !hidden(index, tri))).collect()
    };
    Ok(json!({"triangles": triangles}))
}

fn restore(s: &mut Session, p: &Value) -> Result<Value> {
    let d = s.doc()?;
    let id = linked_id(d, p, RESTORE)?;
    let cut = selected_cut(d, id, p, RESTORE)?
        .or_else(|| cut_handles(d, id).last().copied())
        .ok_or_else(|| bad(RESTORE, "This surface has no saved removal lines."))?;
    let tag = format!("{CUT_PREFIX}{id}");
    let d = s.doc_mut()?;
    if let Some(at) = d.groups.iter().position(|g| g.description == tag) {
        d.groups[at].members.retain(|&h| h != cut);
        if d.groups[at].members.is_empty() {
            d.groups.remove(at);
        }
    }
    d.model.remove(cut);
    Ok(json!({"cut": cut.hex()}))
}

type Edge = [Vec2; 2];
#[derive(Clone, Copy)]
struct PathEdge {
    points: Edge,
    order: usize,
    restore: bool,
}

type Index = RTree<GeomWithData<Rectangle<[f64; 2]>, PathEdge>>;

pub(super) fn remove(s: &mut Session, group: &str, path: Vec<Vec3>) -> Result<Value> {
    let doc = s.doc()?;
    let surface = doc.groups.iter().find(|g| g.name.eq_ignore_ascii_case(group)).ok_or_else(|| bad(REMOVE_CROSSED, "Choose a terrain surface."))?;
    let cutter = line_index(&path);
    if cutter.size() == 0 {
        return Err(bad(REMOVE_CROSSED, "The removal line needs two distinct points in plan."));
    }
    let crossed = surface
        .members
        .iter()
        .map(|&handle| {
            let face = doc
                .model
                .get(handle)
                .and_then(|e| super::super::triangle(&e.kind))
                .ok_or_else(|| bad(REMOVE_CROSSED, "Choose a terrain group containing only triangles."))?;
            Ok(crosses_face(&cutter, face).then_some(handle))
        })
        .collect::<Result<Vec<_>>>()?;
    let removed = crossed.iter().flatten().count();
    if removed == 0 {
        return Err(bad(REMOVE_CROSSED, "The removal line did not cross any triangles."));
    }

    if let Some(id) = surface.linked_terrain_id().map(str::to_owned) {
        store_path(s, &id, path, false, REMOVE_CROSSED)?;
    } else {
        let name = surface.name.clone();
        let removed_handles: HashSet<_> = crossed.into_iter().flatten().collect();
        let mut next = doc.clone();
        if let Some(surface) = next.groups.iter_mut().find(|g| g.name == name) {
            surface.members.retain(|h| !removed_handles.contains(h));
        }
        for handle in removed_handles {
            next.model.remove(handle);
        }
        *s.doc_mut()? = next;
    }

    Ok(json!({"removed": removed}))
}

pub(super) fn restore_crossed(s: &mut Session, group: &str, path: Vec<Vec3>) -> Result<Value> {
    let doc = s.doc()?;
    let id = linked_id(doc, &json!({"group": group}), RESTORE_CROSSED)?;
    let line = line_index(&path);
    if line.size() == 0 {
        return Err(bad(RESTORE_CROSSED, "The restoration line needs two distinct points in plan."));
    }
    let history = history_index(doc, id, None);
    let mesh = build(doc, sources(doc, &format!("{SOURCE_PREFIX}{id}")))?;
    let restored = mesh.triangles.into_iter().filter(|&tri| crosses_face(&line, tri) && hidden(&history, tri)).count();
    if restored == 0 {
        return Err(bad(RESTORE_CROSSED, "The line did not cross any removed triangles."));
    }
    let id = id.to_owned();
    store_path(s, &id, path, true, RESTORE_CROSSED)?;
    Ok(json!({"restored":restored}))
}

fn store_path(s: &mut Session, id: &str, path: Vec<Vec3>, restore: bool, command: &str) -> Result<()> {
    let tag = format!("{CUT_PREFIX}{id}");
    let doc = s.doc()?;
    let occupied = sources(doc, &tag).iter().filter_map(|&h| cut_points(doc, h)).fold(0usize, |n, p| n.saturating_add(p.len().saturating_sub(1)));
    if occupied.saturating_add(path.len() - 1) > MAX_EDGES {
        return Err(bad(command, "The surface has too many removal or restoration line segments."));
    }
    let doc = s.doc_mut()?;
    let common = Common { visible: false, ..Common::default() };
    let handle = doc.add(&Space::Model, common, EntityKind::Polyline3d(Polyline3d { points: path, closed: false }))?;
    add_to_group(doc, tag, format!("*TINCUTS{id}"), handle);
    if restore {
        add_to_group(doc, format!("{RESTORE_PREFIX}{id}"), format!("*TINRESTORES{id}"), handle);
    }
    Ok(())
}

fn add_to_group(doc: &mut Drawing, tag: String, name: String, handle: Handle) {
    if let Some(group) = doc.groups.iter_mut().find(|g| g.description == tag) {
        group.members.push(handle);
    } else {
        doc.groups.push(Group { name, description: tag, selectable: false, members: vec![handle] });
    }
}

fn edges(points: &[Vec3]) -> impl Iterator<Item = Edge> + '_ {
    points.windows(2).filter_map(|edge| {
        let [a, b] = [edge[0].xy(), edge[1].xy()];
        (a.is_finite() && b.is_finite() && a != b).then_some([a, b])
    })
}

fn index(edges: impl Iterator<Item = PathEdge>) -> Index {
    RTree::bulk_load(edges.map(|edge| GeomWithData::new(bounds(edge.points), edge)).collect())
}

fn line_index(path: &[Vec3]) -> Index {
    index(edges(path).map(|points| PathEdge { points, order: 0, restore: false }))
}

fn bounds(points: impl IntoIterator<Item = Vec2>) -> Rectangle<[f64; 2]> {
    let b = cadcraft_geom::Bounds2::from_points(points);
    Rectangle::from_corners([b.min.x, b.min.y], [b.max.x, b.max.y])
}

fn crosses_face(cutter: &Index, face: Triangle) -> bool {
    if !face.iter().all(|p| p.is_finite()) {
        return false;
    }
    let tri = face.map(Vec3::xy);
    cutter.locate_in_envelope_intersecting(bounds(tri).envelope()).any(|edge| crosses_interior(tri, edge.data.points))
}

/// Clip the segment against the triangle's open half-planes. Merely touching
/// an edge or a vertex should leave the adjacent faces intact.
fn crosses_interior([a, b, c]: [Vec2; 3], [p, q]: Edge) -> bool {
    let area = (b - a).cross(c - a);
    if area == 0.0 {
        return false;
    }
    let orientation = area.signum();
    let mut enter: f64 = 0.0;
    let mut exit: f64 = 1.0;
    for [start, end] in [[a, b], [b, c], [c, a]] {
        let side = end - start;
        let inset = side.len() * 1e-9;
        let signed = |point| orientation * side.cross(point - start) - inset;
        let (f, g) = (signed(p), signed(q));
        if f <= 0.0 && g <= 0.0 {
            return false;
        }
        if f <= 0.0 {
            enter = enter.max(f / (f - g));
        } else if g <= 0.0 {
            exit = exit.min(f / (f - g));
        }
    }
    enter < exit
}

fn cut_points(d: &Drawing, handle: Handle) -> Option<&[Vec3]> {
    match &d.model.get(handle)?.kind {
        EntityKind::Polyline3d(path) => Some(path.points.as_slice()),
        _ => None,
    }
}

fn history_index(d: &Drawing, id: &str, skip: Option<Handle>) -> Index {
    let restores: HashSet<_> = sources(d, &format!("{RESTORE_PREFIX}{id}")).iter().copied().collect();
    index(
        sources(d, &format!("{CUT_PREFIX}{id}"))
            .iter()
            .copied()
            .enumerate()
            .filter(|&(_, h)| Some(h) != skip)
            .filter_map(|(order, handle)| Some((order, restores.contains(&handle), cut_points(d, handle)?)))
            .flat_map(|(order, restore, path)| edges(path).map(move |points| PathEdge { points, order, restore }))
            .take(MAX_EDGES),
    )
}

fn hidden(history: &Index, face: Triangle) -> bool {
    if !face.iter().all(|p| p.is_finite()) {
        return false;
    }
    let tri = face.map(Vec3::xy);
    history
        .locate_in_envelope_intersecting(bounds(tri).envelope())
        .filter(|edge| crosses_interior(tri, edge.data.points))
        .max_by_key(|edge| edge.data.order)
        .is_some_and(|edge| !edge.data.restore)
}

pub(super) fn changed(old: &Drawing, d: &Drawing, id: &str) -> bool {
    let tag = format!("{CUT_PREFIX}{id}");
    let now = sources(d, &tag);
    let restores = format!("{RESTORE_PREFIX}{id}");
    now != sources(old, &tag) || sources(old, &restores) != sources(d, &restores) || now.iter().any(|&handle| geometry_changed(old, d, handle))
}

pub(super) fn mask(d: &Drawing, id: &str, mesh: &mut Mesh) {
    let history = history_index(d, id, None);
    if history.size() != 0 {
        mesh.triangles.retain(|&tri| !hidden(&history, tri));
    }
}
