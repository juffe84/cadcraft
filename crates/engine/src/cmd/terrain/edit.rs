//! Direct editing of the model-space triangles that make up an existing surface.
use std::collections::HashSet;

use cadcraft_doc::{Drawing, EntityKind, Face3d, Group, Handle, Line, TIN_GROUP_DESCRIPTION};
use cadcraft_geom::{Bounds2, Vec2, Vec3, terrain};
use serde_json::{Value, json};

use super::{MAX_VERTICES, SelOutcome, SelectPhase, model_space, triangle};
use crate::cmd::{CommandSpec, Factory, Run, bad, point_value, point3_value, str_param, targets};
use crate::{Accept, Input, Interactive, Prompt, Result, Session, Step};

const MAX_FACES: usize = MAX_VERTICES * 2;

#[derive(Clone, Copy)]
enum Edit {
    Elevation,
    Move,
    Remove,
    Clip,
    Hole,
    Swap,
    Breakline,
}

impl Edit {
    fn id(self) -> &'static str {
        match self {
            Self::Elevation => "tin.elevation",
            Self::Move => MOVE_VERTEX,
            Self::Remove => "tin.removetriangles",
            Self::Clip => "tin.clip",
            Self::Hole => "tin.cuthole",
            Self::Swap => "tin.swapedge",
            Self::Breakline => "tin.breakline",
        }
    }

    fn menu(self) -> &'static [&'static str] {
        match self {
            Self::Elevation => &["Draw", "Terrain", "Edit Surface", "Set Vertex Elevation"],
            Self::Move => &["Draw", "Terrain", "Edit Surface", "Move Surface Vertex"],
            Self::Remove => &["Draw", "Terrain", "Edit Surface", "Remove Surface Triangles"],
            Self::Clip => &["Draw", "Terrain", "Edit Surface", "Clip Surface to Boundary"],
            Self::Hole => &["Draw", "Terrain", "Edit Surface", "Cut Hole in Surface"],
            Self::Swap => &["Draw", "Terrain", "Edit Surface", "Swap Triangle Edge"],
            Self::Breakline => &["Draw", "Terrain", "Edit Surface", "Apply 3D Breakline"],
        }
    }

    fn params(self) -> &'static str {
        match self {
            Self::Elevation => "{group: terrain group name, at: [x,y], elevation: number}",
            Self::Move => "{group: terrain group name, at: [x,y], to: [x,y] or [x,y,z]}",
            Self::Remove => "{group: terrain group name, handles: triangle handles}",
            Self::Clip | Self::Hole => "{group: terrain group name, points?: [[x,y]], handle?: closed polyline}",
            Self::Swap => "{group: terrain group name, handles: two adjacent triangle handles}",
            Self::Breakline => "{group: terrain group name, points?: [[x,y,z]], handle?: LINE or 3DPOLYLINE}",
        }
    }

    fn reject(self, reason: impl Into<String>) -> crate::EngineError {
        bad(self.id(), reason)
    }
}

pub(crate) const MOVE_VERTEX: &str = "tin.movevertex";

pub(super) fn specs() -> [CommandSpec; 7] {
    let cmd = |edit: Edit, run: Run, factory: Factory| {
        CommandSpec::new(edit.id(), edit.menu()[3], run).menu(edit.menu()).params(edit.params()).enabled(model_space).interactive(factory)
    };
    [
        cmd(Edit::Elevation, |s, p| run(s, Edit::Elevation, p), |_| Ok(Box::new(Editor::new(Edit::Elevation)))),
        cmd(Edit::Move, |s, p| run(s, Edit::Move, p), |_| Ok(Box::new(Editor::new(Edit::Move)))),
        cmd(Edit::Remove, |s, p| run(s, Edit::Remove, p), |_| Ok(Box::new(Editor::new(Edit::Remove)))),
        cmd(Edit::Clip, |s, p| run(s, Edit::Clip, p), |_| Ok(Box::new(Editor::new(Edit::Clip)))),
        cmd(Edit::Hole, |s, p| run(s, Edit::Hole, p), |_| Ok(Box::new(Editor::new(Edit::Hole)))),
        cmd(Edit::Swap, |s, p| run(s, Edit::Swap, p), |_| Ok(Box::new(Editor::new(Edit::Swap)))),
        cmd(Edit::Breakline, |s, p| run(s, Edit::Breakline, p), |_| Ok(Box::new(Editor::new(Edit::Breakline)))),
    ]
}

