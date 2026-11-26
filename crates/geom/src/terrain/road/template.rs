//! Resolve numbered points in a road cross section and join corresponding points along the road.
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{
    Crossfall, ReferenceInput, ReferencePoint, RoadSections, SectionReference, Triangle, Width, WidthQuery, check_overlap, indexed,
    reference::ReferenceQuery, section, signed_area2, slope, vertex_normal,
};
use crate::{Vec2, Vec3, line_line_infinite};

pub struct Rule<'a> {
    pub number: u32,
    pub from: u32,
    pub from_layer: Option<usize>,
    pub from_reference: Option<ReferencePoint>,
    pub stations: Option<[f64; 2]>,
    pub offset: f64,
    pub height: f64,
    pub crossfall: [f64; 2],
    pub edges: Option<&'a [[Vec2; 2]]>,
    pub extension: Option<Extension>,
    pub helper: bool,
}

pub struct Extension {
    pub direction: ExtensionDirection,
    pub target: ExtensionTarget,
}

#[derive(Clone, Copy)]
pub enum ExtensionTarget {
    Segment { points: [u32; 2], layer: Option<usize> },
    Layer { index: usize, side: ExtensionSide },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ExtensionSide {
    Left,
    Right,
}

#[derive(Clone, Copy)]
pub enum ExtensionDirection {
    Through(u32),
    Grade(f64),
}

impl ExtensionDirection {
    fn through(self) -> Option<u32> {
        match self {
            Self::Through(number) => Some(number),
            Self::Grade(_) => None,
        }
    }
}

impl Rule<'_> {
    fn valid_extension(&self, earlier: usize) -> bool {
        self.extension.as_ref().is_none_or(|extension| {
            let direction_valid = match extension.direction {
                ExtensionDirection::Through(through) => {
                    self.from_reference.is_none() && through != self.from && (self.from_layer.is_some() || through != self.number)
                }
                ExtensionDirection::Grade(grade) => grade.is_finite(),
            };
            let target_valid = match extension.target {
                ExtensionTarget::Segment { points, layer } => {
                    points[0] != points[1] && layer.is_none_or(|index| index < earlier) && (layer.is_some() || !points.contains(&self.number))
                }
                ExtensionTarget::Layer { index, .. } => index < earlier && matches!(extension.direction, ExtensionDirection::Grade(_)),
            };
            direction_valid && target_valid && self.edges.is_none() && self.offset == 0.0 && self.height == 0.0 && self.crossfall == [0.0; 2]
        })
    }

    fn references_ready(&self, row: &Row, earlier: &[Row]) -> bool {
        let source = self.from_layer.map_or(row, |i| &earlier[i]);
        (self.from_reference.is_some() || source.nodes.contains_key(&self.from))
            && self.extension.as_ref().is_none_or(|extension| {
                extension.direction.through().is_none_or(|number| source.nodes.contains_key(&number))
                    && match extension.target {
                        ExtensionTarget::Segment { points, layer } => {
                            points.iter().all(|number| layer.map_or(row, |i| &earlier[i]).nodes.contains_key(number))
                        }
                        ExtensionTarget::Layer { .. } => true,
                    }
            })
    }

    fn references_clipped(&self, row: &Row, earlier: &[Row], references: &BTreeMap<u32, SectionReference>) -> bool {
        let source = self.from_layer.map_or(row, |i| &earlier[i]);
        let clipped_source = if self.from_reference.is_some() { !references.contains_key(&self.from) } else { source.clipped.contains(&self.from) };
        clipped_source
            || self.extension.as_ref().is_some_and(|extension| {
                extension.direction.through().is_some_and(|number| source.clipped.contains(&number))
                    || match extension.target {
                        ExtensionTarget::Segment { points, layer } => {
                            points.iter().any(|number| layer.map_or(row, |i| &earlier[i]).clipped.contains(number))
                        }
                        ExtensionTarget::Layer { .. } => false,
                    }
            })
    }

