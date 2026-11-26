//! LandXML 1.2 / buildingSMART Finland Inframodel route data, sections 3.3–3.5:
//! <https://buildingsmart.fi/infra/bSI_LandXML12_MVD/pages/3_Alignments.html>.
//! The centerline stays in model coordinates. Profile sheets live in named paper layouts.

mod horizontal;
mod profile;

use std::collections::HashSet;

use cadcraft_doc::{Common, Drawing, EntityKind, Group, Polyline3d, Space};
use cadcraft_geom::Vec2;
use quick_xml::{
    Reader, XmlVersion,
    events::{BytesStart, Event},
};

use self::horizontal::{Point, Span};
use self::profile::{Curve, Profile, Pvi, units_value};
use super::{MAX_DEPTH, MAX_RECORDS, attr, error, group_name};
use crate::Result;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tag {
    Geometry,
    Span,
    Profile,
    Design,
    Other,
}

enum Kind {
    Line,
    Arc,
    Spiral,
    PointList,
    Unknown,
}

struct Element {
    kind: Kind,
    direction: Option<f64>,
    length: Option<f64>,
    station: Option<f64>,
    curvatures: Option<[f64; 2]>,
    start: Option<Point>,
    end: Option<Point>,
    center: Option<Point>,
    points: Vec<Point>,
}

impl Element {
    fn new(xml: &BytesStart<'_>) -> Result<Self> {
        let kind = match xml.local_name().as_ref() {
            "Line" => Kind::Line,
            "Curve" => Kind::Arc,
            "Spiral" => Kind::Spiral,
            "IrregularLine" => Kind::PointList,
            _ => Kind::Unknown,
        };
        let direction = match attr(xml, "rot")?.as_deref() {
            Some("ccw") => Some(1.0),
            Some("cw") => Some(-1.0),
            _ => None,
        };
        let curvatures = if matches!(kind, Kind::Spiral) && attr(xml, "spiType")?.is_some_and(|t| t.eq_ignore_ascii_case("clothoid")) {
            direction.zip(curvature(xml, "radiusStart")?).zip(curvature(xml, "radiusEnd")?).map(|((sign, a), b)| [sign * a, sign * b])
        } else {
            None
        };
        Ok(Self {
            kind,
            direction,
            length: number(xml, "length")?,
            station: number(xml, "staStart")?,
            curvatures,
            start: None,
            end: None,
            center: None,
            points: Vec::new(),
        })
    }

    fn spans(self, tolerance: f64, out: &mut Vec<Span>) -> std::result::Result<(), &'static str> {
        if matches!(self.kind, Kind::PointList) {
            let mut points = self.points;
            if let Some(start) = self.start {
                if points.first().is_some_and(|p| p.xy.near(start.xy, tolerance)) {
                    points[0] = start;
                } else {
                    points.insert(0, start);
                }
            }
            if let Some(end) = self.end {
                if let Some(last) = points.last_mut().filter(|p| p.xy.near(end.xy, tolerance)) {
                    *last = end;
                } else {
                    points.push(end);
                }
            }
            if points.len() < 2 {
                return Err("irregular line has fewer than two points");
            }
            for (i, pair) in points.windows(2).enumerate() {
                let station = if i == 0 { self.station } else { None };
                out.push(Span::line(pair[0], pair[1], station)?);
            }
            return Ok(());
        }

        if matches!(self.kind, Kind::Unknown) {
            return Err("unsupported element in CoordGeom");
        }
        let (start, end) = self.start.zip(self.end).ok_or("horizontal segment is missing its start or end coordinates")?;
        let station = self.station;
        let span = match self.kind {
            Kind::Line => Span::line(start, end, station),
            Kind::Arc => Span::arc(
                start,
                end,
                self.center.ok_or("circular arc is missing its center")?.xy,
                self.direction.ok_or("circular arc has no supported rotation direction")?,
                self.length,
                station,
                tolerance,
            ),
            Kind::Spiral => Span::clothoid(
                start,
                end,
                self.length.ok_or("clothoid is missing its length")?,
                self.curvatures.ok_or("unsupported spiral type, radii or rotation direction")?,
                station,
                tolerance,
            ),
            _ => return Err("unsupported element in CoordGeom"),
        }?;
        out.push(span);
        Ok(())
    }
}