fn group<'a>(d: &'a Drawing, edit: Edit, name: &str) -> Result<&'a Group> {
    d.groups
        .iter()
        .find(|g| g.name.eq_ignore_ascii_case(name))
        .filter(|g| !g.members.is_empty() && g.members.len() <= MAX_FACES)
        .filter(|g| g.members.iter().all(|&h| d.model.get(h).is_some_and(|e| triangle(&e.kind).is_some())))
        .ok_or_else(|| edit.reject("Choose a model-space terrain group of at most 100,000 triangles."))
}

fn read_triangles(d: &Drawing, surface: &Group, edit: Edit) -> Result<Vec<terrain::Triangle>> {
    surface
        .members
        .iter()
        .map(|&h| {
            let face =
                d.model.get(h).and_then(|e| triangle(&e.kind)).ok_or_else(|| edit.reject("The terrain group contains a non-triangle object."))?;
            if face.iter().any(|p| !p.is_finite() || p.x.abs().max(p.y.abs()).max(p.z.abs()) > 1e12) {
                return Err(edit.reject("A surface vertex has coordinates outside the drawing range."));
            }
            Ok(face)
        })
        .collect()
}

fn run(s: &mut Session, edit: Edit, p: &Value) -> Result<Value> {
    let d = s.doc()?;
    let name = str_param(p, "group").ok_or_else(|| edit.reject("Specify a terrain group."))?;
    let surface = group(d, edit, name)?;
    if surface.linked_terrain_id().is_some() {
        return Err(edit.reject("Edit the source lines or points to update a linked terrain surface."));
    }
    let handles = surface.members.as_slice();
    let triangles = read_triangles(d, surface, edit)?;
    match edit {
        Edit::Elevation | Edit::Move => {
            let at = p.get("at").and_then(point_value).ok_or_else(|| edit.reject("Specify a terrain vertex as [x,y]."))?;
            let from = nearest_vertex(&triangles, at, vertex_tolerance(at), edit)?;
            let to = match edit {
                Edit::Elevation => {
                    from.xy().to3(p.get("elevation").and_then(Value::as_f64).ok_or_else(|| edit.reject("Specify the new elevation."))?)
                }
                _ => {
                    let value = p.get("to").and_then(Value::as_array).ok_or_else(|| edit.reject("Specify the new position as [x,y] or [x,y,z]."))?;
                    match value.len() {
                        2 => p.get("to").and_then(point_value).map(|xy| xy.to3(from.z)),
                        3 => p.get("to").and_then(point3_value),
                        _ => None,
                    }
                    .ok_or_else(|| edit.reject("Specify the new position as [x,y] or [x,y,z]."))?
                }
            };
            move_vertex(s, edit, handles.to_vec(), triangles, from, to)
        }
        Edit::Remove => {
            let picked: HashSet<_> = targets(s, p)?.into_iter().collect();
            if picked.is_empty() {
                return Err(edit.reject("Pick the triangles to remove."));
            }
            let members: HashSet<_> = handles.iter().copied().collect();
            if !picked.is_subset(&members) {
                return Err(edit.reject("Every picked triangle must belong to the chosen surface."));
            }
            let retained: Vec<_> = triangles.into_iter().enumerate().filter(|(i, _)| !picked.contains(&handles[*i])).collect();
            let n = handles.len();
            replace_triangles(s, edit, handles.to_vec(), retained, n)
        }
        Edit::Swap => {
            let selected = targets(s, p)?;
            let [first, second] = selected.as_slice() else { return Err(edit.reject("Pick two adjacent triangles.")) };
            let index = |h| handles.iter().position(|id| id == h).ok_or_else(|| edit.reject("Both triangles must belong to the selected surface."));
            let (a, b) = (index(first)?, index(second)?);
            let swapped = terrain::swap_diagonal(triangles[a], triangles[b]).map_err(|error| edit.reject(error))?;
            let mut after = triangles;
            after[a] = swapped[0];
            after[b] = swapped[1];
            check_overlap(edit, &after, &[a, b])?;
            let count = handles.len();
            replace_triangles(s, edit, handles.to_vec(), after.into_iter().enumerate().collect(), count)
        }
        Edit::Breakline => {
            let line = breakline(s, edit, p)?;
            let n = triangles.len();
            let retained = terrain::imprint_breakline(&triangles, &line, MAX_FACES).map_err(|error| edit.reject(error))?;
            replace_triangles(s, edit, handles.to_vec(), retained, n)
        }
        Edit::Clip | Edit::Hole => {
            let outline = outline(s, edit, p)?;
            let n = triangles.len();
            let retained = terrain::clip_surface(&triangles, &outline, matches!(edit, Edit::Clip), MAX_FACES).map_err(|error| edit.reject(error))?;
            replace_triangles(s, edit, handles.to_vec(), retained, n)
        }
    }
}

