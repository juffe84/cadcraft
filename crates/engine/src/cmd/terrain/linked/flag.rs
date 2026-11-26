use std::collections::HashSet;

use cadcraft_doc::{Drawing, EntityKind, Group, PolyfaceMesh};
use cadcraft_geom::Vec3;

use super::{FLAG_LAYER, FLAG_PREFIX};

pub(super) fn mesh(anchor: Vec3, size: f64) -> PolyfaceMesh {
    // Two slender faces make an upright mast. The pennant bends across the ridge and
    // tilts towards its foot, so its cloth has area in both orbit and plan.
    let offsets = [
        Vec3::ZERO,
        Vec3::Z,
        Vec3::new(0.035, 0.0, 0.0),
        Vec3::new(0.035, 0.0, 1.0),
        Vec3::new(0.0, 0.035, 0.0),
        Vec3::new(0.0, 0.035, 1.0),
        Vec3::new(0.0, 0.0, 0.59),
        Vec3::new(0.32, 0.16, 1.04),
        Vec3::new(0.33, 0.19, 0.64),
        Vec3::new(0.74, -0.055, 0.83),
    ];
    // Negative indices hide the internal diagonal between the first two cloth triangles.
    let faces = vec![[1, 3, 4, 2], [1, 5, 6, 2], [2, 8, -9, 0], [-2, 9, 7, 0], [8, 10, 9, 0]];
    PolyfaceMesh { vertices: offsets.into_iter().map(|offset| anchor + offset * size).collect(), faces }
}

/// Replace the generated wire flags in older drawings while keeping their handles and layer.
pub(crate) fn upgrade_legacy_flags(d: &mut Drawing) {
    let linked: HashSet<_> = d.groups.iter().filter_map(Group::linked_terrain_id).collect();
    let flags = d.groups.iter().filter(|g| g.description.strip_prefix(FLAG_PREFIX).is_some_and(|id| linked.contains(id)));
    for &handle in flags.flat_map(|g| &g.members) {
        let Some(entity) = d.model.get(handle).filter(|e| e.common.layer.eq_ignore_ascii_case(FLAG_LAYER)) else { continue };
        let EntityKind::Polyline3d(line) = &entity.kind else { continue };
        let &[anchor, mast, _, _, end] = line.points.as_slice() else { continue };
        let size = anchor.xy().dist(mast.xy());
        if !line.closed && end == mast && anchor.is_finite() && (0.049..=5.001).contains(&size) {
            d.model.modify(handle, |e| e.kind = EntityKind::PolyfaceMesh(mesh(anchor, size)));
        }
    }
}
