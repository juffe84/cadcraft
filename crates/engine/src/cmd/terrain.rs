//! Build and section terrain surfaces using grouped CAD faces.
use cadcraft_doc::{EntityKind, Face3d, Handle, Polyline3d, Space, Text};
use cadcraft_geom::{Vec3, terrain};
use serde_json::{Value, json};

use super::{CommandSpec, bad, point3_value, str_param, targets};
use crate::{Accept, Input, Interactive, Prompt, Result, Session, Step};

use super::machines::{SelOutcome, SelectPhase};

const BUILD: &str = "tin";
const CONTOURS: &str = "tin.contours";
const VOLUMES: &str = "tin.volumes";
const MAX_VERTICES: usize = 50_000;
const MAX_CONTOURS: usize = 100_000;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new(BUILD, "Build Terrain Surface", build)
            .menu(&["Draw", "Terrain", "Build Terrain Surface"])
            .params("{points?: [[x,y,z]], handles?: POINT, LINE and 3D polyline handles, breaklines?: [[[x,y,z]]], outer?: [[x,y,z]], holes?: [[[x,y,z]]], name?: group name}")
            .enabled(model_space)
            .interactive(|_| Ok(Box::new(super::machines::SelectRun::new(BUILD, "TIN")))),
        CommandSpec::new(CONTOURS, "Draw Terrain Contours", contours)
            .menu(&["Draw", "Terrain", "Draw Terrain Contours"])
            .params("{handles?: triangle face handles, group?: terrain group name, interval?: contour interval (default 1), majorEvery?: number of intervals per index contour (default 5), labelHeight?: height of index labels}")
            .enabled(model_space)
            .interactive(|_| Ok(Box::new(ContourPrompt::default()))),
        CommandSpec::new(VOLUMES, "Measure Terrain Earthwork", volumes)
            .menu(&["Draw", "Terrain", "Measure Terrain Earthwork"])
            .params("{existing: terrain group name, design: terrain group name} -> {cut, fill, netFill, overlapArea}")
            .enabled(model_space)
            .interactive(|_| Ok(Box::new(VolumePrompt::default())))
            .noundo(),
    ]
}

#[derive(Default)]
struct ContourPrompt {
    selection: SelectPhase,
}

impl Interactive for ContourPrompt {
    fn name(&self) -> &'static str {
        "TIN.CONTOURS"
    }

    fn begin(&mut self, s: &mut Session) -> Result<Step> {
        self.selection = SelectPhase::begin(s);
        Ok(Step::Continue)
    }

    fn prompt(&self, _s: &Session) -> Prompt {
        if self.selection.done { Prompt::new("Specify contour interval", Accept::NUMBER).default("1") } else { self.selection.prompt() }
    }

    fn input(&mut self, s: &mut Session, i: Input) -> Result<Step> {
        if !self.selection.done {
            return match self.selection.feed(s, &i)? {
                SelOutcome::Empty => Ok(Step::Done),
                _ => Ok(Step::Continue),
            };
        }
        let interval = match i {
            Input::Text(text) => crate::units::parse_distance(&text).ok_or_else(|| bad(CONTOURS, "Expected a positive contour interval."))?,
            Input::Enter => 1.0,
            _ => return Ok(Step::Continue),
        };
        let result = contours(s, &json!({ "handles": hex(&self.selection.picked), "interval": interval }))?;
        if let Some(message) = result["message"].as_str() {
            s.echo(message);
        }
        Ok(Step::Done)
    }
}

#[derive(Default)]
struct VolumePrompt {
    existing: Option<String>,
}

impl Interactive for VolumePrompt {
    fn name(&self) -> &'static str {
        "TIN.VOLUMES"
    }

    fn prompt(&self, _s: &Session) -> Prompt {
        let message = if self.existing.is_some() { "Enter design terrain group name" } else { "Enter existing terrain group name" };
        Prompt::new(message, Accept::TEXT)
    }

    fn input(&mut self, s: &mut Session, input: Input) -> Result<Step> {
        let Input::Text(name) = input else { return Ok(Step::Continue) };
        if let Some(existing) = &self.existing {
            let value = volumes(s, &json!({"existing": existing, "design": name.trim()}))?;
            if let Some(message) = value["message"].as_str() {
                s.echo(message);
            }
            Ok(Step::Done)
        } else {
            faces(s, &json!({"group": name.trim()}))?;
            self.existing = Some(name.trim().to_owned());
            Ok(Step::Continue)
        }
    }
}

fn model_space(s: &Session) -> std::result::Result<(), String> {
    let state = s.state().map_err(|error| error.to_string())?;
    if state.space == Space::Model && state.block_editor().is_none() {
        Ok(())
    } else {
        Err("Terrain commands require model space outside the Block Editor.".into())
    }
}