fn breakline(s: &Session, edit: Edit, p: &Value) -> Result<Vec<Vec3>> {
    if let Some(points) = p.get("points") {
        let points = points
            .as_array()
            .filter(|p| (2..=terrain::MAX_BOUNDARY_POINTS).contains(&p.len()))
            .ok_or_else(|| edit.reject("A breakline needs 2 to 1,024 XYZ points."))?;
        return points
            .iter()
            .map(|p| {
                p.as_array()
                    .filter(|p| p.len() == 3)
                    .and_then(|_| point3_value(p))
                    .ok_or_else(|| edit.reject("Enter all three coordinates of each breakline point."))
            })
            .collect();
    }
    let handles = targets(s, p)?;
    let [handle] = handles.as_slice() else { return Err(edit.reject("Select one line or 3D polyline.")) };
    match s.doc()?.model.get(*handle).map(|e| &e.kind) {
        Some(EntityKind::Line(line)) => Ok(vec![line.a, line.b]),
        Some(EntityKind::Polyline3d(poly)) if poly.points.len() <= terrain::MAX_BOUNDARY_POINTS => {
            let mut points = poly.points.clone();
            if poly.closed
                && points.first() != points.last()
                && let Some(&first) = points.first()
            {
                points.push(first);
            }
            Ok(points)
        }
        _ => Err(edit.reject("Select a model-space line or 3D polyline.")),
    }
}

fn outline(s: &Session, edit: Edit, p: &Value) -> Result<Vec<Vec2>> {
    let mut points = if let Some(input) = p.get("points") {
        let points = input
            .as_array()
            .filter(|points| points.len() <= terrain::MAX_BOUNDARY_POINTS + 1)
            .ok_or_else(|| edit.reject("Specify up to 1,024 boundary vertices."))?;
        points.iter().map(|v| point_value(v).ok_or_else(|| edit.reject("A boundary point must be [x,y]."))).collect::<Result<Vec<_>>>()?
    } else {
        let selected = targets(s, p)?;
        let [handle] = selected.as_slice() else { return Err(edit.reject("Select one closed boundary polyline.")) };
        let d = s.doc()?;
        let entity = d.model.get(*handle).ok_or_else(|| edit.reject("Select a model-space boundary polyline."))?;
        match &entity.kind {
            EntityKind::LwPolyline(p) if p.closed && p.vertices.len() <= terrain::MAX_BOUNDARY_POINTS + 1 => {
                if p.vertices.iter().any(|v| v.bulge != 0.0) || entity.common.extrusion != Vec3::Z {
                    return Err(edit.reject("Use a horizontal polyline with straight segments for the boundary."));
                }
                p.vertices.iter().map(|v| v.p).collect()
            }
            EntityKind::Polyline3d(p) if p.closed && p.points.len() <= terrain::MAX_BOUNDARY_POINTS + 1 => p.points.iter().map(|p| p.xy()).collect(),
            _ => return Err(edit.reject("Select a closed 2D or 3D polyline of up to 1,024 vertices.")),
        }
    };
    if points.len() > 1 && points.first() == points.last() {
        points.pop();
    }
    Ok(points)
}

fn vertex_tolerance(p: Vec2) -> f64 {
    (f64::EPSILON * p.x.abs().max(p.y.abs()) * 4.0).max(1e-8)
}

fn nearest_vertex(triangles: &[terrain::Triangle], at: Vec2, radius: f64, edit: Edit) -> Result<Vec3> {
    triangles
        .iter()
        .flatten()
        .copied()
        .map(|p| ((p.xy() - at).len2(), p))
        .filter(|(dist, _)| *dist <= radius * radius)
        .min_by(|(a, _), (b, _)| a.total_cmp(b))
        .map(|(_, p)| p)
        .ok_or_else(|| edit.reject("No terrain vertex was found at that location. Use the Endpoint object snap."))
}