#[derive(Default)]
struct Design {
    name: String,
    points: Vec<Pvi>,
    unsupported: bool,
}

pub(super) struct Alignment {
    name: String,
    start: Option<f64>,
    station_equation: bool,
    elements: Vec<Element>,
    designs: Vec<Design>,
}

pub(super) fn read(reader: &mut Reader<&[u8]>, start: &BytesStart<'_>, records: &mut usize) -> Result<Alignment> {
    let mut alignment = Alignment {
        name: attr(start, "name")?.unwrap_or_default().chars().take(240).collect(),
        start: number(start, "staStart")?,
        station_equation: false,
        elements: Vec::new(),
        designs: Vec::new(),
    };
    count(records, 1)?;
    let mut path = Vec::new();
    let mut element = None;
    let mut design: Option<Design> = None;
    loop {
        match reader.read_event().map_err(error)? {
            Event::Start(xml) => {
                if path.len() + 3 >= MAX_DEPTH {
                    return Err(error("XML elements are nested too deeply"));
                }
                use Tag::*;
                let name = xml.local_name();
                let name = name.as_ref();
                let tag = match (path.as_slice(), name) {
                    ([], "CoordGeom") => Geometry,
                    ([], "Profile") => Profile,
                    ([], "StaEquation") => {
                        let back = number(&xml, "staBack")?;
                        let ahead = number(&xml, "staAhead")?;
                        let internal = number(&xml, "staInternal")?;
                        alignment.station_equation |= back != ahead || internal.is_some_and(|s| Some(s) != ahead);
                        Other
                    }
                    ([Geometry], "Feature") => Other,
                    ([Geometry], _) => {
                        count(records, 1)?;
                        element = Some(Element::new(&xml)?);
                        Span
                    }
                    ([Geometry, Span], "Start" | "Center" | "End") => {
                        let value = text(reader, &xml)?;
                        let location = point(&value)?;
                        let e = element.as_mut().ok_or_else(|| error("missing horizontal element"))?;
                        let slot = match name {
                            "Start" => &mut e.start,
                            "End" => &mut e.end,
                            _ => &mut e.center,
                        };
                        if slot.replace(location).is_some() {
                            return Err(error("repeated horizontal coordinate"));
                        }
                        continue;
                    }
                    ([Geometry, Span], "PntList3D" | "PntList2D") => {
                        let value = text(reader, &xml)?;
                        let stride = if name == "PntList3D" { 3 } else { 2 };
                        let e = element.as_mut().ok_or_else(|| error("missing horizontal element"))?;
                        let mut values = value.split_ascii_whitespace();
                        while let Some(north) = values.next() {
                            count(records, 1)?;
                            let east = values.next().ok_or_else(|| error("incomplete alignment point list"))?;
                            let z = if stride == 3 {
                                Some(scalar(values.next().ok_or_else(|| error("incomplete alignment point list"))?)?)
                            } else {
                                None
                            };
                            e.points.push(Point { xy: Vec2::new(scalar(east)?, scalar(north)?), z });
                        }
                        continue;
                    }
                    ([Profile], "ProfAlign") => {
                        count(records, 1)?;
                        design =
                            Some(self::Design { name: attr(&xml, "name")?.unwrap_or_default().chars().take(240).collect(), ..Default::default() });
                        Design
                    }
                    ([Profile, Design], "PVI" | "ParaCurve" | "CircCurve") => {
                        count(records, 1)?;
                        let value = text(reader, &xml)?;
                        let mut fields = value.split_ascii_whitespace();
                        let station = fields.next().ok_or_else(|| error("missing profile station")).and_then(scalar)?;
                        let z = fields.next().ok_or_else(|| error("missing profile elevation")).and_then(scalar)?;
                        if fields.next().is_some() {
                            return Err(error("expected one station and one elevation in profile"));
                        }
                        let curve = match name {
                            "ParaCurve" => Curve::Parabola(number(&xml, "length")?.ok_or_else(|| error("missing vertical curve length"))?),
                            "CircCurve" => Curve::Circle {
                                radius: number(&xml, "radius")?.ok_or_else(|| error("missing vertical curve radius"))?,
                                length: number(&xml, "length")?,
                            },
                            _ => Curve::Tangent,
                        };
                        let profile = design.as_mut().ok_or_else(|| error("missing design profile"))?;
                        profile.points.push(Pvi { station, z, curve });
                        continue;
                    }
                    ([Profile, Design], "UnsymParaCurve") => {
                        if let Some(profile) = &mut design {
                            profile.unsupported = true;
                        }
                        Other
                    }
                    _ => Other,
                };
                path.push(tag);
            }
            Event::End(_) => match path.pop() {
                Some(Tag::Span) => alignment.elements.extend(element.take()),
                Some(Tag::Design) => alignment.designs.extend(design.take()),
                Some(_) => {}
                None => break,
            },
            Event::DocType(_) => return Err(error("DOCTYPE declarations are not supported")),
            Event::Eof => return Err(error("incomplete alignment element")),
            _ => {}
        }
    }
    Ok(alignment)
}

