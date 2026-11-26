use super::{Arc, Drawing, Handle, Interactive, Serialize, Vec2};

/// An interactive command in progress.
pub struct Running {
    pub id: String,
    pub machine: Box<dyn Interactive>,
    pub before: Arc<Drawing>,
    pub selection_before: Vec<Handle>,
    /// The first point's world height for an orbit command that makes planar objects.
    pub(crate) draft_height: Option<f64>,
}

impl Running {
    pub(crate) fn spatial_drafting(&self) -> bool {
        matches!(
            self.id.as_str(),
            "line"
                | "3dpoly"
                | "point"
                | "point.multiple"
                | "pointcloud.copybase"
                | crate::cmd::terrain::MOVE_VERTEX
                | crate::cmd::terrain::DRAW_SOURCE
                | crate::cmd::terrain::REMOVE_CROSSED
                | crate::cmd::terrain::RESTORE_CROSSED
        )
    }

    pub(crate) fn planar_drafting(&self) -> bool {
        matches!(self.id.as_str(), "pline" | "rectang" | "circle" | "arc" | "polygon" | "ellipse" | "donut")
    }
}

#[derive(Clone, Copy, Debug)]
pub struct OrbitDraftPlane {
    pub height: f64,
    pub locked: bool,
    pub cloud_snap: bool,
}

/// A pending window/crossing selection started by a click on empty space.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct PendingWindow {
    pub corner: Vec2,
    pub during_command: bool,
}