fn move_vertex(s: &mut Session, edit: Edit, handles: Vec<Handle>, triangles: Vec<terrain::Triangle>, from: Vec3, to: Vec3) -> Result<Value> {
    if !to.is_finite() || to.x.abs().max(to.y.abs()).max(to.z.abs()) > 1e12 {
        return Err(edit.reject("The new vertex coordinates are outside the drawing range."));
    }
    if from == to {
        return Ok(json!({ "updated": 0, "message": "The terrain vertex stayed at the same position." }));
    }
    let horizontal = from.xy() != to.xy();
    if horizontal && triangles.iter().flatten().any(|p| (p.xy() - to.xy()).len() <= vertex_tolerance(to.xy())) {
        return Err(edit.reject("Another terrain vertex already occupies the new location."));
    }
    let mut updated = triangles.clone();
    let mut changed = Vec::new();
    for (i, triangle) in updated.iter_mut().enumerate() {
        if triangle.iter().any(|p| p.xy() == from.xy()) {
            for point in triangle.iter_mut().filter(|p| p.xy() == from.xy()) {
                if *point != from {
                    return Err(edit.reject("Faces at that XY location have conflicting elevations."));
                }
                *point = to;
            }
            if horizontal && terrain::signed_area2(*triangle) * terrain::signed_area2(triangles[i]) <= 0.0 {
                return Err(edit.reject("The move would flatten or invert a triangle. Move the vertex a shorter distance."));
            }
            changed.push(i);
        }
    }
    if horizontal {
        check_overlap(edit, &updated, &changed)?;
    }
    ensure_editable(s.doc()?, edit, &changed.iter().map(|&i| handles[i]).collect())?;
    let d = s.doc_mut()?;
    for &i in &changed {
        d.model.modify(handles[i], |entity| {
            if let EntityKind::Face3d(face) = &mut entity.kind {
                let [a, b, c] = updated[i];
                face.corners = [a, b, c, c];
            }
        });
    }
    Ok(json!({
        "updated": changed.len(),
        "point": [to.x, to.y, to.z],
        "message": format!("Updated {} triangles at {:.6}, {:.6}, {:.6}.", changed.len(), to.x, to.y, to.z)
    }))
}

fn check_overlap(edit: Edit, triangles: &[terrain::Triangle], changed: &[usize]) -> Result<()> {
    use rstar::{
        RTree, RTreeObject,
        primitives::{GeomWithData, Rectangle},
    };
    let bounds = |t: terrain::Triangle| {
        let b = Bounds2::from_points(t.map(Vec3::xy));
        Rectangle::from_corners([b.min.x, b.min.y], [b.max.x, b.max.y])
    };
    let tree = RTree::bulk_load(triangles.iter().enumerate().map(|(i, &t)| GeomWithData::new(bounds(t), i)).collect());
    let mut pairs = 0;
    for &i in changed {
        let a = triangles[i];
        for neighbor in tree.locate_in_envelope_intersecting(bounds(a).envelope()).filter(|candidate| candidate.data != i) {
            pairs += 1;
            if pairs > 2_000_000 {
                return Err(edit.reject("The terrain has too many overlapping candidate triangles to check the move."));
            }
            let b = triangles[neighbor.data];
            let epsilon = terrain::signed_area2(a).abs().min(terrain::signed_area2(b).abs()) * 1e-8 + 1e-12;
            if terrain::triangle_earthwork(a, b).area > epsilon {
                return Err(edit.reject("The move would make terrain triangles overlap."));
            }
        }
    }
    Ok(())
}

fn ensure_editable(d: &Drawing, edit: Edit, changed: &HashSet<Handle>) -> Result<()> {
    for &h in changed {
        if d.model.get(h).and_then(|e| d.layer(&e.common.layer)).is_some_and(|layer| layer.locked) {
            return Err(edit.reject("The edit touches a face on a locked layer."));
        }
    }
    let mut seen = HashSet::new();
    for group in d.groups.iter().filter(|g| g.is_terrain_surface()) {
        if group.members.iter().filter(|h| changed.contains(h)).any(|&h| !seen.insert(h)) {
            return Err(edit.reject("A triangle is shared by multiple terrain surfaces."));
        }
    }
    Ok(())
}

