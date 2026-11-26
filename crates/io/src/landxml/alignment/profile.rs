//! Vertical geometry in LandXML station/elevation coordinates, and its CAD profile sheet.

use std::collections::HashSet;

use cadcraft_doc::{Common, Drawing, EntityKind, Layout, Line, Polyline3d, Space, Text};
use cadcraft_geom::{Vec2, Vec3};

use super::super::{error, group_name};

#[derive(Clone, Copy)]
pub(super) enum Curve {
    Tangent,
    Parabola(f64),
    Circle { radius: f64, length: Option<f64> },
}

#[derive(Clone, Copy)]
pub(super) struct Pvi {
    pub station: f64,
    pub z: f64,
    pub curve: Curve,
}

#[derive(Clone, Copy)]
enum Elevation {
    Grade { start: f64, grade: f64 },
    Parabola { start: f64, grade: f64, curvature: f64 },
    Circle { x: f64, z: f64, radius: f64, sign: f64 },
}

struct Segment {
    start: f64,
    end: f64,
    elevation: Elevation,
}

impl Segment {
    fn at(&self, station: f64) -> f64 {
        let s = station - self.start;
        match self.elevation {
            Elevation::Grade { start, grade } => start + s * grade,
            Elevation::Parabola { start, grade, curvature } => start + s * (grade + s * curvature * 0.5),
            Elevation::Circle { x, z, radius, sign } => {
                let dx = (station - x).abs().min(radius);
                z - sign * ((radius - dx) * (radius + dx)).sqrt()
            }
        }
    }
}

pub(super) struct Profile {
    segments: Vec<Segment>,
}

impl Profile {
    pub(super) fn new(pvis: &[Pvi], tolerance: f64) -> Result<Self, &'static str> {
        let Some((first, last)) = pvis.first().zip(pvis.last()) else { return Err("empty profile") };
        if pvis.len() < 2
            || !matches!(first.curve, Curve::Tangent)
            || !matches!(last.curve, Curve::Tangent)
            || pvis.windows(2).any(|pair| pair[0].station >= pair[1].station)
        {
            return Err("profile requires increasing stations and plain PVIs at both ends");
        }

        let mut segments = Vec::new();
        let mut tail = (first.station, first.z);
        for pvi in pvis.windows(3) {
            let &[before, at, after] = pvi else { continue };
            let g0 = (at.z - before.z) / (at.station - before.station);
            let g1 = (after.z - at.z) / (after.station - at.station);
            let curve = match at.curve {
                Curve::Tangent => None,
                Curve::Parabola(length) => {
                    if length <= 0.0 {
                        return Err("parabolic vertical curve has a non-positive length");
                    }
                    let start = at.station - length * 0.5;
                    let z = at.z - g0 * length * 0.5;
                    Some(Segment {
                        start,
                        end: at.station + length * 0.5,
                        elevation: Elevation::Parabola { start: z, grade: g0, curvature: (g1 - g0) / length },
                    })
                }
                Curve::Circle { radius, length } => {
                    let (a, b) = (g0.atan(), g1.atan());
                    let turn = b - a;
                    let radius = radius.abs();
                    let tangent = radius * (turn * 0.5).tan().abs();
                    let start = at.station - tangent * a.cos();
                    let end = at.station + tangent * b.cos();
                    // Inframodel specifies the horizontal span. Some files give the length along the arc.
                    let length_matches = |reported: f64| {
                        let error = ((end - start) - reported).abs().min((radius * turn.abs() - reported).abs());
                        reported > 0.0 && error <= tolerance.max(reported.abs() * 1e-5)
                    };
                    if radius == 0.0 || end <= start || length.is_some_and(|l| !length_matches(l)) {
                        return Err("circular vertical curve radius, tangents and length disagree");
                    }
                    let sign = turn.signum();
                    let z = at.z - (at.station - start) * g0;
                    Some(Segment {
                        start,
                        end,
                        elevation: Elevation::Circle { x: start - sign * radius * a.sin(), z: z + sign * radius * a.cos(), radius, sign },
                    })
                }
            };
            let (start, z) = curve.as_ref().map_or((at.station, at.z), |curve| (curve.start, at.z - (at.station - curve.start) * g0));
            if start < tail.0 || curve.as_ref().is_some_and(|c| c.end > after.station) {
                return Err("vertical curves overlap or extend past the neighboring PVIs");
            }
            add_grade(&mut segments, tail, (start, z))?;
            tail = (start, z);
            if let Some(curve) = curve {
                tail = (curve.end, curve.at(curve.end));
                segments.push(curve);
            }
        }
        add_grade(&mut segments, tail, (last.station, last.z))?;
        if segments.is_empty() || segments.iter().any(|s| !valid(s.at(s.start)) || !valid(s.at(s.end))) {
            return Err("vertical geometry is outside the supported coordinate range");
        }
        Ok(Self { segments })
    }

    pub(super) fn covers(&self, from: f64, to: f64, tolerance: f64) -> bool {
        self.segments.first().zip(self.segments.last()).is_some_and(|(a, b)| a.start <= from + tolerance && b.end >= to - tolerance)
    }

    pub(super) fn at(&self, station: f64) -> Option<f64> {
        let index = self.segments.partition_point(|segment| segment.end < station);
        let segment = self.segments.get(index).or_else(|| self.segments.last())?;
        Some(segment.at(station.clamp(segment.start, segment.end)))
    }

    /// Sample curved vertical segments by midpoint error in drawing units. Include every grade break.
    pub(super) fn samples(&self, tolerance: f64, budget: usize) -> Result<Vec<Vec2>, &'static str> {
        let mut out = Vec::new();
        for segment in &self.segments {
            let a = Vec2::new(segment.start, segment.at(segment.start));
            let b = Vec2::new(segment.end, segment.at(segment.end));
            if out.is_empty() {
                out.push(a);
            }
            let mut pending = vec![[a, b]];
            while let Some([a, b]) = pending.pop() {
                if out.len() + pending.len() >= budget {
                    return Err("vertical geometry exceeds the sample limit");
                }
                let x = (a.x + b.x) * 0.5;
                let z = segment.at(x);
                if !valid(z) {
                    return Err("vertical geometry is outside the supported coordinate range");
                }
                if x != a.x && x != b.x && (z - (a.y + b.y) * 0.5).abs() > tolerance {
                    let mid = Vec2::new(x, z);
                    pending.extend([[mid, b], [a, mid]]);
                } else {
                    out.push(b);
                }
            }
        }
        Ok(out)
    }
}

