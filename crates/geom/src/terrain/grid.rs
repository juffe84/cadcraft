//! Snap-round a terrain export in plan while retaining its elevations and coverage.
use std::collections::{BTreeSet, HashMap, HashSet};

use rstar::{
    RTree, RTreeObject,
    primitives::{GeomWithData, Rectangle},
};
use spade::{ConstrainedDelaunayTriangulation, Point2, Triangulation};

use crate::{Bounds2, Vec2, Vec3};

use super::{Triangle, signed_area2};

const MAX_SEGMENTS: usize = 250_000;
const MAX_CANDIDATES: usize = 8_000_000;
type Pixel = [i64; 2];
type Edge = [Vec2; 2];
type Extent = Rectangle<[f64; 2]>;
type Spatial<T> = RTree<GeomWithData<Extent, T>>;

#[derive(Clone, Copy)]
struct Grid {
    step: f64,
    origin: Vec2,
}

impl Grid {
    fn new(first: Vec2, step: f64) -> Result<Self, &'static str> {
        if !(step.is_finite() && step > 0.0 && first.is_finite()) {
            return Err("Invalid terrain export grid.");
        }
        let origin = vec(pixel(first / step));
        Ok(Self { step, origin })
    }

    fn local(self, world: Vec2) -> Vec2 {
        world / self.step - self.origin
    }

    fn world(self, p: Pixel) -> Vec2 {
        (vec(p) + self.origin) * self.step
    }
}

fn pixel(v: Vec2) -> Pixel {
    [(v.x + 0.5).floor() as i64, (v.y + 0.5).floor() as i64]
}

fn vec(p: Pixel) -> Vec2 {
    Vec2::new(p[0] as f64, p[1] as f64)
}

fn extent(points: impl IntoIterator<Item = Vec2>, margin: f64) -> Extent {
    let b = Bounds2::from_points(points).expand(margin);
    Rectangle::from_corners([b.min.x, b.min.y], [b.max.x, b.max.y])
}

fn parameter([a, b]: Edge, at: Vec2) -> f64 {
    let delta = b - a;
    if a == b { 0.0 } else { ((at - a).dot(delta) / delta.len2()).clamp(0.0, 1.0) }
}

fn crossing([a, b]: Edge, [c, d]: Edge) -> Option<Vec2> {
    let (ab, cd, delta) = (b - a, d - c, c - a);
    let cross = ab.cross(cd);
    if cross == 0.0 {
        return None;
    }
    let (u, v) = (delta.cross(cd) / cross, delta.cross(ab) / cross);
    ((0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v)).then(|| a + ab * u)
}

/// The parameter at which a segment enters a half-open grid pixel.
fn entry([a, b]: Edge, p: Pixel) -> Option<f64> {
    let p = vec(p);
    let mut lo: f64 = 0.0;
    let mut hi: f64 = 1.0;
    for (from, to, center) in [(a.x, b.x, p.x), (a.y, b.y, p.y)] {
        let min = center - 0.5;
        let max = center + 0.5;
        if from == to {
            if !(min..max).contains(&from) {
                return None;
            }
        } else {
            let t1 = (min - from) / (to - from);
            let t2 = (max - from) / (to - from);
            lo = lo.max(t1.min(t2));
            hi = hi.min(t1.max(t2));
        }
    }
    if lo == hi && pixel(a.lerp(b, lo)) != pixel(p) {
        return None;
    }
    (lo <= hi).then_some(lo)
}

/// Node every segment at crossings and at touched pixels of existing endpoints.
fn node(edges: &[Edge]) -> Result<Vec<Vec<Pixel>>, &'static str> {
    if edges.len() > MAX_SEGMENTS || edges.iter().flatten().any(|p| !p.is_finite() || p.x.abs().max(p.y.abs()) > (1_u64 << 51) as f64) {
        return Err("Terrain exceeds the survey export grid limits.");
    }
    let index: Spatial<_> = RTree::bulk_load(edges.iter().enumerate().map(|(i, &edge)| GeomWithData::new(extent(edge, 0.0), i)).collect());
    let mut pixels: BTreeSet<_> = edges.iter().flatten().copied().map(pixel).collect();
    let mut checks = 0;
    for (i, &edge) in edges.iter().enumerate() {
        for candidate in index.locate_in_envelope_intersecting(extent(edge, 0.0).envelope()).filter(|candidate| candidate.data < i) {
            checks += 1;
            if checks > MAX_CANDIDATES || pixels.len() > MAX_SEGMENTS {
                return Err("There are too many intersections for survey export.");
            }
            if let Some(at) = crossing(edge, edges[candidate.data]) {
                pixels.insert(pixel(at));
            }
        }
    }
    route(edges, pixels)
}