fn replace_triangles(
    s: &mut Session,
    edit: Edit,
    handles: Vec<Handle>,
    retained: Vec<(usize, terrain::Triangle)>,
    source_len: usize,
) -> Result<Value> {
    if retained.is_empty() {
        return Err(edit.reject("The operation would leave the surface empty. Check the boundary or triangle selection."));
    }
    let d = s.doc()?;
    let final_count = retained.len();
    let mut pieces = vec![Vec::new(); source_len];
    for (i, triangle) in retained {
        pieces[i].push(triangle);
    }
    let original = |h| d.model.get(h).and_then(|e| triangle(&e.kind));
    let affected: HashSet<_> = handles
        .iter()
        .copied()
        .zip(&pieces)
        .filter(|&(h, triangles)| !matches!(triangles.as_slice(), [t] if Some(*t) == original(h)))
        .map(|(h, _)| h)
        .collect();
    ensure_editable(d, edit, &affected)?;
    let count = affected.len();
    if count == 0 {
        return Ok(json!({ "changed": 0, "triangles": final_count, "message": "The surface already matches the requested edit." }));
    }
    // Use a staged drawing so even a late entity error cannot commit part of the trim.
    let mut next = d.clone();
    let mut replacements = std::collections::HashMap::new();
    for (&h, pieces) in handles.iter().zip(pieces).filter(|(h, _)| affected.contains(h)) {
        let common = next.model.get(h).ok_or_else(|| edit.reject("A source triangle no longer exists."))?.common.clone();
        let kind = |[a, b, c]: terrain::Triangle| EntityKind::Face3d(Face3d { corners: [a, b, c, c], hidden_edges: 0 });
        let mut members = Vec::new();
        for (index, piece) in pieces.into_iter().enumerate() {
            if index == 0 {
                next.model.modify(h, |e| e.kind = kind(piece));
                members.push(h);
            } else {
                members.push(next.add(&cadcraft_doc::Space::Model, common.clone(), kind(piece))?);
            }
        }
        if members.is_empty() {
            next.model.remove(h);
        }
        replacements.insert(h, members);
    }
    for group in &mut next.groups {
        if group.members.iter().any(|h| replacements.contains_key(h)) {
            group.members = group.members.iter().flat_map(|h| replacements.get(h).map_or(std::slice::from_ref(h), Vec::as_slice)).copied().collect();
        }
    }
    *s.doc_mut()? = next;
    s.set_selection(Vec::new());
    Ok(
        json!({ "changed": count, "triangles": final_count, "message": format!("Edited {count} source triangles. The surface now has {final_count} triangles.") }),
    )
}

struct Editor {
    edit: Edit,
    group: Option<String>,
    by_name: bool,
    vertex: Option<Vec3>,
    neighbors: Vec<Vec3>,
    selection: SelectPhase,
}

impl Editor {
    fn new(edit: Edit) -> Self {
        Self { edit, group: None, by_name: false, vertex: None, neighbors: Vec::new(), selection: SelectPhase::default() }
    }

    fn apply(&self, s: &mut Session, params: Value) -> Result<Step> {
        let value = run(s, self.edit, &params)?;
        if let Some(message) = value.get("message").and_then(Value::as_str) {
            s.echo(message);
        }
        Ok(Step::Done)
    }

    fn eligible(d: &Drawing) -> impl Iterator<Item = &Group> {
        d.groups.iter().filter(|g| g.description == TIN_GROUP_DESCRIPTION && !g.members.is_empty() && g.members.len() <= MAX_FACES)
    }
}

