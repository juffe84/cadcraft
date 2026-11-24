//! Read the explicit TIN triangles of a LandXML or Inframodel surface.
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
};

use cadcraft_doc::{Common, Drawing, EntityKind, Face3d, Group, Space};
use cadcraft_geom::Vec3;
use quick_xml::{
    Reader, XmlVersion,
    events::{BytesStart, Event},
};

use crate::{IoError, Result};

const MAX_RECORDS: usize = 2_000_000;
const MAX_DEPTH: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tag {
    LandXml,
    Units,
    Metric,
    Imperial,
    Surfaces,
    Surface,
    Definition,
    Points,
    Faces,
    Other,
}

#[derive(Default)]
struct Surface {
    name: String,
    tin: bool,
    points: HashMap<u64, Vec3>,
    faces: Vec<[u64; 3]>,
}

pub(crate) fn read(bytes: &[u8]) -> Result<Drawing> {
    use Tag::*;

    let xml = std::str::from_utf8(bytes).map_err(|_| error("expected UTF-8 XML"))?;
    let mut reader = Reader::from_str(xml);
    reader.config_mut().expand_empty_elements = true;
    reader.config_mut().trim_text(true);

    let mut drawing = Drawing::new_metric();
    drawing.header.set_i64("INSUNITS", 0);
    let mut path = Vec::new();
    let mut surface = self::Surface::default();
    let mut names = HashSet::new();
    let mut root_seen = false;
    let mut records = 0;
    let mut skipped_surfaces = 0;
    loop {
        match reader.read_event().map_err(error)? {
            Event::Start(start) => {
                if path.is_empty() {
                    if root_seen || start.local_name().as_ref() != "LandXML" {
                        return Err(error("expected a LandXML root element"));
                    }
                    root_seen = true;
                }
                if path.len() >= MAX_DEPTH {
                    return Err(error("XML elements are nested too deeply"));
                }
                let tag = match start.local_name().as_ref() {
                    "LandXML" => LandXml,
                    "Units" => Units,
                    "Metric" => Metric,
                    "Imperial" => Imperial,
                    "Surfaces" => Surfaces,
                    "Surface" => Surface,
                    "Definition" => Definition,
                    "Pnts" => Points,
                    "Faces" => Faces,
                    name => {
                        if let [LandXml, Surfaces, Surface, Definition, container] = path.as_slice()
                            && surface.tin
                            && matches!((container, name), (Points, "P") | (Faces, "F"))
                        {
                            records += 1;
                            if records > MAX_RECORDS {
                                return Err(error("more than 2,000,000 TIN point and face records"));
                            }
                            let content = reader.read_text(start.name()).map_err(error)?;
                            let raw = content.xml_content(XmlVersion::Implicit1_0);
                            let value = quick_xml::escape::unescape(&raw).map_err(error)?;
                            if *container == Points {
                                surface.add_point(&start, &value)?;
                            } else if attr(&start, "i")?.as_deref().map(str::trim) != Some("1") {
                                surface.faces.push(triple(&value, "face point IDs")?);
                            }
                            continue;
                        }
                        Other
                    }
                };
                path.push(tag);
                match path.as_slice() {
                    [LandXml, Units, unit @ (Metric | Imperial)] => set_units(&mut drawing, &start, *unit == Metric)?,
                    [LandXml, Surfaces, Surface] => {
                        surface.name = attr(&start, "name")?.unwrap_or_default().into_owned();
                    }
                    [LandXml, Surfaces, Surface, Definition] => {
                        surface.tin = match attr(&start, "surfType")? {
                            Some(kind) => kind.eq_ignore_ascii_case("TIN"),
                            None => true,
                        };
                    }
                    _ => {}
                }
            }
            Event::End(_) => {
                if path.as_slice() == [LandXml, Surfaces, Surface] && !std::mem::take(&mut surface).add_to(&mut drawing, &mut names)? {
                    skipped_surfaces += 1;
                }
                path.pop();
            }
            Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) if path.is_empty() => {
                return Err(error("unexpected content outside the root element"));
            }
            Event::DocType(_) => return Err(error("DOCTYPE declarations are not supported")),
            Event::Eof => break,
            _ => {}
        }
    }
    if !path.is_empty() || !root_seen {
        return Err(error("incomplete LandXML document"));
    }
    if drawing.groups.is_empty() {
        return Err(error("no surfaces with visible TIN triangles were found"));
    }
    if skipped_surfaces > 0 {
        drawing.read_notes.push(format!("LandXML: {skipped_surfaces} surfaces had no supported visible TIN triangles."));
    }
    Ok(drawing)
}

