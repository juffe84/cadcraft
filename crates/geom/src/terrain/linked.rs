//! Planar triangulation of linked terrain sources. The caller assigns heights
//! from the source objects after intersections have been inserted.
use std::collections::{HashMap, HashSet};

use spade::{ConstrainedDelaunayTriangulation, Point2, Triangulation};

use crate::Vec2;

/// Split crossing source segments into shared XY vertices. Nearby endpoints
/// are welded to the location of the first endpoint within `weld`.
pub fn triangulate_linked(paths: &[Vec<Vec2>], weld: f64, limit: usize) -> Result<Vec<[Vec2; 3]>, &'static str> {
    if !(weld.is_finite() && weld >= 1e-6) || paths.iter().map(Vec::len).sum::<usize>() > limit {
        return Err("Too many vertices in the linked terrain sources.");
    }
    let mut vertices = Vec::<Vec2>::new();
    let mut cells = HashMap::<(i64, i64), Vec<usize>>::new();
    let mut edges = Vec::new();
    let mut seen = HashSet::new();
    for path in paths {
        let mut previous: Option<usize> = None;
        for &p in path {
            if !p.is_finite() || p.x.abs().max(p.y.abs()) > 1e12 {
                return Err("A source vertex is outside the drawing coordinate range.");
            }
            let x = (p.x / weld).floor() as i64;
            let y = (p.y / weld).floor() as i64;
            let index = (x - 1..=x + 1)
                .flat_map(|a| (y - 1..=y + 1).map(move |b| (a, b)))
                .filter_map(|key| cells.get(&key))
                .flatten()
                .copied()
                .filter(|&i| (vertices[i] - p).len2() <= weld * weld)
                .min()
                .unwrap_or_else(|| {
                    let new = vertices.len();
                    vertices.push(p);
                    cells.entry((x, y)).or_default().push(new);
                    new
                });
            if let Some(old) = previous.filter(|&old| old != index) {
                let ordered = [old.min(index), old.max(index)];
                if seen.insert(ordered) {
                    edges.push((old, index));
                }
            }
            previous = Some(index);
        }
    }
    let mut mesh = ConstrainedDelaunayTriangulation::<Point2<f64>>::new();
    let handles = vertices
        .into_iter()
        .map(|p| mesh.insert(Point2::new(p.x, p.y)).map_err(|_| "Could not insert a terrain source vertex."))
        .collect::<Result<Vec<_>, _>>()?;
    for (a, b) in edges {
        if mesh.add_constraint_and_split(handles[a], handles[b], |p| p).is_empty() {
            return Err("A source segment could not be represented by the linked terrain.");
        }
        if mesh.num_vertices() > limit {
            return Err("The linked terrain contains too many source intersections.");
        }
    }
    let faces: Vec<_> = mesh.inner_faces().map(|f| f.vertices().map(|p| Vec2::new(p.position().x, p.position().y))).collect();
    if faces.is_empty() {
        return Err("The linked terrain needs at least three non-collinear source points.");
    }
    if faces.len() > limit.saturating_mul(2) {
        return Err("The linked terrain would exceed the triangle limit.");
    }
    Ok(faces)
}
