//! Changes to authored TINs that preserve the original surface between retained points.
use std::collections::HashMap;

use spade::{ConstrainedDelaunayTriangulation, FloatTriangulation, Point2, PositionInTriangulation, Triangulation};

use crate::{Bounds2, Vec2, Vec3, point_in_polygon, shoelace};

use super::{Inputs, Sample, Triangle, key};

pub const MAX_BOUNDARY_POINTS: usize = 1_024;

fn triangle_key(points: Triangle) -> [[u64; 3]; 3] {
    let mut k = points.map(key);
    k.sort_unstable();
    k
}

pub fn signed_area2(triangle: Triangle) -> f64 {
    let [a, b, c] = triangle;
    (b - a).xy().cross((c - a).xy())
}

/// Cut a closed XY outline into a TIN, retaining either its interior or exterior.
/// Each output face refers to the source plane it came from. Areas that already
/// had no surface stay empty, even if they lie on the retained side.
pub fn clip_surface(triangles: &[Triangle], outline: &[Vec2], keep_inside: bool, limit: usize) -> Result<Vec<(usize, Triangle)>, &'static str> {
    validate_outline(outline)?;
    let mut input = Inputs::default();
    let mut indices = HashMap::with_capacity(triangles.len());
    for (i, &triangle) in triangles.iter().enumerate() {
        if signed_area2(triangle) == 0.0 || indices.insert(triangle_key(triangle), i).is_some() {
            return Err("The terrain contains a degenerate or duplicate triangle.");
        }
        input.path(&triangle, true)?;
    }
    let mut crossing = false;
    let original = ConstrainedDelaunayTriangulation::<Sample>::try_bulk_load_cdt(input.vertices, input.edges.into_iter().collect(), |_| {
        crossing = true;
    })
    .map_err(|_| "Could not index the terrain triangles.")?;
    let mut source = HashMap::with_capacity(indices.len());
    for face in original.inner_faces() {
        let k = triangle_key(face.vertices().map(|v| v.data().0));
        if let Some(&index) = indices.get(&k) {
            source.insert(face.fix(), index);
        }
    }
    if crossing || source.len() != triangles.len() {
        return Err("The terrain has overlapping faces or vertices that split another face's edge.");
    }
    let height = original.barycentric();
    let on_surface = |p: Point2<f64>| {
        let z = height.interpolate(|v| v.data().0.z, p).unwrap_or_else(|| {
            if let PositionInTriangulation::OutsideOfConvexHull(edge) = original.locate(p) {
                let [a, b] = original.directed_edge(edge).vertices().map(|v| v.data().0);
                let delta = (b - a).xy();
                let t = ((Vec2::new(p.x, p.y) - a.xy()).dot(delta) / delta.len2()).clamp(0.0, 1.0);
                a.z + t * (b.z - a.z)
            } else {
                0.0
            }
        });
        Sample(Vec3::new(p.x, p.y, z))
    };
    let mut mesh = original.clone();
    let mut ring = Vec::with_capacity(outline.len());
    for point in outline {
        let p = Point2::new(point.x, point.y);
        let id = match mesh.locate(p) {
            PositionInTriangulation::OnVertex(id) => id,
            _ => mesh.insert(on_surface(p)).map_err(|_| "Could not insert a boundary vertex.")?,
        };
        ring.push(id);
    }
    for (&a, &b) in ring.iter().zip(ring.iter().cycle().skip(1)) {
        mesh.add_constraint_and_split(a, b, on_surface);
        if mesh.num_vertices() > limit * 2 {
            return Err("The boundary creates too many terrain vertices.");
        }
    }
    let bounds = Bounds2::from_points(outline.iter().copied());
    let mut result = Vec::new();
    let mut kept_area = vec![0.0; triangles.len()];
    for face in mesh.inner_faces() {
        let t = face.vertices().map(|v| v.data().0);
        if signed_area2(t) == 0.0 {
            continue;
        }
        let [a, b, c] = t;
        let center = a.xy() + ((b - a).xy() + (c - a).xy()) * (1.0 / 3.0);
        if (bounds.contains(center) && point_in_polygon(outline, center)) != keep_inside {
            continue;
        }
        if let PositionInTriangulation::OnFace(location) = original.locate(Point2::new(center.x, center.y))
            && let Some(&index) = source.get(&location)
        {
            // Preserve the existing corners and winding on unchanged triangles.
            let output = if triangle_key(t) == triangle_key(triangles[index]) {
                triangles[index]
            } else if signed_area2(t).signum() == signed_area2(triangles[index]).signum() {
                t
            } else {
                [a, c, b]
            };
            kept_area[index] += signed_area2(output).abs();
            result.push((index, output));
            if result.len() > limit {
                return Err("Clipping would exceed the editable terrain triangle limit.");
            }
        }
    }
    // Intersections at survey coordinates can round slightly to either side of a boundary.
    // Keep the original triangle when the cut removed no area that can be resolved at that
    // coordinate scale, so a repeated cut does not subdivide intact faces into slivers.
    let unchanged: Vec<_> = triangles
        .iter()
        .zip(kept_area)
        .map(|(&t, kept)| {
            let [a, b, c] = t;
            let area = signed_area2(t).abs();
            let max_coordinate = t.iter().fold(1.0_f64, |scale, p| scale.max(p.x.abs()).max(p.y.abs()));
            let max_edge = (b - a).xy().len().max((c - a).xy().len()).max((c - b).xy().len());
            let tolerance = (8.0 * f64::EPSILON * max_coordinate * max_edge).min(area * 1e-4);
            (area - kept).abs() <= tolerance
        })
        .collect();
    result.retain(|(i, _)| !unchanged[*i]);
    result.extend(triangles.iter().copied().enumerate().filter(|(i, _)| unchanged[*i]));
    Ok(result)
}