impl Surface {
    fn add_point(&mut self, start: &BytesStart<'_>, content: &str) -> Result<()> {
        let id = attr(start, "id")?.and_then(|id| id.trim().parse::<u64>().ok()).ok_or_else(|| error("invalid surface point ID"))?;
        // LandXML stores northing, easting, elevation. CAD uses easting as X.
        let [y, x, z] = triple::<f64>(content, "surface coordinates")?;
        if [x, y, z].into_iter().any(|n| !n.is_finite() || n.abs() > crate::MAX_WINDOW_COORD) {
            return Err(error(format!("point {id} has invalid coordinates")));
        }
        if self.points.insert(id, Vec3::new(x, y, z)).is_some() {
            return Err(error(format!("duplicate surface point ID {id}")));
        }
        Ok(())
    }

    fn add_to(self, drawing: &mut Drawing, names: &mut HashSet<String>) -> Result<bool> {
        if self.faces.is_empty() {
            return Ok(false);
        }
        let name = group_name(&self.name, names);
        let mut members = Vec::with_capacity(self.faces.len());
        let point = |id: u64| self.points.get(&id).copied().ok_or_else(|| error(format!("surface {name}: face references missing point {id}")));
        for [a, b, c] in self.faces {
            let [a, b, c] = [point(a)?, point(b)?, point(c)?];
            let face = Face3d { corners: [a, b, c, c], hidden_edges: 0 };
            members.push(drawing.add(&Space::Model, Common::default(), EntityKind::Face3d(face)).map_err(error)?);
        }
        drawing.groups.push(Group { name, description: cadcraft_doc::TIN_GROUP_DESCRIPTION.into(), selectable: true, members });
        Ok(true)
    }
}

fn group_name(name: &str, names: &mut HashSet<String>) -> String {
    let mut base = String::new();
    for ch in name.trim().chars() {
        let ch = if ch.is_control() || "<>/\\\":;?*|,=`".contains(ch) { '_' } else { ch };
        // Leave room for a suffix under the 255-byte GROUP name limit.
        if base.len() + ch.len_utf8() > 240 {
            break;
        }
        base.push(ch);
    }
    if base.trim().is_empty() {
        base = "LandXML Surface".into();
    }
    let mut name = base.trim_end().to_string();
    let mut suffix = names.len() + 1;
    while !names.insert(name.to_ascii_lowercase()) {
        name = format!("{} ({suffix})", base.trim_end());
        suffix += 1;
    }
    name
}

fn set_units(drawing: &mut Drawing, start: &BytesStart<'_>, metric: bool) -> Result<()> {
    let code = match attr(start, "linearUnit")?.as_deref() {
        Some("inch") => 1,
        Some("foot") => 2,
        Some("mile") => 3,
        Some("millimeter") => 4,
        Some("centimeter") => 5,
        Some("meter") => 6,
        Some("kilometer") => 7,
        Some("yard") => 10,
        Some("USSurveyFoot") => 21,
        _ => {
            drawing.read_notes.push("LandXML: The linear unit is unknown. Coordinates were kept and the drawing is unitless.".into());
            0
        }
    };
    drawing.header.set_i64("MEASUREMENT", i64::from(metric));
    drawing.header.set_i64("INSUNITS", code);
    Ok(())
}