    fn extended_point(&self, extension: &Extension, start: Vec2, row: &Row, earlier: &[Row]) -> Option<Vec2> {
        let source = self.from_layer.map_or(row, |i| &earlier[i]);
        match extension.target {
            ExtensionTarget::Segment { points, layer } => {
                let target = points.map(|number| layer.map_or(row, |i| &earlier[i]).nodes[&number].point());
                match extension.direction {
                    ExtensionDirection::Through(through) => section_extension_hit([start, source.nodes[&through].point()], target),
                    ExtensionDirection::Grade(grade) => section_grade_hit(start, grade, target),
                }
            }
            ExtensionTarget::Layer { index, side } => {
                let ExtensionDirection::Grade(grade) = extension.direction else { return None };
                let row = &earlier[index];
                if !row.missed_extensions.is_empty() {
                    return None;
                }
                let mut points: Vec<_> = row.nodes.values().filter(|node| !node.helper).map(|node| node.point()).collect();
                points.sort_by(|a, b| a.x.total_cmp(&b.x));
                section_layer_grade_hit(start, grade, side, points.windows(2).map(|pair| [pair[0], pair[1]]))
            }
        }
    }
}

/// Continue past the second source point until the ray reaches the finite target segment.
/// Both pairs use section offset and elevation as their coordinates.
pub fn section_extension_hit([start, end]: [Vec2; 2], [first, last]: [Vec2; 2]) -> Option<Vec2> {
    target_hit(start, end - start, [first, last], 1.0)
}

/// Reach the target at a signed rise per horizontal run, choosing the nearest valid side.
pub fn section_grade_hit(start: Vec2, grade: f64, target: [Vec2; 2]) -> Option<Vec2> {
    [-1.0, 1.0]
        .into_iter()
        .filter_map(|side| target_hit(start, Vec2::new(side, grade * 0.01), target, 0.0))
        .min_by(|left, right| left.dist(start).total_cmp(&right.dist(start)))
}

pub fn section_layer_grade_hit(start: Vec2, grade: f64, side: ExtensionSide, segments: impl IntoIterator<Item = [Vec2; 2]>) -> Option<Vec2> {
    let sign = match side {
        ExtensionSide::Left => -1.0,
        ExtensionSide::Right => 1.0,
    };
    segments
        .into_iter()
        .filter_map(|segment| target_hit(start, Vec2::new(sign, grade * 0.01), segment, 0.0))
        .min_by(|left, right| left.dist(start).total_cmp(&right.dist(start)))
}

fn target_hit(start: Vec2, direction: Vec2, [first, last]: [Vec2; 2], minimum: f64) -> Option<Vec2> {
    let (_, ray, segment) = line_line_infinite(Vec2::ZERO, direction, first - start, last - start)?;
    const TOLERANCE: f64 = 1e-9;
    if !(ray.is_finite() && ray >= minimum - TOLERANCE && (-TOLERANCE..=1.0 + TOLERANCE).contains(&segment)) {
        return None;
    }
    // Snap a hit within the tolerance of a target endpoint to the endpoint itself.
    Some(first + (last - first) * segment.clamp(0.0, 1.0))
}

pub struct Template<'a> {
    pub name: &'a str,
    pub thickness: f64,
    pub inherit: bool,
    pub rules: Vec<Rule<'a>>,
}

struct PreparedRule<'a> {
    rule: Rule<'a>,
    edge: Option<WidthQuery>,
}

impl PreparedRule<'_> {
    fn offset(&self, xy: Vec2, normal: Vec2, layer: &str, station: f64) -> Result<Option<f64>, String> {
        let Some(edge) = &self.edge else { return Ok(Some(self.rule.offset)) };
        let on_side = |sign| edge.at(xy, -normal * sign).map(|width| sign * width);
        let width = if self.rule.offset == 0.0 {
            match (on_side(-1.0), on_side(1.0)) {
                (Some(_), Some(_)) => {
                    return Err(format!(
                        "Selected edges for line {} on {layer} intersect both sides at station {station:.3}. Set a negative side for left or a positive side for right.",
                        self.rule.number
                    ));
                }
                (left, right) => left.or(right),
            }
        } else {
            on_side(self.rule.offset.signum())
        };
        Ok(width)
    }
}

