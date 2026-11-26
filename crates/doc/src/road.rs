//! Drawing metadata for road designs and imported alignment stationing.
use serde::{Deserialize, Serialize};

use crate::{Common, Drawing, RawTag};
use cadcraft_geom::terrain::road::{ExtensionSide, Pipe, ReferencePoint};

pub const ALIGNMENT_GROUP: &str = "LandXML alignment";
pub const DESIGN_GROUP: &str = "CADCraft road design";
pub const SURFACE_PREFIX: &str = "CADCraft road TIN ";
pub const STRING_PREFIX: &str = "CADCraft road strings ";
pub const STATION_APP: &str = "CADCRAFT_STATION";
pub const DESIGN_APP: &str = "CADCRAFT_ROAD";
pub const LINE_APP: &str = "CADCRAFT_ROAD_LINE";
pub const EDIT_APP: &str = "CADCRAFT_ROAD_EDIT";
pub const MAX_LAYERS: usize = 12;
pub const MAX_REFERENCES: usize = 64;
pub const MAX_REFERENCE_VERTICES: usize = 50_000;
const MAX_TEXT: usize = 10_000;

pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 90 && !name.chars().any(|c| c.is_control() || "<>/\\\":;?*|,=`".contains(c))
}

#[derive(Serialize, Deserialize)]
pub struct Stationing {
    pub from: f64,
    pub to: f64,
    pub chord_length: f64,
    /// Exact stations at tessellated alignment vertices, when they fit in DXF extended data.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vertices: Vec<f64>,
}

pub fn metadata(common: &Common, app: &str) -> Option<String> {
    let tags = common.xdata.iter().skip_while(|tag| tag.code != 1001 || tag.value != app).skip(1);
    let text: String = tags.take_while(|tag| tag.code == 1000).map(|tag| tag.value.as_str()).collect();
    (!text.is_empty() && text.len() <= MAX_TEXT).then_some(text)
}

pub fn store_metadata(common: &mut Common, app: &str, text: &str) -> bool {
    if text.is_empty() || text.len() > MAX_TEXT {
        return false;
    }
    let mut inside = false;
    common.xdata.retain(|tag| {
        if tag.code == 1001 {
            inside = tag.value == app;
        }
        !inside
    });
    common.xdata.push(RawTag { code: 1001, value: app.into() });
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + 240).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        common.xdata.push(RawTag { code: 1000, value: text[start..end].into() });
        start = end;
    }
    true
}

pub fn register(drawing: &mut Drawing, app: &str) {
    if !drawing.foreign.appids.iter().any(|name| name.eq_ignore_ascii_case(app)) {
        std::sync::Arc::make_mut(&mut drawing.foreign).appids.push(app.into());
    }
}

pub fn line_number(common: &Common) -> Option<u32> {
    metadata(common, LINE_APP)?.parse().ok()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Design {
    pub name: String,
    pub axis: String,
    pub range: [f64; 2],
    pub interval: f64,
    /// Designs saved before the numbered centerline used point 0.
    #[serde(default)]
    pub centerline_number: u32,
    pub left: WidthControl,
    pub right: WidthControl,
    pub crossfall: Vec<cadcraft_geom::terrain::road::Crossfall>,
    #[serde(default, skip_serializing_if = "Template::is_default")]
    pub top: Template,
    #[serde(default)]
    pub layers: Vec<ConstructionLayer>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<ReferenceLine>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceLine {
    pub number: u32,
    pub handle: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipe: Option<Pipe>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WidthControl {
    pub width: f64,
    #[serde(default)]
    pub edges: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConstructionLayer {
    pub name: String,
    /// Vertical thickness below the previous interface.
    pub thickness: f64,
    #[serde(default, skip_serializing_if = "Template::is_default")]
    pub template: Template,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Template {
    /// Use the standard road edges for Top, or copy the section above at a vertical offset.
    pub inherit: bool,
    /// Instructions in authoring order. Later instructions win where station ranges overlap.
    pub rules: Vec<SectionRule>,
}

impl Default for Template {
    fn default() -> Self {
        Self { inherit: true, rules: Vec::new() }
    }
}

impl Template {
    pub fn is_default(&self) -> bool {
        self.inherit && self.rules.is_empty()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SectionRule {
    pub number: u32,
    pub from: u32,
    /// An earlier surface name, or the current surface when absent.
    pub from_layer: Option<String>,
    /// The part of numbered reference `from` to use in place of a surface point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_reference: Option<ReferencePoint>,
    pub stations: Option<[f64; 2]>,
    /// Relative horizontal offset, or the search side for an edge. Zero detects the edge side.
    #[serde(default)]
    pub offset: f64,
    #[serde(default)]
    pub height: f64,
    #[serde(default)]
    pub crossfall: [f64; 2],
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension: Option<SectionExtension>,
    /// Available to other instructions, omitted from surface triangles and output strings.
    #[serde(default, skip_serializing_if = "is_false")]
    pub helper: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Extend to a section segment or meet a layer at a specified grade.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SectionExtension {
    /// The second point on the source layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub through: Option<u32>,
    /// Percentage rise away from the starting point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grade: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<[u32; 2]>,
    /// An earlier surface name, or the current surface when absent.
    pub target_layer: Option<String>,
    /// Search the whole named layer in this direction instead of targeting a pair of points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_side: Option<ExtensionSide>,
}
