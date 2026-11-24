//! Full-resolution LAZ export through the native host's streaming writer.
use crate::{Result, Session};
use serde_json::{Value, json};

use super::super::{bad, str_param};

const ID: &str = "pointcloud.export";

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
    return prepare(s, path)?.write();
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (s, path, json!(null));
        Err(bad(ID, "LAZ export requires the desktop app or CLI"))
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::{CloudFile, Job, prepare, set_writer};

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::{ID, Result, Session, Value, bad, json};
    use cadcraft_doc::point_cloud::CloudRegion;
    use std::fs::File;
    use std::io::{self, BufReader, BufWriter};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, OnceLock};
    use std::time::SystemTime;

    type Writer = fn(BufReader<File>, BufWriter<File>, &[CloudRegion]) -> std::result::Result<u64, String>;
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

    impl CloudFile {
        pub(crate) fn open(&self) -> std::result::Result<File, String> {
            crate::file_access::check_read(&self.path)?;
            let source = crate::capped_read::open_regular(&self.path).map_err(|error| error.to_string())?;
            self.check(&source).map_err(|error| error.to_string())?;
            Ok(source)
        }

        pub(crate) fn check(&self, file: &File) -> io::Result<()> {
            if Stamp::read(file)? == self.stamp { Ok(()) } else { Err(io::Error::other("The source point cloud has changed. Reopen the file.")) }
        }
    }

    pub struct Job {
        file: CloudFile,
        output: PathBuf,
        removals: Arc<Vec<CloudRegion>>,
        writer: Writer,
        roots: Option<Arc<crate::file_access::AutomationRoots>>,
        source_points: u64,
    }

    pub fn prepare(s: &Session, path: &str) -> Result<Job> {
        let state = s.state()?;
        let file = state.cloud_file.clone().ok_or_else(|| bad(ID, "open the source LAS or LAZ file from disk first"))?;
        let cloud = state.doc.point_cloud.as_ref().ok_or_else(|| bad(ID, "there is no open point cloud"))?;
        if !Path::new(path).extension().and_then(|ext| ext.to_str()).is_some_and(|ext| ext.eq_ignore_ascii_case("laz")) {
            return Err(bad(ID, "the output file must have a .laz extension"));
        }
        let output = std::path::absolute(path).map_err(|e| bad(ID, e.to_string()))?;
        let writer = WRITER.get().copied().ok_or_else(|| bad(ID, "LAZ export is not available in this build"))?;
        Ok(Job {
            file,
            output,
            removals: state.doc.point_cloud_removals.clone(),
            writer,
            roots: crate::file_access::current(),
            source_points: cloud.source_count,
        })
    }

    impl Job {
        pub fn write(self) -> Result<Value> {
            crate::file_access::confine(self.roots.as_ref(), || self.write_checked())
        }

        fn write_checked(&self) -> Result<Value> {
            crate::file_access::check_write(&self.output).map_err(|e| bad(ID, e))?;
            if self.output == self.file.path || self.output.canonicalize().is_ok_and(|path| path == self.file.path) {
                return Err(bad(ID, "choose a different file name from the source point cloud"));
            }
            let source = self.file.open().map_err(|error| bad(ID, error))?;
            let mut kept = 0;
            crate::atomic_write::write_atomic_with(&self.output, None, |file| {
                let input = BufReader::new(source.try_clone()?);
                let output = BufWriter::new(file.try_clone()?);
                kept = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.writer)(input, output, &self.removals)))
                    .map_err(|_| io::Error::other("Could not encode the LAZ file"))?
                    .map_err(io::Error::other)?;
                self.file.check(&source)
            })
            .map_err(|e| bad(ID, format!("{}: {e}", self.output.display())))?;
            Ok(json!({ "path": self.output, "points": kept, "removedPoints": self.source_points.saturating_sub(kept) }))
        }
    }
}