struct PreparedLayer<'a> {
    name: &'a str,
    thickness: f64,
    inherit: bool,
    rules: Vec<PreparedRule<'a>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct SectionNode {
    pub number: u32,
    pub offset: f64,
    pub elevation: f64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub helper: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl SectionNode {
    pub fn point(self) -> Vec2 {
        Vec2::new(self.offset, self.elevation)
    }

    fn world(self, [center, normal]: [Vec3; 2]) -> Vec3 {
        (center.xy() - normal.xy() * self.offset).to3(self.elevation)
    }
}
#[derive(Deserialize, Serialize)]
pub struct SectionLayer {
    pub name: String,
    pub nodes: Vec<SectionNode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clipped: Vec<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missed_extensions: Vec<u32>,
}

#[derive(Deserialize, Serialize)]
pub struct Section {
    pub layers: Vec<SectionLayer>,
    pub origin: Vec3,
    pub right: Vec3,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<SectionReference>,
}

#[derive(Default)]
pub struct Mesh {
    pub triangles: Vec<Triangle>,
    pub lines: BTreeMap<u32, Vec<Vec<Vec3>>>,
    pub incomplete: bool,
}

pub struct Sampler<'a> {
    axis: &'a [(f64, Vec3)],
    normals: Vec<Vec2>,
    limits: [f64; 2],
    interval: f64,
    centerline_number: u32,
    crossfall: &'a [Crossfall],
    widths: Option<[WidthQuery; 2]>,
    layers: Vec<PreparedLayer<'a>>,
    references: Vec<ReferenceQuery<'a>>,
    mesh_rules: BTreeSet<(usize, u32)>,
}

