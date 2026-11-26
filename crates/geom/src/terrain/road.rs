//! Station-based road sections and connected terrain strips.
use rstar::{
    AABB, RTree,
    primitives::{GeomWithData, Line as IndexedLine, Rectangle},
};
use serde::{Deserialize, Serialize};

use crate::{Vec2, Vec3, line_line_infinite};

use super::{Triangle, signed_area2, triangle_earthwork};

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Crossfall {
    pub station: f64,
    pub left: f64,
    pub right: f64,
}

pub enum Width<'a> {
    Fixed(f64),
    FromEdges(&'a [[Vec2; 2]]),
}

type EdgeTree = RTree<GeomWithData<IndexedLine<[f64; 2]>, usize>>;

enum WidthQuery {
    Fixed(f64),
    Edges(EdgeTree),
}

impl WidthQuery {
    fn new(source: Width<'_>) -> Result<Self, String> {
        match source {
            Width::Fixed(width) if width.is_finite() && width > 0.0 => Ok(Self::Fixed(width)),
            Width::Fixed(_) => Err("Width must be positive.".into()),
            Width::FromEdges(edges) if !edges.is_empty() => Ok(Self::Edges(indexed(edges.iter().copied()))),
            Width::FromEdges(_) => Err("A width from edges requires at least one line.".into()),
        }
    }

    fn at(&self, xy: Vec2, direction: Vec2) -> Option<f64> {
        let Self::Edges(tree) = self else { return if let Self::Fixed(width) = self { Some(*width) } else { None } };
        let bounds = tree.root().envelope();
        let [x0, y0] = bounds.lower();
        let [x1, y1] = bounds.upper();
        let reach = [Vec2::new(x0, y0), Vec2::new(x0, y1), Vec2::new(x1, y0), Vec2::new(x1, y1)]
            .into_iter()
            .map(|p| (p - xy).dot(direction))
            .fold(0.0, f64::max);
        let limit = xy + direction * reach;
        let query = envelope([xy, limit]);
        tree.locate_in_envelope_intersecting(query)
            .filter_map(|line| {
                let line = line.geom();
                let point = |p: [f64; 2]| Vec2::new(p[0], p[1]);
                let (p, q) = (point(line.from), point(line.to));
                let (_, along, edge) = line_line_infinite(xy, xy + direction, p, q)?;
                (along > 1e-8 && (-1e-8..=1.0 + 1e-8).contains(&edge)).then_some(along)
            })
            .min_by(f64::total_cmp)
    }

    fn add_stations(&self, axis: &[(f64, Vec3)], normals: &[Vec2], index: &EdgeTree, stations: &mut Vec<f64>, limits: [f64; 2]) {
        if let Self::Edges(tree) = self {
            let endpoints = tree.iter().flat_map(|entry| [entry.geom().from, entry.geom().to]);
            append_endpoint_stations(endpoints, axis, normals, index, stations, limits);
        }
    }
}

fn append_endpoint_stations(
    endpoints: impl IntoIterator<Item = [f64; 2]>,
    axis: &[(f64, Vec3)],
    normals: &[Vec2],
    index: &EdgeTree,
    stations: &mut Vec<f64>,
    [from, to]: [f64; 2],
) {
    for end in endpoints {
        if let Some(nearest) = index.nearest_neighbor(end)
            && let Some(at) = endpoint_station(axis, normals, nearest.data, Vec2::new(end[0], end[1]))
            && at > from
            && at < to
        {
            stations.push(at);
        }
    }
}

fn endpoint_station(axis: &[(f64, Vec3)], normals: &[Vec2], nearest: usize, endpoint: Vec2) -> Option<f64> {
    // On a tessellated curve the closest chord and the intersecting section can
    // sit on opposite sides of a sampled vertex.
    (nearest.saturating_sub(1)..=(nearest + 1).min(axis.len() - 2))
        .flat_map(|i| {
            let (start, end) = (axis[i], axis[i + 1]);
            let travel = end.1.xy() - start.1.xy();
            let normal = normals[i];
            let change = normals[i + 1] - normal;
            let relative = endpoint - start.1.xy();
            // The endpoint is on the section when (relative - t * travel)
            // is parallel to (normal + t * change).
            roots(relative.cross(normal), relative.cross(change) - travel.cross(normal), -travel.cross(change))
                .into_iter()
                .flatten()
                .filter(|t| (-1e-9..=1.0 + 1e-9).contains(t))
                .map(move |t| {
                    let t = t.clamp(0.0, 1.0);
                    ((relative - travel * t).len2(), start.0 + t * (end.0 - start.0))
                })
        })
        .min_by(|(left, _), (right, _)| left.total_cmp(right))
        .map(|(_, station)| station)
}

