//! Offset source chains and daylight a grade onto a triangular surface.
use rstar::{
    RTree, RTreeObject,
    primitives::{GeomWithData, Rectangle},
};

use crate::{Bounds2, Vec2, Vec3};

use super::{Triangle, edit::intersects};

const CHORD_ERROR: f64 = 0.005;
const EPS: f64 = 1e-8;
const MISS: &str = "Part of the grade does not reach the target surface. Check the side, rise or fall, and surface coverage.";
type Box2 = Rectangle<[f64; 2]>;
type Spatial<T> = RTree<GeomWithData<Box2, T>>;

pub enum GradeTarget<'a> {
    Offset { distance: f64, rise: f64 },
    Elevation { elevation: f64, run_per_rise: f64 },
    Surface { faces: &'a [Triangle], run_per_rise: f64, uphill: bool },
}

/// `side` is 1 for left and -1 for right, looking along the source.
pub fn grade_path(points: &[Vec3], closed: bool, side: f64, round: bool, target: GradeTarget<'_>, limit: usize) -> Result<Vec<Vec3>, &'static str> {
    if points.len() < if closed { 3 } else { 2 } || points.len() > limit || points.iter().any(|p| !p.is_finite()) {
        return Err("Select a source chain with valid XYZ points.");
    }
    let surface = match target {
        GradeTarget::Surface { faces, .. } => Some(Surface::new(faces)?),
        _ => None,
    };
    let hit = |origin: Vec3, dir: Vec2| -> Result<Vec3, &'static str> {
        match target {
            GradeTarget::Offset { distance, rise } => Ok((origin.xy() + dir * distance).to3(origin.z + rise)),
            GradeTarget::Elevation { elevation, run_per_rise } => {
                Ok((origin.xy() + dir * (elevation - origin.z).abs() * run_per_rise).to3(elevation))
            }
            GradeTarget::Surface { run_per_rise, uphill, .. } => {
                surface.as_ref().and_then(|s| s.hit(origin, dir, (if uphill { 1.0 } else { -1.0 }) / run_per_rise)).ok_or(MISS)
            }
        }
        .and_then(|p| {
            if p.is_finite() && p.x.abs().max(p.y.abs()).max(p.z.abs()) <= 1e12 {
                Ok(p)
            } else {
                Err("The copy exceeds the drawing coordinate range.")
            }
        })
    };
    let joins = joins(points, closed, side, round)?;
    let mut trace = Trace { points: Vec::new(), limit, hit, surface: surface.as_ref() };
    for i in 0..points.len() {
        let [incoming, outgoing] = joins[i];
        let origin = points[i];
        let turn = incoming.cross(outgoing).atan2(incoming.dot(outgoing));
        if turn.abs() > EPS {
            trace.span(|t| (origin, incoming.rotate(turn * t)))?;
        }
        if i + 1 == points.len() && !closed {
            break;
        }
        let next = (i + 1) % points.len();
        let end = points[next];
        let before = joins[next][0];
        let start_hit = (trace.hit)(origin, outgoing)?;
        let end_hit = (trace.hit)(end, before)?;
        if (end_hit - start_hit).xy().dot((end - origin).xy().normalized()) <= EPS {
            return Err("The offset folds along a segment. Use a smaller distance or a different side.");
        }
        trace.span(|t| (origin + (end - origin) * t, outgoing.lerp(before, t)))?;
    }
    if closed && trace.points.first().zip(trace.points.last()).is_some_and(|(a, b)| (*a - *b).len() <= EPS) {
        trace.points.pop();
    }
    validate(&trace.points, closed)?;
    Ok(trace.points)
}

fn joins(points: &[Vec3], closed: bool, side: f64, round: bool) -> Result<Vec<[Vec2; 2]>, &'static str> {
    let last = points.len() - 1;
    points
        .iter()
        .enumerate()
        .map(|(i, &here)| {
            let before = (here
                - points[if i > 0 {
                    i - 1
                } else if closed {
                    last
                } else {
                    i
                }])
            .xy();
            let after = (points[if i < last {
                i + 1
            } else if closed {
                0
            } else {
                i
            }] - here)
                .xy();
            if i == 0 && !closed {
                return direction(after).map(|d| [d.perp() * side; 2]);
            }
            if i == last && !closed {
                return direction(before).map(|d| [d.perp() * side; 2]);
            }
            let (a, b) = (direction(before)?, direction(after)?);
            let (left, right) = (a.perp() * side, b.perp() * side);
            let bend = 1.0 + a.dot(b);
            if bend < 1e-6 {
                return Err("A source chain cannot double back at an offset corner.");
            }
            if round && a.cross(b) * side < -EPS { Ok([left, right]) } else { Ok([(left + right) / bend; 2]) }
        })
        .collect()
}