#[derive(Default)]
struct Row {
    nodes: BTreeMap<u32, SectionNode>,
    clipped: BTreeSet<u32>,
    missed_extensions: Vec<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RowPurpose {
    Section,
    PreviewMesh,
    Build,
}

fn required_rules(layers: &[PreparedLayer<'_>]) -> BTreeSet<(usize, u32)> {
    let mut by_number = BTreeMap::<_, Vec<_>>::new();
    let mut pending = Vec::new();
    for (index, layer) in layers.iter().enumerate() {
        for prepared in &layer.rules {
            let rule = &prepared.rule;
            let id = (index, rule.number);
            by_number.entry(id).or_default().push(rule);
            if !rule.helper {
                pending.push(id);
            }
        }
    }
    let mut required = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !required.insert(id) {
            continue;
        }
        for rule in by_number.get(&id).into_iter().flatten() {
            let source_layer = rule.from_layer.unwrap_or(id.0);
            if rule.from_reference.is_none() {
                pending.push((source_layer, rule.from));
            }
            if let Some(extension) = &rule.extension {
                pending.extend(extension.direction.through().map(|point| (source_layer, point)));
                if let ExtensionTarget::Segment { points, layer } = extension.target {
                    pending.extend(points.map(|point| (layer.unwrap_or(id.0), point)));
                }
            }
        }
    }
    required
}

impl<'a> Sampler<'a> {
    pub fn new(
        input: RoadSections<'a>,
        templates: impl IntoIterator<Item = Template<'a>>,
        centerline_number: u32,
        sampled_alignment: bool,
        references: impl IntoIterator<Item = ReferenceInput<'a>>,
    ) -> Result<Self, String> {
        let RoadSections { axis, limits: [from, to], interval, left, right, crossfall } = input;
        if centerline_number > 1 {
            return Err("The centerline number must be 0 or 1.".into());
        }
        if axis.len() < 2
            || axis.iter().any(|(station, p)| !station.is_finite() || !p.is_finite())
            || axis.windows(2).any(|v| v[0].0 >= v[1].0 || v[0].1.xy() == v[1].1.xy())
        {
            return Err("The road centerline must advance at every point.".into());
        }
        if !(from >= axis[0].0 - 1e-8 && to <= axis[axis.len() - 1].0 + 1e-8 && from < to && interval.is_finite() && interval > 0.0) {
            return Err("Set increasing start and end stations within the centerline and a positive section interval.".into());
        }
        let limits = [from.max(axis[0].0), to.min(axis[axis.len() - 1].0)];
        if limits[0] >= limits[1] {
            return Err("The road range must have a positive length.".into());
        }
        let mut layers = Vec::new();
        for template in templates {
            if !template.thickness.is_finite() || template.thickness < 0.0 {
                return Err("The vertical thickness must be finite and non-negative.".into());
            }
            let mut rules = Vec::new();
            for rule in template.rules {
                if rule.number == 0
                    || (layers.is_empty() && rule.number == centerline_number)
                    || rule.from_layer.is_some_and(|i| i >= layers.len())
                    || rule.from_layer.is_none() && rule.number == rule.from
                    || rule.from_reference.is_some() && rule.from_layer.is_some()
                    || !rule.offset.is_finite()
                    || !rule.height.is_finite()
                    || rule.crossfall.iter().any(|s| !s.is_finite() || s.abs() > 100.0)
                    || rule.stations.is_some_and(|[a, b]| !a.is_finite() || !b.is_finite() || a >= b)
                {
                    return Err(format!(
                        "Invalid instruction for line {} on {}. Use a positive line number, an earlier layer or another point, and a valid station range.",
                        rule.number, template.name
                    ));
                }
                if !rule.valid_extension(layers.len()) {
                    return Err(format!(
                        "Invalid extension for line {} on {}. Use a grade or a second source point, and two distinct target points.",
                        rule.number, template.name
                    ));
                }
                let edge = rule.edges.map(|edges| WidthQuery::new(Width::FromEdges(edges))).transpose()?;
                rules.push(PreparedRule { rule, edge });
            }
            layers.push(PreparedLayer { name: template.name, thickness: template.thickness, inherit: template.inherit, rules });
        }
        let Some(top) = layers.first() else { return Err("Add a top road section.".into()) };
        let widths = if top.inherit {
            if crossfall.is_empty()
                || crossfall
                    .iter()
                    .any(|v| !v.station.is_finite() || !v.left.is_finite() || !v.right.is_finite() || v.left.abs().max(v.right.abs()) > 100.0)
                || crossfall.windows(2).any(|pair| pair[0].station >= pair[1].station)
            {
                return Err("Use increasing crossfall stations and percentages between -100 and 100.".into());
            }
            Some([WidthQuery::new(left)?, WidthQuery::new(right)?])
        } else {
            None
        };
        let normals = (0..axis.len()).map(|index| vertex_normal(axis, index, sampled_alignment)).collect();
        let mesh_rules = required_rules(&layers);
        Self { axis, normals, limits, interval, centerline_number, crossfall, widths, layers, references: Vec::new(), mesh_rules }
            .with_references(references)
    }

    fn with_references(mut self, references: impl IntoIterator<Item = ReferenceInput<'a>>) -> Result<Self, String> {
        let rules = || self.layers.iter().flat_map(|layer| layer.rules.iter().map(|rule| &rule.rule));
        let reserved = self.centerline_number + if self.widths.is_some() { 2 } else { 0 };
        let mut numbers = BTreeSet::new();
        for reference in references {
            let number = reference.number;
            if number <= reserved || !numbers.insert(number) || rules().any(|rule| rule.number == number) {
                return Err(format!("Reference number {number} must be unique and distinct from the road line numbers."));
            }
            let used = self.surface_rules().any(|rule| rule.rule.from_reference.is_some() && rule.rule.from == number);
            self.references.push(ReferenceQuery::new(reference, used)?);
        }
        for rule in rules() {
            if let Some(part) = rule.from_reference {
                let reference = self
                    .references
                    .iter()
                    .find(|reference| reference.number == rule.from)
                    .ok_or_else(|| format!("Line {} uses missing reference {}.", rule.number, rule.from))?;
                if part != ReferencePoint::Line && reference.pipe.is_none() {
                    return Err(format!("Line {} uses a pipe point on reference {}, which has no pipe.", rule.number, rule.from));
                }
            }
        }
        Ok(self)
    }