fn roots(constant: f64, linear: f64, quadratic: f64) -> [Option<f64>; 2] {
    if quadratic.abs() <= 1e-14 * linear.abs().max(constant.abs()) {
        return [(linear != 0.0).then(|| -constant / linear), None];
    }
    let discriminant = linear.mul_add(linear, -4.0 * quadratic * constant);
    if discriminant < 0.0 {
        return [None; 2];
    }
    let stable = -0.5 * (linear + discriminant.sqrt().copysign(linear));
    if stable == 0.0 { [Some(-linear / (2.0 * quadratic)), None] } else { [Some(stable / quadratic), Some(constant / stable)] }
}

fn indexed(lines: impl Iterator<Item = [Vec2; 2]>) -> EdgeTree {
    RTree::bulk_load(lines.enumerate().map(|(i, [a, b])| GeomWithData::new(IndexedLine::new([a.x, a.y], [b.x, b.y]), i)).collect())
}

fn envelope([a, b]: [Vec2; 2]) -> AABB<[f64; 2]> {
    let margin = 1e-8;
    AABB::from_corners([a.x.min(b.x) - margin, a.y.min(b.y) - margin], [a.x.max(b.x) + margin, a.y.max(b.y) + margin])
}

pub struct RoadSections<'a> {
    pub axis: &'a [(f64, Vec3)],
    pub limits: [f64; 2],
    pub interval: f64,
    pub left: Width<'a>,
    pub right: Width<'a>,
    pub crossfall: &'a [Crossfall],
}

mod reference;
mod template;
pub use reference::{Pipe, PipeLevel, ReferenceInput, ReferencePoint, SectionReference};
pub use template::{
    Extension, ExtensionDirection, ExtensionSide, ExtensionTarget, Mesh, Rule, Sampler, Section, SectionLayer, SectionNode, Template,
    section_extension_hit, section_grade_hit, section_layer_grade_hit,
};

/// Intersect width controls with each normal section. The crest pivots on the centerline.
pub fn road_triangles(input: RoadSections<'_>, max_sections: usize) -> Result<Vec<Triangle>, String> {
    let sampler = Sampler::new(input, [Template { name: "Top", thickness: 0.0, inherit: true, rules: Vec::new() }], 0, false, [])?;
    let mut models = sampler.mesh(max_sections, max_sections.saturating_mul(4))?;
    Ok(models.pop().map_or_else(Vec::new, |model| model.triangles))
}

fn vertex_normal(axis: &[(f64, Vec3)], index: usize, sampled_alignment: bool) -> Vec2 {
    let center = axis[index].1.xy();
    let before = index.checked_sub(1).map(|i| center - axis[i].1.xy());
    let after = axis.get(index + 1).map(|p| p.1.xy() - center);
    let tangent = match (before, after) {
        // The chord directions estimate the tangents at their midpoints. Interpolate
        // those directions to the vertex, including when a profile adds a close sample.
        (Some(a), Some(b)) if sampled_alignment => {
            let weight = a.len() / (a.len() + b.len());
            a.normalized().lerp(b.normalized(), weight)
        }
        (Some(a), Some(b)) => a.normalized() + b.normalized(),
        (Some(a), None) | (None, Some(a)) => a,
        _ => Vec2::ZERO,
    };
    tangent.normalized().perp()
}

fn section(axis: &[(f64, Vec3)], normals: &[Vec2], station: f64) -> (Vec3, Vec2) {
    let index = axis.partition_point(|(s, _)| *s < station).min(axis.len() - 1);
    if station == axis[index].0 {
        (axis[index].1, normals[index])
    } else {
        let (a, b) = (axis[index - 1], axis[index]);
        let t = (station - a.0) / (b.0 - a.0);
        let normal = normals[index - 1].lerp(normals[index], t).normalized();
        (a.1 + (b.1 - a.1) * t, normal)
    }
}

