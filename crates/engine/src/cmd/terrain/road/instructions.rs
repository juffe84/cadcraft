//! Standalone copies of a road's settings and rules. Geometry remains in the drawing.
use serde_json::Value;

use crate::{Result, Session};

#[cfg(target_arch = "wasm32")]
fn unavailable(command: &str) -> Result<Value> {
    Err(crate::cmd::bad(command, "Road instruction files require the desktop application."))
}

pub(super) fn save(session: &mut Session, params: &Value) -> Result<Value> {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (session, params);
        unavailable("tin.road.instructions.save")
    }
    #[cfg(not(target_arch = "wasm32"))]
    native::save(session, params)
}

pub(super) fn load(session: &mut Session, params: &Value) -> Result<Value> {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (session, params);
        unavailable("tin.road.instructions.load")
    }
    #[cfg(not(target_arch = "wasm32"))]
    native::load(session, params)
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::{io::Read, path::Path};

    use cadcraft_doc::road::{self, Design};
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};

    use super::super::{validate_templates, with_sampler};
    use crate::{Result, Session, cmd::bad};

    const SAVE: &str = "tin.road.instructions.save";
    const LOAD: &str = "tin.road.instructions.load";
    const FORMAT: &str = "cadcraft-road-instructions";
    const MAX_BYTES: usize = 1024 * 1024;

    #[derive(Deserialize, Serialize)]
    struct InstructionFile {
        format: String,
        version: u32,
        design: Design,
    }

    fn path<'a>(command: &str, params: &'a Value) -> Result<&'a str> {
        params["path"].as_str().filter(|path| !path.trim().is_empty()).ok_or_else(|| bad(command, "Choose a road instruction file."))
    }

    fn validate(session: &Session, command: &str, design: &mut Design) -> Result<()> {
        design.name = design.name.trim().into();
        if !road::valid_name(&design.name) || design.crossfall.len() > 512 {
            return Err(bad(command, "Enter a valid road name and up to 512 crossfall rows."));
        }
        let drawing = session.doc()?;
        validate_templates(design).and_then(|()| with_sampler(drawing, design, |_, _| Ok(()))).map_err(|error| match error {
            crate::EngineError::BadParams { msg, .. } => bad(command, msg),
            other => other,
        })
    }

    pub(super) fn save(session: &mut Session, params: &Value) -> Result<Value> {
        let path = path(SAVE, params)?;
        crate::file_access::check_write(path).map_err(|error| bad(SAVE, error))?;
        let mut design = serde_json::from_value(params["design"].clone()).map_err(|error| bad(SAVE, error.to_string()))?;
        validate(session, SAVE, &mut design)?;
        let file = InstructionFile { format: FORMAT.into(), version: 1, design };
        let bytes = serde_json::to_vec_pretty(&file).map_err(|error| bad(SAVE, error.to_string()))?;
        if bytes.len() > MAX_BYTES {
            return Err(bad(SAVE, "The road instruction file is too large."));
        }
        crate::atomic_write::write_atomic(Path::new(path), &bytes).map_err(|error| bad(SAVE, format!("{path}: {error}")))?;
        Ok(json!({"path": path}))
    }

    pub(super) fn load(session: &mut Session, params: &Value) -> Result<Value> {
        let path = path(LOAD, params)?;
        crate::file_access::check_read(path).map_err(|error| bad(LOAD, error))?;
        let metadata = std::fs::metadata(path).map_err(|error| bad(LOAD, format!("{path}: {error}")))?;
        if !metadata.is_file() {
            return Err(bad(LOAD, "Choose a road instruction file."));
        }
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .and_then(|file| file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes))
            .map_err(|error| bad(LOAD, format!("{path}: {error}")))?;
        if bytes.len() > MAX_BYTES {
            return Err(bad(LOAD, "The road instruction file is too large."));
        }
        let mut file: InstructionFile = serde_json::from_slice(&bytes).map_err(|error| bad(LOAD, error.to_string()))?;
        if file.format != FORMAT || file.version != 1 {
            return Err(bad(LOAD, "Unsupported road instruction file format or version."));
        }
        validate(session, LOAD, &mut file.design)?;
        serde_json::to_value(file.design).map_err(|error| bad(LOAD, error.to_string()))
    }
}