fn direction(p: Vec2) -> Result<Vec2, &'static str> {
    (p.len() > EPS).then(|| p.normalized()).ok_or("A source chain has a segment with no horizontal length.")
}

struct Trace<'a, F> {
    points: Vec<Vec3>,
    limit: usize,
    hit: F,
    surface: Option<&'a Surface>,
}

impl<F: Fn(Vec3, Vec2) -> Result<Vec3, &'static str>> Trace<'_, F> {
    fn span(&mut self, ray: impl Fn(f64) -> (Vec3, Vec2)) -> Result<(), &'static str> {
        let at = |t| {
            let (p, d) = ray(t);
            (self.hit)(p, d)
        };
        let (a, b) = (at(0.0)?, at(1.0)?);
        let mut points = std::mem::take(&mut self.points);
        push(&mut points, a, self.limit)?;
        let mut stack = vec![(0.0, 1.0, a, b, 0)];
        while let Some((lo, hi, a, b, depth)) = stack.pop() {
            let t = (lo + hi) * 0.5;
            let mid = at(t)?;
            let source_span = (ray(hi).0 - ray(lo).0).xy().len();
            let needs_check = self.surface.is_some_and(|surface| source_span > 1.0 || !surface.contains_chord(a, b));
            if needs_check || (mid - (a + (b - a) * 0.5)).len() > CHORD_ERROR {
                if depth == 20 {
                    return Err("The grade changes abruptly at the target surface boundary.");
                }
                stack.push((t, hi, mid, b, depth + 1));
                stack.push((lo, t, a, mid, depth + 1));
            } else {
                push(&mut points, b, self.limit)?;
            }
        }
        self.points = points;
        Ok(())
    }
}

fn push(points: &mut Vec<Vec3>, p: Vec3, limit: usize) -> Result<(), &'static str> {
    if points.last().is_some_and(|&previous| (p - previous).len() <= EPS) {
        return Ok(());
    }
    if points.len() == limit {
        return Err("The grade exceeds the terrain vertex limit.");
    }
    points.push(p);
    Ok(())
}

fn envelope(bounds: Bounds2) -> Box2 {
    Rectangle::from_corners([bounds.min.x, bounds.min.y], [bounds.max.x, bounds.max.y])
}

fn validate(points: &[Vec3], closed: bool) -> Result<(), &'static str> {
    let minimum = if closed { 3 } else { 2 };
    if points.len() < minimum {
        return Err("The copied chain has collapsed.");
    }
    let xy = points.iter().map(|p| p.xy());
    let segments: Vec<_> = xy.clone().zip(xy.cycle().skip(1)).take(points.len() - usize::from(!closed)).collect();
    let index: Spatial<_> =
        RTree::bulk_load(segments.iter().enumerate().map(|(i, &(a, b))| GeomWithData::new(envelope(Bounds2::new(a, b)), i)).collect());
    let mut checks = 0;
    for (i, &(a, b)) in segments.iter().enumerate() {
        for item in index.locate_in_envelope_intersecting(envelope(Bounds2::new(a, b)).envelope()).filter(|item| item.data > i) {
            checks += 1;
            if checks > 2_000_000 {
                return Err("The copied chain is too complex to check for overlaps.");
            }
            let j = item.data;
            let (c, d) = segments[j];
            let backtracks = |u: Vec2, v: Vec2| u.cross(v).abs() <= EPS * u.len() * v.len() && u.dot(v) > 0.0;
            let invalid = if j == i + 1 {
                backtracks(a - b, d - c)
            } else if closed && i == 0 && j + 1 == segments.len() {
                backtracks(b - a, c - d)
            } else {
                intersects(a, b, c, d)
            };
            if invalid {
                return Err("The copied chain crosses or retraces itself. Use a smaller offset or a different side.");
            }
        }
    }
    Ok(())
}

struct Surface {
    faces: Spatial<Triangle>,
    min: f64,
    max: f64,
}

