//! Triangulated terrain in which each XY location has one elevation.
use std::collections::{BTreeMap, BTreeSet};

use spade::{ConstrainedDelaunayTriangulation, HasPosition, Point2, Triangulation};

use crate::{Vec2, Vec3, point_in_polygon};

mod edit;
pub use edit::{MAX_BOUNDARY_POINTS, clip_surface, signed_area2};

pub type Triangle = [Vec3; 3];
type Key = [u64; 3];

#[derive(Clone)]
struct Sample(Vec3);

impl HasPosition for Sample {
    type Scalar = f64;

    fn position(&self) -> Point2<f64> {
        Point2::new(self.0.x, self.0.y)
    }
}

fn key(p: Vec3) -> Key {
    [p.x, p.y, p.z].map(|v| if v == 0.0 { 0 } else { v.to_bits() })
}

#[derive(Default)]
struct Inputs {
    vertices: Vec<Sample>,
    xy: BTreeMap<[u64; 2], usize>,
    edges: BTreeSet<[usize; 2]>,
}

impl Inputs {
    fn vertex(&mut self, point: Vec3) -> Result<usize, &'static str> {
        if !point.is_finite() || point.x.abs().max(point.y.abs()).max(point.z.abs()) > 1e12 {
            return Err("Surface coordinates are outside the supported range.");
        }
        let [x, y, _] = key(point);
        let i = *self.xy.entry([x, y]).or_insert(self.vertices.len());
        if let Some(previous) = self.vertices.get(i) {
            if previous.0.z != point.z {
                return Err("Two surface points at the same XY have different elevations.");
            }
        } else {
            self.vertices.push(Sample(point));
        }
        Ok(i)
    }

    fn path(&mut self, points: &[Vec3], closed: bool) -> Result<(), &'static str> {
        let mut indices = points.iter().map(|&p| self.vertex(p)).collect::<Result<Vec<_>, _>>()?;
        indices.dedup();
        if closed && indices.first() == indices.last() {
            indices.pop();
        }
        if indices.len() < if closed { 3 } else { 2 } {
            return Err("A breakline needs two distinct points and a closed boundary needs three.");
        }
        if closed {
            if indices.iter().collect::<BTreeSet<_>>().len() != indices.len() {
                return Err("A boundary may not visit a vertex more than once.");
            }
            indices.push(indices[0]);
        }
        self.edges.extend(indices.windows(2).map(|pair| {
            let mut edge = [pair[0], pair[1]];
            edge.sort_unstable();
            edge
        }));
        Ok(())
    }
}

/// Build a constrained Delaunay terrain from XYZ samples. Closed boundaries use their supplied
/// heights. A crossing constraint must already share a vertex with the intersected constraint.
pub fn triangulate(points: &[Vec3], breaklines: &[Vec<Vec3>], outer: Option<&[Vec3]>, holes: &[Vec<Vec3>]) -> Result<Vec<Triangle>, &'static str> {
    let mut input = Inputs::default();
    for &point in points {
        input.vertex(point)?;
    }
    for line in breaklines {
        input.path(line, false)?;
    }
    for boundary in outer.into_iter().chain(holes.iter().map(Vec::as_slice)) {
        input.path(boundary, true)?;
    }
    let mut crossing = false;
    let mesh = ConstrainedDelaunayTriangulation::<Sample>::try_bulk_load_cdt(input.vertices, input.edges.into_iter().collect(), |_| crossing = true)
        .map_err(|_| "Could not triangulate the supplied surface coordinates.")?;
    if crossing {
        return Err("Split crossing breaklines or boundaries at their common XYZ vertex.");
    }
    let xy = |ring: &[Vec3]| ring.iter().map(|p| p.xy()).collect::<Vec<Vec2>>();
    let outer = outer.map(xy);
    let holes: Vec<_> = holes.iter().map(|ring| xy(ring)).collect();
    let faces = mesh
        .inner_faces()
        .map(|face| face.vertices().map(|v| v.data().0))
        .filter(|[a, b, c]| {
            let center = a.xy() + ((b.xy() - a.xy()) + (c.xy() - a.xy())) * (1.0 / 3.0);
            outer.as_ref().is_none_or(|ring| point_in_polygon(ring, center)) && !holes.iter().any(|ring| point_in_polygon(ring, center))
        })
        .collect::<Vec<_>>();
    if faces.is_empty() {
        return Err("The surface needs at least three non-collinear XY points inside its boundaries.");
    }
    Ok(faces)
}