    pub fn preview(&self, station: f64) -> Result<Section, String> {
        if !station.is_finite() || station < self.limits[0] - 1e-8 || station > self.limits[1] + 1e-8 {
            return Err("The section station is outside the road range.".into());
        }
        let at = station.clamp(self.limits[0], self.limits[1]);
        let [center, normal] = self.frame(at);
        let references = self.reference_hits(center, normal, true);
        let layers = self
            .rows_with_references(at, at, center, normal, &references, RowPurpose::Section)?
            .into_iter()
            .zip(&self.layers)
            .map(|(row, layer)| {
                let mut nodes: Vec<_> = row.nodes.into_values().collect();
                nodes.sort_by(|a, b| a.offset.total_cmp(&b.offset));
                let mut missed_extensions = row.missed_extensions;
                missed_extensions.sort_unstable();
                SectionLayer { name: layer.name.into(), nodes, clipped: row.clipped.into_iter().collect(), missed_extensions }
            })
            .collect();
        Ok(Section { layers, origin: center, right: -normal, references: references.into_values().collect() })
    }

    fn frame(&self, station: f64) -> [Vec3; 2] {
        let (center, normal) = section(self.axis, &self.normals, station);
        [center, normal.to3(0.0)]
    }

    fn rows(&self, station: f64, scope: f64, center: Vec3, normal: Vec3, purpose: RowPurpose) -> Result<Vec<Row>, String> {
        let references = self.reference_hits(center, normal, false);
        self.rows_with_references(station, scope, center, normal, &references, purpose)
    }

    fn reference_hits(&self, center: Vec3, normal: Vec3, show_all: bool) -> BTreeMap<u32, SectionReference> {
        self.references
            .iter()
            .filter(|reference| show_all || reference.used)
            .filter_map(|reference| reference.at(center.xy(), normal.xy()).map(|hit| (reference.number, hit)))
            .collect()
    }

