use std::{
    collections::{HashMap, HashSet},
    io::Write,
};

use cadcraft_doc::{
    Drawing, EntityKind, Group,
    terrain::{SOURCE_PREFIX, group_members, source_vertices},
};
use cadcraft_geom::{
    Vec3,
    terrain::{Triangle, snap_terrain_mesh, snap_terrain_paths},
};

use super::{MAX_RECORDS, error, metres_per_unit};
use crate::Result;

fn units(drawing: &Drawing) -> Result<(&'static str, &'static str)> {
    let unit = match (drawing.header.i64("INSUNITS", 0), drawing.header.i64("MEASUREMENT", 1)) {
        (0, 0) => 2,
        (0, _) => 6,
        (unit, _) => unit,
    };
    Ok(match unit {
        1 => ("Imperial", "inch"),
        2 => ("Imperial", "foot"),
        3 => ("Imperial", "mile"),
        4 => ("Metric", "millimeter"),
        5 => ("Metric", "centimeter"),
        6 => ("Metric", "meter"),
        7 => ("Metric", "kilometer"),
        21 => ("Imperial", "USSurveyFoot"),
        _ => return Err(error("the drawing's linear unit is not supported for export")),
    })
}

pub(crate) fn write(drawing: &Drawing) -> Result<Vec<u8>> {
    write_document(drawing, false)
}

pub(crate) fn write_survey(drawing: &Drawing) -> Result<Vec<u8>> {
    write_document(drawing, true)
}

fn grid_step(unit: &str) -> Result<f64> {
    let metres = metres_per_unit(unit).ok_or_else(|| error("unknown survey grid unit"))?;
    Ok(0.001 / metres)
}

fn write_document(drawing: &Drawing, survey: bool) -> Result<Vec<u8>> {
    let mut groups = drawing.groups.iter().filter(|g| g.is_terrain_surface() && !g.members.is_empty()).peekable();
    if groups.peek().is_none() {
        return Err(error("no terrain surfaces with triangles to export"));
    }
    let (system, unit) = units(drawing)?;
    let step = survey.then(|| grid_step(unit)).transpose()?;
    let seconds = web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map_err(error)?.as_secs();
    let now = i64::try_from(seconds).ok().and_then(chrono::DateTime::from_timestamp_secs).ok_or_else(|| error("invalid export date"))?;
    let mut out = Vec::new();
    writeln!(out, "<?xml version=\"1.0\" encoding=\"UTF-8\"?>").map_err(error)?;
    writeln!(out, "<LandXML xmlns=\"http://www.landxml.org/schema/LandXML-1.2\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:schemaLocation=\"http://www.landxml.org/schema/LandXML-1.2 http://www.landxml.org/schema/LandXML-1.2/LandXML-1.2.xsd\" version=\"1.2\" date=\"{}\" time=\"{}\">", now.format("%Y-%m-%d"), now.format("%H:%M:%SZ")).map_err(error)?;
    let (area, volume, temperature, pressure) =
        if system == "Metric" { ("squareMeter", "cubicMeter", "celsius", "HPA") } else { ("squareFoot", "cubicFeet", "fahrenheit", "inchHG") };
    writeln!(out, "  <Units><{system} linearUnit=\"{unit}\" areaUnit=\"{area}\" volumeUnit=\"{volume}\" temperatureUnit=\"{temperature}\" pressureUnit=\"{pressure}\"/></Units>").map_err(error)?;
    writeln!(out, "  <Surfaces>").map_err(error)?;
    let mut remaining = MAX_RECORDS;
    for group in groups {
        write_surface(&mut out, drawing, group, &mut remaining, step)?;
    }
    writeln!(out, "  </Surfaces>\n</LandXML>").map_err(error)?;
    Ok(out)
}

fn check_point(p: Vec3) -> Result<()> {
    if !p.is_finite() || p.x.abs().max(p.y.abs()).max(p.z.abs()) > crate::MAX_WINDOW_COORD {
        return Err(error("terrain coordinates are outside the supported export range"));
    }
    Ok(())
}

fn read_surface(drawing: &Drawing, group: &Group) -> Result<Vec<Triangle>> {
    group
        .members
        .iter()
        .map(|&handle| {
            let Some(EntityKind::Face3d(face)) = drawing.model.get(handle).map(|e| &e.kind) else {
                return Err(error(format!("surface {} has a missing or unsupported face", group.name)));
            };
            let [a, b, c, d] = face.corners;
            if c != d {
                return Err(error(format!("surface {} contains a face with four corners", group.name)));
            }
            for p in [a, b, c] {
                check_point(p)?;
            }
            Ok([a, b, c])
        })
        .collect()
}

fn write_surface(out: &mut Vec<u8>, drawing: &Drawing, group: &Group, remaining: &mut usize, step: Option<f64>) -> Result<()> {
    if group.members.len() > *remaining {
        return Err(error("too many TIN records"));
    }
    let triangles = read_surface(drawing, group)?;
    let triangles = if let Some(step) = step { snap_terrain_mesh(&triangles, step).map_err(error)? } else { triangles };
    *remaining = remaining.checked_sub(triangles.len()).ok_or_else(|| error("too many TIN records"))?;
    let mut points = Vec::new();
    let mut ids = HashMap::new();
    let mut faces = Vec::with_capacity(triangles.len());
    for triangle in triangles {
        let mut face = [0; 3];
        for (id, p) in face.iter_mut().zip(triangle) {
            check_point(p)?;
            let key = [p.x, p.y, p.z].map(|value| if value == 0.0 { 0 } else { value.to_bits() });
            *id = *ids.entry(key).or_insert_with(|| {
                points.push(p);
                points.len()
            });
        }
        faces.push(face);
    }
    *remaining = remaining.checked_sub(points.len()).ok_or_else(|| error("too many TIN records"))?;
    let name = quick_xml::escape::escape(&group.name);
    writeln!(out, "    <Surface name=\"{name}\">").map_err(error)?;
    write_sources(out, drawing, group, remaining, step)?;
    writeln!(out, "      <Definition surfType=\"TIN\">\n        <Pnts>").map_err(error)?;
    for (i, p) in points.iter().enumerate() {
        write!(out, "          <P id=\"{}\">", i + 1).map_err(error)?;
        write_coordinate(out, *p, step)?;
        writeln!(out, "</P>").map_err(error)?;
    }
    writeln!(out, "        </Pnts>\n        <Faces>").map_err(error)?;
    for [a, b, c] in faces {
        writeln!(out, "          <F>{a} {b} {c}</F>").map_err(error)?;
    }
    writeln!(out, "        </Faces>\n      </Definition>\n    </Surface>").map_err(error)?;
    Ok(())
}

