//! In-memory XYZ samples and a spatial index for model-space previews.
use cadcraft_geom::{Bounds2, Vec2, Vec3};
use serde::{Deserialize, Serialize};
use std::ops::Range;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CloudPoint {
    pub position: Vec3,
    pub rgb: [u16; 3],
    pub intensity: u16,
    pub classification: u8,
}

/// A selection through the point cloud, saved in model space so it follows the data.
/// Each plane points inward and is relative to a local origin for survey coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CloudRegion {
    pub origin: Vec3,
    pub planes: [[f64; 4]; 5],
}

/// A full-resolution copy of a selected volume, expressed relative to a world-space base point.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CloudCopy {
    pub region: CloudRegion,
    pub base: Vec3,
}

/// Also bounds the portable renderer's uniform containing the removal regions.
pub const MAX_CLOUD_REMOVALS: usize = 128;

impl CloudRegion {
    pub fn plan(a: Vec2, b: Vec2) -> Self {
        let bounds = Bounds2::new(a, b);
        let origin = bounds.center();
        Self {
            origin: origin.to3(0.0),
            planes: [
                [1.0, 0.0, 0.0, origin.x - bounds.min.x],
                [-1.0, 0.0, 0.0, bounds.max.x - origin.x],
                [0.0, 1.0, 0.0, origin.y - bounds.min.y],
                [0.0, -1.0, 0.0, bounds.max.y - origin.y],
                [0.0, 0.0, 0.0, 1.0],
            ],
        }
    }

    /// Clip a rectangle in normalized screen space, through either camera projection.
    pub fn projected(origin: Vec3, [x, y, _, w]: [[f64; 4]; 4], bounds: Bounds2) -> Self {
        let lower = |axis: [f64; 4], edge| std::array::from_fn(|i| axis[i] - edge * w[i]);
        let upper = |axis: [f64; 4], edge| std::array::from_fn(|i| edge * w[i] - axis[i]);
        let mut front = w;
        front[3] -= 0.01;
        Self { origin, planes: [lower(x, bounds.min.x), upper(x, bounds.max.x), lower(y, bounds.min.y), upper(y, bounds.max.y), front] }
    }

    pub fn valid(&self) -> bool {
        [self.origin.x, self.origin.y, self.origin.z].iter().all(|v| v.abs() <= 1.0e12) && self.planes.iter().flatten().all(|v| v.abs() <= 1.0e20)
    }

    pub fn contains(&self, position: Vec3) -> bool {
        let p = position - self.origin;
        self.planes.iter().all(|[a, b, c, d]| a * p.x + b * p.y + c * p.z + d >= 0.0)
    }

    pub fn planes_at(&self, origin: Vec3) -> [[f64; 4]; 5] {
        let p = origin - self.origin;
        self.planes.map(|[a, b, c, d]| [a, b, c, a * p.x + b * p.y + c * p.z + d])
    }
}

#[derive(Debug, PartialEq)]
pub struct PointCloud {
    points: Vec<CloudPoint>,
    root: Tile,
    pub extent: Bounds2,
    pub z_range: [f64; 2],
    pub detail: Option<DetailSource>,
    pub source_count: u64,
    pub has_rgb: bool,
    pub rgb_is_8_bit: bool,
}

#[derive(Debug, PartialEq)]
struct Tile {
    bounds: Bounds2,
    range: Range<usize>,
    children: Option<Box<[Tile; 2]>>,
}

impl Tile {
    fn build(points: &mut [CloudPoint], start: usize) -> Self {
        let bounds = Bounds2::from_points(points.iter().map(|p| p.position.xy()));
        let range = start..start + points.len();
        let children = if points.len() > 512 && bounds.min != bounds.max {
            let by_x = bounds.max.x - bounds.min.x >= bounds.max.y - bounds.min.y;
            let mid = points.len() / 2;
            points.select_nth_unstable_by(mid, |a, b| {
                let axis = |p: &CloudPoint| if by_x { p.position.x } else { p.position.y };
                axis(a).total_cmp(&axis(b))
            });
            let (left, right) = points.split_at_mut(mid);
            Some(Box::new([Self::build(left, start), Self::build(right, start + mid)]))
        } else {
            None
        };
        Self { bounds, range, children }
    }

    fn visible_ranges(&self, view: &Bounds2, points: &[CloudPoint], out: &mut Vec<Range<usize>>) {
        if !self.bounds.intersects(view) {
            return;
        }
        if view.contains_box(&self.bounds) {
            out.push(self.range.clone());
        } else if let Some(children) = &self.children {
            for child in children.iter() {
                child.visible_ranges(view, points, out);
            }
        } else {
            // Boundary leaves can contain fewer visible points than a single sampling stride.
            // Count just their visible runs, so zooming in never skips the entire leaf.
            let mut run_start = None;
            for i in self.range.clone() {
                if view.contains(points[i].position.xy()) {
                    run_start.get_or_insert(i);
                } else if let Some(start) = run_start.take() {
                    out.push(start..i);
                }
            }
            if let Some(start) = run_start {
                out.push(start..self.range.end);
            }
        }
    }
}