fn triple<T: std::str::FromStr>(text: &str, description: &str) -> Result<[T; 3]> {
    let mut fields = text.split_ascii_whitespace();
    let [a, b, c, rest] = std::array::from_fn(|_| fields.next());
    match (a, b, c, rest) {
        (Some(a), Some(b), Some(c), None) => match (a.parse(), b.parse(), c.parse()) {
            (Ok(a), Ok(b), Ok(c)) => Ok([a, b, c]),
            _ => Err(error(format!("invalid {description}"))),
        },
        _ => Err(error(format!("expected three values for {description}"))),
    }
}

fn attr<'a>(start: &'a BytesStart<'_>, key: &str) -> Result<Option<Cow<'a, str>>> {
    start.try_get_attribute(key).map_err(error)?.map(|attr| attr.normalized_value(XmlVersion::Implicit1_0).map_err(error)).transpose()
}

fn error(message: impl std::fmt::Display) -> IoError {
    IoError::Format(format!("LandXML: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic mesh with one hidden triangle, authored for this test.
    const MESH: &str = r#"
        <LandXML xmlns="http://www.inframodel.fi/inframodel">
          <Units><Metric linearUnit="meter"/></Units>
          <Surfaces>
            <Surface name="Cut &amp; Fill">
              <Definition surfType="TIN">
                <Pnts>
                  <P id="10">7000000 450000 4</P>
                  <P id="20">7000010 450000 6</P>
                  <P id="30">7000000 450010 5</P>
                  <P id="40">7000010 450010 5</P>
                </Pnts>
                <Faces><F>10 20 30</F><F i="1">30 20 40</F></Faces>
              </Definition>
            </Surface>
          </Surfaces>
        </LandXML>
    "#;

    #[test]
    fn opens_a_named_terrain_with_world_coordinates_and_its_invisible_faces_excluded() {
        let drawing = crate::read(MESH.as_bytes(), "site.mm.xml").unwrap();
        assert_eq!(drawing.header.i64("INSUNITS", 0), 6);
        let [surface] = drawing.groups.as_slice() else { panic!("expected one terrain group") };
        assert_eq!(surface.name, "Cut & Fill");
        let [member] = surface.members.as_slice() else { panic!("expected only the visible triangle") };
        let entity = drawing.model.get(*member).unwrap();
        let EntityKind::Face3d(face) = &entity.kind else { panic!("expected 3D face") };
        assert_eq!(
            &face.corners[..3],
            &[Vec3::new(450000.0, 7000000.0, 4.0), Vec3::new(450000.0, 7000010.0, 6.0), Vec3::new(450010.0, 7000000.0, 5.0)]
        );
    }

    #[test]
    fn a_face_with_an_undefined_point_fails_the_open() {
        let xml = MESH.replace("10 20 30</F>", "10 99 30</F>");
        let err = crate::read(xml.as_bytes(), "site.landxml").unwrap_err();
        assert!(err.to_string().contains("face references missing point 99"));
    }

    #[test]
    fn surface_point_ids_are_local_and_names_are_unique() {
        let start = MESH.find("<Surface ").unwrap();
        let end = MESH.find("</Surface>").unwrap() + "</Surface>".len();
        let second = MESH[start..end].replace("450000 4", "450000 9");
        let xml = MESH.replace("</Surfaces>", &format!("{second}</Surfaces>"));
        let drawing = crate::read(xml.as_bytes(), "site.landxml").unwrap();
        let [first, second] = drawing.groups.as_slice() else { panic!("expected two terrain groups") };
        assert_ne!(first.name, second.name);
        let first_face = &drawing.model.get(first.members[0]).unwrap().kind;
        let second_face = &drawing.model.get(second.members[0]).unwrap().kind;
        assert_ne!(first_face, second_face);
    }
}