fn slope(values: &[Crossfall], station: f64) -> Vec2 {
    let i = values.partition_point(|value| value.station < station);
    let xy = |value: Crossfall| Vec2::new(value.left, value.right);
    if i == 0 {
        xy(values[0])
    } else if i == values.len() {
        xy(values[i - 1])
    } else {
        let (a, b) = (values[i - 1], values[i]);
        xy(a).lerp(xy(b), (station - a.station) / (b.station - a.station))
    }
}

fn check_overlap(triangles: &[Triangle]) -> Result<(), String> {
    let bounds = |face: Triangle| {
        let [a, b, c] = face;
        envelope([Vec2::new(a.x.min(b.x).min(c.x), a.y.min(b.y).min(c.y)), Vec2::new(a.x.max(b.x).max(c.x), a.y.max(b.y).max(c.y))])
    };
    let entries = triangles.iter().copied().enumerate().map(|(i, tri)| GeomWithData::new(Rectangle::from_aabb(bounds(tri)), i)).collect();
    let tree = RTree::bulk_load(entries);
    for (i, &tri) in triangles.iter().enumerate() {
        for entry in tree.locate_in_envelope_intersecting(bounds(tri)).filter(|entry| entry.data > i) {
            let other = triangles[entry.data];
            if triangle_earthwork(tri, other).area > 1e-8_f64.max(signed_area2(tri).min(signed_area2(other)) * 1e-9) {
                return Err("Road strips overlap in plan. Change the boundary lines, width or station range.".into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_alignment_samples_do_not_fold_an_inside_shoulder() {
        let radius = 30.0;
        let axis = [0.0, 0.05, 0.095, 0.10, 0.15, 0.20].map(|angle| (radius * angle, Vec2::polar(Vec2::ZERO, radius, angle).to3(0.0)));
        let grades = [Crossfall { station: 0.0, left: 0.0, right: 0.0 }];
        let build = |inside_width| {
            Sampler::new(
                RoadSections {
                    axis: &axis,
                    limits: [0.0, axis[axis.len() - 1].0],
                    interval: 0.5,
                    left: Width::Fixed(inside_width),
                    right: Width::Fixed(3.0),
                    crossfall: &grades,
                },
                [Template { name: "Top", thickness: 0.0, inherit: true, rules: Vec::new() }],
                1,
                true,
                [],
            )
            .and_then(|sampler| sampler.mesh(100, 400))
        };
        let mesh = build(7.5).unwrap();
        assert!(mesh[0].triangles.iter().any(|triangle| signed_area2(*triangle) > 0.0));
        assert!(matches!(build(radius + 1.0), Err(error) if error.contains("fold")));
    }

    #[test]
    fn ends_edge_driven_faces_at_the_edge_endpoints_on_a_curve() {
        let radius = 50.0;
        let axis = [0.0, 0.05, 0.10, 0.18, 0.22, 0.26].map(|angle| (radius * angle, Vec2::polar(Vec2::ZERO, radius, angle).to3(10.0)));
        let normals: Vec<_> = (0..axis.len()).map(|index| vertex_normal(&axis, index, true)).collect();
        let edges = [[1.41, 9.17].map(|at| {
            let (point, normal) = section(&axis, &normals, at);
            point.xy() + normal * 5.0
        })];
        let mesh = Sampler::new(
            RoadSections { axis: &axis, limits: [0.0, 13.0], interval: 1.0, left: Width::Fixed(3.0), right: Width::Fixed(3.0), crossfall: &[] },
            [Template {
                name: "Top",
                thickness: 0.0,
                inherit: false,
                rules: vec![Rule {
                    number: 2,
                    from: 1,
                    from_layer: None,
                    from_reference: None,
                    stations: None,
                    offset: 0.0,
                    height: 0.0,
                    crossfall: [0.0; 2],
                    edges: Some(&edges),
                    extension: None,
                    helper: false,
                }],
            }],
            1,
            true,
            [],
        )
        .unwrap()
        .mesh(100, 400)
        .unwrap();
        let line = &mesh[0].lines[&2];
        assert_eq!(line.len(), 1);
        assert!(line[0][0].xy().near(edges[0][0], 1e-7));
        assert!(line[0].last().unwrap().xy().near(edges[0][1], 1e-7));
    }
}