impl Alignment {
    pub(super) fn add_to(self, drawing: &mut Drawing, names: &mut HashSet<String>, remaining_samples: &mut usize) -> Result<()> {
        let Self { name, start, station_equation, elements, designs } = self;
        let name = group_name(&name, "LandXML Alignment", names);
        let units = linear_unit(drawing);
        let tolerance = units_value(drawing, 0.01);
        let continuity = units_value(drawing, 0.02);
        let profile_end_tolerance = units_value(drawing, 0.10);
        let mut span_list = Vec::with_capacity(elements.len());
        let horizontal = elements.into_iter().try_for_each(|e| e.spans(continuity, &mut span_list)).and_then(|()| {
            for pair in span_list.windows(2) {
                let (a, b) = (pair[0].end, pair[1].start);
                if !a.xy.near(b.xy, continuity) {
                    return Err("horizontal elements do not join");
                }
            }
            if span_list.is_empty() { Err("no horizontal geometry") } else { Ok(()) }
        });
        let start = start.or_else(|| span_list.first().and_then(|s| s.station)).unwrap_or(0.0);
        let mut station = start;
        let mut stations = Vec::with_capacity(span_list.len() + 1);
        let mut consistent = !station_equation;
        for (i, span) in span_list.iter().enumerate() {
            if span.station.is_some_and(|s| (s - station).abs() > continuity) {
                consistent = false;
            }
            stations.push(station);
            let end = station + span.length;
            station = span_list.get(i + 1).and_then(|next| next.station).filter(|&s| s > station && (s - end).abs() <= continuity).unwrap_or(end);
        }
        stations.push(station);

        let mut primary = None;
        let has_design = !designs.is_empty();
        for design in designs {
            let profile_name = design.name.trim();
            let title =
                if profile_name.is_empty() || profile_name.eq_ignore_ascii_case(&name) { name.clone() } else { format!("{name} - {profile_name}") };
            let result = if design.unsupported {
                Err("unsupported asymmetric vertical curve")
            } else {
                Profile::new(&design.points, continuity).and_then(|p| p.samples(tolerance, *remaining_samples).map(|samples| (p, samples)))
            };
            match result {
                Ok((profile, samples)) => {
                    if drawing.layouts.len() >= 256 {
                        note(drawing, &name, "profile sheet limit reached");
                        break;
                    }
                    profile::add_layout(drawing, &title, &samples, units)?;
                    *remaining_samples -= samples.len();
                    if primary.is_none() && consistent && horizontal.is_ok() && profile.covers(start, station, profile_end_tolerance) {
                        if !profile.covers(start, station, continuity) {
                            note(drawing, &title, "a short alignment end outside the profile uses the nearest profile elevation");
                        }
                        primary = Some((profile, samples));
                    }
                }
                Err(reason) => note(drawing, &title, reason),
            }
        }

        if let Err(reason) = horizontal {
            note(drawing, &name, reason);
            return Ok(());
        }
        if primary.is_none() && has_design {
            let reason = if consistent {
                "no supported design profile covers the alignment; used horizontal coordinates for Z"
            } else {
                "station equations or discontinuities prevent matching the profile; used horizontal coordinates for Z"
            };
            note(drawing, &name, reason);
        }
        let primary = primary.as_ref().map(|(p, s)| (p, s.as_slice()));
        if primary.is_none() && span_list.iter().any(|s| s.start.z.is_none() || s.end.z.is_none()) {
            note(drawing, &name, "missing Z values were filled from the other endpoint or zero");
        }
        let mut points = Vec::new();
        let sampled = span_list.iter().zip(stations.windows(2)).try_for_each(|(span, bounds)| {
            if bounds[0] >= bounds[1] {
                return Err("alignment is too short for its station values");
            }
            span.append(&mut points, bounds[0], bounds[1], primary, tolerance, *remaining_samples)
        });
        if let Err(reason) = sampled {
            note(drawing, &name, reason);
            return Ok(());
        }
        *remaining_samples -= points.len();
        let (points, vertex_stations): (Vec<_>, Vec<_>) = points.into_iter().unzip();
        use cadcraft_doc::road::{ALIGNMENT_GROUP, STATION_APP, Stationing, register, store_metadata};
        let mut common = Common::default();
        if consistent {
            let chord_length = points.windows(2).map(|v| v[0].xy().dist(v[1].xy())).sum();
            let mut stationing = Stationing { from: start, to: station, chord_length, vertices: vertex_stations };
            if !store_metadata(&mut common, STATION_APP, &serde_json::to_string(&stationing).map_err(error)?) {
                stationing.vertices.clear();
                store_metadata(&mut common, STATION_APP, &serde_json::to_string(&stationing).map_err(error)?);
            }
            register(drawing, STATION_APP);
        }
        let handle = drawing.add(&Space::Model, common, EntityKind::Polyline3d(Polyline3d { points, closed: false })).map_err(error)?;
        drawing.groups.push(Group { name, description: ALIGNMENT_GROUP.into(), selectable: true, members: vec![handle] });
        Ok(())
    }
}

