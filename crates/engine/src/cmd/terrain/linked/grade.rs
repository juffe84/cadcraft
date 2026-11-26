use cadcraft_doc::{Entity, EntityKind, Polyline3d, Space};
use cadcraft_geom::{
    Vec3, shoelace,
    terrain::{GradeTarget, grade_path},
};
use serde_json::{Value, json};

use super::{MAX_VERTICES, SOURCE_PREFIX, build, editable_source, linked_id, model_space, vertices};
use crate::cmd::{CommandSpec, bad, bool_or, str_param};
use crate::{Result, Session};

const ELEVATION: &str = "tin.source.chain.elevation";
const COPY: &str = "tin.source.chain.copy";
const PREVIEW: &str = "tin.source.chain.preview";
const PARAMS: &str = "{group, handle, side?: outside|inside|left|right, round?: bool, rise?: relative elevation change, distance?: horizontal offset, slope?: [vertical,horizontal], elevation?: target elevation, target?: another terrain name, uphill?: bool, attach?: bool}";

pub(super) fn elevation_spec() -> CommandSpec {
    CommandSpec::new(ELEVATION, "Set Source Chain Elevation", flatten).params("{group, handle, elevation}").enabled(model_space)
}

pub(super) fn copy_spec() -> CommandSpec {
    CommandSpec::new(COPY, "Copy Terrain Source Chain", copy).params(PARAMS).enabled(model_space)
}

pub(super) fn preview_spec() -> CommandSpec {
    CommandSpec::new(PREVIEW, "Preview Terrain Chain Copy", preview).params(PARAMS).enabled(model_space).noundo()
}

fn coordinate(p: &Value, key: &str, command: &str) -> Result<f64> {
    p.get(key)
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && n.abs() <= 1e12)
        .ok_or_else(|| bad(command, format!("Enter a valid value for {key}.")))
}

fn flatten(s: &mut Session, p: &Value) -> Result<Value> {
    let elevation = coordinate(p, "elevation", ELEVATION)?;
    let source = editable_source(s.doc()?, p, ELEVATION)?;
    let handle = source.handle;
    let mut output = source.kind.clone();
    let mut converted = false;
    match &mut output {
        EntityKind::Line(line) => {
            line.a.z = elevation;
            line.b.z = elevation;
        }
        EntityKind::Polyline3d(poly) => poly.points.iter_mut().for_each(|point| point.z = elevation),
        EntityKind::LwPolyline(poly) => poly.elevation = elevation,
        EntityKind::Arc(arc) if source.common.extrusion.x == 0.0 && source.common.extrusion.y == 0.0 => arc.center.z = elevation,
        EntityKind::Arc(_) => {
            let points = vertices(source, MAX_VERTICES)?.into_iter().map(|p| p.xy().to3(elevation)).collect();
            output = EntityKind::Polyline3d(Polyline3d { points, closed: false });
            converted = true;
        }
        _ => return Err(bad(ELEVATION, "Select a line, arc or polyline.")),
    }
    if output == source.kind {
        return Ok(json!({"changed":false}));
    }
    s.doc_mut()?.model.modify(handle, |e| {
        e.kind = output;
        if converted {
            e.common.extrusion = Vec3::Z;
        }
    });
    Ok(json!({"changed":true,"elevation":elevation}))
}

fn preview(s: &mut Session, p: &Value) -> Result<Value> {
    let (poly, _) = plan(s, p, PREVIEW)?;
    Ok(json!({"points":poly.points,"closed":poly.closed}))
}

fn copy(s: &mut Session, p: &Value) -> Result<Value> {
    let (poly, source) = plan(s, p, COPY)?;
    let mut common = source.common.clone();
    common.extrusion = Vec3::Z;
    let mut drawing = s.doc()?.clone();
    let id = linked_id(&drawing, p, COPY)?.to_owned();
    let attach = bool_or(p, "attach", true);
    let handle = drawing.add(&Space::Model, common, EntityKind::Polyline3d(poly))?;
    if attach {
        let tag = format!("{SOURCE_PREFIX}{id}");
        let group = drawing.groups.iter_mut().find(|g| g.description == tag).ok_or_else(|| bad(COPY, "The linked source group is missing."))?;
        group.members.push(handle);
        let handles = group.members.clone();
        build(&drawing, &handles).map_err(|e| bad(COPY, format!("The copy cannot be added to this surface. {e}")))?;
    }
    *s.doc_mut()? = drawing;
    Ok(json!({"handle":handle.hex(),"attached":attach}))
}