fn write_sources(out: &mut Vec<u8>, drawing: &Drawing, group: &Group, remaining: &mut usize, step: Option<f64>) -> Result<()> {
    let Some(id) = group.linked_terrain_id() else { return Ok(()) };
    let mut handles = Vec::new();
    let mut paths = Vec::new();
    for &handle in group_members(drawing, &format!("{SOURCE_PREFIX}{id}")) {
        let entity = drawing.model.get(handle).ok_or_else(|| error(format!("missing terrain source {}", handle.hex())))?;
        let mut path = source_vertices(entity, *remaining).map_err(error)?;
        *remaining -= path.len();
        for &p in &path {
            check_point(p)?;
        }
        path.dedup();
        if path.is_empty() {
            return Err(error(format!("empty terrain source {}", handle.hex())));
        }
        handles.push(handle.hex());
        paths.push(path);
    }
    let (lines, points) = source_records(handles, paths, step)?;
    if lines.is_empty() && points.is_empty() {
        return Ok(());
    }
    writeln!(out, "      <SourceData>").map_err(error)?;
    if !points.is_empty() {
        write!(out, "        <DataPoints><PntList3D>").map_err(error)?;
        write_path(out, &points, step)?;
        writeln!(out, "</PntList3D></DataPoints>").map_err(error)?;
    }
    if !lines.is_empty() {
        writeln!(out, "        <Breaklines>").map_err(error)?;
        for (name, path) in lines {
            write!(out, "          <Breakline name=\"{name}\" brkType=\"standard\"><PntList3D>").map_err(error)?;
            write_path(out, &path, step)?;
            writeln!(out, "</PntList3D></Breakline>").map_err(error)?;
        }
        writeln!(out, "        </Breaklines>").map_err(error)?;
    }
    writeln!(out, "      </SourceData>").map_err(error)?;
    Ok(())
}

type SourceRecords = (Vec<(String, Vec<Vec3>)>, Vec<Vec3>);

fn source_records(handles: Vec<String>, paths: Vec<Vec<Vec3>>, step: Option<f64>) -> Result<SourceRecords> {
    let paths = if let Some(step) = step { snap_terrain_paths(&paths, step).map_err(error)? } else { paths };
    let mut lines = Vec::new();
    let mut points = Vec::new();
    let mut edges = HashSet::new();
    for (name, path) in handles.into_iter().zip(paths) {
        if path.len() == 1 {
            points.extend(path);
        } else if let Some(step) = step {
            append_survey_lines(&mut lines, &mut points, &mut edges, name, path, step);
        } else {
            lines.push((name, path));
        }
    }
    if step.is_some() {
        let mut unique = HashSet::new();
        points.retain(|p| unique.insert([p.x.to_bits(), p.y.to_bits(), p.z.to_bits()]));
    }
    Ok((lines, points))
}

fn append_survey_lines(
    lines: &mut Vec<(String, Vec<Vec3>)>,
    points: &mut Vec<Vec3>,
    edges: &mut HashSet<[[u64; 3]; 2]>,
    name: String,
    path: Vec<Vec3>,
    step: f64,
) {
    let mut current = Vec::new();
    let mut part = 0;
    let flush = |lines: &mut Vec<_>, current: &mut Vec<_>, part: &mut usize| {
        if current.len() > 1 {
            *part += 1;
            let label = if *part == 1 { name.clone() } else { format!("{name}-{part}") };
            lines.push((label, std::mem::take(current)));
        }
    };
    for pair in path.windows(2) {
        let [a, b] = [pair[0], pair[1]];
        if a.xy() == b.xy() {
            points.extend([a, b]);
            flush(lines, &mut current, &mut part);
            continue;
        }
        let mut edge = [a, b].map(|p| [p.x.to_bits(), p.y.to_bits(), (p.z / (step * 0.1)).round().to_bits()]);
        edge.sort_unstable();
        if !edges.insert(edge) {
            flush(lines, &mut current, &mut part);
            continue;
        }
        if current.is_empty() {
            current.push(a);
        }
        current.push(b);
    }
    flush(lines, &mut current, &mut part);
}

fn write_coordinate(out: &mut Vec<u8>, p: Vec3, step: Option<f64>) -> Result<()> {
    if step == Some(0.001) {
        write!(out, "{:.3} {:.3} {}", p.y, p.x, p.z).map_err(error)
    } else {
        write!(out, "{} {} {}", p.y, p.x, p.z).map_err(error)
    }
}

fn write_path(out: &mut Vec<u8>, path: &[Vec3], step: Option<f64>) -> Result<()> {
    for (i, p) in path.iter().enumerate() {
        if i > 0 {
            write!(out, " ").map_err(error)?;
        }
        write_coordinate(out, *p, step)?;
    }
    Ok(())
}
