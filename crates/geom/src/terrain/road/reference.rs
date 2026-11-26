//! Sample 3D reference lines in a road section and derive pipe points from a diameter.
use serde::{Deserialize, Serialize};

use crate::{Vec2, Vec3};

use super::{EdgeTree, append_endpoint_stations, envelope, indexed};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PipeLevel {
    #[default]
    Invert,
    Crown,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Pipe {
    pub diameter: f64,
    pub level: PipeLevel,
}

impl Pipe {
    pub fn valid(self) -> bool {
        self.diameter.is_finite() && self.diameter > 0.0 && self.diameter <= 1e12
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ReferencePoint {
    #[default]
    Line,
    LeftEdge,
    RightEdge,
    Crown,
    Invert,
}

impl ReferencePoint {
    pub const PIPE_POINTS: [Self; 4] = [Self::LeftEdge, Self::RightEdge, Self::Crown, Self::Invert];
}

pub struct ReferenceInput<'a> {
    pub number: u32,
    pub points: &'a [Vec3],
    pub pipe: Option<Pipe>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SectionReference {
    pub number: u32,
    /// Section offset and elevation at the original line.
    pub line: Vec2,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipe: Option<Pipe>,
}

impl SectionReference {
    pub fn circle(self) -> Option<(Vec2, f64)> {
        let pipe = self.pipe?;
        let radius = pipe.diameter * 0.5;
        let height = match pipe.level {
            PipeLevel::Invert => radius,
            PipeLevel::Crown => -radius,
        };
        Some((self.line + Vec2::new(0.0, height), radius))
    }

    pub fn point(self, part: ReferencePoint) -> Option<Vec2> {
        if part == ReferencePoint::Line {
            return Some(self.line);
        }
        let (center, radius) = self.circle()?;
        let offset = match part {
            ReferencePoint::LeftEdge => -Vec2::X,
            ReferencePoint::RightEdge => Vec2::X,
            ReferencePoint::Crown => Vec2::Y,
            ReferencePoint::Invert => -Vec2::Y,
            ReferencePoint::Line => Vec2::ZERO,
        };
        Some(center + offset * radius)
    }
}

pub(super) struct ReferenceQuery<'a> {
    pub number: u32,
    pub pipe: Option<Pipe>,
    pub used: bool,
    points: &'a [Vec3],
    segments: Vec<[Vec3; 2]>,
    index: EdgeTree,
}

impl<'a> ReferenceQuery<'a> {
    pub(super) fn new(input: ReferenceInput<'a>, used: bool) -> Result<Self, String> {
        let ReferenceInput { number, points, pipe } = input;
        if pipe.is_some_and(|pipe| !pipe.valid()) {
            return Err(format!("Reference {number} needs a positive pipe diameter within the drawing range."));
        }
        if points.iter().any(|point| !point.is_finite() || point.x.abs().max(point.y.abs()).max(point.z.abs()) > 1e12) {
            return Err(format!("Reference {number} has invalid coordinates."));
        }
        let segments: Vec<_> = points.windows(2).filter(|pair| pair[0].xy() != pair[1].xy()).map(|pair| [pair[0], pair[1]]).collect();
        if segments.is_empty() {
            return Err(format!("Reference {number} needs a line with a horizontal extent."));
        }
        let index = indexed(segments.iter().map(|segment| segment.map(Vec3::xy)));
        Ok(Self { number, pipe, used, points, segments, index })
    }

    pub(super) fn at(&self, center: Vec2, normal: Vec2) -> Option<SectionReference> {
        if normal == Vec2::ZERO {
            return None;
        }
        let bounds = self.index.root().envelope();
        let [min_x, min_y] = bounds.lower();
        let [max_x, max_y] = bounds.upper();
        let [near, far] = [Vec2::new(min_x, min_y), Vec2::new(min_x, max_y), Vec2::new(max_x, min_y), Vec2::new(max_x, max_y)]
            .into_iter()
            .map(|corner| (corner - center).dot(normal))
            .fold([f64::INFINITY, f64::NEG_INFINITY], |[near, far], offset| [near.min(offset), far.max(offset)]);
        let search = envelope([center + normal * near, center + normal * far]);
        let line = self
            .index
            .locate_in_envelope_intersecting(search)
            .filter_map(|entry| crossing(self.segments[entry.data], center, normal).map(|point| (point, entry.data)))
            .min_by(|(left, i), (right, j)| left.x.abs().total_cmp(&right.x.abs()).then(i.cmp(j)))?
            .0;
        Some(SectionReference { number: self.number, line, pipe: self.pipe })
    }

    pub(super) fn add_stations(&self, axis: &[(f64, Vec3)], normals: &[Vec2], index: &EdgeTree, stations: &mut Vec<f64>, limits: [f64; 2]) {
        append_endpoint_stations(self.points.iter().map(|point| [point.x, point.y]), axis, normals, index, stations, limits);
    }
}

fn crossing([start, end]: [Vec3; 2], center: Vec2, normal: Vec2) -> Option<Vec2> {
    let from = start.xy() - center;
    let advance = end.xy() - start.xy();
    let distance = from.cross(normal);
    let change = advance.cross(normal);
    let tolerance = 1e-8 + 16.0 * f64::EPSILON * center.x.abs().max(center.y.abs());
    let fraction = if change.abs() <= tolerance {
        if distance.abs() > tolerance || (distance + change).abs() > tolerance {
            return None;
        }
        -from.dot(advance) / advance.len2()
    } else {
        let fraction = -distance / change;
        let tolerance = tolerance / change.abs();
        if !(-tolerance..=1.0 + tolerance).contains(&fraction) {
            return None;
        }
        fraction
    }
    .clamp(0.0, 1.0);
    let offset = -(from + advance * fraction).dot(normal);
    Some(Vec2::new(offset, start.z + (end.z - start.z) * fraction))
}