    fn rows_with_references(
        &self,
        station: f64,
        scope: f64,
        center: Vec3,
        normal: Vec3,
        references: &BTreeMap<u32, SectionReference>,
        purpose: RowPurpose,
    ) -> Result<Vec<Row>, String> {
        let mut result: Vec<Row> = Vec::with_capacity(self.layers.len());
        for (layer_index, layer) in self.layers.iter().enumerate() {
            let available = |rule: &PreparedRule<'_>| purpose == RowPurpose::Section || self.mesh_rules.contains(&(layer_index, rule.rule.number));
            let active: BTreeMap<_, _> = layer
                .rules
                .iter()
                .filter(|r| available(r) && r.rule.stations.is_none_or(|[a, b]| a <= scope && scope <= b))
                .map(|r| (r.rule.number, r))
                .collect();
            let mut row = Row::default();
            // A shorter helper can support a longer instruction. The dependent
            // line stops where the helper is absent from the section.
            row.clipped
                .extend(layer.rules.iter().filter(|r| r.rule.helper && available(r) && !active.contains_key(&r.rule.number)).map(|r| r.rule.number));
            if let Some(upper) = result.last() {
                if layer.inherit {
                    row.clipped.extend(upper.clipped.iter().copied().filter(|number| !active.contains_key(number)));
                }
                for node in upper.nodes.values().filter(|node| !node.helper && (layer.inherit || (self.centerline_number == 0 && node.number == 0))) {
                    if !active.contains_key(&node.number) {
                        row.clipped.remove(&node.number);
                        row.nodes.insert(node.number, SectionNode { elevation: node.elevation - layer.thickness, ..*node });
                    }
                }
            } else {
                row.nodes
                    .insert(self.centerline_number, SectionNode { number: self.centerline_number, offset: 0.0, elevation: center.z, helper: false });
                if let Some(queries) = &self.widths {
                    let grades = slope(self.crossfall, station);
                    for (i, (sign, grade)) in [(-1.0, grades.x), (1.0, grades.y)].into_iter().enumerate() {
                        let number = self.centerline_number + i as u32 + 1;
                        if !active.contains_key(&number) {
                            if let Some(width) = queries[i].at(center.xy(), -normal.xy() * sign) {
                                row.clipped.remove(&number);
                                row.nodes.insert(
                                    number,
                                    SectionNode { number, offset: sign * width, elevation: center.z + grade * width / 100.0, helper: false },
                                );
                            } else {
                                row.clipped.insert(number);
                            }
                        }
                    }
                }
            }

            let mut pending: Vec<_> = active.into_values().collect();
            while !pending.is_empty() {
                let ready = pending.iter().position(|prepared| {
                    prepared.rule.references_ready(&row, &result) || prepared.rule.references_clipped(&row, &result, references)
                });
                let Some(ready) = ready else {
                    let rule = &pending[0].rule;
                    if let Some(ExtensionTarget::Segment { points: [a, b], .. }) = rule.extension.as_ref().map(|extension| extension.target) {
                        return Err(format!(
                            "Line {} on {} has a missing or circular source or target reference for segment {a}–{b} at station {station:.3}.",
                            rule.number, layer.name
                        ));
                    }
                    return Err(format!(
                        "Line {} on {} has a missing or circular reference to line {} at station {station:.3}.",
                        rule.number, layer.name, rule.from
                    ));
                };
                let prepared = pending.swap_remove(ready);
                let rule = &prepared.rule;
                if rule.references_clipped(&row, &result, references) {
                    row.clipped.insert(rule.number);
                    continue;
                }
                let source = if let Some(part) = rule.from_reference {
                    references.get(&rule.from).and_then(|reference| reference.point(part))
                } else {
                    let source = rule.from_layer.map_or(&row, |i| &result[i]).nodes[&rule.from];
                    Some(source.point())
                };
                let Some(source) = source else {
                    row.clipped.insert(rule.number);
                    continue;
                };
                if let Some(extension) = &rule.extension {
                    let Some(hit) = rule.extended_point(extension, source, &row, &result) else {
                        if purpose == RowPurpose::Build {
                            let target = match extension.target {
                                ExtensionTarget::Segment { points: [a, b], .. } => format!("segment {a}–{b}"),
                                ExtensionTarget::Layer { index, .. } => format!("layer {} on the chosen side", self.layers[index].name),
                            };
                            return Err(format!(
                                "Line {} on {} cannot extend from point {} to {target} at station {station:.3}. Check the grade or direction.",
                                rule.number, layer.name, rule.from
                            ));
                        }
                        row.clipped.insert(rule.number);
                        row.missed_extensions.push(rule.number);
                        continue;
                    };
                    row.nodes.insert(rule.number, SectionNode { number: rule.number, offset: hit.x, elevation: hit.y, helper: rule.helper });
                    continue;
                }
                let xy = center.xy() - normal.xy() * source.x;
                let Some(offset) = prepared.offset(xy, normal.xy(), layer.name, station)? else {
                    row.clipped.insert(rule.number);
                    continue;
                };
                let [a, b] = rule.stations.unwrap_or(self.limits);
                let t = ((station - a) / (b - a)).clamp(0.0, 1.0);
                let grade = rule.crossfall[0] + (rule.crossfall[1] - rule.crossfall[0]) * t;
                let elevation = source.y + rule.height + grade * offset.abs() / 100.0;
                row.nodes.insert(rule.number, SectionNode { number: rule.number, offset: source.x + offset, elevation, helper: rule.helper });
            }
            for node in row.nodes.values() {
                let xy = center.xy() - normal.xy() * node.offset;
                if !xy.is_finite() || !node.elevation.is_finite() || xy.x.abs().max(xy.y.abs()).max(node.elevation.abs()) > 1e12 {
                    return Err(format!("Line {} of layer {} is outside the drawing range at station {station:.3}.", node.number, layer.name));
                }
            }
            result.push(row);
        }
        Ok(result)
    }

    fn surface_rules(&self) -> impl Iterator<Item = &PreparedRule<'a>> {
        self.layers
            .iter()
            .enumerate()
            .flat_map(move |(index, layer)| layer.rules.iter().filter(move |rule| self.mesh_rules.contains(&(index, rule.rule.number))))
    }

