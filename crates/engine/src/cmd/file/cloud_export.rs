//! Full-resolution LAZ export through the native host's streaming writer.
use crate::{Accept, Input, Interactive, Prompt, Result, Session, Step};
use cadcraft_doc::point_cloud::CloudCopy;
use serde_json::{Value, json};

use super::super::{bad, point3_value, str_param};

const ID: &str = "pointcloud.export";
pub(crate) const COPY_ID: &str = "pointcloud.copybase";

pub(crate) fn enabled(s: &Session) -> std::result::Result<(), String> {
    #[cfg(not(target_arch = "wasm32"))]
    if native::WRITER.get().is_some() && s.state().is_ok_and(|state| state.cloud_file.is_some() && state.doc.point_cloud.is_some()) {
        return Ok(());
    }
    let _ = s;
    Err("LAZ export requires a LAS or LAZ file opened from disk.".into())
}

pub(crate) fn run(s: &mut Session, params: &Value) -> Result<Value> {
    let path = str_param(params, "path").ok_or_else(|| bad(ID, "a .laz output path is required"))?;
    #[cfg(not(target_arch = "wasm32"))]
    return prepare(s, path, None)?.write();
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (s, path, json!(null));
        Err(bad(ID, "LAZ export requires the desktop app or CLI"))
    }
}

pub(crate) fn copy_enabled(s: &Session) -> std::result::Result<(), String> {
    enabled(s)?;
    let state = s.state().map_err(|e| e.to_string())?;
    if state.space == cadcraft_doc::Space::Model && state.block_editor().is_none() && state.cloud_selection.is_some() {
        Ok(())
    } else {
        Err("Select point cloud points before copying.".into())
    }
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

pub(crate) fn run_copy(s: &mut Session, params: &Value) -> Result<Value> {
    let region = s.state()?.cloud_selection.ok_or_else(|| bad(COPY_ID, "select point cloud points before copying"))?;
    let path = str_param(params, "path").ok_or_else(|| bad(COPY_ID, "a .laz output path is required"))?;
    let base = params
        .get("base")
        .and_then(point3_value)
        .filter(|p| p.is_finite() && p.x.abs().max(p.y.abs()).max(p.z.abs()) <= 1e12)
        .ok_or_else(|| bad(COPY_ID, "base must be a finite [x,y,z] point within the drawing range"))?;
    let copy = CloudCopy { region, base };
    #[cfg(not(target_arch = "wasm32"))]
    return prepare(s, path, Some(copy))?.write();
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (path, copy);
        Err(bad(COPY_ID, "LAZ export requires the desktop app or CLI"))
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::{CloudFile, Job, prepare, set_writer};

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::{COPY_ID, CloudCopy, ID, Result, Session, Value, bad, json};
    use cadcraft_doc::point_cloud::CloudRegion;
    use std::fs::File;
    use std::io::{self, BufReader, BufWriter};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, OnceLock};
    use std::time::SystemTime;

    type Writer = fn(BufReader<File>, BufWriter<File>, &[CloudRegion], Option<CloudCopy>) -> std::result::Result<u64, String>;
    pub(super) static WRITER: OnceLock<Writer> = OnceLock::new();

    pub fn set_writer(writer: Writer) {
        let _ = WRITER.set(writer);
    }

    #[derive(Clone, Debug)]
    pub struct CloudFile {
        path: PathBuf,
        stamp: Stamp,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Stamp(u64, Option<SystemTime>);

    impl Stamp {
        fn read(file: &File) -> io::Result<Self> {
            let meta = file.metadata()?;
            Ok(Self(meta.len(), meta.modified().ok()))
        }
    }

    impl CloudFile {
        pub(crate) fn new(path: &Path, file: &File) -> io::Result<Self> {
            Ok(Self { path: path.canonicalize()?, stamp: Stamp::read(file)? })
        }
    }

    pub struct Job {
        file: CloudFile,
        output: PathBuf,
        removals: Arc<Vec<CloudRegion>>,
        writer: Writer,
        roots: Option<Arc<crate::file_access::AutomationRoots>>,
        source_points: u64,
        copy: Option<CloudCopy>,
        command: &'static str,
    }

    pub fn prepare(s: &Session, path: &str, copy: Option<CloudCopy>) -> Result<Job> {
        let command = if copy.is_some() { COPY_ID } else { ID };
        let state = s.state()?;
        let file = state.cloud_file.clone().ok_or_else(|| bad(command, "open the source LAS or LAZ file from disk first"))?;
        let cloud = state.doc.point_cloud.as_ref().ok_or_else(|| bad(command, "there is no open point cloud"))?;
        if !Path::new(path).extension().and_then(|ext| ext.to_str()).is_some_and(|ext| ext.eq_ignore_ascii_case("laz")) {
            return Err(bad(command, "the output file must have a .laz extension"));
        }
        let output = std::path::absolute(path).map_err(|e| bad(command, e.to_string()))?;
        let writer = WRITER.get().copied().ok_or_else(|| bad(command, "LAZ export is not available in this build"))?;
        Ok(Job {
            file,
            output,
            removals: state.doc.point_cloud_removals.clone(),
            writer,
            roots: crate::file_access::current(),
            source_points: cloud.source_count,
            copy,
            command,
        })
    }

    impl Job {
        pub fn write(self) -> Result<Value> {
            crate::file_access::confine(self.roots.as_ref(), || self.write_checked())
        }

        fn write_checked(&self) -> Result<Value> {
            let command = self.command;
            crate::file_access::check_read(&self.file.path).map_err(|e| bad(command, e))?;
            crate::file_access::check_write(&self.output).map_err(|e| bad(command, e))?;
            if self.output == self.file.path || self.output.canonicalize().is_ok_and(|path| path == self.file.path) {
                return Err(bad(command, "choose a different file name from the source point cloud"));
            }
            let source = crate::capped_read::open_regular(&self.file.path).map_err(|e| bad(command, e.to_string()))?;
            let check_source = |file: &File| {
                if Stamp::read(file)? == self.file.stamp {
                    Ok(())
                } else {
                    Err(io::Error::other("The source point cloud has changed. Reopen it before exporting."))
                }
            };
            check_source(&source).map_err(|e| bad(command, e.to_string()))?;
            let mut kept = 0;
            crate::atomic_write::write_atomic_with(&self.output, None, |file| {
                let input = BufReader::new(source.try_clone()?);
                let output = BufWriter::new(file.try_clone()?);
                kept = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.writer)(input, output, &self.removals, self.copy)))
                    .map_err(|_| io::Error::other("Could not encode the LAZ file"))?
                    .map_err(io::Error::other)?;
                check_source(&source)
            })
            .map_err(|e| bad(command, format!("{}: {e}", self.output.display())))?;
            Ok(match self.copy {
                Some(copy) => json!({ "path": self.output, "points": kept, "base": [copy.base.x, copy.base.y, copy.base.z] }),
                None => json!({ "path": self.output, "points": kept, "removedPoints": self.source_points.saturating_sub(kept) }),
            })
        }
    }
}