impl PointCloud {
    pub fn new(mut points: Vec<CloudPoint>, source_count: u64, has_rgb: bool, rgb_is_8_bit: bool) -> Self {
        let root = Tile::build(&mut points, 0);
        let extent = root.bounds;
        let z_range = points.iter().fold([f64::INFINITY, f64::NEG_INFINITY], |[min, max], p| [min.min(p.position.z), max.max(p.position.z)]);
        Self { points, root, extent, z_range, detail: None, source_count, has_rgb, rgb_is_8_bit }
    }

    pub fn points(&self) -> &[CloudPoint] {
        &self.points
    }

    pub fn bounds(&self) -> Bounds2 {
        self.extent
    }

    /// Walk only overlapping tiles, then sample their combined population with a fixed budget.
    /// Zooming into a smaller area reveals more of its original points.
    pub fn visible(&self, view: Bounds2, limit: usize, mut visit: impl FnMut(&CloudPoint)) {
        if limit == 0 {
            return;
        }
        let mut ranges = Vec::new();
        self.root.visible_ranges(&view, &self.points, &mut ranges);
        let count = ranges.iter().map(Range::len).sum::<usize>();
        let stride = count.div_ceil(limit).max(1);
        let mut offset = 0;
        for range in ranges {
            let start = (stride - offset) % stride;
            offset = (offset + range.len()) % stride;
            for point in self.points[range].iter().skip(start).step_by(stride) {
                visit(point);
            }
        }
    }
}

/// Returned data covers the requested view, or supplies the latest whole-cloud overview.
#[derive(Default)]
pub struct CloudView {
    pub points: Option<Arc<PointCloud>>,
    /// Stable coarse points drawn underneath a separate detailed layer.
    pub overview: Option<Arc<PointCloud>>,
    /// Full-resolution chunks in the view, with stable identities across requests.
    pub blocks: Vec<Arc<PointCloud>>,
    pub block_streaming: bool,
    pub detail_limited: bool,
    /// Retained detail points once the current request finishes.
    pub detail_count: Option<usize>,
    /// The coarse preview is still being refined. Detail may already be available.
    pub indexing: bool,
    pub pending: bool,
    pub failed: bool,
}

impl CloudView {
    pub fn layers<'a>(&'a self, root: &'a Arc<PointCloud>) -> impl Iterator<Item = &'a Arc<PointCloud>> {
        let overview = self.overview.as_ref().or_else(|| (self.indexing && self.points.is_some()).then_some(root));
        overview
            .into_iter()
            .chain(std::iter::once(self.points.as_ref().unwrap_or(root)))
            .chain(&self.blocks)
            .filter(|cloud| !cloud.points().is_empty())
    }
}

/// Controls for native streaming. Zero delegates the setting to the reader.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DetailOptions {
    pub radius: f64,
    pub radius_factor: f64,
    pub batch_points: u32,
    pub block_points: u32,
}

impl DetailOptions {
    pub const MAX_RADIUS: f64 = 1_000.0;
    pub const MAX_RADIUS_FACTOR: f64 = 5.0;
    pub const MIN_RADIUS_FACTOR: f64 = 0.01;
    pub const MIN_BATCH_POINTS: u32 = 10_000;
    pub const MAX_BATCH_POINTS: u32 = 2_000_000;
    pub const MIN_BLOCK_POINTS: u32 = 50_000;
    pub const MAX_BLOCK_POINTS: u32 = 2_000_000;
}

/// Zero leaves the overview density to the reader. Otherwise retain every Nth source point.
pub const MAX_PREVIEW_STEP: u16 = 1_000;

/// Reports processed and total source points while an overview is being prepared.
pub type OverviewProgress<'a> = &'a (dyn Fn(u64, u64) + std::panic::RefUnwindSafe);

pub trait CloudDetails: std::fmt::Debug + std::panic::RefUnwindSafe + Send + Sync {
    /// Request more detail without blocking the caller.
    fn view(&self, bounds: Bounds2) -> CloudView {
        self.view_with(bounds, DetailOptions::default())
    }
    fn view_with(&self, bounds: Bounds2, options: DetailOptions) -> CloudView;
    /// Current data without changing the worker's request.
    fn current(&self, bounds: Bounds2) -> CloudView;
}

#[derive(Debug)]
pub struct DetailSource(pub Arc<dyn CloudDetails>);

impl PartialEq for DetailSource {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
