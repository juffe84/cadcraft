use cadcraft_doc::{EntityKind, Polyline3d};
use cadcraft_geom::{Vec2, Vec3};
use serde_json::{Value, json};

use super::{MAX_VERTICES, SOURCE_PREFIX, fence, linked_id, model_space};
use crate::cmd::{CommandSpec, bad, helpers::line3, point3_value, str_param};
use crate::{Accept, Input, Interactive, Prompt, Result, Session, Step};

pub(crate) const DRAW_SOURCE: &str = "tin.source.draw";
pub(crate) const REMOVE_CROSSED: &str = "tin.removecrossed";
pub(crate) const RESTORE_CROSSED: &str = "tin.restorecrossed";

#[derive(Clone, Copy)]
enum PathTool {
    Draw,
    Remove,
    Restore,
}

impl PathTool {
    fn id(self) -> &'static str {
        match self {
            Self::Draw => DRAW_SOURCE,
            Self::Remove => REMOVE_CROSSED,
            Self::Restore => RESTORE_CROSSED,
        }
    }

    fn validate_group(self, s: &Session, group: &str) -> Result<()> {
        let doc = s.doc()?;
        let g = doc.groups.iter().find(|g| g.name.eq_ignore_ascii_case(group));
        if g.is_some_and(|g| match self {
            Self::Draw | Self::Restore => g.linked_terrain_id().is_some(),
            Self::Remove => {
                !g.members.is_empty() && g.members.iter().all(|&h| doc.model.get(h).is_some_and(|e| super::super::triangle(&e.kind).is_some()))
            }
        }) {
            Ok(())
        } else {
            Err(bad(self.id(), "Choose a terrain surface for this tool."))
        }
    }

    fn commit(self, s: &mut Session, group: &str, points: Vec<Vec3>) -> Result<Value> {
        if points.len() < 2 || points.len() > MAX_VERTICES || points.iter().any(|p| !valid(*p)) {
            return Err(bad(self.id(), "Specify at least two valid points within the drawing range."));
        }
        match self {
            Self::Draw => draw(s, group, points),
            Self::Remove => fence::remove(s, group, points),
            Self::Restore => fence::restore_crossed(s, group, points),
        }
    }

    fn run(self, s: &mut Session, p: &Value) -> Result<Value> {
        let group = str_param(p, "group").ok_or_else(|| bad(self.id(), "Choose a terrain surface."))?;
        let points: Vec<Vec3> = p
            .get("points")
            .and_then(Value::as_array)
            .filter(|a| a.len() <= MAX_VERTICES)
            .and_then(|a| a.iter().map(point3_value).collect())
            .ok_or_else(|| bad(self.id(), "Specify the path points as coordinates."))?;
        self.commit(s, group, points)
    }
}

pub(super) fn draw_spec() -> CommandSpec {
    CommandSpec::new(DRAW_SOURCE, "Draw Source Line", |s, p| PathTool::Draw.run(s, p))
        .params("{group: linked terrain name, points: [[x,y,z], ...]}")
        .enabled(model_space)
        .interactive(|_| Ok(Box::new(Draft::new(PathTool::Draw))))
}

pub(super) fn remove_spec() -> CommandSpec {
    CommandSpec::new(REMOVE_CROSSED, "Remove Triangles Along Line", |s, p| PathTool::Remove.run(s, p))
        .params("{group: terrain name, points: [[x,y], ...]}")
        .enabled(model_space)
        .interactive(|_| Ok(Box::new(Draft::new(PathTool::Remove))))
}

pub(super) fn restore_spec() -> CommandSpec {
    CommandSpec::new(RESTORE_CROSSED, "Restore Triangles Along Line", |s, p| PathTool::Restore.run(s, p))
        .params("{group: linked terrain name, points: [[x,y], ...]}")
        .enabled(model_space)
        .interactive(|_| Ok(Box::new(Draft::new(PathTool::Restore))))
}

fn valid(p: Vec3) -> bool {
    p.is_finite() && p.x.abs().max(p.y.abs()).max(p.z.abs()) <= 1e12
}