    fn edge_queries(&self) -> impl Iterator<Item = &WidthQuery> {
        self.widths
            .iter()
            .flatten()
            .chain(self.surface_rules().filter_map(|rule| rule.edge.as_ref()))
            .filter(|query| matches!(query, WidthQuery::Edges(_)))
    }

    fn stations(&self, max_sections: usize) -> Result<Vec<f64>, String> {
        let [from, to] = self.limits;
        let steps = ((to - from) / self.interval).ceil().max(1.0);
        if steps >= max_sections as f64 {
            return Err("The section interval produces too many road sections.".into());
        }
        let mut stations: Vec<_> = (0..steps as usize).map(|n| from + (to - from) * n as f64 / steps).collect();
        stations.extend(self.crossfall.iter().map(|v| v.station).chain(self.axis.iter().map(|(s, _)| *s)).filter(|&s| s > from && s < to));
        let rule_limits = self.surface_rules().filter_map(|r| r.rule.stations).flatten().filter(|&s| s > from && s < to);
        let mut breaks: Vec<_> = rule_limits.collect();
        breaks.extend(self.limits);
        breaks.sort_unstable_by(f64::total_cmp);
        breaks.dedup();
        if self.edge_queries().next().is_some() || self.references.iter().any(|reference| reference.used) {
            let index = indexed(self.axis.windows(2).map(|pair| [pair[0].1.xy(), pair[1].1.xy()]));
            let mut edge_stations = Vec::new();
            for query in self.edge_queries() {
                query.add_stations(self.axis, &self.normals, &index, &mut edge_stations, self.limits);
            }
            for reference in self.references.iter().filter(|reference| reference.used) {
                reference.add_stations(self.axis, &self.normals, &index, &mut edge_stations, self.limits);
            }
            edge_stations.sort_unstable_by(f64::total_cmp);
            edge_stations.dedup_by(|b, a| *b - *a <= 1e-8);
            edge_stations.retain(|&station| {
                let index = breaks.partition_point(|&limit| limit < station - 1e-8);
                !breaks.get(index).is_some_and(|&limit| limit <= station + 1e-8)
            });
            breaks.extend(edge_stations);
            breaks.sort_unstable_by(f64::total_cmp);
        }
        // Keep exact rule boundaries even when another sample is almost coincident.
        stations.retain(|s| {
            let i = breaks.partition_point(|&b| b < *s);
            !breaks.get(i).is_some_and(|&b| b - *s <= 1e-7) && !i.checked_sub(1).is_some_and(|j| *s - breaks[j] <= 1e-7)
        });
        stations.sort_unstable_by(f64::total_cmp);
        stations.dedup_by(|b, a| *b - *a <= 1e-7);
        stations.extend(breaks);
        stations.sort_unstable_by(f64::total_cmp);
        if stations.len() > max_sections {
            return Err("The road has too many sections, including centerline vertices, instruction limits and edge points.".into());
        }
        Ok(stations)
    }

    pub fn mesh(&self, max_sections: usize, max_faces: usize) -> Result<Vec<Mesh>, String> {
        self.make_mesh(max_sections, max_faces, RowPurpose::Build)
    }

    pub fn preview_mesh(&self, max_sections: usize, max_faces: usize) -> Result<Vec<Mesh>, String> {
        self.make_mesh(max_sections, max_faces, RowPurpose::PreviewMesh)
    }