impl Surface {
    fn new(faces: &[Triangle]) -> Result<Self, &'static str> {
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        let mut entries = Vec::new();
        for &face in faces {
            if face.iter().any(|p| !p.is_finite()) || super::signed_area2(face) == 0.0 {
                continue;
            }
            for point in face {
                min = min.min(point.z);
                max = max.max(point.z);
            }
            entries.push(GeomWithData::new(envelope(Bounds2::from_points(face.map(Vec3::xy))), face));
        }
        if entries.is_empty() {
            return Err("The target surface has no usable triangles.");
        }
        Ok(Self { faces: RTree::bulk_load(entries), min, max })
    }

    fn hit(&self, origin: Vec3, dir: Vec2, slope: f64) -> Option<Vec3> {
        let limit = ((if slope > 0.0 { self.max } else { self.min }) - origin.z) / slope;
        if !limit.is_finite() || limit < -EPS {
            return None;
        }
        let limit = limit.max(0.0);
        let bounds = envelope(Bounds2::new(origin.xy(), origin.xy() + dir * limit).expand(EPS));
        let mut best = None;
        for item in self.faces.locate_in_envelope_intersecting(bounds.envelope()) {
            if let Some(distance) = face_hit(item.data, origin, dir, slope, best.unwrap_or(limit)) {
                best = Some(distance);
            }
        }
        best.map(|distance| (origin.xy() + dir * distance).to3(origin.z + slope * distance))
    }

    /// Check that every part of the exported chord stays within the grading tolerance
    /// of the target, including gaps smaller than the source sampling interval.
    fn contains_chord(&self, start: Vec3, end: Vec3) -> bool {
        let delta = end - start;
        let bounds = envelope(Bounds2::new(start.xy(), end.xy()).expand(EPS));
        let mut intervals: Vec<_> = self
            .faces
            .locate_in_envelope_intersecting(bounds.envelope())
            .filter_map(|item| {
                let (height, rise, [lo, hi]) = face_ray(item.data, start, delta.xy(), delta.z, 1.0)?;
                let v = height - rise * lo;
                let w = height - rise * hi;
                (v.abs() <= CHORD_ERROR && w.abs() <= CHORD_ERROR).then_some([lo, hi])
            })
            .collect();
        intervals.sort_unstable_by(|a, b| a[0].total_cmp(&b[0]));
        let mut covered = 0.0_f64;
        for [lo, hi] in intervals {
            if lo > covered + 1e-8 {
                return false;
            }
            covered = covered.max(hi);
            if covered >= 1.0 - 1e-8 {
                return true;
            }
        }
        false
    }
}

fn face_ray([a, b, c]: Triangle, origin: Vec3, dir: Vec2, slope: f64, limit: f64) -> Option<(f64, f64, [f64; 2])> {
    let (ab, ac, ap) = ((b - a).xy(), (c - a).xy(), origin.xy() - a.xy());
    let det = ab.cross(ac);
    let (v, w) = (ap.cross(ac) / det, ab.cross(ap) / det);
    let (dv, dw) = (dir.cross(ac) / det, ab.cross(dir) / det);
    let (dz_ab, dz_ac) = (b.z - a.z, c.z - a.z);
    let height = (a.z - origin.z) + v * dz_ab + w * dz_ac;
    let rise = slope - dv * dz_ab - dw * dz_ac;
    let mut lo = 0.0_f64;
    let mut hi = limit + EPS;
    for (start, direction) in [(v, dv), (w, dw), (1.0 - v - w, -dv - dw)] {
        if direction > 0.0 {
            lo = lo.max((-1e-10 - start) / direction);
        } else if direction < 0.0 {
            hi = hi.min((-1e-10 - start) / direction);
        } else if start < -1e-10 {
            return None;
        }
    }
    (lo <= hi).then_some((height, rise, [lo, hi]))
}

fn face_hit(face: Triangle, origin: Vec3, dir: Vec2, slope: f64, limit: f64) -> Option<f64> {
    let (height, rise, [lo, hi]) = face_ray(face, origin, dir, slope, limit)?;
    let distance = if rise.abs() > 1e-12 {
        height / rise
    } else if height.abs() < EPS {
        lo
    } else {
        return None;
    };
    (distance.is_finite() && distance >= lo - EPS && distance <= hi).then(|| distance.clamp(lo, limit.max(lo)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grade_does_not_bridge_a_narrow_gap_in_the_target() {
        let source = [Vec3::new(0.0, 0.0, 10.0), Vec3::new(4.0, 0.0, 10.0)];
        let strip = |x1, x2| {
            let a = Vec3::new(x1, 1.0, 9.0);
            let b = Vec3::new(x2, 1.0, 9.0);
            let c = Vec3::new(x2, 4.0, 9.0);
            let d = Vec3::new(x1, 4.0, 9.0);
            [[a, b, c], [a, c, d]]
        };
        let continuous = strip(-1.0, 5.0);
        let target = |faces| GradeTarget::Surface { faces, run_per_rise: 2.0, uphill: false };
        let line = grade_path(&source, false, 1.0, true, target(&continuous), 100).unwrap();
        assert!(line.iter().all(|p| (p.y - 2.0).abs() < 1e-8 && p.z == 9.0));
        let mut gap = strip(-1.0, 0.10).to_vec();
        gap.extend(strip(0.13, 5.0));
        assert!(grade_path(&source, false, 1.0, true, target(&gap), 200).is_err());
    }
}
