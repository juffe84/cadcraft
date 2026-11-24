//! Copy a point-cloud selection as CAD POINT entities at their original positions.
use crate::{Accept, Input, Interactive, Prompt, Result, Session, Step};
use serde_json::Value;

use super::super::{bad, point3_value};

pub(crate) const COPY_ID: &str = "pointcloud.copybase";

pub(crate) fn enabled(s: &Session) -> std::result::Result<(), String> {
    let state = s.state().map_err(|e| e.to_string())?;
    #[cfg(not(target_arch = "wasm32"))]
    if native::READER.get().is_some() && state.cloud_file.is_some() && state.doc.point_cloud.is_some() {
        if state.space == cadcraft_doc::Space::Model && state.block_editor().is_none() && state.cloud_selection.is_some() {
            return Ok(());
        }
        return Err("Select point cloud points before copying.".into());
    }
    let _ = state;
    Err("Copying cloud points requires a LAS or LAZ file opened from disk.".into())
}

pub(crate) struct BasePointPrompt;

impl Interactive for BasePointPrompt {
    fn name(&self) -> &'static str {
        "COPY CLOUD POINTS"
    }

    fn prompt(&self, _s: &Session) -> Prompt {
        Prompt::new("Specify base point (X,Y,Z)", Accept::POINT)
    }

    fn input(&mut self, _s: &mut Session, input: Input) -> Result<Step> {
        Ok(if matches!(input, Input::Point(_)) { Step::Done } else { Step::Continue })
    }
}

pub(crate) fn run(s: &mut Session, params: &Value) -> Result<Value> {
    let region = s.state()?.cloud_selection.ok_or_else(|| bad(COPY_ID, "select point cloud points before copying"))?;
    let base = params.get("base").and_then(point3_value).ok_or_else(|| bad(COPY_ID, "base must be an [x,y,z] point"))?;
    #[cfg(not(target_arch = "wasm32"))]
    {
        let copied = prepare(s, region, base)?.read()?.apply(s)?;
        Ok(serde_json::json!({ "copied": copied }))
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (region, base);
        Err(bad(COPY_ID, "LAZ point copying requires the desktop app or CLI"))
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::{ReadyCopy, prepare, set_reader};

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::{COPY_ID, Result, Session, bad};
    use crate::cmd::clipboard;
    use cadcraft_doc::point_cloud::CloudRegion;
    use cadcraft_doc::{Drawing, Entity, EntityKind, Point};
    use cadcraft_geom::Vec3;
    use std::{
        fs::File,
        io::BufReader,
        sync::{Arc, OnceLock},
    };

    type Reader = fn(BufReader<File>, CloudRegion, &[CloudRegion], usize) -> std::result::Result<Vec<Vec3>, String>;
    pub(super) static READER: OnceLock<Reader> = OnceLock::new();

    pub fn set_reader(reader: Reader) {
        let _ = READER.set(reader);
    }

    pub struct Job {
        file: super::super::cloud_export::CloudFile,
        region: CloudRegion,
        base: Vec3,
        source: Arc<Drawing>,
        uid: u64,
        previous_clipboard: Option<u64>,
        reader: Reader,
        roots: Option<Arc<crate::file_access::AutomationRoots>>,
    }

    pub fn prepare(s: &Session, region: CloudRegion, base: Vec3) -> Result<Job> {
        let state = s.state()?;
        let file = state.cloud_file.clone().ok_or_else(|| bad(COPY_ID, "open the source LAS or LAZ file from disk first"))?;
        let reader = READER.get().copied().ok_or_else(|| bad(COPY_ID, "LAZ point copying is unavailable"))?;
        if !region.valid() || !base.is_finite() || base.x.abs().max(base.y.abs()).max(base.z.abs()) > 1.0e12 {
            return Err(bad(COPY_ID, "the selected region or base is outside the drawing range"));
        }
        Ok(Job {
            file,
            region,
            base,
            source: state.doc.clone(),
            uid: state.uid,
            previous_clipboard: s.clipboard_source.as_ref().map(|c| c.serial),
            reader,
            roots: crate::file_access::current(),
        })
    }

    pub struct ReadyCopy {
        objects: Vec<Entity>,
        base: Vec3,
        source: Arc<Drawing>,
        uid: u64,
        previous_clipboard: Option<u64>,
    }

    impl Job {
        pub fn read(self) -> Result<ReadyCopy> {
            let points = crate::file_access::confine(self.roots.as_ref(), || {
                let source = self.file.open().map_err(|e| bad(COPY_ID, e))?;
                let input = BufReader::new(source.try_clone().map_err(|e| bad(COPY_ID, e.to_string()))?);
                let points = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    (self.reader)(input, self.region, &self.source.point_cloud_removals, clipboard::MAX_SYSTEM_OBJECTS)
                }))
                .map_err(|_| bad(COPY_ID, "could not read the selected points"))?
                .map_err(|e| bad(COPY_ID, e))?;
                self.file.check(&source).map_err(|e| bad(COPY_ID, e.to_string()))?;
                Ok::<_, crate::EngineError>(points)
            })?;
            if points.is_empty() {
                return Err(bad(COPY_ID, "there are no points in the selected area"));
            }
            let objects = points.into_iter().map(|p| Entity::new(cadcraft_doc::Handle(0), EntityKind::Point(Point { p, angle: 0.0 }))).collect();
            Ok(ReadyCopy { objects, base: self.base, source: self.source, uid: self.uid, previous_clipboard: self.previous_clipboard })
        }
    }

    impl ReadyCopy {
        pub fn apply(self, s: &mut Session) -> Result<usize> {
            if s.clipboard_source.as_ref().map(|c| c.serial) != self.previous_clipboard {
                return Err(bad(COPY_ID, "the clipboard changed while the selection was being read"));
            }
            let count = self.objects.len();
            clipboard::copy_from(s, &self.source, self.uid, self.objects, self.base);
            Ok(count)
        }
    }
}