fn section(triangle: Triangle, z: f64) -> Option<[Vec3; 2]> {
    let min = triangle.iter().fold(f64::INFINITY, |v, p| v.min(p.z));
    let max = triangle.iter().fold(f64::NEG_INFINITY, |v, p| v.max(p.z));
    if z < min || z > max || min == max {
        return None;
    }
    let mut hits = Vec::with_capacity(2);
    for [a, b] in [[triangle[0], triangle[1]], [triangle[1], triangle[2]], [triangle[2], triangle[0]]] {
        if a.z == z {
            hits.push(a);
        }
        let (low, high) = if a.z < b.z { (a, b) } else { (b, a) };
        if low.z < z && high.z > z {
            hits.push((low.xy() + (high.xy() - low.xy()) * ((z - low.z) / (high.z - low.z))).to3(z));
        }
    }
    match hits.as_slice() {
        [a, b] if a != b => Some([*a, *b]),
        _ => None,
    }
}

/// Straight contour segments at multiples of `interval`. Shared coincident edges are emitted once.
pub fn contours(triangles: &[Triangle], interval: f64, limit: usize) -> Result<Vec<[Vec3; 2]>, &'static str> {
    if !interval.is_finite() || interval <= 0.0 {
        return Err("Contour interval must be a finite positive number.");
    }
    let mut segments = BTreeMap::new();
    let mut steps = 0usize;
    for &triangle in triangles {
        let lo = (triangle.iter().fold(f64::INFINITY, |v, p| v.min(p.z)) / interval).ceil();
        let hi = (triangle.iter().fold(f64::NEG_INFINITY, |v, p| v.max(p.z)) / interval).floor();
        if lo.abs().max(hi.abs()) >= (1_u64 << 52) as f64 || hi - lo > limit as f64 {
            return Err("Contour interval is too small for the elevation range.");
        }
        for level in lo as i64..=hi as i64 {
            steps += 1;
            if steps > limit.saturating_mul(4) {
                return Err("Too many contour intersections. Increase the interval or select fewer triangles.");
            }
            if let Some(segment) = section(triangle, level as f64 * interval) {
                let mut keys = segment.map(key);
                keys.sort_unstable();
                segments.entry(keys).or_insert(segment);
                if segments.len() > limit {
                    return Err("Too many contour segments. Increase the interval or select fewer triangles.");
                }
            }
        }
    }
    Ok(segments.into_values().collect())
}

/// Cut and fill in cubic drawing units, plus the XY area common to both surfaces.
#[derive(Debug, Default)]
pub struct Earthwork {
    pub cut: f64,
    pub fill: f64,
    pub area: f64,
}

impl std::ops::AddAssign for Earthwork {
    fn add_assign(&mut self, other: Self) {
        self.cut += other.cut;
        self.fill += other.fill;
        self.area += other.area;
    }
}

fn clip<P: Copy>(polygon: &[P], field: impl Fn(P) -> f64, interpolate: impl Fn(P, P, f64) -> P) -> Vec<P> {
    let Some(mut previous) = polygon.last().copied() else { return Vec::new() };
    let mut prev_distance = field(previous);
    let mut result = Vec::with_capacity(polygon.len() + 1);
    for &current in polygon {
        let distance = field(current);
        if (distance < 0.0) != (prev_distance < 0.0) {
            result.push(interpolate(previous, current, prev_distance / (prev_distance - distance)));
        }
        if distance >= 0.0 {
            result.push(current);
        }
        previous = current;
        prev_distance = distance;
    }
    result
}

fn xy_area(a: Vec2, b: Vec2, c: Vec2) -> f64 {
    (b - a).cross(c - a).abs() * 0.5
}

fn plane(triangle: Triangle, p: Vec2) -> f64 {
    let [a, b, c] = triangle;
    let u = (b - a).xy();
    let v = (c - a).xy();
    let q = p - a.xy();
    a.z + (q.cross(v) * (b.z - a.z) + u.cross(q) * (c.z - a.z)) / u.cross(v)
}

fn integral(polygon: &[Vec3]) -> f64 {
    let Some(a) = polygon.first() else { return 0.0 };
    polygon[1..].windows(2).map(|pair| xy_area(a.xy(), pair[0].xy(), pair[1].xy()) * ((a.z + pair[0].z + pair[1].z) / 3.0)).sum()
}

/// Compare a pair of piecewise-linear surface triangles over their common XY footprint.
pub fn triangle_earthwork(existing: Triangle, design: Triangle) -> Earthwork {
    let [a, b, c] = design.map(Vec3::xy);
    let area = (b - a).cross(c - a);
    if area == 0.0 || xy_area(existing[0].xy(), existing[1].xy(), existing[2].xy()) == 0.0 {
        return Earthwork::default();
    }
    let mut overlap = existing.map(Vec3::xy).to_vec();
    for (p, q) in [(a, b), (b, c), (c, a)] {
        overlap = clip(&overlap, |point| (q - p).cross(point - p) * area.signum(), |u, v, t| u + (v - u) * t);
        if overlap.len() < 3 {
            return Earthwork::default();
        }
    }
    let footprint = overlap[1..].windows(2).map(|pair| xy_area(overlap[0], pair[0], pair[1])).sum();
    let delta = overlap.into_iter().map(|p| p.to3(plane(design, p) - plane(existing, p))).collect::<Vec<_>>();
    let interpolate = |a: Vec3, b: Vec3, t: f64| (a.xy() + (b.xy() - a.xy()) * t).to3(0.0);
    let fill = integral(&clip(&delta, |p| p.z, interpolate));
    let cut = -integral(&clip(&delta, |p| -p.z, interpolate));
    Earthwork { cut, fill, area: footprint }
}