fn draw(s: &mut Session, group: &str, points: Vec<Vec3>) -> Result<Value> {
    let rejected = || bad(DRAW_SOURCE, "Choose a linked terrain source group.");
    if points.windows(2).any(|pts| pts[0] == pts[1]) {
        return Err(bad(DRAW_SOURCE, "Consecutive source points must be distinct."));
    }
    let doc = s.doc()?;
    let id = linked_id(doc, &json!({"group":group}), DRAW_SOURCE)?;
    let i = doc.groups.iter().position(|g| g.description == format!("{SOURCE_PREFIX}{id}")).ok_or_else(rejected)?;
    let handle = s.add_entity(EntityKind::Polyline3d(Polyline3d { points, closed: false }))?;
    s.doc_mut()?.groups[i].members.push(handle);
    Ok(json!({"handle": handle.hex()}))
}

struct Draft {
    tool: PathTool,
    group: Option<String>,
    points: Vec<Vec3>,
}

impl Draft {
    fn new(tool: PathTool) -> Self {
        Self { tool, group: None, points: Vec::new() }
    }
}

impl Interactive for Draft {
    fn name(&self) -> &'static str {
        match self.tool {
            PathTool::Draw => "TIN.SOURCE.DRAW",
            PathTool::Remove => "TIN.REMOVECROSSED",
            PathTool::Restore => "TIN.RESTORECROSSED",
        }
    }

    fn prompt(&self, _s: &Session) -> Prompt {
        if self.group.is_none() {
            return Prompt::new("Enter terrain surface name", Accept::TEXT);
        }
        let message = match (self.tool, self.points.len()) {
            (PathTool::Draw, 0) => "Specify start point of source line (X,Y,Z)",
            (PathTool::Draw, _) => "Specify next source point (X,Y,Z) or press Enter to finish",
            (PathTool::Remove, 0) => "Specify start point of removal line",
            (PathTool::Remove, _) => "Specify next removal point or press Enter to remove crossed triangles",
            (PathTool::Restore, 0) => "Specify start point of restoration line",
            (PathTool::Restore, _) => "Specify next restoration point or press Enter to restore crossed triangles",
        };
        let p = Prompt::new(message, Accept::POINT).base3_opt(self.points.last().copied());
        if self.points.is_empty() { p } else { p.kw(&["Undo", "Clear"]) }
    }

    fn input(&mut self, s: &mut Session, i: Input) -> Result<Step> {
        match i {
            Input::Text(group) if self.group.is_none() => {
                self.tool.validate_group(s, &group)?;
                self.group = Some(group);
            }
            Input::Point(p) if self.group.is_some() => {
                let p = s.point_position(p);
                if !valid(p) || self.points.len() == MAX_VERTICES {
                    return Err(bad(self.tool.id(), "The path point exceeds the drawing limits."));
                }
                if self.points.last() != Some(&p) {
                    self.points.push(p);
                }
            }
            Input::Keyword(k) if k == "Undo" => {
                self.points.pop();
                if let Some(&p) = self.points.last() {
                    s.note_point3(p);
                }
            }
            Input::Keyword(k) if k == "Clear" => self.points.clear(),
            Input::Enter if let Some(group) = &self.group => {
                let repeated = !matches!(self.tool, PathTool::Draw);
                if repeated && self.points.is_empty() {
                    return Ok(Step::Done);
                }
                let result = self.tool.commit(s, group, self.points.clone())?;
                if let Some(removed) = result["removed"].as_u64() {
                    s.echo(format!("Removed {removed} crossed triangles."));
                }
                if let Some(restored) = result["restored"].as_u64() {
                    s.echo(format!("Restored {restored} crossed triangles."));
                }
                self.points.clear();
                return Ok(if repeated { Step::Checkpoint } else { Step::Done });
            }
            _ => {}
        }
        Ok(Step::Continue)
    }

    fn preview(&self, s: &Session, at: Vec2) -> Vec<EntityKind> {
        let z = self.points.last().map_or(0.0, |p| p.z);
        self.preview_3d(s, at.to3(z))
    }

    fn preview_3d(&self, _s: &Session, p: Vec3) -> Vec<EntityKind> {
        let Some(&last) = self.points.last() else { return Vec::new() };
        let mut preview = vec![line3(last, p)];
        if self.points.len() >= 2 {
            preview.push(EntityKind::Polyline3d(Polyline3d { points: self.points.clone(), closed: false }));
        }
        preview
    }
}