fn note(drawing: &mut Drawing, name: &str, message: &str) {
    if drawing.read_notes.len() < 128 {
        drawing.read_notes.push(format!("LandXML: {name}: {message}."));
    }
}

fn count(records: &mut usize, n: usize) -> Result<()> {
    *records += n;
    if *records > MAX_RECORDS { Err(error("more than 2,000,000 geometry records")) } else { Ok(()) }
}

fn text(reader: &mut Reader<&[u8]>, xml: &BytesStart<'_>) -> Result<String> {
    let content = reader.read_text(xml.name()).map_err(error)?;
    let raw = content.xml_content(XmlVersion::Implicit1_0);
    quick_xml::escape::unescape(&raw).map(|value| value.into_owned()).map_err(error)
}

fn point(value: &str) -> Result<Point> {
    let mut fields = value.split_ascii_whitespace();
    let north = fields.next().ok_or_else(|| error("missing northing")).and_then(scalar)?;
    let east = fields.next().ok_or_else(|| error("missing easting")).and_then(scalar)?;
    let z = fields.next().map(scalar).transpose()?;
    if fields.next().is_some() {
        return Err(error("unexpected extra values in alignment coordinates"));
    }
    Ok(Point { xy: Vec2::new(east, north), z })
}

fn scalar(value: &str) -> Result<f64> {
    value
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && v.abs() <= crate::MAX_WINDOW_COORD)
        .ok_or_else(|| error("invalid numeric value in alignment or profile"))
}