fn validate_outline(outline: &[Vec2]) -> Result<(), &'static str> {
    if !(3..=MAX_BOUNDARY_POINTS).contains(&outline.len()) {
        return Err("A closed boundary needs 3 to 1,024 distinct vertices.");
    }
    if outline.iter().any(|p| !p.is_finite() || p.x.abs().max(p.y.abs()) > 1e12) {
        return Err("Boundary coordinates are outside the drawing range.");
    }
    let edges: Vec<_> = outline.iter().copied().zip(outline.iter().copied().cycle().skip(1)).collect();
    for (i, &(a, b)) in edges.iter().enumerate() {
        if a == b {
            return Err("A boundary has two consecutive vertices at the same location.");
        }
        for (j, &(c, d)) in edges.iter().enumerate().skip(i + 1) {
            let adjacent = j == i + 1 || (i == 0 && j == edges.len() - 1);
            if adjacent {
                let (u, v) = if j == i + 1 { (a - b, d - c) } else { (b - a, c - d) };
                if u.cross(v) == 0.0 && u.dot(v) > 0.0 {
                    return Err("A boundary may not double back on itself.");
                }
            } else if intersects(a, b, c, d) {
                return Err("A boundary may not cross or touch itself.");
            }
        }
    }
    if shoelace(outline) == 0.0 {
        return Err("The boundary does not enclose an area.");
    }
    Ok(())
}

fn intersects(a: Vec2, b: Vec2, c: Vec2, d: Vec2) -> bool {
    if !Bounds2::new(a, b).intersects(&Bounds2::new(c, d)) {
        return false;
    }
    let sides = |p: Vec2, q: Vec2, x: Vec2, y: Vec2| {
        let dx = (q - p).cross(x - p);
        let dy = (q - p).cross(y - p);
        dx == 0.0 || dy == 0.0 || dx.is_sign_positive() != dy.is_sign_positive()
    };
    sides(a, b, c, d) && sides(c, d, a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_concave_boundary_splits_the_source_planes_at_survey_coordinates() {
        let x = 173000.0;
        let y = 7273000.0;
        let xy = |u, v| Vec2::new(x + u, y + v);
        let p = |u, v| xy(u, v).to3(10.0 + u * 0.25 + v * 0.5);
        let a = p(0.0, 0.0);
        let b = p(12.0, 0.0);
        let c = p(12.0, 10.0);
        let d = p(0.0, 10.0);
        let triangles = [[a, b, c], [a, c, d]];
        let outline = [xy(2.0, 2.0), xy(9.0, 2.0), xy(9.0, 4.0), xy(6.0, 4.0), xy(6.0, 8.0), xy(2.0, 8.0)];
        let area = |tris: &[(usize, Triangle)]| tris.iter().map(|(_, t)| signed_area2(*t).abs() * 0.5).sum::<f64>();
        let clipped = clip_surface(&triangles, &outline, true, 100).unwrap();
        let hole = clip_surface(&triangles, &outline, false, 100).unwrap();
        assert!((area(&clipped) - shoelace(&outline)).abs() < 1e-7);
        assert!((area(&hole) + area(&clipped) - 120.0).abs() < 1e-7);
        for (_, t) in clipped.into_iter().chain(hole) {
            for point in t {
                assert!((point.z - p(point.x - x, point.y - y).z).abs() < 1e-7);
            }
        }
    }

    #[test]
    fn trim_respects_empty_areas_and_rejects_a_self_crossing_outline() {
        let at = |x, y| Vec3::new(x, y, 1.0);
        let left = [at(0.0, 0.0), at(2.0, 0.0), at(1.0, 1.0)];
        let right = [at(6.0, 0.0), at(8.0, 0.0), at(7.0, 1.0)];
        let outline = [at(-1.0, -1.0), at(9.0, -1.0), at(9.0, 3.0), at(-1.0, 3.0)];
        let mut kept = clip_surface(&[left, right], &outline.map(Vec3::xy), true, 100).unwrap();
        kept.sort_unstable_by_key(|(i, _)| *i);
        assert_eq!(kept, [(0, left), (1, right)]);
        let crossing = [outline[0], outline[2], outline[1], outline[3]];
        assert!(clip_surface(&[left, right], &crossing.map(Vec3::xy), true, 100).is_err());
    }
    #[test]
    fn cutting_the_same_outline_twice_preserves_the_existing_faces() {
        let xy = |u, v| Vec2::new(173000.0 + u, 7273000.0 + v);
        let p = |u, v| xy(u, v).to3(u + v);
        let source = [[p(0.0, 0.0), p(12.0, 0.0), p(12.0, 10.0)], [p(0.0, 0.0), p(12.0, 10.0), p(0.0, 10.0)]];
        let outline = [xy(2.321, 2.387), xy(9.351, 2.387), xy(9.351, 8.913), xy(2.321, 8.913)];
        let clipped: Vec<_> = clip_surface(&source, &outline, false, 100).unwrap().into_iter().map(|(_, t)| t).collect();
        let second: Vec<_> = clip_surface(&clipped, &outline, false, 100).unwrap().into_iter().map(|(_, t)| t).collect();
        assert_eq!(second, clipped);
    }
}