impl Interactive for Editor {
    fn name(&self) -> &'static str {
        self.edit.menu()[3]
    }

    fn begin(&mut self, s: &mut Session) -> Result<Step> {
        self.group = {
            let mut groups = Self::eligible(s.doc()?);
            let first = groups.next().ok_or_else(|| self.edit.reject("Open or build an editable terrain surface first."))?;
            groups.next().is_none().then(|| first.name.clone())
        };
        s.set_selection(Vec::new());
        Ok(Step::Continue)
    }

    fn prompt(&self, _s: &Session) -> Prompt {
        if self.group.is_none() {
            return if self.by_name {
                Prompt::new("Enter terrain surface name", Accept::TEXT)
            } else {
                Prompt::new("Select a triangle of the surface to edit", Accept::SELECT).kw(&["Name"])
            };
        }
        match self.edit {
            Edit::Clip | Edit::Hole => Prompt::new("Select a closed boundary polyline", Accept::SELECT),
            Edit::Breakline => Prompt::new("Select a line or 3D polyline for the breakline", Accept::SELECT),
            Edit::Swap => Prompt::new("Select two adjacent triangles, then press Enter", Accept::SELECT),
            Edit::Remove => {
                let mut prompt = self.selection.prompt();
                prompt.message = if self.selection.removing { "Deselect terrain triangles" } else { "Select terrain triangles to remove" }.into();
                prompt
            }
            Edit::Elevation if self.vertex.is_some() => Prompt::new("Enter the new vertex elevation", Accept::NUMBER),
            Edit::Move if self.vertex.is_some() => Prompt::new("Specify the new vertex position", Accept::POINT).base3_opt(self.vertex),
            _ => Prompt::new("Pick a terrain vertex using Endpoint snap", Accept::POINT),
        }
    }

    fn preview(&self, s: &Session, point: Vec2) -> Vec<EntityKind> {
        self.preview_3d(s, point.to3(self.vertex.map_or(0.0, |p| p.z)))
    }

    fn preview_3d(&self, _s: &Session, point: Vec3) -> Vec<EntityKind> {
        if self.vertex == Some(point) {
            return Vec::new();
        }
        self.neighbors.iter().map(|&b| EntityKind::Line(Line { a: point, b })).collect()
    }

    fn input(&mut self, s: &mut Session, input: Input) -> Result<Step> {
        let Some(group_name) = &self.group else {
            match input {
                Input::Keyword(keyword) if keyword == "Name" => self.by_name = true,
                Input::Text(name) if self.by_name => {
                    self.group = Some(group(s.doc()?, self.edit, name.trim())?.name.clone());
                }
                Input::Pick(hs) => {
                    if let [h] = hs.as_slice() {
                        self.group = Self::eligible(s.doc()?).find(|g| g.members.contains(h)).map(|g| g.name.clone());
                    }
                    if self.group.is_none() {
                        return Err(self.edit.reject("Pick one triangle from the surface to edit."));
                    }
                }
                _ => {}
            }
            return Ok(Step::Continue);
        };
        match self.edit {
            Edit::Remove | Edit::Swap => {
                if let Input::Pick(hs) = &input {
                    let surface = group(s.doc()?, self.edit, group_name)?;
                    if hs.iter().any(|h| !surface.members.contains(h)) {
                        return Err(self.edit.reject("Pick only triangles in the chosen terrain surface."));
                    }
                }
                match self.selection.feed(s, &input)? {
                    SelOutcome::Done(hs) => self.apply(s, json!({ "group": group_name, "handles": super::hex(&hs) })),
                    SelOutcome::Empty => Ok(Step::Done),
                    SelOutcome::More => Ok(Step::Continue),
                }
            }
            Edit::Clip | Edit::Hole | Edit::Breakline => {
                if let Input::Pick(hs) = input {
                    let [handle] = hs.as_slice() else { return Err(self.edit.reject("Pick exactly one line object.")) };
                    return self.apply(s, json!({ "group": group_name, "handle": handle.hex() }));
                }
                Ok(Step::Continue)
            }
            Edit::Elevation | Edit::Move => {
                if let Some(from) = self.vertex {
                    let input = match input {
                        Input::Text(text) if matches!(self.edit, Edit::Elevation) => {
                            let z = crate::units::parse_distance(&text).ok_or_else(|| self.edit.reject("Enter a valid elevation."))?;
                            json!({ "group": group_name, "at": [from.x, from.y], "elevation": z })
                        }
                        Input::Point(p) if matches!(self.edit, Edit::Move) => {
                            let to = s.point_position(p);
                            json!({ "group": group_name, "at": [from.x, from.y], "to": [to.x, to.y, to.z] })
                        }
                        _ => return Ok(Step::Continue),
                    };
                    return self.apply(s, input);
                }
                if let Input::Point(point) = input {
                    let surface = group(s.doc()?, self.edit, group_name)?;
                    let triangles = read_triangles(s.doc()?, surface, self.edit)?;
                    let radius = if s.state()?.orbit.is_some() { vertex_tolerance(point) } else { s.pixel_size() * s.settings.pickbox.max(1.0) };
                    let from = nearest_vertex(&triangles, point, radius, self.edit)?;
                    if matches!(self.edit, Edit::Move) {
                        self.neighbors = triangles.iter().filter(|face| face.contains(&from)).flatten().copied().filter(|&p| p != from).collect();
                        self.neighbors.sort_unstable_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)).then(a.z.total_cmp(&b.z)));
                        self.neighbors.dedup();
                    }
                    s.echo(format!("Terrain vertex: {:.6}, {:.6}, {:.6}", from.x, from.y, from.z));
                    self.vertex = Some(from);
                }
                Ok(Step::Continue)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terrain() -> Session {
        let mut s = Session::new();
        s.execute("tin", &json!({"name": "Site", "points": [[0,0,1],[10,0,1],[10,10,3],[0,10,3],[5,5,2]]})).unwrap();
        s
    }

    #[test]
    fn new_elevation_reaches_every_shared_corner_and_can_be_undone() {
        let mut s = terrain();
        let original = s.doc().unwrap().clone();
        s.start("tin.elevation").unwrap();
        s.input(Input::Point(Vec2::new(5.0, 5.0))).unwrap();
        s.input(Input::Text("2.8".into())).unwrap();
        let d = s.doc().unwrap();
        assert_eq!(d.groups, original.groups);
        let triangles = read_triangles(d, &d.groups[0], Edit::Elevation).unwrap();
        let height = triangles.iter().flatten().filter(|p| p.xy() == Vec2::new(5.0, 5.0)).map(|p| p.z).collect::<Vec<_>>();
        assert!(height.len() > 1);
        assert!(height.iter().all(|&z| z == 2.8));
        s.execute("u", &Value::Null).unwrap();
        assert_eq!(s.doc().unwrap(), &original);
    }

    #[test]
    fn horizontal_move_preserves_height_and_invalid_move_leaves_the_surface_unchanged() {
        let mut s = terrain();
        s.start("tin.movevertex").unwrap();
        s.cmdline("5,5").unwrap();
        let destination = Vec3::new(5.0, 6.0, 2.0);
        let original = Vec3::new(5.0, 5.0, 2.0);
        let proposed = s.preview_3d(destination);
        assert!(proposed.len() > 1);
        assert!(proposed.iter().all(|e| matches!(&e.kind, EntityKind::Line(line) if line.a == destination && line.b != original)));
        s.cmdline("5,6").unwrap();
        let moved = s.doc().unwrap().clone();
        assert!(read_triangles(&moved, &moved.groups[0], Edit::Move).unwrap().iter().flatten().any(|&p| p == Vec3::new(5.0, 6.0, 2.0)));
        let error = s.execute("tin.movevertex", &json!({"group": "Site", "at": [5,6], "to": [15,6]})).unwrap_err();
        assert!(error.to_string().contains("invert"));
        assert_eq!(s.doc().unwrap(), &moved);
    }

    #[test]
    fn a_picked_face_is_removed_without_removing_its_whole_group() {
        let mut s = terrain();
        let original = s.doc().unwrap().clone();
        let h = original.groups[0].members[0];
        assert!(s.settings.pickstyle & 1 != 0);
        s.start("tin.removetriangles").unwrap();
        s.pick_cycled(h, false).unwrap();
        assert_eq!(s.selection(), [h]);
        s.input(Input::Enter).unwrap();
        let d = s.doc().unwrap();
        assert!(d.model.get(h).is_none());
        assert_eq!(d.groups[0].members.len() + 1, original.groups[0].members.len());
        s.execute("u", &Value::Null).unwrap();
        assert_eq!(s.doc().unwrap(), &original);
    }

    #[test]
    fn trim_accepts_a_closed_polyline_and_keeps_it_as_a_separate_object() {
        let mut s = terrain();
        s.execute("rectang", &json!({"p1": [2,2], "p2": [8,8]})).unwrap();
        let d = s.doc().unwrap();
        let handle = d.model.iter().last().unwrap().handle;
        let before = d.clone();
        s.start("tin.clip").unwrap();
        s.pick_cycled(handle, false).unwrap();
        let d = s.doc().unwrap();
        let triangles = read_triangles(d, &d.groups[0], Edit::Clip).unwrap();
        let area: f64 = triangles.iter().map(|&t| terrain::signed_area2(t).abs() * 0.5).sum();
        assert!((area - 36.0).abs() < 1e-9);
        assert_eq!(d.model.get(handle), before.model.get(handle));
        s.execute("u", &Value::Null).unwrap();
        assert_eq!(s.doc().unwrap(), &before);
    }
}
