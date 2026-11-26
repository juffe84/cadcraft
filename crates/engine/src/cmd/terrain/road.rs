//! Road design commands: persistent settings, boundary linework and generated TIN surfaces.
use std::collections::{BTreeMap, HashSet, VecDeque};

use cadcraft_doc::{
    Common, Drawing, Entity, EntityKind, Face3d, Group, Handle, Layer, Point, Polyline3d, Space,
    road::{
        self, ALIGNMENT_GROUP, DESIGN_APP, DESIGN_GROUP, Design, EDIT_APP, LINE_APP, ReferenceLine, STATION_APP, STRING_PREFIX, SURFACE_PREFIX,
        Stationing, WidthControl,
    },
};
use cadcraft_geom::{
    Vec2, Vec3,
    terrain::{
        Triangle,
        road::{Extension, ExtensionDirection, ExtensionTarget, ReferenceInput, RoadSections, Rule, Sampler, Template, Width},
    },
};
use serde_json::{Value, json};

use crate::{
    Result, Session,
    cmd::{CommandSpec, bad},
};

const BUILD: &str = "tin.road.build";
const CONFIG_LAYER: &str = "CADCRAFT-ROAD-DATA";
const MAX_POINTS: usize = 50_000;
const MAX_FACES: usize = 400_000;

mod instructions;

pub(super) fn specs() -> [CommandSpec; 9] {
    [
        CommandSpec::new(BUILD, "Build Road Surfaces", build).enabled(super::model_space)
            .params("{name, axis: handle, range: [from,to], interval, centerlineNumber?, left:{width,edges?}, right:{width,edges?}, crossfall:[{station,left,right}], top?:{inherit,rules?}, layers?:[{name,thickness,template?}], references?:[{number,handle,pipe?:{diameter,level: invert or crown}}]}. Set fromReference to line, leftEdge, rightEdge, crown or invert to use reference number from. An extension takes through or grade (%) and target:[first,last] with targetLayer?, or grade with targetLayer and targetSide (left or right). Set helper:true to leave a point out of the surface."),
        CommandSpec::new("tin.road.list", "List Road Designs", list).enabled(super::model_space).noundo(),
        CommandSpec::new("tin.road.get", "Read Road Design", get).enabled(super::model_space).noundo().params("{name: road design group}"),
        CommandSpec::new("tin.road.section", "Road Cross Section", preview).enabled(super::model_space).noundo()
            .params("{design: road design parameters, station} → {layers: [{name, nodes: [{number, offset, elevation}]}], references?: [{number,line:{x:offset,y:elevation},pipe?}]}"),
        CommandSpec::new("tin.road.preview", "Preview Road Surfaces", preview_command).enabled(super::model_space).noundo()
            .params("{design: road design parameters} → {layers:[{name, triangles, incomplete}]}"),
        CommandSpec::new("tin.road.editable", "Create Editable Road Surface", editable).enabled(super::model_space)
            .params("{surface: road surface name, name?: name for the linked copy, hideOriginal?: bool}"),
        CommandSpec::new("tin.road.axis", "Read Road Centerline", inspect_axis).enabled(super::model_space).noundo().params("{handle?} → {axes:[{name,handle,range}]}"),
        CommandSpec::new("tin.road.instructions.save", "Save Road Instructions", instructions::save).enabled(super::model_space).noundo()
            .params("{path: JSON file to write, design: road design parameters}"),
        CommandSpec::new("tin.road.instructions.load", "Load Road Instructions", instructions::load).enabled(super::model_space).noundo()
            .params("{path: JSON file to read} → road design parameters for the current drawing"),
    ]
}

fn reject(message: impl Into<String>) -> crate::EngineError {
    bad(BUILD, message)
}

fn entity<'a>(doc: &'a Drawing, value: &str) -> Result<&'a Entity> {
    Handle::parse_hex(value)
        .and_then(|handle| doc.model.get(handle))
        .map(AsRef::as_ref)
        .ok_or_else(|| reject(format!("Missing model-space object {value}.")))
}

fn axis_geometry(e: &Entity) -> Result<(Vec<(f64, Vec3)>, bool)> {
    if !matches!(
        &e.kind,
        EntityKind::Line(_)
            | EntityKind::LwPolyline(cadcraft_doc::LwPolyline { closed: false, .. })
            | EntityKind::Polyline3d(cadcraft_doc::Polyline3d { closed: false, .. })
    ) {
        return Err(reject("Choose an open 2D or 3D polyline or a line as the road centerline."));
    }
    let points = cadcraft_doc::terrain::source_vertices(e, MAX_POINTS).map_err(reject)?;
    if points.len() < 2 || points.iter().any(|p| !p.is_finite() || p.x.abs().max(p.y.abs()).max(p.z.abs()) > 1e12) {
        return Err(reject("The centerline must have at least two finite XYZ points."));
    }
    let mut lengths = vec![0.0];
    for pair in points.windows(2) {
        let length = pair[0].xy().dist(pair[1].xy());
        if length == 0.0 {
            return Err(reject("The centerline has consecutive points at the same XY position."));
        }
        lengths.push(lengths.last().copied().unwrap_or_default() + length);
    }
    let length = lengths.last().copied().unwrap_or_default();
    let known = road::metadata(&e.common, STATION_APP).and_then(|s| serde_json::from_str::<Stationing>(&s).ok()).filter(|stations| {
        stations.from.is_finite()
            && stations.to.is_finite()
            && stations.from < stations.to
            && (stations.chord_length - length).abs() <= (length * 1e-7).max(1e-6)
    });
    let sampled_alignment = known.is_some();
    let stations = if let Some(mut known) = known {
        if known.vertices.len() == points.len() && known.vertices.windows(2).all(|p| p[0].is_finite() && p[1].is_finite() && p[0] < p[1]) {
            std::mem::take(&mut known.vertices)
        } else {
            let mut stations: Vec<_> = lengths.into_iter().map(|s| known.from + (known.to - known.from) * (s / length)).collect();
            if let Some(last) = stations.last_mut() {
                *last = known.to;
            }
            stations
        }
    } else {
        lengths
    };
    Ok((stations.into_iter().zip(points).collect(), sampled_alignment))
}

fn inspect_axis(session: &mut Session, params: &Value) -> Result<Value> {
    let doc = session.doc()?;
    let describe = |e: &Entity, name: &str| {
        let (vertices, _) = axis_geometry(e)?;
        let (a, b) = vertices.first().zip(vertices.last()).ok_or_else(|| reject("The centerline has no station range."))?;
        Ok(json!({"name":name, "handle":e.handle.hex(), "range":[a.0,b.0]}))
    };
    if let Some(handle) = params.get("handle").and_then(Value::as_str) {
        let e = entity(doc, handle)?;
        let name = doc.groups.iter().find(|g| g.description == ALIGNMENT_GROUP && g.members.contains(&e.handle)).map_or(handle, |g| &g.name);
        return describe(e, name);
    }
    let axes = doc
        .groups
        .iter()
        .filter(|g| g.description == ALIGNMENT_GROUP)
        .filter_map(|g| g.members.first().and_then(|&h| doc.model.get(h)).and_then(|e| describe(e, &g.name).ok()))
        .collect::<Vec<_>>();
    Ok(json!({"axes":axes}))
}