fn add_grade(segments: &mut Vec<Segment>, a: (f64, f64), b: (f64, f64)) -> Result<(), &'static str> {
    if b.0 > a.0 {
        segments.push(Segment { start: a.0, end: b.0, elevation: Elevation::Grade { start: a.1, grade: (b.1 - a.1) / (b.0 - a.0) } });
    } else if (b.1 - a.1).abs() > 1e-7 {
        return Err("vertical geometry has an elevation gap");
    }
    Ok(())
}

fn valid(value: f64) -> bool {
    value.is_finite() && value.abs() <= crate::MAX_WINDOW_COORD
}

pub(super) fn add_layout(drawing: &mut Drawing, title: &str, points: &[Vec2], units: &str) -> crate::Result<()> {
    let (Some(first), Some(last)) = (points.first(), points.last()) else { return Ok(()) };
    let min = points.iter().map(|p| p.y).fold(f64::INFINITY, f64::min);
    let max = points.iter().map(|p| p.y).fold(f64::NEG_INFINITY, f64::max);
    let padding = ((max - min) * 0.08).max(units_value(drawing, 0.02));
    let low = min - padding;
    let span = max - min + 2.0 * padding;
    // Paper coordinates are millimetres. The axes are independently fitted and labelled with source units.
    let paper = cadcraft_render::paper_size("A4").ok_or_else(|| error("missing A4 page size"))?;
    let mut names: HashSet<_> = drawing.layouts.iter().map(|layout| layout.name.to_ascii_lowercase()).collect();
    let name = group_name(&format!("Profile - {title}"), "Profile", &mut names);
    let order = drawing.layouts.iter().map(|l| l.tab_order).max().unwrap_or(0).saturating_add(1);
    let mut layout = Layout::new(&name, order);
    layout.page.paper = paper.name.into();
    layout.page.width_mm = paper.width_mm;
    layout.page.height_mm = paper.height_mm;
    layout.page.paper_mm = Some(true);
    layout.view = Some((Vec2::new(148.5, 105.0), 220.0));
    drawing.layouts.push(layout);

    let space = Space::Paper(name);
    let mut add = |kind| drawing.add(&space, Common::default(), kind).map(|_| ()).map_err(error);
    let line = |x, y, a, b| EntityKind::Line(Line { a: Vec3::new(x, y, 0.0), b: Vec3::new(a, b, 0.0) });
    let text = |x, y, height, value| {
        EntityKind::Text(Text {
            insert: Vec3::new(x, y, 0.0),
            align_pt: None,
            height,
            value,
            rotation: 0.0,
            width_factor: 1.0,
            oblique: 0.0,
            style: "Standard".into(),
            halign: Default::default(),
            valign: Default::default(),
        })
    };
    let (left, bottom, width, height) = (42.0, 32.0, 225.0, 133.0);
    let title: String = title.chars().filter(|ch| !ch.is_control()).take(70).collect();
    add(text(left, 184.0, 4.2, title))?;
    add(text(left, 172.0, 3.0, axis("Elevation", units)))?;
    add(text(left, 13.0, 3.0, axis("Station", units)))?;
    add(line(left, bottom, left + width, bottom))?;
    add(line(left, bottom, left, bottom + height))?;
    for i in 0..=5 {
        let t = f64::from(i) / 5.0;
        let x = left + t * width;
        let y = bottom + t * height;
        add(line(x, bottom - 2.0, x, bottom))?;
        add(text(x - 4.0, bottom - 7.0, 2.6, format!("{:.3}", first.x + t * (last.x - first.x))))?;
        add(line(left - 2.0, y, left, y))?;
        add(text(5.0, y - 0.8, 2.6, format!("{:.3}", low + t * span)))?;
    }

    let trace =
        points.iter().map(|p| Vec3::new(left + width * (p.x - first.x) / (last.x - first.x), bottom + height * (p.y - low) / span, 0.0)).collect();
    add(EntityKind::Polyline3d(Polyline3d { points: trace, closed: false }))?;
    Ok(())
}

fn axis(label: &str, units: &str) -> String {
    if units.is_empty() { label.into() } else { format!("{label} ({units})") }
}

/// Convert metres to drawing units for geometric sampling and compatibility tolerances.
pub(super) fn units_value(drawing: &Drawing, metres: f64) -> f64 {
    super::super::metres_per_unit(super::linear_unit(drawing)).map_or(metres, |unit| metres / unit)
}