fn source_path(source: &Entity, command: &str) -> Result<(Vec<Vec3>, bool)> {
    let mut path = vertices(source, MAX_VERTICES)?;
    path.dedup();
    let repeated = path.len() > 2 && path.first() == path.last();
    let closed = repeated
        || match &source.kind {
            EntityKind::LwPolyline(poly) => poly.closed,
            EntityKind::Polyline3d(poly) => poly.closed,
            _ => false,
        };
    if repeated {
        path.pop();
    }
    if path.len() < if closed { 3 } else { 2 } || path.len() == MAX_VERTICES {
        return Err(bad(command, "Choose a source line with enough distinct vertices."));
    }
    Ok((path, closed))
}

fn ratio(p: &Value, command: &str) -> Result<f64> {
    p.get("slope")
        .and_then(Value::as_array)
        .filter(|parts| parts.len() == 2)
        .and_then(|parts| Some((parts[0].as_f64()?, parts[1].as_f64()?)))
        .filter(|(v, h)| v.is_finite() && h.is_finite() && *v > 0.0 && *h > 0.0)
        .map(|(v, h)| h / v)
        .filter(|r| (1e-6..=1e6).contains(r))
        .ok_or_else(|| bad(command, "Use a positive vertical to horizontal slope such as 1:2."))
}

fn side(p: &Value, path: &[Vec3], closed: bool, command: &str) -> Result<f64> {
    match str_param(p, "side").unwrap_or(if closed { "outside" } else { "left" }) {
        "left" => Ok(1.0),
        "right" => Ok(-1.0),
        name @ ("outside" | "inside") if closed => {
            let direction = shoelace(&path.iter().map(|p| p.xy()).collect::<Vec<_>>());
            if direction == 0.0 {
                return Err(bad(command, "The chain must enclose an area to choose inside or outside."));
            }
            Ok(if name == "inside" { direction.signum() } else { -direction.signum() })
        }
        _ => Err(bad(command, "Choose an offset side for the selected chain.")),
    }
}

