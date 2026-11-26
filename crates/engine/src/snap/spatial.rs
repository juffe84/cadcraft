use super::{SnapHit, mode};
use crate::{Session, Vec2, Vec3};
use cadcraft_geom::Bounds2;
use cadcraft_render::SpatialSnap;

impl Session {
    /// Lift the chosen plan-view snap to its authored position for a spatial drawing command.
    pub fn plan_snap_xyz(&self, hit: SnapHit, aperture: f64) -> Option<Vec3> {
        self.spatial_draft_reference()?;
        if let Some(z) = hit.world_z {
            return Some(self.project_spatial_snap(hit.point.to3(z)));
        }
        self.authored_snap_xyz(hit, aperture, |_| true).map(|point| self.project_spatial_snap(point))
    }

    /// Lift a plan-view snap to authored XYZ, optionally restricting which entities can supply it.
    pub fn authored_snap_xyz(&self, hit: SnapHit, aperture: f64, accepts: impl Fn(cadcraft_doc::Handle) -> bool) -> Option<Vec3> {
        if hit.deferred.is_some() || hit.viewport.is_some() {
            return None;
        }
        if hit.mode & (mode::END | mode::MID | mode::CEN | mode::QUA | mode::NOD | mode::INS | mode::NEA | mode::PER | mode::INT) == 0 {
            return None;
        }

        let doc = self.doc().ok()?;
        let space = self.space();
        let probe = Bounds2::new(hit.point, hit.point).expand(aperture.max(1e-6));
        let index = crate::spatial::index(doc, &space);
        let candidates = crate::select::candidates(doc, &space, &index, &probe, false, false)?
            .filter(|(entity, known)| {
                accepts(entity.handle)
                    && crate::select::selectable(doc, entity, self.viewport_frozen())
                    && crate::select::bounds_of(doc, entity, *known).intersects(&probe)
            })
            .take(2000)
            .map(|(entity, _)| entity);
        let geometry = cadcraft_render::build_spatial_snaps(doc, candidates);
        let tolerance = 1e-7 + hit.point.len() * f64::EPSILON * 32.0;
        let matches =
            geometry.into_iter().filter_map(|snap| position(snap, hit.mode, hit.point)).filter(|point| point.xy().near(hit.point, tolerance));
        if hit.mode == mode::INT {
            // An XY crossing gives an intersection in space only where the two heights agree.
            let mut heights: Vec<f64> = matches.map(|point| point.z).collect();
            heights.sort_unstable_by(f64::total_cmp);
            return heights.windows(2).rev().find(|pair| (pair[1] - pair[0]).abs() <= tolerance).map(|pair| hit.point.to3(pair[1]));
        }
        matches.max_by(|a, b| b.xy().dist(hit.point).total_cmp(&a.xy().dist(hit.point)).then_with(|| a.z.total_cmp(&b.z)))
    }
}

fn position(snap: SpatialSnap, mode: u32, cursor: Vec2) -> Option<Vec3> {
    match (snap, mode) {
        (SpatialSnap::End(p), mode::END)
        | (SpatialSnap::Mid(p), mode::MID)
        | (SpatialSnap::Center(p), mode::CEN)
        | (SpatialSnap::Quadrant(p), mode::QUA)
        | (SpatialSnap::Node(p), mode::NOD)
        | (SpatialSnap::Insertion(p), mode::INS) => Some(p),
        (SpatialSnap::Segment { ends: [a, b], range: [lo, hi] }, mode::END) => [(lo == 0.0).then_some(a), (hi == 1.0).then_some(b)]
            .into_iter()
            .flatten()
            .min_by(|a, b| a.xy().dist(cursor).total_cmp(&b.xy().dist(cursor)).then_with(|| b.z.total_cmp(&a.z))),
        (SpatialSnap::Segment { ends: [a, b], range: [lo, hi] }, mode::MID) if (lo..=hi).contains(&0.5) => Some(a + (b - a) * 0.5),
        (SpatialSnap::Segment { .. }, mode::NEA | mode::PER | mode::INT) => {
            let [a, b] = snap.positions();
            let d = b.xy() - a.xy();
            let r = d.dot(d);
            (r > 0.0).then(|| a + (b - a) * ((cursor - a.xy()).dot(d) / r).clamp(0.0, 1.0))
        }
        _ => None,
    }
}