fn route(edges: &[Edge], pixels: BTreeSet<Pixel>) -> Result<Vec<Vec<Pixel>>, &'static str> {
    let index: Spatial<_> = RTree::bulk_load(pixels.into_iter().map(|p| GeomWithData::new(extent([vec(p)], 0.0), p)).collect());
    let mut checks = 0;
    edges
        .iter()
        .map(|&edge| {
            let (a, b) = (pixel(edge[0]), pixel(edge[1]));
            if a == b {
                return Ok(vec![a]);
            }
            let mut route = Vec::new();
            for candidate in index.locate_in_envelope_intersecting(extent(edge, 0.5).envelope()) {
                checks += 1;
                if checks > MAX_CANDIDATES {
                    return Err("There are too many intersections for survey export.");
                }
                let p = candidate.data;
                if p != a
                    && p != b
                    && let Some(t) = entry(edge, p)
                {
                    route.push((t, p));
                }
            }
            route.sort_unstable_by(|(t, p), (u, q)| t.total_cmp(u).then(p.cmp(q)));
            Ok(std::iter::once(a).chain(route.into_iter().map(|(_, p)| p)).chain([b]).collect())
        })
        .collect()
}

fn adjacency(paths: &[Vec<Pixel>]) -> Result<(Vec<Pixel>, BTreeSet<[usize; 2]>), &'static str> {
    let mut pixels = Vec::new();
    let mut indices = HashMap::new();
    let mut edges = BTreeSet::new();
    for path in paths {
        let mut last = None;
        for &p in path {
            let i = *indices.entry(p).or_insert_with(|| {
                pixels.push(p);
                pixels.len() - 1
            });
            if let Some(previous) = last.filter(|&j| j != i) {
                edges.insert([i.min(previous), i.max(previous)]);
            }
            last = Some(i);
        }
        if edges.len() > MAX_SEGMENTS || pixels.len() > MAX_SEGMENTS {
            return Err("The snapped terrain exceeds the survey export limits.");
        }
    }
    Ok((pixels, edges))
}

type Mesh = ConstrainedDelaunayTriangulation<Point2<f64>>;

fn triangulation(paths: &[Vec<Pixel>]) -> Result<(Mesh, Vec<Pixel>), &'static str> {
    let (pixels, edges) = adjacency(paths)?;
    let verts = pixels.iter().map(|&p| Point2::new(p[0] as f64, p[1] as f64)).collect();
    let mut conflict = false;
    let mesh = Mesh::try_bulk_load_cdt(verts, edges.into_iter().collect(), |_| conflict = true)
        .map_err(|_| "Could not triangulate the survey export grid.")?;
    if conflict {
        return Err("Could not resolve all intersecting edges on the survey export grid.");
    }
    Ok((mesh, pixels))
}

fn weights([a, b, c]: Triangle, p: Vec2) -> Option<[f64; 3]> {
    let (ab, ac, ap) = ((b - a).xy(), (c - a).xy(), p - a.xy());
    let det = ab.cross(ac);
    if det == 0.0 {
        return None;
    }
    let (u, v) = (ap.cross(ac) / det, ab.cross(ap) / det);
    let w = [1.0 - u - v, u, v];
    w.iter().all(|&v| v >= -1e-10).then_some(w)
}

fn sample(index: &Spatial<Triangle>, p: Vec2) -> Option<f64> {
    let area = extent([p], 0.0);
    for item in index.locate_in_envelope_intersecting(area.envelope()) {
        if let Some(w) = weights(item.data, p) {
            return Some(item.data.iter().zip(w).map(|(v, w)| v.z * w).sum());
        }
    }
    None
}

fn elevation(index: &Spatial<Triangle>, p: Vec2) -> Option<f64> {
    let area = extent([p], 1.5);
    let near = || index.locate_in_envelope_intersecting(area.envelope()).flat_map(|item| item.data);
    let vertex = near().map(|v| (v.xy().dist(p), v.z)).filter(|(dist, _)| *dist <= 0.71).min_by(|a, b| a.0.total_cmp(&b.0));
    vertex.map(|(_, z)| z).or_else(|| sample(index, p)).or_else(|| {
        index
            .locate_in_envelope_intersecting(area.envelope())
            .flat_map(|item| {
                let [a, b, c] = item.data;
                [[a, b], [b, c], [c, a]].map(|[u, v]| {
                    let t = parameter([u.xy(), v.xy()], p);
                    (u.xy().lerp(v.xy(), t).dist(p), u.z + (v.z - u.z) * t)
                })
            })
            .filter(|(dist, _)| *dist <= 1.5)
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, z)| z)
    })
}