fn points(value: &Value) -> Result<Vec<Vec3>> {
    value
        .as_array()
        .filter(|values| values.len() <= MAX_VERTICES)
        .and_then(|values| values.iter().map(point3_value).collect())
        .ok_or_else(|| bad(BUILD, "Expected a bounded list of XYZ points."))
}

fn paths(params: &Value, field: &str) -> Result<Vec<Vec<Vec3>>> {
    let Some(value) = params.get(field) else { return Ok(Vec::new()) };
    value
        .as_array()
        .filter(|paths| paths.iter().map(|p| p.as_array().map_or(0, Vec::len)).sum::<usize>() <= MAX_VERTICES)
        .ok_or_else(|| bad(BUILD, format!("{field} must be a bounded list of paths")))?
        .iter()
        .map(points)
        .collect()
}

fn build(s: &mut Session, p: &Value) -> Result<Value> {
    let mut samples = p.get("points").map(points).transpose()?.unwrap_or_default();
    let mut breaklines = paths(p, "breaklines")?;
    let outer = p.get("outer").map(points).transpose()?;
    let holes = paths(p, "holes")?;
    let handles = if p.get("points").is_some() && p.get("handles").is_none() && p.get("handle").is_none() { Vec::new() } else { targets(s, p)? };
    let src = s.doc()?;
    for handle in handles {
        let entity = src.entity(handle).ok_or_else(|| bad(BUILD, format!("Unknown object {}", handle.hex())))?;
        match &entity.kind {
            EntityKind::Point(point) => samples.push(point.p),
            EntityKind::Line(line) => breaklines.push(vec![line.a, line.b]),
            EntityKind::Polyline3d(line) => {
                let mut path = line.points.clone();
                if line.closed
                    && let Some(first) = path.first().copied()
                {
                    path.push(first);
                }
                breaklines.push(path);
            }
            _ => return Err(bad(BUILD, "Select points, lines or 3D polylines for the terrain.")),
        }
    }
    let count = samples.len()
        + breaklines.iter().map(Vec::len).sum::<usize>()
        + outer.as_ref().map_or(0, Vec::len)
        + holes.iter().map(Vec::len).sum::<usize>();
    if count > MAX_VERTICES {
        return Err(bad(BUILD, "The terrain exceeds 50,000 input vertices."));
    }
    let triangles = terrain::triangulate(&samples, &breaklines, outer.as_deref(), &holes).map_err(|e| bad(BUILD, e))?;
    let before = s.state()?.doc.clone();
    let result = (|| {
        let handles = triangles
            .into_iter()
            .map(|[a, b, c]| s.add_entity(EntityKind::Face3d(Face3d { corners: [a, b, c, c], hidden_edges: 0 })))
            .collect::<Result<Vec<_>>>()?;
        let group = super::groups::create(s, str_param(p, "name"), "TIN terrain surface", &handles, true)?;
        s.set_selection(handles.clone());
        Ok(
            json!({ "group": group, "triangles": handles.len(), "handles": hex(&handles), "message": format!("Created terrain group {group} with {} triangles.", handles.len()) }),
        )
    })();
    if result.is_err() {
        s.state_mut()?.doc = before;
    }
    result
}

fn hex(handles: &[Handle]) -> Vec<String> {
    handles.iter().map(|handle| handle.hex()).collect()
}

fn faces(s: &Session, p: &Value) -> Result<Vec<terrain::Triangle>> {
    let doc = s.doc()?;
    let handles = if let Some(name) = str_param(p, "group") {
        doc.groups.iter().find(|group| group.name.eq_ignore_ascii_case(name)).ok_or_else(|| bad(CONTOURS, "Unknown terrain group."))?.members.clone()
    } else {
        targets(s, p)?
    };
    if handles.is_empty() || handles.len() > MAX_VERTICES * 2 {
        return Err(bad(CONTOURS, "Select terrain triangle faces."));
    }
    handles
        .into_iter()
        .map(|handle| match doc.entity(handle).map(|entity| &entity.kind) {
            Some(EntityKind::Face3d(Face3d { corners: [a, b, c, d], .. })) if c == d => Ok([*a, *b, *c]),
            _ => Err(bad(CONTOURS, "The selection must contain only triangular 3D faces.")),
        })
        .collect()
}

