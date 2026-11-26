//! Display styles for model-space TIN terrain groups.
use std::collections::HashSet;

use cadcraft_color::Rgb;
use cadcraft_doc::{Drawing, EntityKind, Handle};
use cadcraft_geom::Vec3;

use crate::{Builder, Ctx, Ink, Options, Vec2};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TerrainColor {
    #[default]
    Drawing,
    Elevation,
    Slope,
}

impl TerrainColor {
    pub fn name(self) -> &'static str {
        match self {
            Self::Drawing => "drawing",
            Self::Elevation => "elevation",
            Self::Slope => "slope",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerrainStyle {
    pub color: TerrainColor,
    pub bands: u8,
    pub transparency: u8,
    pub edges: bool,
}

impl Default for TerrainStyle {
    fn default() -> Self {
        Self { color: TerrainColor::Drawing, bands: 8, transparency: 0, edges: true }
    }
}

/// Colors and physical range used by a terrain display list.
#[derive(Clone, Debug)]
pub struct TerrainLegend {
    pub color: TerrainColor,
    pub range: [f64; 2],
    pub swatches: Vec<Rgb>,
}

pub(crate) struct TerrainDisplay {
    style: TerrainStyle,
    members: HashSet<Handle>,
    low: f64,
    high: f64,
}

impl TerrainDisplay {
    pub(crate) fn new(d: &Drawing, opts: &Options) -> Option<Self> {
        let style = opts.terrain;
        if !opts.spatial || !opts.fill || (style.color == TerrainColor::Drawing && style.transparency == 0 && style.edges) {
            return None;
        }
        let mut display = Self { style, members: HashSet::new(), low: f64::INFINITY, high: f64::NEG_INFINITY };
        for group in d.groups.iter().filter(|g| g.is_terrain_surface()) {
            for &handle in &group.members {
                if opts.cancelled() {
                    return None;
                }
                let Some(e) = d.model.get(handle).filter(|e| d.is_visible(e)) else { continue };
                if let EntityKind::Face3d(face) = &e.kind
                    && let [a, b, c, d] = face.corners
                    && c == d
                    && [a, b, c].iter().all(|p| p.is_finite())
                {
                    display.members.insert(handle);
                    display.low = display.low.min(a.z).min(b.z).min(c.z);
                    display.high = display.high.max(a.z).max(b.z).max(c.z);
                }
            }
        }
        (!display.members.is_empty()).then_some(display)
    }

    pub(crate) fn legend(&self) -> Option<TerrainLegend> {
        let color = self.style.color;
        let (range, flat) = match color {
            TerrainColor::Drawing => return None,
            TerrainColor::Elevation => ([self.low, self.high], self.low == self.high),
            TerrainColor::Slope => ([0.0, 90.0], false),
        };
        let count = usize::from(self.style.bands.clamp(2, 16));
        let swatches = (0..if flat { 1 } else { count }).map(|i| ink(color, i, count).rgb).collect();
        Some(TerrainLegend { color, range, swatches })
    }
}

pub(crate) fn draw(b: &mut Builder, ctx: &Ctx, color: Ink, lw: f32, face: &cadcraft_doc::Face3d) -> bool {
    let Some(display) = b.terrain.filter(|d| ctx.depth == 0 && d.members.contains(&ctx.top)) else { return false };
    let [a, c, d, _] = face.corners;
    let triangle = [a, c, d];
    let style = display.style;
    let original_transparency = b.transparency;
    b.transparency = original_transparency.max(style.transparency);
    let count = usize::from(style.bands.clamp(2, 16));
    match style.color {
        TerrainColor::Drawing => b.tris3(ctx, color, &triangle),
        TerrainColor::Slope => {
            let n = (c - a).cross(d - a);
            let slope = n.x.hypot(n.y).atan2(n.z.abs()) / std::f64::consts::FRAC_PI_2;
            b.tris3(ctx, ink(TerrainColor::Slope, band(slope, count), count), &triangle);
        }
        TerrainColor::Elevation => {
            let span = display.high - display.low;
            if span > 0.0 && span.is_finite() {
                let min = triangle.iter().map(|p| p.z).fold(f64::INFINITY, f64::min);
                let max = triangle.iter().map(|p| p.z).fold(f64::NEG_INFINITY, f64::max);
                let first = band((min - display.low) / span, count);
                let last = band((max - display.low) / span, count);
                for i in first..=last {
                    let level = |j| match j {
                        0 => display.low,
                        j if j == count => display.high,
                        _ => display.low + span * (j as f64 / count as f64),
                    };
                    let mut above = [Vec3::ZERO; 6];
                    let mut slice = [Vec3::ZERO; 6];
                    let polygon = if first == last {
                        &triangle[..]
                    } else {
                        let n = clip(&triangle, level(i), true, &mut above);
                        let n = clip(&above[..n], level(i + 1), false, &mut slice);
                        &slice[..n]
                    };
                    let mut band_triangles = [Vec3::ZERO; 9];
                    let mut corners = 0;
                    for edge in polygon.get(1..).unwrap_or_default().windows(2) {
                        let [p, q, r] = [polygon[0], edge[0], edge[1]];
                        if (q - p).cross(r - p) != Vec3::ZERO {
                            band_triangles[corners..corners + 3].copy_from_slice(&[p, q, r]);
                            corners += 3;
                        }
                    }
                    if corners > 0 {
                        b.tris3(ctx, ink(TerrainColor::Elevation, i, count), &band_triangles[..corners]);
                    }
                }
            } else {
                b.tris3(ctx, ink(TerrainColor::Elevation, 0, count), &triangle);
            }
        }
    }
    b.transparency = original_transparency;
    if style.edges {
        for (a, c) in face.visible_edges() {
            b.polyline3(ctx, color, lw, &[a, c]);
        }
    }
    true
}

fn band(fraction: f64, count: usize) -> usize {
    ((fraction * count as f64) as usize).min(count - 1)
}

fn ink(mode: TerrainColor, band: usize, count: usize) -> Ink {
    let ramp = match mode {
        TerrainColor::Slope => [Rgb(44, 129, 118), Rgb(167, 177, 85), Rgb(221, 145, 62), Rgb(184, 69, 81)],
        _ => [Rgb(51, 99, 174), Rgb(45, 158, 168), Rgb(122, 180, 102), Rgb(234, 181, 92)],
    };
    let t = band as f64 * 3.0 / (count - 1) as f64;
    let i = (t as usize).min(2);
    let fraction = t - i as f64;
    let (Rgb(r0, g0, b0), Rgb(r1, g1, b1)) = (ramp[i], ramp[i + 1]);
    let mix = |a, b| (f64::from(a) + (f64::from(b) - f64::from(a)) * fraction).round() as u8;
    Ink { rgb: Rgb(mix(r0, r1), mix(g0, g1), mix(b0, b1)), aci7: false, aci: 0 }
}

fn clip(input: &[Vec3], z: f64, above: bool, out: &mut [Vec3; 6]) -> usize {
    let Some(mut a) = input.last().copied() else { return 0 };
    let inside = |p: Vec3| if above { p.z >= z } else { p.z <= z };
    let mut n = 0;
    for &b in input {
        // Only strict crossings need an extra vertex. An endpoint on the plane is kept itself.
        if (a.z < z && b.z > z) || (a.z > z && b.z < z) {
            let (low, high) = if a.z < b.z { (a, b) } else { (b, a) };
            let Vec2 { x, y } = low.xy().lerp(high.xy(), (z - low.z) / (high.z - low.z));
            out[n] = Vec3::new(x, y, z);
            n += 1;
        }
        if inside(b) {
            out[n] = b;
            n += 1;
        }
        a = b;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Kind, build};
    use cadcraft_doc::{Common, Face3d, Group, Space, TIN_GROUP_DESCRIPTION};

    fn face(d: &mut Drawing, [a, b, c]: [Vec3; 3]) -> Handle {
        d.add(&Space::Model, Common::default(), EntityKind::Face3d(Face3d { corners: [a, b, c, c], hidden_edges: 0 })).unwrap()
    }

    #[test]
    fn elevation_bands_split_at_the_requested_level_without_changing_the_surface() {
        let mut d = Drawing::new_metric();
        let points = [Vec3::ZERO, Vec3::new(2.0, 0.0, 0.0), Vec3::new(0.0, 2.0, 2.0)];
        let model = face(&mut d, points);
        let unrelated = face(&mut d, points.map(|p| p + Vec3::Z * 100.0));
        d.groups.push(Group { name: "Terrain".into(), description: TIN_GROUP_DESCRIPTION.into(), selectable: true, members: vec![model] });

        let terrain = TerrainStyle { color: TerrainColor::Elevation, bands: 2, transparency: 48, edges: false };
        let list = build(&d, &Space::Model, &Options { spatial: true, terrain, ..Default::default() });
        let legend = list.terrain_legend.as_ref().unwrap();
        let middle = (legend.range[0] + legend.range[1]) * 0.5;
        assert_eq!(legend.range, [0.0, 2.0]);
        let mut area = 0.0;
        let mut colors = HashSet::new();
        for primitive in list.prims.iter().filter(|p| p.handle == model) {
            assert_eq!(primitive.kind, Kind::Tris);
            assert_eq!(primitive.transparency, terrain.transparency);
            colors.insert(primitive.color);
            let p: Vec<_> = list.spatial_points(primitive).collect();
            assert!(p.iter().all(|p| p.z <= middle) || p.iter().all(|p| p.z >= middle));
            assert!(p.iter().any(|p| p.z == middle));
            area += p.as_chunks::<3>().0.iter().map(|p| (p[1] - p[0]).cross(p[2] - p[0]).len() * 0.5).sum::<f64>();
        }
        assert_eq!(colors.len(), 2);
        let original = (points[1] - points[0]).cross(points[2] - points[0]).len() * 0.5;
        assert!((area - original).abs() < 1e-12);
        assert!(list.prims.iter().filter(|p| p.handle == unrelated).all(|p| p.transparency == 0));
    }

    #[test]
    fn slope_colors_follow_the_steepness_and_ignore_triangle_winding() {
        let mut d = Drawing::new_metric();
        let a = Vec3::ZERO;
        let b = Vec3::new(1.0, 0.0, 0.0);
        let c = Vec3::new(0.0, 1.0, 8.0);
        let slope = face(&mut d, [a, b, c]);
        let reverse = face(&mut d, [a, c, b]);
        let flat = face(&mut d, [a, b, Vec3::new(0.0, 1.0, 0.0)]);
        d.groups.push(Group {
            name: "Terrain".into(),
            description: TIN_GROUP_DESCRIPTION.into(),
            selectable: true,
            members: vec![slope, reverse, flat],
        });
        let terrain = TerrainStyle { color: TerrainColor::Slope, edges: false, ..Default::default() };
        let list = build(&d, &Space::Model, &Options { spatial: true, terrain, ..Default::default() });
        let color = |h| list.prims.iter().find(|p| p.handle == h).unwrap().color;
        assert_eq!(color(slope), color(reverse));
        assert_ne!(color(slope), color(flat));
    }
}