/// Snap and re-triangulate the current faces, while leaving the original holes outside the result.
pub fn snap_terrain_mesh(faces: &[Triangle], step: f64) -> Result<Vec<Triangle>, &'static str> {
    let first = faces.first().ok_or("There are no terrain triangles to export.")?;
    let grid = Grid::new(first[0].xy(), step)?;
    if faces.len() > MAX_SEGMENTS || faces.iter().flatten().any(|p| !p.is_finite()) {
        return Err("Terrain exceeds the survey export grid limits.");
    }
    let mut seen = HashSet::new();
    let mut edges = Vec::new();
    let local: Vec<_> = faces.iter().map(|face| face.map(|p| grid.local(p.xy()).to3(p.z))).collect();
    for &[a, b, c] in &local {
        for edge in [[a.xy(), b.xy()], [b.xy(), c.xy()], [c.xy(), a.xy()]] {
            let mut key = edge.map(|p| [p.x.to_bits(), p.y.to_bits()]);
            key.sort_unstable();
            if seen.insert(key) {
                edges.push(edge);
            }
        }
    }
    let (mesh, pixels) = triangulation(&node(&edges)?)?;
    let old: Spatial<_> =
        RTree::bulk_load(local.into_iter().filter(|&f| signed_area2(f) != 0.0).map(|f| GeomWithData::new(extent(f.map(Vec3::xy), 0.0), f)).collect());
    let mut result = Vec::new();
    let mut heights = HashMap::new();
    for face in mesh.inner_faces() {
        let ids = face.vertices().map(|v| v.index());
        let [a, b, c] = ids.map(|i| vec(pixels[i]));
        let longest = (b - a).len().max((c - b).len()).max((a - c).len());
        if (b - a).cross(c - a).abs() < longest || sample(&old, a + ((b - a) + (c - a)) / 3.0).is_none() {
            continue;
        }
        let mut out = [Vec3::ZERO; 3];
        for (p, i) in out.iter_mut().zip(ids) {
            let q = pixels[i];
            let z = *heights.entry(q).or_insert_with(|| elevation(&old, vec(q)));
            *p = grid.world(q).to3(z.ok_or("Could not sample a snapped terrain elevation.")?);
        }
        result.push(out);
    }
    if result.is_empty() {
        return Err("No triangles remain at the survey export resolution.");
    }
    Ok(result)
}

/// Snap and node source paths in plan. Heights at a crossing are taken from each source.
pub fn snap_terrain_paths(paths: &[Vec<Vec3>], step: f64) -> Result<Vec<Vec<Vec3>>, &'static str> {
    let Some(first) = paths.iter().flatten().next() else { return Ok(Vec::new()) };
    let grid = Grid::new(first.xy(), step)?;
    if paths.iter().map(Vec::len).sum::<usize>() > MAX_SEGMENTS || paths.iter().flatten().any(|p| !p.is_finite()) {
        return Err("Terrain sources exceed the survey export grid limits.");
    }
    let local: Vec<_> = paths.iter().map(|line| line.iter().map(|p| grid.local(p.xy()).to3(p.z)).collect::<Vec<_>>()).collect();
    let edges: Vec<_> = local.iter().flat_map(|line| line.windows(2).map(|pair| [pair[0].xy(), pair[1].xy()])).collect();
    let routed = node(&edges)?;
    // Check the snapped graph before writing it. Every proper crossing must be noded.
    triangulation(&routed)?;
    let mut routed = routed.iter();
    Ok(local
        .iter()
        .map(|line| {
            let mut out = Vec::new();
            if let Some(&p) = line.first() {
                out.push(grid.world(pixel(p.xy())).to3(p.z));
            }
            for (pair, route) in line.windows(2).zip(routed.by_ref()) {
                let [a, b] = [pair[0], pair[1]];
                for &p in route.iter().skip(1).take(route.len().saturating_sub(2)) {
                    let t = parameter([a.xy(), b.xy()], vec(p));
                    out.push(grid.world(p).to3(a.z + (b.z - a.z) * t));
                }
                out.push(grid.world(pixel(b.xy())).to3(b.z));
            }
            out.dedup();
            out
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossing_paths_share_plan_vertices_and_keep_their_own_elevations() {
        let a = [Vec3::new(10.0001, 0.0, 0.0), Vec3::new(10.0099, 0.010, 1.0)];
        let b = [Vec3::new(10.0, 0.010, 2.0), Vec3::new(10.01, 0.0, 2.0)];
        let out = snap_terrain_paths(&[a.to_vec(), b.to_vec()], 0.001).unwrap();
        assert_eq!(out[0].len(), 3);
        assert_eq!(out[1].len(), 3);
        assert_eq!(out[0][1].xy(), out[1][1].xy());
        assert!((0.0..1.0).contains(&out[0][1].z));
        assert_eq!(out[1][1].z, 2.0);
    }

    #[test]
    fn snap_rounding_preserves_an_open_area_and_discards_a_collapsed_face() {
        let p = |x, y| Vec3::new(173_000.0 + x, 7_200_000.0 + y, 2.0);
        let u = |x| [[p(x, 0.0), p(x + 1.0, 0.0), p(x + 1.0, 1.0)], [p(x, 0.0), p(x + 1.0, 1.0), p(x, 1.0)]];
        let [a, b] = u(0.0);
        let [c, d] = u(3.0);
        let sliver = [p(0.0, 0.0), p(0.5, -0.00001), p(1.0, 0.0)];
        let output = snap_terrain_mesh(&[a, b, c, d, sliver], 0.001).unwrap();
        assert!(output.iter().flatten().all(|v| v.z == 2.0 && v.y >= 7_200_000.0));
        assert!(output.iter().all(|t| {
            let span = (t[0].x.min(t[1].x).min(t[2].x), t[0].x.max(t[1].x).max(t[2].x));
            span.1 < 173_002.0 || span.0 > 173_002.0
        }));
        let total: f64 = output.iter().map(|&face| signed_area2(face).abs() / 2.0).sum();
        assert!((total - 2.0).abs() < 1e-6);
    }
}