/// Join sections at shared endpoints. At a saddle, each incident branch stays separate.
pub fn contour_paths(segments: Vec<[Vec3; 2]>) -> Vec<Vec<Vec3>> {
    let mut links: BTreeMap<Key, Vec<usize>> = BTreeMap::new();
    for (i, segment) in segments.iter().enumerate() {
        for endpoint in segment {
            links.entry(key(*endpoint)).or_default().push(i);
        }
    }
    let mut used = vec![false; segments.len()];
    let mut paths = Vec::new();
    let mut trace = |start: Key, first: usize| {
        let mut path = Vec::new();
        let mut at = start;
        let mut edge = first;
        while !used[edge] {
            used[edge] = true;
            let [a, b] = segments[edge];
            let (from, to) = if key(a) == at { (a, b) } else { (b, a) };
            if path.is_empty() {
                path.push(from);
            }
            path.push(to);
            at = key(to);
            let Some(neighbors) = links.get(&at).filter(|list| list.len() == 2) else { break };
            let Some(next) = neighbors.iter().copied().find(|&i| !used[i]) else { break };
            edge = next;
        }
        if !path.is_empty() {
            paths.push(path);
        }
    };
    for (&vertex, edges) in links.iter().filter(|(_, list)| list.len() != 2) {
        for &edge in edges {
            trace(vertex, edge);
        }
    }
    for (edge, segment) in segments.iter().enumerate() {
        trace(key(segment[0]), edge);
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constrained_surface_preserves_the_diagonal_and_an_excluded_area() {
        let [a, b, c, d] = [(0.0, 0.0, 0.0), (10.0, 0.0, 1.0), (10.0, 10.0, 2.0), (0.0, 10.0, 3.0)].map(|(x, y, z)| Vec3::new(x, y, z));
        let hole = [Vec3::new(1.0, 7.0, 2.0), Vec3::new(2.0, 7.0, 2.0), Vec3::new(1.0, 8.0, 2.0)];
        let result = triangulate(&[], &[vec![a, c]], Some(&[a, b, c, d]), &[hole.to_vec()]).unwrap();
        assert!(result.iter().any(|t| t.contains(&a) && t.contains(&c)));
        let surface_area: f64 = result.iter().map(|t| xy_area(t[0].xy(), t[1].xy(), t[2].xy())).sum();
        assert!((surface_area - (100.0 - xy_area(hole[0].xy(), hole[1].xy(), hole[2].xy()))).abs() < 1e-8);
        assert!(triangulate(&[], &[vec![a, c], vec![b, d]], Some(&[a, b, c, d]), &[]).is_err());
    }

    #[test]
    fn contour_across_two_faces_remains_one_line_at_the_requested_height() {
        let [a, b, c, d] = [(0.0, 0.0, 0.0), (10.0, 0.0, 2.0), (10.0, 10.0, 2.0), (0.0, 10.0, 0.0)].map(|(x, y, z)| Vec3::new(x, y, z));
        let faces = [[a, b, c], [a, c, d]];
        let paths = contour_paths(contours(&faces, 1.0, 40).unwrap());
        let at_mid_level = paths.iter().filter(|path| path[0].z == 1.0).collect::<Vec<_>>();
        assert_eq!(at_mid_level.len(), 1);
        assert!(at_mid_level[0].iter().all(|point| point.x == 5.0));
        assert!(at_mid_level[0].iter().any(|point| point.y == 0.0));
        assert!(at_mid_level[0].iter().any(|point| point.y == 10.0));
    }

    #[test]
    fn cut_and_fill_split_a_crossing_inside_the_shared_footprint() {
        let x = 6_000_000.0;
        let y = 24_000_000.0;
        let existing = [Vec3::new(x, y, 0.0), Vec3::new(x + 2.0, y, 0.0), Vec3::new(x, y + 2.0, 0.0)];
        let design = [Vec3::new(x, y, -1.0), Vec3::new(x + 2.0, y, 1.0), Vec3::new(x, y + 2.0, -1.0)];
        let actual = triangle_earthwork(existing, design);
        assert!((actual.area - 2.0).abs() < 1e-9);
        assert!((actual.fill - 1.0 / 6.0).abs() < 1e-9);
        assert!((actual.cut - 5.0 / 6.0).abs() < 1e-9);
        let reversed = triangle_earthwork(design, existing);
        assert!((actual.fill - reversed.cut).abs() < 1e-9);
    }
}