fn list(session: &mut Session, _: &Value) -> Result<Value> {
    let doc = session.doc()?;
    Ok(json!({"roads":doc.groups.iter().filter(|g| g.description == DESIGN_GROUP).map(|g| &g.name).collect::<Vec<_>>()}))
}

fn get(session: &mut Session, params: &Value) -> Result<Value> {
    let name = params.get("name").and_then(Value::as_str).ok_or_else(|| reject("Choose a road design."))?;
    let doc = session.doc()?;
    let data = doc
        .groups
        .iter()
        .find(|g| g.description == DESIGN_GROUP && g.name.eq_ignore_ascii_case(name))
        .and_then(|g| g.members.first())
        .and_then(|&h| doc.model.get(h))
        .and_then(|e| road::metadata(&e.common, DESIGN_APP))
        .ok_or_else(|| reject("The road design parameters are missing."))?;
    serde_json::from_str(&data).map_err(|_| reject("The stored road design parameters could not be read."))
}

fn edges(doc: &Drawing, source: &[String], axis: Handle) -> Result<Vec<[Vec2; 2]>> {
    let mut lines = Vec::new();
    let mut seen = HashSet::new();
    for handle in source {
        let e = entity(doc, handle)?;
        if e.handle == axis {
            return Err(reject("The road centerline cannot also be a width boundary."));
        }
        if !seen.insert(e.handle) {
            continue;
        }
        let points = cadcraft_doc::terrain::source_vertices(e, MAX_POINTS.saturating_sub(lines.len())).map_err(reject)?;
        if points.len() < 2 || points.iter().any(|p| !p.is_finite() || p.x.abs().max(p.y.abs()) > 1e12) {
            return Err(reject(format!("Use linework with valid XY coordinates for boundary {handle}.")));
        }
        lines.extend(points.windows(2).filter_map(|pair| {
            let (a, b) = (pair[0].xy(), pair[1].xy());
            (a != b).then_some([a, b])
        }));
    }
    Ok(lines)
}

fn width<'a>(source: &WidthControl, edges: &'a [[Vec2; 2]]) -> Width<'a> {
    if source.edges.is_empty() { Width::Fixed(source.width) } else { Width::FromEdges(edges) }
}

fn build(session: &mut Session, params: &Value) -> Result<Value> {
    let mut design: Design = serde_json::from_value(params.clone()).map_err(|e| reject(format!("Invalid road design parameters: {e}")))?;
    design.name = design.name.trim().into();
    let doc = session.doc()?;
    if !road::valid_name(&design.name) || design.layers.len() > road::MAX_LAYERS || design.crossfall.len() > 512 {
        return Err(reject("Enter a valid road name, up to 12 layers and up to 512 crossfall rows."));
    }
    validate_templates(&mut design)?;

    let stored = serde_json::to_string(&design).map_err(|e| reject(e.to_string()))?;
    let mut marker_common = Common { layer: CONFIG_LAYER.into(), ..Common::default() };
    if !road::store_metadata(&mut marker_common, DESIGN_APP, &stored) {
        return Err(reject("The road parameters exceed the drawing's metadata limit."));
    }
    let (origin, meshes) = with_sampler(doc, &design, |sampler, origin| Ok((origin, sampler.mesh(MAX_POINTS, MAX_FACES).map_err(reject)?)))?;

    let group = doc.groups.iter().find(|g| g.name.eq_ignore_ascii_case(&design.name));
    let existing = match group {
        Some(g) if g.description == DESIGN_GROUP => {
            Some(*g.members.first().filter(|&&h| doc.model.get(h).is_some()).ok_or_else(|| reject("The road's parameter record was deleted."))?)
        }
        Some(_) => return Err(reject("The road name is already used by another group.")),
        None => None,
    };
    let old_description = existing.map(|handle| format!("{SURFACE_PREFIX}{}", handle.hex()));
    let old_strings = existing.map(|handle| format!("{STRING_PREFIX}{}", handle.hex()));
    let mut outputs = Vec::new();
    let mut output_names = HashSet::new();
    for (level, mesh) in std::iter::once("Top").chain(design.layers.iter().map(|l| l.name.as_str())).zip(meshes) {
        if mesh.triangles.is_empty() {
            continue;
        }
        let name = format!("{} - {level}", design.name);
        let strings = format!("{name} - Lines");
        for (n, owned) in [(&name, &old_description), (&strings, &old_strings)] {
            if !output_names.insert(n.to_ascii_lowercase())
                || doc.groups.iter().any(|g| g.name.eq_ignore_ascii_case(n) && Some(g.description.as_str()) != owned.as_deref())
            {
                return Err(reject(format!("A different group already uses the name {n}.")));
            }
        }
        outputs.push((name, strings, mesh));
    }
    let first_count = outputs.first().map_or(0, |(_, _, mesh)| mesh.triangles.len());

    let mut next = doc.clone();
    road::register(&mut next, DESIGN_APP);
    road::register(&mut next, LINE_APP);
    if next.layer(CONFIG_LAYER).is_none() {
        next.layers.push(Layer { on: false, plot: false, ..Layer::new(CONFIG_LAYER) });
    }
    let marker = if let Some(handle) = existing {
        next.model.modify(handle, |e| e.common = marker_common);
        handle
    } else {
        next.add(&Space::Model, marker_common, EntityKind::Point(Point { p: origin, angle: 0.0 }))?
    };
    let description = format!("{SURFACE_PREFIX}{}", marker.hex());
    let strings_tag = format!("{STRING_PREFIX}{}", marker.hex());
    let mut old = Vec::new();
    next.groups.retain(|g| {
        if g.description == description || g.description == strings_tag {
            old.push(g.clone());
            false
        } else {
            !(g.description == DESIGN_GROUP && g.name.eq_ignore_ascii_case(&design.name))
        }
    });
    next.groups.push(Group { name: design.name, description: DESIGN_GROUP.into(), selectable: false, members: vec![marker] });
    let mut surfaces = Vec::new();
    let mut counts = Vec::new();
    for (index, (name, strings, mesh)) in outputs.into_iter().enumerate() {
        let layer = format!("ROAD - {name}");
        if next.layer(&layer).is_none() {
            let colors = [7, 4, 2, 3, 6];
            next.layers.push(Layer { color: cadcraft_doc::color::Color::Index(colors[index % colors.len()]), on: index == 0, ..Layer::new(&layer) });
        }
        counts.push(mesh.triangles.len());
        let faces = mesh.triangles.into_iter().map(|[a, b, c]| (None, EntityKind::Face3d(Face3d { corners: [a, b, c, c], hidden_edges: 0 })));
        replace_group(&mut next, &mut old, &name, &description, &layer, faces)?;
        let lines = mesh.lines.into_iter().flat_map(|(number, lines)| {
            lines.into_iter().map(move |points| (Some(number), EntityKind::Polyline3d(Polyline3d { points, closed: false })))
        });
        replace_group(&mut next, &mut old, &strings, &strings_tag, &layer, lines)?;
        surfaces.push(name);
    }
    for g in old {
        for handle in g.members {
            next.remove_entity(handle);
        }
    }

    *session.doc_mut()? = next;
    Ok(
        json!({"surfaces":surfaces, "trianglesPerSurface":first_count, "triangleCounts":counts, "message":format!("Built {} road surfaces.", surfaces.len())}),
    )
}

