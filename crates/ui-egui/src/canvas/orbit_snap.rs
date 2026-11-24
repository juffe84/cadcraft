//! Object snaps measured in screen pixels and returned as world XYZ.
use cadcraft_engine::{Session, snap::mode};
use cadcraft_geom::Vec3;
use cadcraft_render::SpatialSnap;
use egui::Pos2;

use super::orbit_scene::{Model, edge};
use super::point_cloud::OrbitFrame;

#[derive(Clone, Copy)]
pub(super) struct Query {
    mode: u32,
    pub radius: f32,
    base: Option<Vec3>,
    height: Option<f64>,
}

impl Query {
    pub fn new(session: &Session) -> Self {
        let plane = session.orbit_draft_plane();
        let base = session.current_prompt().and_then(|prompt| prompt.base).map(|base| {
            let last = session.last_point3();
            if last.xy() == base { last } else { base.to3(plane.height) }
        });
        let osmode = session.settings.osmode;
        let mode = session.snap_override().unwrap_or(if osmode & mode::OFF == 0 { osmode } else { 0 });
        Self { mode, radius: session.settings.aperture as f32, base, height: plane.locked.then_some(plane.height) }
    }

    pub fn accepts(&self, point: Vec3) -> bool {
        self.height.is_none_or(|z| (point.z - z).abs() <= 1e-7 + z.abs() * f64::EPSILON * 8.0)
    }

    pub fn find(self, model: &Model, frame: &OrbitFrame, at: Pos2) -> Option<(Vec3, u32)> {
        const SUPPORTED: u32 = mode::END | mode::MID | mode::CEN | mode::QUA | mode::NOD | mode::INS | mode::NEA | mode::PER | mode::INT;
        if self.mode & SUPPORTED == 0 {
            return None;
        }
        let mut best: Option<((bool, f32, f64), Vec3, u32)> = None;
        let mut offer = |point: Vec3, mode| {
            if self.mode & mode == 0 || !self.accepts(point) {
                return;
            }
            let (projected, depth) = frame.project(point);
            let distance = at.distance(projected);
            if !(distance <= self.radius && frame.camera.weight(depth) > 0.01) {
                return;
            }
            let bias = if matches!(mode, mode::END | mode::INT) { self.radius * 0.15 } else { 0.0 };
            let rank = (mode == mode::NEA, distance - bias, -depth);
            if best.is_none_or(|(old, _, _)| rank < old) {
                best = Some((rank, point, mode));
            }
        };
        let mut nearby = Vec::new();
        for snap in model.snaps_near(frame, at, self.radius) {
            match snap {
                SpatialSnap::End(point) => offer(point, mode::END),
                SpatialSnap::Mid(point) => offer(point, mode::MID),
                SpatialSnap::Center(point) => offer(point, mode::CEN),
                SpatialSnap::Quadrant(point) => offer(point, mode::QUA),
                SpatialSnap::Node(point) => offer(point, mode::NOD),
                SpatialSnap::Insertion(point) => offer(point, mode::INS),
                SpatialSnap::Segment { ends: [a, b], range: [lo, hi] } => {
                    if lo == 0.0 {
                        offer(a, mode::END);
                    }
                    if hi == 1.0 {
                        offer(b, mode::END);
                    }
                    if lo <= 0.5 && hi >= 0.5 {
                        offer(a + (b - a) * 0.5, mode::MID);
                    }
                    if self.mode & mode::PER != 0
                        && let Some(base) = self.base
                    {
                        let dir = b - a;
                        let parameter = (base - a).dot(dir) / dir.dot(dir);
                        if (lo..=hi).contains(&parameter) {
                            offer(a + dir * parameter, mode::PER);
                        }
                    }
                    if self.mode & (mode::INT | mode::NEA) != 0 {
                        let ends = snap.positions();
                        if let Some((distance, point)) = edge(frame, at, ends).filter(|(d, _)| *d <= self.radius) {
                            offer(point, mode::NEA);
                            if self.mode & mode::INT != 0 {
                                nearby.push((distance, ends));
                            }
                        }
                    }
                }
            }
        }
        // Compare local segments only, bounding the work at dense junctions.
        nearby.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
        let nearby = &nearby[..nearby.len().min(64)];
        for (i, &(_, a)) in nearby.iter().enumerate() {
            for &(_, b) in &nearby[..i] {
                if let Some(point) = intersection(a, b) {
                    offer(point, mode::INT);
                }
            }
        }
        best.map(|(_, position, mode)| (position, mode))
    }
}

fn intersection([a, b]: [Vec3; 2], [c, d]: [Vec3; 2]) -> Option<Vec3> {
    let (u, v, w) = (b - a, d - c, c - a);
    let normal = u.cross(v);
    let denom = normal.dot(normal);
    if denom <= f64::EPSILON * u.dot(u) * v.dot(v) {
        return None;
    }
    let t = w.cross(v).dot(normal) / denom;
    let s = w.cross(u).dot(normal) / denom;
    let range = -1e-10..=1.0 + 1e-10;
    if !range.contains(&t) || !range.contains(&s) {
        return None;
    }
    let point = a + u * t.clamp(0.0, 1.0);
    let other = c + v * s.clamp(0.0, 1.0);
    let tolerance = 1e-9 + point.len().max(other.len()) * f64::EPSILON * 16.0;
    ((point - other).len() <= tolerance).then_some(point + (other - point) * 0.5)
}