fn plan<'a>(s: &'a Session, p: &Value, command: &str) -> Result<(Polyline3d, &'a Entity)> {
    let d = s.doc()?;
    let source = editable_source(d, p, command)?;
    let (path, closed) = source_path(source, command)?;
    let side = side(p, &path, closed, command)?;
    let round = bool_or(p, "round", true);
    let faces;
    let target = if let Some(name) = str_param(p, "target") {
        if str_param(p, "group").is_some_and(|group| group.eq_ignore_ascii_case(name)) {
            return Err(bad(command, "Choose a different target terrain surface."));
        }
        faces = super::super::faces(s, &json!({"group":name})).map_err(|_| bad(command, "Choose a target terrain containing triangular faces."))?;
        GradeTarget::Surface { faces: &faces, run_per_rise: ratio(p, command)?, uphill: bool_or(p, "uphill", false) }
    } else if p.get("elevation").is_some() {
        GradeTarget::Elevation { elevation: coordinate(p, "elevation", command)?, run_per_rise: ratio(p, command)? }
    } else {
        let rise = if p.get("rise").is_some() { coordinate(p, "rise", command)? } else { 0.0 };
        let distance = if p.get("distance").is_some() { coordinate(p, "distance", command)? } else { rise.abs() * ratio(p, command)? };
        if !(0.0..=1e12).contains(&distance) {
            return Err(bad(command, "The horizontal offset must be non-negative."));
        }
        if distance == 0.0 && bool_or(p, "attach", true) {
            return Err(bad(command, "Set a horizontal offset or create the copy as a separate drawing object."));
        }
        GradeTarget::Offset { distance, rise }
    };
    let points = grade_path(&path, closed, side, round, target, MAX_VERTICES - usize::from(closed)).map_err(|e| bad(command, e))?;
    if bool_or(p, "attach", true) && points == path {
        return Err(bad(command, "The copy coincides with the source. Change the target or create a separate drawing object."));
    }
    Ok((Polyline3d { points, closed }, source))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cadcraft_doc::Handle;

    fn foundation() -> (Session, Value, Vec<Vec3>) {
        let mut session = Session::new();
        let vertices = [[0.0, 0.0, 4.0], [8.0, 0.0, 5.0], [8.0, 6.0, 6.0], [0.0, 6.0, 7.0]];
        let polygon = session.execute("3dpoly", &json!({"points":vertices,"closed":true})).unwrap();
        let handle = polygon["handle"].clone();
        session.execute("tin.link", &json!({"name":"Footing","handles":[handle]})).unwrap();
        (session, handle, vertices.into_iter().map(|[x, y, z]| Vec3::new(x, y, z)).collect())
    }

    fn polyline(s: &Session, handle: &Value) -> Polyline3d {
        let handle = Handle::parse_hex(handle.as_str().unwrap()).unwrap();
        let EntityKind::Polyline3d(poly) = &s.doc().unwrap().model.get(handle).unwrap().kind else {
            panic!("expected a 3D polyline");
        };
        poly.clone()
    }

    #[test]
    fn elevation_changes_every_vertex_and_one_undo_restores_the_chain() {
        let (mut s, handle, original) = foundation();
        s.execute(ELEVATION, &json!({"group":"Footing","handle":handle,"elevation":3.5})).unwrap();
        let level = polyline(&s, &handle);
        assert!(level.closed);
        assert_eq!(level.points, original.iter().map(|point| point.xy().to3(3.5)).collect::<Vec<_>>());
        let surface = super::super::super::faces(&s, &json!({"group":"Footing"})).unwrap();
        assert!(surface.iter().flatten().all(|point| point.z == 3.5));
        s.execute("u", &Value::Null).unwrap();
        assert_eq!(polyline(&s, &handle).points, original);
    }

    #[test]
    fn slope_copy_rounds_outside_corners_and_is_added_to_the_surface() {
        let (mut s, handle, _) = foundation();
        s.execute(ELEVATION, &json!({"group":"Footing","handle":handle,"elevation":10.0})).unwrap();
        let params = json!({"group":"Footing","handle":handle,"elevation":9.0,"slope":[1,2],"side":"outside","round":true});
        let preview = s.execute(PREVIEW, &params).unwrap();
        let copy = s.execute(COPY, &params).unwrap();
        let path = polyline(&s, &copy["handle"]);
        assert!(path.closed && path.points.len() > 8 && path.points.iter().all(|p| p.z == 9.0));
        assert!(path.points.iter().any(|p| p.x < 0.0 && p.y < 0.0 && (p.xy().len() - 2.0).abs() < 1e-8));
        assert!(path.points.iter().any(|p| (p.y + 2.0).abs() < 1e-8));
        assert_eq!(preview["points"], json!(path.points));
        assert!(polyline(&s, &handle).points.iter().all(|p| p.z == 10.0));
        let result = s.execute("tin.source.points", &json!({"group":"Footing"})).unwrap();
        assert!(result["points"].as_array().unwrap().iter().any(|point| point["handle"] == copy["handle"]));
    }

    #[test]
    fn slope_to_another_surface_uses_the_intersection_and_rejects_a_miss() {
        let (mut s, handle, _) = foundation();
        s.execute(ELEVATION, &json!({"group":"Footing","handle":handle,"elevation":10.0})).unwrap();
        s.execute("tin", &json!({"name":"Ground","standalone":true,"points":[[-20,-20,2],[30,-20,7],[30,30,7],[-20,30,2]]})).unwrap();
        let mut params = json!({"group":"Footing","handle":handle,"target":"Ground","slope":[1,2],"side":"outside","round":true,"attach":false});
        let copy = s.execute(COPY, &params).unwrap();
        let path = polyline(&s, &copy["handle"]);
        assert!(path.points.len() > 8);
        assert!(path.points.iter().all(|p| (p.z - (4.0 + p.x * 0.1)).abs() < 1e-7));
        let count = s.doc().unwrap().entity_count();
        params["uphill"] = json!(true);
        assert!(s.execute(COPY, &params).is_err());
        assert_eq!(s.doc().unwrap().entity_count(), count);
    }
}