fn contours(s: &mut Session, p: &Value) -> Result<Value> {
    let interval = p
        .get("interval")
        .map(|value| value.as_f64().ok_or_else(|| bad(CONTOURS, "Expected a numeric contour interval.")))
        .transpose()?
        .unwrap_or(1.0);
    let label_height = p
        .get("labelHeight")
        .map(|v| v.as_f64().filter(|h| h.is_finite() && *h > 0.0 && *h <= 1e12).ok_or_else(|| bad(CONTOURS, "Expected a positive label height.")))
        .transpose()?;
    let major_every = p
        .get("majorEvery")
        .map(|v| v.as_u64().filter(|n| (1..=1000).contains(n)).ok_or_else(|| bad(CONTOURS, "majorEvery must be an integer between 1 and 1000.")))
        .transpose()?
        .unwrap_or(5);
    let triangles = faces(s, p)?;
    let sections = terrain::contours(&triangles, interval, MAX_CONTOURS).map_err(|e| bad(CONTOURS, e))?;
    let style = s.doc()?.header.str("TEXTSTYLE", "Standard");
    let mut handles = Vec::new();
    let mut labels = Vec::new();
    for mut points in terrain::contour_paths(sections) {
        let closed = points.first() == points.last();
        if closed {
            points.pop();
        }
        let elevation = points[0].z;
        let major = ((elevation / interval).round() as i64).rem_euclid(major_every as i64) == 0;
        if major && let Some(height) = label_height {
            let insert = points[points.len() / 2];
            let text = Text {
                insert,
                align_pt: None,
                height,
                value: format!("{elevation:.6}").trim_end_matches('0').trim_end_matches('.').to_owned(),
                rotation: 0.0,
                width_factor: 1.0,
                oblique: 0.0,
                style: style.clone(),
                halign: Default::default(),
                valign: Default::default(),
            };
            labels.push(s.add_entity(EntityKind::Text(text))?);
        }
        handles.push(s.add_entity(EntityKind::Polyline3d(Polyline3d { points, closed }))?);
    }
    s.set_selection(handles.clone());
    Ok(
        json!({ "contours": handles.len(), "handles": hex(&handles), "labels": hex(&labels), "message": format!("Created {} terrain contours.", handles.len()) }),
    )
}

fn volumes(s: &mut Session, p: &Value) -> Result<Value> {
    use rstar::primitives::{GeomWithData, Rectangle};
    use rstar::{AABB, RTree};

    let read = |key| {
        let name = str_param(p, key).ok_or_else(|| bad(VOLUMES, format!("{key} must be a terrain group name")))?;
        faces(s, &json!({"group": name}))
    };
    let existing = read("existing")?;
    let design = read("design")?;
    let bounds = |triangle: terrain::Triangle| {
        let bounds = cadcraft_geom::Bounds2::from_points(triangle.map(Vec3::xy));
        ([bounds.min.x, bounds.min.y], [bounds.max.x, bounds.max.y])
    };
    let index = RTree::bulk_load(
        design
            .into_iter()
            .map(|triangle| {
                let (min, max) = bounds(triangle);
                GeomWithData::new(Rectangle::from_corners(min, max), triangle)
            })
            .collect(),
    );
    let mut totals = terrain::Earthwork::default();
    let mut pairs = 0;
    for triangle in existing {
        let (min, max) = bounds(triangle);
        for candidate in index.locate_in_envelope_intersecting(AABB::from_corners(min, max)) {
            pairs += 1;
            if pairs > 2_000_000 {
                return Err(bad(VOLUMES, "Too many overlapping triangle pairs."));
            }
            totals += terrain::triangle_earthwork(triangle, candidate.data);
        }
    }
    if totals.area <= 0.0 {
        return Err(bad(VOLUMES, "The two surfaces have no common area."));
    }
    let format = |v| format!("{v:.6}");
    let message = format!("Cut {} | Fill {} | Common area {}", format(totals.cut), format(totals.fill), format(totals.area));
    Ok(json!({"cut": totals.cut, "fill": totals.fill, "netFill": totals.fill - totals.cut, "overlapArea": totals.area, "message": message}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_from_selected_points_and_accepts_an_interactive_contour_interval() {
        let mut session = Session::new();
        let points = [[0, 0, 0], [10, 0, 2], [10, 10, 2], [0, 10, 0]];
        let added = session.execute("point", &json!({"points": points})).unwrap();
        let handles = added["handles"].clone();
        session.execute("select", &json!({"handles": handles})).unwrap();
        session.start(BUILD).unwrap();
        assert!(session.doc().unwrap().model.iter().any(|entity| matches!(entity.kind, EntityKind::Face3d(_))));
        session.start(CONTOURS).unwrap();
        assert_eq!(session.current_prompt().unwrap().message, "Specify contour interval");
        session.cmdline("0.5").unwrap();
        assert!(!session.prompting());
        assert!(
            session
                .doc()
                .unwrap()
                .model
                .iter()
                .any(|entity| matches!(&entity.kind, EntityKind::Polyline3d(pline) if pline.points.iter().all(|p| p.z == 0.5)))
        );
        session.undo().unwrap();
        assert!(session.doc().unwrap().model.iter().all(|entity| !matches!(entity.kind, EntityKind::Polyline3d(_))));
    }
}