fn validate_templates(design: &mut Design) -> Result<()> {
    if design.layers.len() > road::MAX_LAYERS || design.top.rules.len() + design.layers.iter().map(|l| l.template.rules.len()).sum::<usize>() > 512 {
        return Err(reject("The road exceeds the limit on layers or section instructions."));
    }
    if design.references.len() > road::MAX_REFERENCES {
        return Err(reject("The road exceeds the limit on reference lines."));
    }
    let mut names = HashSet::from(["top".to_string()]);
    for layer in &mut design.layers {
        layer.name = layer.name.trim().into();
        let valid_thickness = layer.thickness.is_finite() && (layer.thickness > 0.0 || (!layer.template.inherit && layer.thickness == 0.0));
        if !road::valid_name(&layer.name) || !names.insert(layer.name.to_ascii_lowercase()) || !valid_thickness {
            return Err(reject("Use a unique layer name and a positive thickness for layers that copy the section above."));
        }
    }
    Ok(())
}

fn reference_paths(doc: &Drawing, references: &[ReferenceLine]) -> Result<Vec<Vec<Vec3>>> {
    let mut remaining = road::MAX_REFERENCE_VERTICES;
    references
        .iter()
        .map(|reference| {
            let entity = entity(doc, &reference.handle)?;
            if !matches!(entity.kind, EntityKind::Line(_) | EntityKind::Polyline3d(_)) {
                return Err(reject(format!("Reference {} must be a line or 3D polyline.", reference.number)));
            }
            let points = cadcraft_doc::terrain::source_vertices(entity, remaining).map_err(reject)?;
            remaining -= points.len();
            Ok(points)
        })
        .collect()
}