fn number(start: &BytesStart<'_>, key: &str) -> Result<Option<f64>> {
    attr(start, key)?.as_deref().map(scalar).transpose()
}

fn curvature(xml: &BytesStart<'_>, key: &str) -> Result<Option<f64>> {
    let value = attr(xml, key)?;
    match value.as_deref().map(str::trim) {
        Some(value) if value.eq_ignore_ascii_case("INF") => Ok(Some(0.0)),
        Some(value) => Ok(Some(scalar(value)?).filter(|&radius| radius > 0.0).map(f64::recip)),
        None => Ok(None),
    }
}

fn linear_unit(drawing: &Drawing) -> &'static str {
    match drawing.header.i64("INSUNITS", 0) {
        1 => "inch",
        2 => "foot",
        3 => "mile",
        4 => "millimeter",
        5 => "centimeter",
        6 => "meter",
        7 => "kilometer",
        10 => "yard",
        21 => "USSurveyFoot",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use cadcraft_geom::Vec3;

    use super::*;
    use crate::landxml::read;

    // Synthetic LandXML 1.2 geometry authored for these tests from Inframodel sections 3.3–3.5.
    const ROAD: &[u8] = br#"<l:LandXML xmlns:l="http://www.landxml.org/schema/LandXML-1.2" version="1.2">
        <l:Units><l:Metric linearUnit="meter"/></l:Units>
        <l:Alignments><l:Alignment name="Road &amp; ramp" staStart="1000" length="100">
          <l:CoordGeom><l:Line staStart="1000"><l:Start>7000000 450000</l:Start><l:End>7000000 450100</l:End></l:Line></l:CoordGeom>
          <l:Profile><l:ProfAlign name="Design"><l:PVI>1000 10</l:PVI><l:ParaCurve length="40">1050 20</l:ParaCurve><l:PVI>1100 10</l:PVI></l:ProfAlign></l:Profile>
        </l:Alignment></l:Alignments>
      </l:LandXML>"#;

    fn model_path<'a>(d: &'a Drawing, name: &str) -> &'a [Vec3] {
        let group = d.groups.iter().find(|g| g.name == name).unwrap();
        let EntityKind::Polyline3d(line) = &d.model.get(group.members[0]).unwrap().kind else { panic!("expected a 3D centerline") };
        &line.points
    }

    #[test]
    fn opens_alignment_without_tin_and_preserves_the_profile_in_dxf() {
        let d = read(ROAD).unwrap();
        assert!(d.read_notes.is_empty(), "{:?}", d.read_notes);
        let check = |d: &Drawing| {
            let points = model_path(d, "Road & ramp");
            assert_eq!(points.first(), Some(&Vec3::new(450_000.0, 7_000_000.0, 10.0)));
            assert_eq!(points.last(), Some(&Vec3::new(450_100.0, 7_000_000.0, 10.0)));
            let crest = points.iter().max_by(|a, b| a.z.total_cmp(&b.z)).unwrap();
            assert!((crest.x - 450_050.0).abs() < 0.01 && (crest.z - 18.0).abs() < 0.01);
            let profile = d.layout("Profile - Road & ramp - Design").unwrap();
            assert!(profile.entities.iter().any(|e| matches!(&e.kind, EntityKind::Text(t) if t.value == "Station (meter)")));
            assert!(profile.entities.iter().any(|e| matches!(&e.kind, EntityKind::Polyline3d(poly) if poly.points.len() > 3)));
        };
        check(&d);
        let dxf = crate::write_dxf(&d);
        check(&crate::read_dxf(dxf.as_bytes()).unwrap());
    }

    #[test]
    fn imports_curves_clothoids_and_tin_without_changing_easting_northing_order() {
        let d = read(
            br#"<LandXML>
          <Surfaces><Surface name="Ground"><Definition surfType="TIN"><Pnts>
            <P id="1">0 0 1</P><P id="2">0 1 1</P><P id="3">1 0 1</P>
          </Pnts><Faces><F>1 2 3</F></Faces></Definition></Surface></Surfaces>
          <Alignments>
            <Alignment name="Arc"><CoordGeom><Curve rot="ccw">
              <Start>0 10 2</Start><Center>0 0</Center><End>10 0 3</End>
            </Curve></CoordGeom></Alignment>
            <Alignment name="Clothoid"><CoordGeom><Spiral rot="ccw" spiType="clothoid" length="40" radiusStart="INF" radiusEnd="120">
              <Start>0 0 1</Start><End>2.217816946252 39.889031687131 1</End>
            </Spiral></CoordGeom></Alignment>
          </Alignments>
        </LandXML>"#,
        )
        .unwrap();
        assert!(d.read_notes.is_empty(), "{:?}", d.read_notes);
        assert!(d.groups.iter().any(|g| g.name == "Ground" && g.is_terrain_surface()));
        let arc = model_path(&d, "Arc");
        assert_eq!(arc.last(), Some(&Vec3::new(0.0, 10.0, 3.0)));
        assert!(arc.iter().any(|p| p.x > 6.5 && p.y > 6.5 && p.z > 2.4 && p.z < 2.6));
        let spiral = model_path(&d, "Clothoid");
        assert!((spiral.last().unwrap().y - 2.217816946252).abs() < 1e-8);
        // Midpoint independently evaluated from the Fresnel power series at s=20, L=40, R=120.
        let near = Vec2::new(19.996528056848, 0.277743333032);
        assert!(spiral.windows(2).any(|edge| { cadcraft_geom::Line::new(edge[0].xy(), edge[1].xy()).closest(near).near(near, 0.01) }));
    }

    #[test]
    fn a_circular_profile_accepts_length_along_the_arc() {
        let d = read(
            br#"<LandXML><Alignments><Alignment name="Crest" staStart="1000"><CoordGeom>
          <Line><Start>0 0</Start><End>0 200</End></Line>
        </CoordGeom><Profile><ProfAlign name="Grade"><PVI>1000 10</PVI>
          <CircCurve length="199.33730498232408" radius="-1000">1100 20</CircCurve>
          <PVI>1200 10</PVI></ProfAlign></Profile></Alignment></Alignments></LandXML>"#,
        )
        .unwrap();
        assert!(d.read_notes.is_empty(), "{:?}", d.read_notes);
        assert!(d.layout("Profile - Crest - Grade").is_some());
        let peak = model_path(&d, "Crest").iter().max_by(|a, b| a.z.total_cmp(&b.z)).unwrap();
        assert!((peak.x - 100.0).abs() < 0.01 && (peak.z - 15.0124378879).abs() < 0.01);
    }

    #[test]
    fn an_unsupported_horizontal_element_has_a_note_and_no_invented_centerline() {
        let d = read(
            br#"<LandXML><Alignments><Alignment name="Main"><CoordGeom>
          <Spiral length="30" spiType="cubic"><Start>0 0 0</Start><End>0 30 0</End></Spiral>
        </CoordGeom><Profile><ProfAlign name="Grade"><PVI>0 1</PVI><PVI>30 2</PVI></ProfAlign></Profile></Alignment></Alignments></LandXML>"#,
        )
        .unwrap();
        assert!(d.model.is_empty());
        assert!(d.layout("Profile - Main - Grade").is_some());
        assert!(d.read_notes.iter().any(|note| note.contains("unsupported spiral")));
    }
}