    fn make_mesh(&self, max_sections: usize, max_faces: usize, purpose: RowPurpose) -> Result<Vec<Mesh>, String> {
        let stations = self.stations(max_sections)?;
        let has_external_controls = self.edge_queries().next().is_some() || self.references.iter().any(|reference| reference.used);
        let mut meshes: Vec<_> = self.layers.iter().map(|_| Mesh::default()).collect();
        let mut count = 0;
        for pair in stations.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let scope = a + (b - a) * 0.5;
            let [ca, na] = self.frame(a);
            let [cb, nb] = self.frame(b);
            let rows_a = self.rows(a, scope, ca, na, purpose)?;
            let rows_b = self.rows(b, scope, cb, nb, purpose)?;
            let middle = if has_external_controls {
                let [center, normal] = self.frame(scope);
                Some(self.rows(scope, scope, center, normal, purpose)?)
            } else {
                None
            };
            for (index, ((left, right), (mesh, layer))) in rows_a.iter().zip(&rows_b).zip(meshes.iter_mut().zip(&self.layers)).enumerate() {
                if purpose == RowPurpose::PreviewMesh
                    && [Some(left), Some(right), middle.as_ref().map(|rows| &rows[index])]
                        .into_iter()
                        .flatten()
                        .any(|row| !row.missed_extensions.is_empty())
                {
                    mesh.incomplete = true;
                    continue;
                }
                let mut ordered: Vec<_> = left
                    .nodes
                    .values()
                    .filter(|node| {
                        !node.helper
                            && right.nodes.get(&node.number).is_some_and(|node| !node.helper)
                            && middle.as_ref().is_none_or(|rows| rows[index].nodes.get(&node.number).is_some_and(|node| !node.helper))
                    })
                    .collect();
                for node in ordered.iter().filter(|_| purpose == RowPurpose::Build) {
                    let end = &right.nodes[&node.number];
                    let (start, end) = (node.world([ca, na]), end.world([cb, nb]));
                    let lines = mesh.lines.entry(node.number).or_default();
                    if let Some(last) = lines.last_mut().filter(|line| line.last() == Some(&start)) {
                        last.push(end);
                    } else {
                        lines.push(vec![start, end]);
                    }
                }
                ordered.sort_by(|x, y| x.offset.total_cmp(&y.offset));
                let previous = mesh.triangles.len();
                if let Err(error) = append_strip_faces(&mut mesh.triangles, &ordered, right, [[ca, na], [cb, nb]], layer.name, scope) {
                    if purpose == RowPurpose::Build {
                        return Err(error);
                    }
                    mesh.incomplete = true;
                    mesh.triangles.truncate(previous);
                }
                count += mesh.triangles.len() - previous;
                if count > max_faces {
                    return Err("The road exceeds the triangle limit.".into());
                }
            }
        }
        if purpose == RowPurpose::PreviewMesh {
            return Ok(meshes);
        }
        for (index, (mesh, layer)) in meshes.iter().zip(&self.layers).enumerate() {
            if mesh.triangles.is_empty() {
                if index > 0 && !layer.inherit && layer.rules.iter().all(|rule| rule.rule.helper) {
                    continue;
                }
                return Err(format!("{} needs at least two section points over a station interval.", layer.name));
            }
            check_overlap(&mesh.triangles)?;
        }
        Ok(meshes)
    }
}

fn append_strip_faces(
    triangles: &mut Vec<Triangle>,
    ordered: &[&SectionNode],
    right: &Row,
    [start, end]: [[Vec3; 2]; 2],
    layer: &str,
    station: f64,
) -> Result<(), String> {
    for nodes in ordered.windows(2) {
        let [x, y] = [nodes[0], nodes[1]];
        let (end_x, end_y) = (&right.nodes[&x.number], &right.nodes[&y.number]);
        if end_x.offset > end_y.offset + 1e-8 {
            return Err(format!("Lines {} and {} cross on {layer} around station {station:.3}.", x.number, y.number));
        }
        for (u, v) in [(x, y), (end_x, end_y)] {
            if (u.offset - v.offset).abs() < 1e-8 && (u.elevation - v.elevation).abs() > 1e-6 {
                return Err(format!(
                    "Lines {} and {} have different heights at the same offset on {layer} around station {station:.3}.",
                    u.number, v.number
                ));
            }
        }
        let [a, b, c, d] = [x.world(start), y.world(start), end_y.world(end), end_x.world(end)];
        for face in [[a, b, c], [a, c, d]] {
            let area = signed_area2(face);
            if area < -1e-12 {
                return Err(format!(
                    "Road sections fold between lines {} and {} on {layer} around station {station:.3}. Check the widths and centerline turn.",
                    x.number, y.number
                ));
            }
            if area > 1e-12 {
                triangles.push(face);
            }
        }
    }
    Ok(())
}
