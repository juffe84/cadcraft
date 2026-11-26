//! Per-surface display controls for grouped model-space terrain.
use cadcraft_doc::{
    Drawing, Group, Handle, road,
    terrain::{SOURCE_PREFIX, group_members},
};
use serde_json::{Value, json};

use super::{bad, model_space, str_param};
use crate::{Result, Session, cmd::CommandSpec};

const COMMAND: &str = "tin.surface.visibility";

pub(super) fn spec() -> CommandSpec {
    CommandSpec::new(COMMAND, "Set Terrain Surface Visibility", run)
        .enabled(model_space)
        .params("{group: terrain group name, visible?: bool, only?: bool (show this surface and hide all other surfaces)}")
}

pub(super) fn is_visible(drawing: &Drawing, group: &Group) -> bool {
    if let Some(id) = group.linked_terrain_id() {
        if group.members.is_empty() {
            return road_sources_visible(drawing, id).unwrap_or(true);
        }
        // The separate Show Linked Triangles control can turn faces off while
        // the sources remain visible and editable.
        return group.members.iter().any(|&handle| drawing.model.get(handle).is_some_and(|entity| entity.common.visible));
    }
    group.members.iter().any(|&handle| drawing.model.get(handle).is_some_and(|entity| drawing.is_visible(entity)))
}

pub(super) fn road_sources_visible(drawing: &Drawing, id: &str) -> Option<bool> {
    let mut sources = owned_road_sources(drawing, id);
    let first = sources.next()?;
    Some(std::iter::once(first).chain(sources).any(|handle| drawing.model.get(handle).is_some_and(|entity| entity.common.visible)))
}

fn owned_road_sources(drawing: &Drawing, id: &str) -> impl Iterator<Item = Handle> {
    group_members(drawing, &format!("{SOURCE_PREFIX}{id}"))
        .iter()
        .copied()
        .filter(|&handle| drawing.model.get(handle).is_some_and(|entity| road::metadata(&entity.common, road::EDIT_APP).is_some()))
}

pub(super) fn set(drawing: &mut Drawing, index: usize, visible: bool) {
    let group = &drawing.groups[index];
    let mut handles = group.members.clone();
    if let Some(id) = group.linked_terrain_id() {
        handles.extend(group_members(drawing, &format!("{}{id}", super::linked::FLAG_PREFIX)));
        handles.extend(owned_road_sources(drawing, id));
    }
    if let Some(id) = group.description.strip_prefix(road::SURFACE_PREFIX) {
        let name = format!("{} - Lines", group.name);
        let description = format!("{}{id}", road::STRING_PREFIX);
        if let Some(lines) = drawing.groups.iter().find(|g| g.name == name && g.description == description) {
            handles.extend_from_slice(&lines.members);
        }
        // Generated faces and strings share a layer reserved for this surface.
        let layer = format!("ROAD - {}", group.name);
        if let Some(layer) = drawing.layer_mut(&layer) {
            layer.on = visible;
            if visible {
                layer.frozen = false;
            }
        }
    }
    for handle in handles {
        if visible {
            drawing.hidden_objects.remove(&handle);
        }
        if drawing.model.get(handle).is_some_and(|entity| entity.common.visible != visible) {
            drawing.model.modify(handle, |entity| entity.common.visible = visible);
        }
    }
}

fn run(session: &mut Session, params: &Value) -> Result<Value> {
    let name = str_param(params, "group").ok_or_else(|| bad(COMMAND, "Choose a terrain surface."))?;
    let only = params["only"].as_bool().unwrap_or(false);
    let visible = only || params["visible"].as_bool().ok_or_else(|| bad(COMMAND, "Choose whether to show or hide the surface."))?;
    let doc = session.doc_mut()?;
    let index = doc
        .groups
        .iter()
        .position(|group| group.is_terrain_surface() && group.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| bad(COMMAND, "Unknown terrain surface."))?;
    if only {
        for other in 0..doc.groups.len() {
            if other != index && doc.groups[other].is_terrain_surface() {
                set(doc, other, false);
            }
        }
    }
    set(doc, index, visible);
    let shown = is_visible(doc, &doc.groups[index]);
    let mut result = json!({"group":doc.groups[index].name, "visible":shown});
    if visible && !shown {
        result["message"] = json!("The surface is on a hidden layer.");
    }
    let doc = session.doc()?;
    let selected: Vec<_> =
        session.selection().iter().copied().filter(|&handle| doc.entity(handle).is_some_and(|entity| doc.is_visible(entity))).collect();
    if selected.len() != session.selection().len() {
        session.set_selection(selected);
    }
    Ok(result)
}
