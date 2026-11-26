//! Horizontal LandXML curves in easting/northing, sampled by distance along the centerline.

use std::f64::consts::TAU;

use cadcraft_geom::{Vec2, Vec3};

#[derive(Clone, Copy)]
pub(super) struct Point {
    pub xy: Vec2,
    pub z: Option<f64>,
}

pub(super) enum Shape {
    Line,
    Arc { center: Vec2, radius: f64, start_angle: f64, sweep: f64 },
    Clothoid { heading: f64, start_curvature: f64, change: f64 },
}

pub(super) struct Span {
    pub start: Point,
    pub end: Point,
    pub length: f64,
    pub station: Option<f64>,
    shape: Shape,
}

impl Span {
    pub(super) fn line(start: Point, end: Point, station: Option<f64>) -> Result<Self, &'static str> {
        let length = start.xy.dist(end.xy);
        if length <= 0.0 {
            return Err("horizontal line has zero length");
        }
        Ok(Self { start, end, length, station, shape: Shape::Line })
    }

    pub(super) fn arc(
        start: Point,
        end: Point,
        center: Vec2,
        direction: f64,
        declared: Option<f64>,
        station: Option<f64>,
        tolerance: f64,
    ) -> Result<Self, &'static str> {
        let radius = center.dist(start.xy);
        if radius <= 0.0 || (center.dist(end.xy) - radius).abs() > tolerance.max(radius * 1e-7) {
            return Err("circular arc endpoints do not match its center");
        }
        let start_angle = center.angle_to(start.xy);
        let end_angle = center.angle_to(end.xy);
        let mut magnitude = (direction * (end_angle - start_angle)).rem_euclid(TAU);
        if magnitude == 0.0 && declared.is_some_and(|length| (length - TAU * radius).abs() <= tolerance) {
            magnitude = TAU;
        }
        if magnitude <= 0.0 {
            return Err("circular arc has no sweep");
        }
        let length = magnitude * radius;
        let shape = Shape::Arc { center, radius, start_angle, sweep: direction * magnitude };
        Ok(Self { start, end, length, station, shape })
    }

    pub(super) fn clothoid(
        start: Point,
        end: Point,
        length: f64,
        curvatures: [f64; 2],
        station: Option<f64>,
        tolerance: f64,
    ) -> Result<Self, &'static str> {
        let [start_curvature, end_curvature] = curvatures;
        let change = (end_curvature - start_curvature) / length;
        let steps = (length * start_curvature.abs().max(end_curvature.abs()) / 0.05).ceil();
        if length <= 0.0 || !change.is_finite() || !(0.0..=100_000.0).contains(&steps) {
            return Err("invalid clothoid length or radius");
        }
        let n = (steps as usize).max(1);
        let local = (0..n)
            .fold(Vec2::ZERO, |p, i| p + integrate(0.0, start_curvature, change, length * i as f64 / n as f64, length * (i + 1) as f64 / n as f64));
        let chord = end.xy - start.xy;
        if local.len() <= 0.0 || chord.len() <= 0.0 || (chord.len() - local.len()).abs() > tolerance.max(length * 1e-6) {
            return Err("clothoid endpoints disagree with its radii and length");
        }
        let heading = Vec2::ZERO.angle_to(chord) - Vec2::ZERO.angle_to(local);
        let shape = Shape::Clothoid { heading, start_curvature, change };
        Ok(Self { start, end, length, station, shape })
    }

    pub(super) fn append(
        &self,
        out: &mut Vec<(Vec3, f64)>,
        start: f64,
        finish: f64,
        profile: Option<(&super::profile::Profile, &[Vec2])>,
        tolerance: f64,
        limit: usize,
    ) -> Result<(), &'static str> {
        let curvature = match self.shape {
            Shape::Line => 0.0,
            Shape::Arc { radius, .. } => radius.recip(),
            Shape::Clothoid { start_curvature, change, .. } => start_curvature.abs().max((start_curvature + change * self.length).abs()),
        };
        // A chord of length l deviates from a circular arc by at most k*l²/8 to first order.
        // An angular step cap also bounds the quadrature error on transition curves.
        let steps = (self.length * (curvature / (8.0 * tolerance)).sqrt()).max(self.length * curvature / 0.05).ceil().max(1.0);
        let grades = profile.map_or(&[][..], |(_, points)| {
            let lo = points.partition_point(|p| p.x <= start);
            let hi = points.partition_point(|p| p.x < finish);
            &points[lo..hi.max(lo)]
        });
        let initial = usize::from(out.is_empty());
        if !steps.is_finite() || steps > limit.saturating_sub(out.len() + grades.len() + initial) as f64 {
            return Err("alignment exceeds the geometry sample limit");
        }
        let steps = steps as usize;
        let mut i = usize::from(!out.is_empty());
        let mut j = 0;
        let mut xy = self.start.xy;
        let mut previous = 0.0;
        while i <= steps || j < grades.len() {
            let regular = if i < steps {
                start + (finish - start) * i as f64 / steps as f64
            } else if i == steps {
                finish
            } else {
                f64::INFINITY
            };
            let extra = grades.get(j).map_or(f64::INFINITY, |p| p.x);
            let at = regular.min(extra);
            i += usize::from(regular <= extra);
            j += usize::from(extra <= regular);
            let t = ((at - start) / (finish - start)).clamp(0.0, 1.0);
            let s = self.length * t;
            xy = if t == 0.0 {
                self.start.xy
            } else if t == 1.0 {
                self.end.xy
            } else {
                match self.shape {
                    Shape::Line => self.start.xy.lerp(self.end.xy, t),
                    Shape::Arc { center, radius, start_angle, sweep } => {
                        let angle = start_angle + sweep * t;
                        center + Vec2::new(angle.cos(), angle.sin()) * radius
                    }
                    Shape::Clothoid { heading, start_curvature, change } => xy + integrate(heading, start_curvature, change, previous, s),
                }
            };
            previous = s;
            let z = profile.and_then(|(profile, _)| profile.at(at)).unwrap_or_else(|| match (self.start.z, self.end.z) {
                (Some(a), Some(b)) => a + (b - a) * t,
                (a, b) => a.or(b).unwrap_or(0.0),
            });
            let point = xy.to3(z);
            if !point.is_finite() || xy.x.abs().max(xy.y.abs()).max(z.abs()) > crate::MAX_WINDOW_COORD {
                return Err("alignment geometry is outside the supported coordinate range");
            }
            out.push((point, at));
        }
        Ok(())
    }
}

/// Integrate the tangent whose heading is the integral of linearly varying curvature.
fn integrate(heading: f64, k: f64, change: f64, a: f64, b: f64) -> Vec2 {
    let tangent = |s| {
        let angle: f64 = heading + s * (k + 0.5 * change * s);
        Vec2::new(angle.cos(), angle.sin())
    };
    // Five-point Gauss-Legendre integration over this short span.
    let mid = (a + b) * 0.5;
    let half = (b - a) * 0.5;
    let sample = |x| tangent(mid - half * x) + tangent(mid + half * x);
    (tangent(mid) * (128.0 / 225.0)
        + sample(0.538_469_310_105_683_1) * 0.478_628_670_499_366_47
        + sample(0.906_179_845_938_664) * 0.236_926_885_056_189_08)
        * half
}