fn with_sampler<T>(doc: &Drawing, design: &Design, run: impl FnOnce(&Sampler<'_>, Vec3) -> Result<T>) -> Result<T> {
    let source = entity(doc, &design.axis)?;
    let (axis, sampled_alignment) = axis_geometry(source)?;
    let paths = reference_paths(doc, &design.references)?;
    let left = if design.top.inherit { edges(doc, &design.left.edges, source.handle)? } else { Vec::new() };
    let right = if design.top.inherit { edges(doc, &design.right.edges, source.handle)? } else { Vec::new() };
    let settings: Vec<_> = std::iter::once(("Top", 0.0, &design.top))
        .chain(design.layers.iter().map(|layer| (layer.name.as_str(), layer.thickness, &layer.template)))
        .collect();
    let targets =
        settings.iter().flat_map(|(_, _, layer)| &layer.rules).map(|rule| edges(doc, &rule.edges, source.handle)).collect::<Result<Vec<_>>>()?;
    let mut targets = targets.iter();
    let mut templates = Vec::new();
    for &(name, thickness, layer) in &settings {
        let mut rules = Vec::new();
        for rule in &layer.rules {
            let resolve_layer = |layer_name: Option<&str>| {
                layer_name
                    .map(|name| {
                        settings[..templates.len()]
                            .iter()
                            .position(|(layer, _, _)| layer.eq_ignore_ascii_case(name))
                            .ok_or_else(|| reject(format!("Line {} references missing or later layer {name}.", rule.number)))
                    })
                    .transpose()
            };
            let extension = rule
                .extension
                .as_ref()
                .map(|extension| -> Result<Extension> {
                    let target_layer = resolve_layer(extension.target_layer.as_deref())?;
                    let direction = match (extension.through, extension.grade) {
                        (Some(through), None) => ExtensionDirection::Through(through),
                        (None, Some(grade)) => ExtensionDirection::Grade(grade),
                        _ => return Err(reject("An extension needs either a grade or a second source point.")),
                    };
                    let target = match (extension.target, target_layer, extension.target_side) {
                        (Some(points), layer, None) => ExtensionTarget::Segment { points, layer },
                        (None, Some(index), Some(side)) => ExtensionTarget::Layer { index, side },
                        _ => return Err(reject("Choose either two target points or a target layer and side.")),
                    };
                    Ok(Extension { direction, target })
                })
                .transpose()?;
            rules.push(Rule {
                number: rule.number,
                from: rule.from,
                from_layer: resolve_layer(rule.from_layer.as_deref())?,
                from_reference: rule.from_reference,
                stations: rule.stations,
                offset: rule.offset,
                height: rule.height,
                crossfall: rule.crossfall,
                edges: targets.next().filter(|_| !rule.edges.is_empty()).map(Vec::as_slice),
                extension,
                helper: rule.helper,
            });
        }
        templates.push(Template { name, thickness, inherit: layer.inherit, rules });
    }
    let sampler = Sampler::new(
        RoadSections {
            axis: &axis,
            limits: design.range,
            interval: design.interval,
            left: width(&design.left, &left),
            right: width(&design.right, &right),
            crossfall: &design.crossfall,
        },
        templates,
        design.centerline_number,
        sampled_alignment,
        design.references.iter().zip(&paths).map(|(reference, points)| ReferenceInput { number: reference.number, points, pipe: reference.pipe }),
    )
    .map_err(reject)?;
    run(&sampler, axis[0].1)
}

fn preview(session: &mut Session, params: &Value) -> Result<Value> {
    let mut design: Design = serde_json::from_value(params["design"].clone()).map_err(|e| reject(e.to_string()))?;
    validate_templates(&mut design)?;
    let station = params["station"].as_f64().ok_or_else(|| reject("Enter a section station."))?;
    with_sampler(session.doc()?, &design, |sampler, _| {
        serde_json::to_value(sampler.preview(station).map_err(reject)?).map_err(|error| reject(error.to_string()))
    })
}

#[derive(serde::Serialize)]
pub struct RoadPreviewLayer {
    pub name: String,
    pub triangles: Vec<Triangle>,
    pub incomplete: bool,
}

/// Calculate temporary surfaces without changing the drawing or its undo history.
pub fn preview_road_surfaces(doc: &Drawing, mut design: Design) -> Result<Vec<RoadPreviewLayer>> {
    validate_templates(&mut design)?;
    with_sampler(doc, &design, |sampler, _| {
        let meshes = sampler.preview_mesh(MAX_POINTS, MAX_FACES).map_err(|error| bad("tin.road.preview", error))?;
        Ok(std::iter::once("Top")
            .chain(design.layers.iter().map(|layer| layer.name.as_str()))
            .zip(meshes)
            .map(|(name, mesh)| RoadPreviewLayer { name: name.into(), triangles: mesh.triangles, incomplete: mesh.incomplete })
            .collect())
    })
}

fn preview_command(session: &mut Session, params: &Value) -> Result<Value> {
    let design = serde_json::from_value(params["design"].clone()).map_err(|error| bad("tin.road.preview", error.to_string()))?;
    Ok(json!({"layers":preview_road_surfaces(session.doc()?, design)?}))
}

fn replace_group(
    doc: &mut Drawing,
    old: &mut Vec<Group>,
    name: &str,
    description: &str,
    layer: &str,
    kinds: impl Iterator<Item = (Option<u32>, EntityKind)>,
) -> Result<()> {
    let former = old.iter().position(|g| g.name.eq_ignore_ascii_case(name)).map(|i| old.swap_remove(i));
    let mut handles: BTreeMap<_, VecDeque<_>> = BTreeMap::new();
    for handle in former.into_iter().flat_map(|g| g.members) {
        if let Some(entity) = doc.model.get(handle) {
            handles.entry(road::line_number(&entity.common)).or_default().push_back(handle);
        }
    }
    let mut members = Vec::new();
    for (number, kind) in kinds {
        let handle = if let Some(handle) = handles.get_mut(&number).and_then(VecDeque::pop_front) {
            doc.model.modify(handle, |e| {
                e.kind = kind;
                e.common.layer = layer.into();
            });
            handle
        } else {
            let mut common = Common { layer: layer.into(), ..Common::default() };
            if let Some(number) = number {
                road::store_metadata(&mut common, LINE_APP, &number.to_string());
            }
            doc.add(&Space::Model, common, kind)?
        };
        members.push(handle);
    }
    for handle in handles.into_values().flatten() {
        doc.remove_entity(handle);
    }
    doc.groups.push(Group { name: name.into(), description: description.into(), selectable: description.starts_with(SURFACE_PREFIX), members });
    Ok(())
}

fn editable(session: &mut Session, params: &Value) -> Result<Value> {
    let name = params["surface"].as_str().ok_or_else(|| reject("Choose a road surface."))?;
    let hide_original = params["hideOriginal"].as_bool().unwrap_or(false);
    let doc = session.doc()?;
    let source_index = doc.groups.iter().position(|g| g.name.eq_ignore_ascii_case(name)).ok_or_else(|| reject("Build the road surface first."))?;
    let group = &doc.groups[source_index];
    let id = group.description.strip_prefix(SURFACE_PREFIX).ok_or_else(|| reject("Choose a generated road surface."))?;
    let origin = format!("{id} {}", group.name);
    let name = params["name"].as_str().map(str::to_owned).unwrap_or_else(|| format!("{} - Editable", group.name));
    if let Some((index, existing)) = doc.groups.iter().enumerate().find(|(_, g)| g.name.eq_ignore_ascii_case(&name)) {
        if let Some(id) = existing.linked_terrain_id()
            && cadcraft_doc::terrain::group_members(doc, &format!("{}{id}", cadcraft_doc::terrain::SOURCE_PREFIX))
                .iter()
                .any(|&handle| doc.model.get(handle).is_some_and(|e| road::metadata(&e.common, EDIT_APP).as_deref() == Some(&origin)))
        {
            let result = json!({"group":existing.name});
            if hide_original {
                let doc = session.doc_mut()?;
                super::visibility::set(doc, source_index, false);
                super::visibility::set(doc, index, true);
            }
            return Ok(result);
        }
        return Err(reject("The editable surface name is already in use."));
    }
    if group.members.is_empty() || group.members.len() > MAX_FACES {
        return Err(reject("The road surface is empty or exceeds the triangle limit."));
    }
    let strings = format!("{} - Lines", group.name);
    let source = doc
        .groups
        .iter()
        .find(|g| g.name == strings && g.description == format!("{STRING_PREFIX}{id}"))
        .ok_or_else(|| reject("Rebuild the road to create its numbered source lines."))?;
    let shapes = source
        .members
        .iter()
        .map(|&handle| {
            let line = doc
                .model
                .get(handle)
                .filter(|e| matches!(e.kind, EntityKind::Polyline3d(_)))
                .ok_or_else(|| reject("A numbered road line is missing. Rebuild the road first."))?;
            let mut common = Common { xdata: line.common.xdata.clone(), ..Common::default() };
            road::store_metadata(&mut common, EDIT_APP, &origin);
            Ok((common, line.kind.clone()))
        })
        .collect::<Result<Vec<_>>>()?;
    let triangles = group
        .members
        .iter()
        .map(|&handle| {
            doc.model.get(handle).and_then(|e| super::triangle(&e.kind)).ok_or_else(|| reject("A road face is missing. Rebuild the road first."))
        })
        .collect::<Result<Vec<_>>>()?;
    let before = session.state()?.doc.clone();
    let result = (|| {
        road::register(session.doc_mut()?, EDIT_APP);
        let handles = shapes
            .into_iter()
            .map(|(common, kind)| session.doc_mut()?.add(&Space::Model, common, kind).map_err(Into::into))
            .collect::<Result<Vec<_>>>()?;
        let result = super::linked::seed_surface(session, &name, handles, triangles)?;
        if hide_original {
            super::visibility::set(session.doc_mut()?, source_index, false);
        }
        Ok(result)
    })();
    if result.is_err() {
        session.state_mut()?.doc = before;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trench_section_survives_a_missed_segment_and_whole_layer_follows_the_pipe_across_it() {
        let mut session = Session::default();
        let axis = session.execute("3dpoly", &json!({"points":[[0,0,10],[60,0,10]]})).unwrap();
        let pipe = session.execute("3dpoly", &json!({"points":[[0,2,8],[60,-4,8]]})).unwrap();
        let mut design = json!({
            "name":"Trench","axis":axis["handle"],"range":[0,60],"interval":1,"centerlineNumber":1,
            "left":{"width":0},"right":{"width":0},"crossfall":[],
            "top":{"inherit":false,"rules":[{"number":2,"from":1,"offset":-6},{"number":3,"from":1,"offset":6}]},
            "layers":[{"name":"Cut","thickness":0,"template":{"inherit":false,"rules":[
                {"number":7,"from":2,"fromLayer":"Top","height":-0.05},
                {"number":6,"from":1,"fromLayer":"Top","height":-0.05},
                {"number":8,"from":3,"fromLayer":"Top","height":-0.05}
            ]}},{"name":"Trench","thickness":0,"template":{"inherit":false,"rules":[
                {"number":17,"from":11,"fromReference":"leftEdge","offset":-0.2,"height":-0.5},
                {"number":18,"from":11,"fromReference":"rightEdge","offset":0.2,"height":-0.5},
                {"number":19,"from":17,"extension":{"grade":500,"target":[7,6],"targetLayer":"Cut"}},
                {"number":20,"from":18,"extension":{"grade":500,"target":[6,8],"targetLayer":"Cut"}}
            ]}}],
            "references":[{"number":11,"handle":pipe["handle"],"pipe":{"diameter":0.315,"level":"invert"}}]
        });
        let section = |s: &mut Session, design: &Value, station| s.execute("tin.road.section", &json!({"design":design,"station":station})).unwrap();
        let preview = |s: &mut Session, design: &Value| s.execute("tin.road.preview", &json!({"design":design})).unwrap();
        let revision = session.state().unwrap().revision;
        let failed = section(&mut session, &design, 29);
        let trench = &failed["layers"][2];
        assert_eq!(trench["missed_extensions"], json!([19]));
        assert!(trench["nodes"].as_array().unwrap().iter().any(|node| node["number"] == 20));
        assert_eq!(failed["origin"]["x"].as_f64(), Some(29.0));
        let partial = preview(&mut session, &design);
        assert!(partial["layers"][0]["triangles"].as_array().is_some_and(|triangles| !triangles.is_empty()));
        assert!(partial["layers"][2]["triangles"].as_array().is_some_and(|triangles| !triangles.is_empty()));
        assert_eq!(partial["layers"][2]["incomplete"], true);
        assert_eq!(session.state().unwrap().revision, revision);
        assert!(session.execute(BUILD, &design).is_err());

        let rules = &mut design["layers"][1]["template"]["rules"];
        rules[2]["extension"] = json!({"grade":500,"targetLayer":"Cut","targetSide":"left"});
        rules[3]["extension"] = json!({"grade":500,"targetLayer":"Cut","targetSide":"right"});
        let lip = |result: &Value, number| {
            let node = result["layers"][2]["nodes"].as_array().unwrap().iter().find(|node| node["number"] == number).unwrap();
            Vec2::new(node["offset"].as_f64().unwrap(), node["elevation"].as_f64().unwrap())
        };
        assert!(lip(&section(&mut session, &design, 10), 19).x < 0.0);
        let at = section(&mut session, &design, 29);
        let (bottom, left, right) = (lip(&at, 17), lip(&at, 19), lip(&at, 20));
        assert!(0.0 < left.x && left.x < bottom.x && bottom.x < right.x);
        assert!(((left.y - bottom.y) / (bottom.x - left.x) - 5.0).abs() < 1e-9);
        let complete = preview(&mut session, &design);
        assert_eq!(complete["layers"][2]["incomplete"], false);
        assert_eq!(session.state().unwrap().revision, revision);
        assert_eq!(session.execute(BUILD, &design).unwrap()["surfaces"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn road_instruction_file_restores_an_unbuilt_design_with_its_geometry_references() {
        let mut session = Session::default();
        let axis = session.execute("3dpoly", &json!({"points":[[0,0,10],[100,0,10]]})).unwrap();
        let reference = session.execute("3dpoly", &json!({"points":[[0,6,8],[100,6,8]]})).unwrap();
        let design = json!({
            "name":"Unbuilt road","axis":axis["handle"],"range":[0,100],"interval":2,"centerlineNumber":1,
            "left":{"width":0},"right":{"width":0},"crossfall":[],
            "top":{"inherit":false,"rules":[
                {"number":2,"from":1,"offset":-4},
                {"number":4,"from":2,"offset":-2,"height":-1},
                {"number":10,"from":1,"offset":-4.5,"height":-0.1,"helper":true},
                {"number":11,"from":1,"offset":-5.5,"height":-0.9,"helper":true,"stations":[20,60]}
            ]},
            "layers":[{"name":"Base","thickness":0,"template":{"inherit":false,"rules":[
                {"number":8,"from":1,"fromLayer":"Top","height":-0.15},
                {"number":9,"from":2,"fromLayer":"Top","height":-0.35},
                {"number":12,"from":8,"extension":{"through":9,"target":[10,11],"targetLayer":"Top"},"stations":[20,60]}
            ]}}],
            "references":[{"number":5,"handle":reference["handle"],"pipe":{"diameter":1,"level":"invert"}}]
        });
        let before = session.execute("tin.road.section", &json!({"design":design,"station":40})).unwrap();
        let directory = crate::temp_dir::TempDir::new("road-instructions");
        let path = directory.path_str("unbuilt.road.json");
        session.execute("tin.road.instructions.save", &json!({"path":path,"design":design})).unwrap();
        let restored = session.execute("tin.road.instructions.load", &json!({"path":path})).unwrap();
        let after = session.execute("tin.road.section", &json!({"design":restored,"station":40})).unwrap();
        assert_eq!(after, before);
        assert_eq!(restored["layers"][0]["template"]["rules"][2]["stations"], json!([20.0, 60.0]));
        assert_eq!(restored["references"][0]["handle"], reference["handle"]);
        assert!(session.doc().unwrap().groups.iter().all(|group| group.description != DESIGN_GROUP));
        assert!(Session::default().execute("tin.road.instructions.load", &json!({"path":path})).is_err());
    }

    #[test]
    fn unused_helper_points_leave_the_mesh_alone_and_can_start_a_graded_extension() {
        let mut s = Session::default();
        let axis = s.execute("3dpoly", &json!({"points":[[0,0,10],[100,0,10]]})).unwrap();
        let mut design = json!({
            "name":"Helper road","axis":axis["handle"],"range":[0,100],"interval":12,"centerlineNumber":1,
            "left":{"width":0},"right":{"width":0},"crossfall":[],
            "top":{"inherit":false,"rules":[{"number":2,"from":1,"offset":-4},{"number":4,"from":2,"offset":-2,"height":-1}]}
        });
        s.execute(BUILD, &design).unwrap();
        let top_faces = |s: &Session| {
            let drawing = s.doc().unwrap();
            let group = drawing.groups.iter().find(|group| group.name == "Helper road - Top").unwrap();
            group.members.iter().map(|&handle| drawing.model.get(handle).unwrap().kind.clone()).collect::<Vec<_>>()
        };
        let baseline = top_faces(&s);
        design["top"]["rules"].as_array_mut().unwrap().push(json!({"number":5,"from":1,"helper":true,"offset":-1,"height":-0.3,"stations":[20,80]}));
        let section = |s: &mut Session, design: &Value, station| s.execute("tin.road.section", &json!({"design":design,"station":station})).unwrap();
        let preview = section(&mut s, &design, 40);
        let top = preview["layers"][0]["nodes"].as_array().unwrap();
        assert!(top.iter().any(|node| node["number"] == 5 && node["helper"] == true));
        s.execute(BUILD, &design).unwrap();
        assert_eq!(top_faces(&s), baseline);

        design["layers"] = json!([{"name":"Base","thickness":0,"template":{"inherit":false,"rules":[
            {"number":8,"from":5,"fromLayer":"Top","offset":1},
            {"number":9,"from":5,"fromLayer":"Top","extension":{"grade":-3,"target":[2,4],"targetLayer":"Top"}}
        ]}}]);
        let preview = section(&mut s, &design, 40);
        let node = preview["layers"][1]["nodes"].as_array().unwrap().iter().find(|node| node["number"] == 9).unwrap();
        let x = node["offset"].as_f64().unwrap();
        let z = node["elevation"].as_f64().unwrap();
        assert!((-6.0..-4.0).contains(&x));
        assert!((z - (9.7 - 0.03 * (x + 1.0).abs())).abs() < 1e-9);
        assert!((z - (10.0 + (x + 4.0) * 0.5)).abs() < 1e-9);
        assert_eq!(section(&mut s, &design, 10)["layers"][1]["clipped"], json!([8, 9]));
        s.execute(BUILD, &design).unwrap();
        let d = s.doc().unwrap();
        let line = |number| d.model.iter().find(|entity| road::line_number(&entity.common) == Some(number));
        assert!(line(5).is_none());
        assert!(matches!(&line(9).unwrap().kind, EntityKind::Polyline3d(line)
            if line.points[0].x == 20.0 && line.points.last().unwrap().x == 80.0));
    }

    #[test]
    fn numbered_references_supply_pipe_points_and_stop_where_the_3d_line_ends() {
        let mut s = Session::default();
        let axis = s.execute("3dpoly", &json!({"points":[[0,0,10],[100,0,10]]})).unwrap();
        let pipe = s.execute("3dpoly", &json!({"points":[[20,-3,5],[40,-3,5.6],[80,-3,6]]})).unwrap();
        let line = s.execute("line", &json!({"points":[[0,4,8],[100,4,9]]})).unwrap();
        let mut design = json!({
            "name":"Pipe references", "axis":axis["handle"], "range":[0,100], "interval":12, "centerlineNumber":1,
            "left":{"width":0}, "right":{"width":0}, "crossfall":[],
            "top":{"inherit":false,"rules":[]},
            "references":[{"number":2,"handle":line["handles"][0]},
                {"number":3,"handle":pipe["handle"],"pipe":{"diameter":2.0,"level":"invert"}}]
        });
        let section = |s: &mut Session, design: &Value, station| s.execute("tin.road.section", &json!({"design":design,"station":station})).unwrap();
        let preview = section(&mut s, &design, 40);
        assert_eq!(preview["layers"][0]["nodes"].as_array().unwrap().len(), 1);
        assert_eq!(preview["references"][0]["line"]["x"], json!(-4.0));
        for (part, offset, elevation) in [("leftEdge", 2.0, 6.6), ("rightEdge", 4.0, 6.6), ("crown", 3.0, 7.6), ("invert", 3.0, 5.6)] {
            design["top"]["rules"] = json!([{"number":4,"from":3,"fromReference":part}]);
            let preview = section(&mut s, &design, 40);
            let node = &preview["layers"][0]["nodes"][1];
            assert_eq!(node["number"], 4);
            assert!((node["offset"].as_f64().unwrap() - offset).abs() < 1e-9);
            assert!((node["elevation"].as_f64().unwrap() - elevation).abs() < 1e-9);
        }
        design["references"][1]["pipe"]["level"] = json!("crown");
        let preview = section(&mut s, &design, 40);
        assert!((preview["layers"][0]["nodes"][1]["elevation"].as_f64().unwrap() - 3.6).abs() < 1e-9);
        let outside = section(&mut s, &design, 10);
        assert_eq!(outside["layers"][0]["clipped"], json!([4]));
        s.execute(BUILD, &design).unwrap();
        let d = s.doc().unwrap();
        let group = d.groups.iter().find(|group| group.name == "Pipe references - Top - Lines").unwrap();
        let edge = group.members.iter().filter_map(|&handle| d.model.get(handle)).find(|line| road::line_number(&line.common) == Some(4)).unwrap();
        assert!(matches!(&edge.kind, EntityKind::Polyline3d(line)
            if line.points[0].x == 20.0 && line.points.last().unwrap().x == 80.0 && line.points.iter().any(|point| point.x == 40.0)));
        let saved = s.execute("tin.road.get", &json!({"name":"Pipe references"})).unwrap();
        assert_eq!(saved["references"], design["references"]);
    }

    #[test]
    fn follows_a_selected_polyline_with_crossfall_and_detects_its_side() {
        let mut s = Session::default();
        let axis = s.execute("3dpoly", &json!({"points":[[0,0,10],[100,0,10]]})).unwrap();
        let left = s.execute("3dpoly", &json!({"points":[[0,4,80],[50,5,90],[100,6,85]]})).unwrap();
        let right = s.execute("3dpoly", &json!({"points":[[0,-3,80],[100,-3,80]]})).unwrap();
        let mut design = json!({
            "name":"Edge", "axis":axis["handle"], "range":[0,100], "interval":10, "centerlineNumber":1,
            "left":{"width":0}, "right":{"width":0}, "crossfall":[],
            "top":{"inherit":false,"rules":[{"number":2,"from":1,"crossfall":[-3,-3],"edges":[left["handle"]]}]}
        });
        let section = |s: &mut Session, design: &Value| s.execute("tin.road.section", &json!({"design":design,"station":50}));
        let offset_and_height = |data: Value| {
            let node = data["layers"][0]["nodes"].as_array().unwrap().iter().find(|n| n["number"] == 2).unwrap();
            (node["offset"].as_f64().unwrap(), node["elevation"].as_f64().unwrap())
        };
        let (offset, elevation) = offset_and_height(section(&mut s, &design).unwrap());
        assert!((offset + 5.0).abs() < 1e-9);
        assert!((elevation - (10.0 - 0.03 * offset.abs())).abs() < 1e-9);
        s.execute(BUILD, &design).unwrap();
        design["top"]["rules"][0]["edges"] = json!([left["handle"], right["handle"]]);
        assert!(section(&mut s, &design).unwrap_err().to_string().contains("intersect both sides"));
        design["top"]["rules"][0]["offset"] = json!(1);
        let (offset, elevation) = offset_and_height(section(&mut s, &design).unwrap());
        assert!(offset > 0.0);
        assert!((elevation - (10.0 - 0.03 * offset)).abs() < 1e-9);
    }

    #[test]
    fn truncates_an_edge_to_the_alignment_and_can_continue_its_number_by_station() {
        let mut s = Session::default();
        let axis = s.execute("3dpoly", &json!({"points":[[0,0,10],[100,0,10]]})).unwrap();
        let edge = s.execute("3dpoly", &json!({"points":[[-15,4,80],[30,4,80],[67.3,4,80]]})).unwrap();
        let start = json!({"number":2,"from":1,"crossfall":[-3,-3],"edges":[edge["handle"]]});
        let mut design = json!({
            "name":"Edge scope", "axis":axis["handle"], "range":[0,100], "interval":10, "centerlineNumber":1,
            "left":{"width":0}, "right":{"width":0}, "crossfall":[],
            "top":{"inherit":false,"rules":[start, {"number":3,"from":2,"offset":-1,"height":-0.1}]}
        });
        let end_points = |s: &Session| {
            let d = s.doc().unwrap();
            let group = d.groups.iter().find(|group| group.name == "Edge scope - Top - Lines").unwrap();
            let mut lines =
                group.members.iter().filter_map(|handle| d.model.get(*handle)).filter(|entity| road::line_number(&entity.common) == Some(2));
            let EntityKind::Polyline3d(polyline) = &lines.next().unwrap().kind else { panic!("expected a numbered 3D line") };
            assert!(lines.next().is_none(), "expected the joined line to stay continuous");
            [polyline.points[0], *polyline.points.last().unwrap()]
        };
        s.execute(BUILD, &design).unwrap();
        let [first, last] = end_points(&s);
        assert!(first.xy().near(Vec2::new(0.0, 4.0), 1e-8));
        assert!(last.xy().near(Vec2::new(67.3, 4.0), 1e-8));
        let outside = s.execute("tin.road.section", &json!({"design":design, "station":85})).unwrap();
        assert_eq!(outside["layers"][0]["clipped"], json!([2, 3]));

        design["top"]["rules"].as_array_mut().unwrap().push(json!({"number":2,"from":1,"offset":-4,"crossfall":[-3,-3],"stations":[67.3,100]}));
        s.execute(BUILD, &design).unwrap();
        let [first, last] = end_points(&s);
        assert!(first.xy().near(Vec2::new(0.0, 4.0), 1e-8));
        assert!(last.xy().near(Vec2::new(100.0, 4.0), 1e-8));
    }

    #[test]
    fn edits_a_scoped_road_copy_in_place_and_controls_visibility_by_surface() {
        let mut s = Session::default();
        let axis = s.execute("3dpoly", &json!({"points":[[0,0,10],[100,0,10]]})).unwrap();
        s.execute(
            BUILD,
            &json!({
                "name":"Scoped", "axis":axis["handle"], "range":[0,100], "interval":5, "centerlineNumber":1,
                "left":{"width":0}, "right":{"width":0}, "crossfall":[],
                "top":{"inherit":false,"rules":[{"number":2,"from":1,"offset":4,"stations":[20,80]}]}
            }),
        )
        .unwrap();
        let original = "Scoped - Top";
        let created = s.execute("tin.road.editable", &json!({"surface":original,"hideOriginal":true})).unwrap();
        let copy = created["group"].as_str().unwrap().to_owned();
        let visible = |s: &mut Session| {
            s.execute("tin.surfaces", &json!({"editable":true, "road":true, "linked":true, "visibility":true})).unwrap()["visibility"].clone()
        };
        let faces = |s: &Session, name: &str| {
            let d = s.doc().unwrap();
            d.groups
                .iter()
                .find(|group| group.name == name)
                .unwrap()
                .members
                .iter()
                .map(|&handle| super::super::triangle(&d.model.get(handle).unwrap().kind).unwrap())
                .collect::<Vec<_>>()
        };
        let plan = |faces: &[[Vec3; 3]]| faces.iter().map(|face| face.map(Vec3::xy)).collect::<Vec<_>>();
        assert_eq!(visible(&mut s), json!({original:false, copy.clone():true}));
        let initial = faces(&s, original);
        let points = s.execute("tin.source.points", &json!({"group":copy})).unwrap();
        let point = points["points"].as_array().unwrap().iter().find(|point| point["lineNumber"] == 2 && point["vertex"] == 2).unwrap();
        let elevation = |s: &mut Session, value| {
            s.execute("tin.source.elevation", &json!({"group":copy,"handle":point["handle"],"vertex":point["vertex"],"elevation":value})).unwrap();
        };
        elevation(&mut s, 8.25);
        let edited = faces(&s, &copy);
        assert_eq!(plan(&initial), plan(&edited));
        assert!(edited.iter().flatten().any(|point| (point.z - 8.25).abs() < 1e-9));
        assert_eq!(faces(&s, original), initial);

        s.execute("tin.surface.visibility", &json!({"group":original,"only":true})).unwrap();
        elevation(&mut s, 9.25);
        assert_eq!(plan(&faces(&s, &copy)), plan(&initial));
        assert_eq!(visible(&mut s), json!({original:true, copy.clone():false}));
        s.execute("tin.surface.visibility", &json!({"group":copy,"visible":true})).unwrap();
        assert_eq!(visible(&mut s), json!({original:true, copy.clone():true}));
    }

    #[test]
    fn extends_a_layer_to_a_numbered_segment_and_keeps_the_resulting_line() {
        let mut session = Session::default();
        let axis = session.execute("3dpoly", &json!({"points":[[0,0,10],[100,0,10]]})).unwrap();
        let mut design = json!({
            "name":"Base extension", "axis":axis["handle"], "range":[0,100], "interval":5,
            "centerlineNumber":1, "left":{"width":0}, "right":{"width":0}, "crossfall":[],
            "top":{"inherit":false,"rules":[{"number":2,"from":1,"offset":-4},{"number":4,"from":2,"offset":-2,"height":-1}]},
            "layers":[{"name":"Base","thickness":0,"template":{"inherit":false,"rules":[
                {"number":11,"from":8,"extension":{"through":9,"target":[2,4],"targetLayer":"Top"}},
                {"number":8,"from":1,"fromLayer":"Top","height":-0.15},
                {"number":9,"from":2,"fromLayer":"Top","height":-0.15}
            ]}}]
        });
        let preview = session.execute("tin.road.section", &json!({"design":design,"station":40})).unwrap();
        let nodes = preview["layers"][1]["nodes"].as_array().unwrap();
        let hit = nodes.iter().find(|node| node["number"] == 11).unwrap();
        let offset = hit["offset"].as_f64().unwrap();
        let elevation = hit["elevation"].as_f64().unwrap();
        assert!((-6.0..-4.0).contains(&offset));
        assert!((elevation - 9.85).abs() < 1e-9);
        assert!((elevation - (10.0 + (offset + 4.0) * 0.5)).abs() < 1e-9);
        session.execute(BUILD, &design).unwrap();
        let doc = session.doc().unwrap();
        let lines = doc.groups.iter().find(|group| group.name == "Base extension - Base - Lines").unwrap();
        assert!(lines.members.iter().any(|handle| doc.model.get(*handle).is_some_and(|line| road::line_number(&line.common) == Some(11))));
        let stored = session.execute("tin.road.get", &json!({"name":"Base extension"})).unwrap();
        assert_eq!(stored["layers"][0]["template"]["rules"][0]["extension"], design["layers"][0]["template"]["rules"][0]["extension"]);

        let extension = &mut design["layers"][0]["template"]["rules"][0];
        extension["from"] = json!(9);
        extension["extension"]["through"] = json!(8);
        let error = session.execute(BUILD, &design).unwrap_err().to_string();
        assert!(error.contains("cannot extend") && error.contains("segment 2–4"));
    }

    #[test]
    fn numbered_sections_use_the_latest_rule_on_its_range_and_keep_editable_lines() {
        let mut s = Session::default();
        let axis = s.execute("3dpoly", &json!({"points":[[0,0,10],[100,0,10]]})).unwrap();
        let edge = s.execute("line", &json!({"points":[[0,7,80],[100,7,80]]})).unwrap();
        let design = json!({
            "name":"Section road", "axis":axis["handle"], "range":[0,100], "interval":13,
            "left":{"width":3}, "right":{"width":4},
            "crossfall":[{"station":0,"left":-3,"right":-3}],
            "top":{"rules":[
                {"number":1,"from":0,"stations":[20,80],"offset":-3,"height":0,"crossfall":[-3,3]},
                {"number":1,"from":0,"stations":[40,60],"offset":-5,"height":0,"crossfall":[-2,-2]},
                {"number":3,"from":1,"stations":[40,60],"offset":-1,"height":0,"crossfall":[5,5],"edges":[edge["handles"][0]]}
            ]},
            "layers":[{"name":"Base","thickness":0.4,"template":{"rules":[
                {"number":3,"from":3,"fromLayer":"Top","stations":[40,60],"offset":0,"height":-0.25,"crossfall":[0,0]}
            ]}}]
        });
        let preview = |session: &mut Session, station| session.execute("tin.road.section", &json!({"design":design,"station":station})).unwrap();
        for (station, offset) in [(10, -3.0), (35, -3.0), (50, -5.0), (65, -3.0), (95, -3.0)] {
            let response = preview(&mut s, station);
            let node = response["layers"][0]["nodes"].as_array().unwrap().iter().find(|n| n["number"] == 1).unwrap();
            assert_eq!(node["offset"].as_f64(), Some(offset));
        }
        let response = preview(&mut s, 50);
        let node = |layer: usize| response["layers"][layer]["nodes"].as_array().unwrap().iter().find(|n| n["number"] == 3).unwrap();
        assert_eq!(node(0)["offset"].as_f64(), Some(-7.0));
        assert!((node(0)["elevation"].as_f64().unwrap() - node(1)["elevation"].as_f64().unwrap() - 0.25).abs() < 1e-8);
        assert!((node(0)["elevation"].as_f64().unwrap() - 10.0).abs() < 1e-8);

        s.execute(BUILD, &design).unwrap();
        let d = s.doc().unwrap();
        let line_group = d.groups.iter().find(|g| g.name == "Section road - Top - Lines").unwrap();
        assert!(line_group.members.iter().any(|&h| road::line_number(&d.model.get(h).unwrap().common) == Some(3)));
        let top = d.groups.iter().find(|g| g.name == "Section road - Top").unwrap().members.clone();
        let original = d.model.get(top[0]).unwrap().kind.clone();
        let editable = s.execute("tin.road.editable", &json!({"surface":"Section road - Top"})).unwrap();
        let group = editable["group"].clone();
        let points = s.execute("tin.source.points", &json!({"group":group})).unwrap();
        let line = points["points"].as_array().unwrap().iter().find(|p| p["lineNumber"] == 3).unwrap();
        s.execute("tin.source.elevation", &json!({"handle":line["handle"],"vertex":line["vertex"],"elevation":10.2,"group":group})).unwrap();
        assert_eq!(s.doc().unwrap().model.get(top[0]).unwrap().kind, original);
        assert_eq!(s.execute("tin.road.editable", &json!({"surface":"Section road - Top"})).unwrap()["group"], group);
    }

    #[test]
    fn builds_and_rebuilds_the_same_surfaces_using_crossfalls_and_boundary_widths() {
        let mut s = Session::default();
        let axis = s.execute("3dpoly", &json!({"points":[[0,0,10],[100,0,11]]})).unwrap();
        let edge = s.execute("3dpoly", &json!({"points":[[0,3,999],[50,5,999],[100,4,999]]})).unwrap();
        let design = json!({
            "name":"Route", "axis":axis["handle"], "range":[0,100], "interval":24,
            "left":{"width":3,"edges":[edge["handle"]]}, "right":{"width":4},
            "crossfall":[{"station":0,"left":-3,"right":-3},{"station":100,"left":3,"right":-3}],
            "layers":[{"name":"Pavement","thickness":0.1},{"name":"Base","thickness":0.2}]
        });
        s.execute(BUILD, &design).unwrap();
        let layer = |s: &Session, name: &str| {
            let d = s.doc().unwrap();
            let group = d.groups.iter().find(|g| g.name == name).unwrap();
            assert!(group.is_terrain_surface());
            let points: Vec<_> = group
                .members
                .iter()
                .flat_map(|&h| {
                    let EntityKind::Face3d(face) = &d.model.get(h).unwrap().kind else { panic!("expected a triangle") };
                    face.corners
                })
                .collect();
            (group.members.clone(), points)
        };
        let (ids, top) = layer(&s, "Route - Top");
        let (_, base) = layer(&s, "Route - Base");
        assert_eq!(top.len(), base.len());
        assert!(top.iter().zip(&base).all(|(top, bottom)| top.xy() == bottom.xy() && (top.z - bottom.z - 0.3).abs() < 1e-10));
        let has = |x: f64, y: f64, z: f64| top.iter().any(|p| p.xy().near(Vec2::new(x, y), 1e-8) && (p.z - z).abs() < 1e-8);
        assert!(has(0.0, 3.0, 9.91) && has(50.0, 5.0, 10.5) && has(100.0, 4.0, 11.12));
        assert!(has(0.0, -4.0, 9.88) && has(100.0, -4.0, 10.88));

        let mut changed = design.clone();
        changed["layers"][1]["thickness"] = json!(0.4);
        s.execute(BUILD, &changed).unwrap();
        assert_eq!(layer(&s, "Route - Top").0, ids);
        let saved = s.execute("tin.road.get", &json!({"name":"Route"})).unwrap();
        let expected: Design = serde_json::from_value(changed.clone()).unwrap();
        assert_eq!(saved, serde_json::to_value(expected).unwrap());
        s.undo().unwrap();
        assert_eq!(layer(&s, "Route - Base").1, base);

        changed["left"]["edges"] = json!([design["axis"]]);
        let before = s.doc().unwrap().clone();
        assert!(s.execute(BUILD, &changed).is_err());
        assert_eq!(&before, s.doc().unwrap());
    }
}
