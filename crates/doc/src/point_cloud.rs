//! In-memory XYZ samples and a spatial index for model-space previews.
use cadcraft_geom::{Bounds2, Vec3};
use std::ops::Range;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CloudPoint {
    pub position: Vec3,
    pub rgb: [u16; 3],
    pub intensity: u16,
    pub classification: u8,
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
    /// Retained detail points once the current request finishes.
    pub detail_count: Option<usize>,
    /// The coarse preview is still being refined. Detail may already be available.
    pub indexing: bool,
    pub pending: bool,
    pub failed: bool,
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
