//! CADCraft's import policy on top of the Rust LAS/LAZ decoder.
#[cfg(not(target_arch = "wasm32"))]
mod export;
#[cfg(not(target_arch = "wasm32"))]
pub use export::export_laz;
#[cfg(not(target_arch = "wasm32"))]
mod progressive;
#[cfg(not(target_arch = "wasm32"))]
pub use progressive::{read_progressive, read_progressive_cached, read_progressive_with_preview, read_with_overview_progress};

use super::{IoError, Result};
use cadcraft_doc::Drawing;
use cadcraft_doc::point_cloud::{CloudPoint, PointCloud};
use cadcraft_geom::Vec3;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

const MAX_FULL_POINTS: u64 = if cfg!(target_arch = "wasm32") { 2_000_000 } else { 25_000_000 };
const LARGE_PREVIEW_POINTS: u64 = 5_000_000;
const MAX_SOURCE_POINTS: u64 = if cfg!(target_arch = "wasm32") { MAX_FULL_POINTS } else { 1_000_000_000 };
const BATCH: u64 = 250_000;

pub fn read(input: impl Read + Seek + Send + Sync + 'static) -> Result<Drawing> {
    // Decoder errors, including malformed input which trips a decoder assertion, must not
    // take down a host opening the file through automation or the browser.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decode(input)))
        .map_err(|_| IoError::Format("Could not decode the LAS/LAZ point cloud".into()))?
}

fn decode(mut input: impl Read + Seek + Send + Sync + 'static) -> Result<Drawing> {
    let error = |e: las::Error| IoError::Format(format!("LAS/LAZ: {e}"));
    preflight(&mut input)?;
    let mut reader = las::Reader::new(input).map_err(error)?;
    let count = reader.header().number_of_points();
    let has_rgb = reader.header().point_format().has_color;
    let mut batch = las::PointDataBuilder::new().for_header(reader.header()).build();
    let samples = if count > MAX_FULL_POINTS { LARGE_PREVIEW_POINTS } else { count };
    let stride = count.div_ceil(samples);
    let sample_count = count.div_ceil(stride);
    let mut points = Vec::new();
    points.try_reserve_exact(sample_count as usize).map_err(|e| IoError::Format(format!("LAS/LAZ point storage: {e}")))?;
    let mut max_rgb = 0;
    let mut consumed = 0;
    while consumed < count {
        let n = reader.fill_points((16 * 1024 * 1024 / batch.record_len() as u64).min(BATCH).min(count - consumed), &mut batch).map_err(error)?;
        if n == 0 || n as usize != batch.len() || n > count - consumed {
            return Err(IoError::Format("LAS/LAZ ended before its declared point count".into()));
        }
        let start = (stride - consumed % stride) % stride;
        consumed += n;
        let positions = batch.x().zip(batch.y()).zip(batch.z()).skip(start as usize).step_by(stride as usize);
        let attributes = batch.intensity().zip(batch.classification()).skip(start as usize).step_by(stride as usize);
        let mut rgb = batch.rgb().map(|colors| colors.skip(start as usize).step_by(stride as usize));
        for (((x, y), z), (intensity, classification)) in positions.zip(attributes) {
            if [x, y, z].into_iter().any(|v| !v.is_finite() || v.abs() > super::MAX_WINDOW_COORD) {
                return Err(IoError::Format("LAS/LAZ contains coordinates outside the supported range".into()));
            }
            let (r, g, b) = rgb.as_mut().and_then(Iterator::next).unwrap_or_default();
            max_rgb = max_rgb.max(r).max(g).max(b);
            points.push(CloudPoint { position: Vec3::new(x, y, z), rgb: [r, g, b], intensity, classification });
        }
    }
    let mut drawing = Drawing::new_metric();
    drawing.point_cloud = Some(Arc::new(PointCloud::new(points, count, has_rgb && max_rgb > 0, max_rgb <= 255)));
    Ok(drawing)
}

// las allocates metadata before exposing a Reader. Check the regions it may buffer first.
fn preflight(input: &mut (impl Read + Seek)) -> Result<Option<laz::laszip::ChunkTable>> {
    const MAX_METADATA: u64 = 16 << 20;
    let io_error = |e: std::io::Error| IoError::Format(format!("LAS/LAZ: {e}"));
    let invalid = || IoError::Format("LAS/LAZ metadata is invalid or exceeds the supported size".into());
    let file_len = input.seek(SeekFrom::End(0)).map_err(io_error)?;
    input.rewind().map_err(io_error)?;
    let header = las::raw::Header::read_from(&mut *input).map_err(|e| IoError::Format(format!("LAS/LAZ: {e}")))?;
    let count = match (header.number_of_point_records, header.large_file) {
        (0, Some(large)) => large.number_of_point_records,
        (legacy, _) => u64::from(legacy),
    };
    if count == 0 || count > MAX_SOURCE_POINTS {
        return Err(IoError::Format(format!("LAS/LAZ point count {count} is outside the supported range (1–{MAX_SOURCE_POINTS})")));
    }
    let format = las::point::Format::new(header.point_data_record_format).map_err(|e| IoError::Format(format!("LAS/LAZ: {e}")))?;
    let point_end = header.offset_to_end_of_points();
    if header.point_data_record_length < format.len() || (!format.is_compressed && point_end > file_len) {
        return Err(invalid());
    }
    let point_offset = u64::from(header.offset_to_point_data);
    let header_size = u64::from(header.header_size);
    if point_offset < header_size || point_offset > file_len || point_offset > MAX_METADATA {
        return Err(invalid());
    }
    // Each standard VLR has a 54-byte header and 16-bit payload length.
    let mut laszip = None;
    for _ in 0..header.number_of_variable_length_records {
        let start = input.stream_position().map_err(io_error)?;
        if start + 54 > point_offset {
            return Err(invalid());
        }
        let mut record = [0; 54];
        input.read_exact(&mut record).map_err(io_error)?;
        let len = u64::from(u16::from_le_bytes([record[20], record[21]]));
        let end = start + 54 + len;
        if end > point_offset {
            return Err(invalid());
        }
        if format.is_compressed
            && record[2..18].split(|&byte| byte == 0).next() == Some(laz::LazVlr::USER_ID.as_bytes())
            && record[18..20] == laz::LazVlr::RECORD_ID.to_le_bytes()
        {
            let vlr = laz::LazVlr::read_from(input.by_ref().take(len)).map_err(|_| invalid())?;
            if laszip.replace(vlr).is_some() {
                return Err(invalid());
            }
        }
        input.seek(SeekFrom::Start(end)).map_err(io_error)?;
    }
    if let Some(evlr) = header.evlr {
        let start = evlr.start_of_first_evlr;
        if start < point_offset || start > file_len.saturating_sub(60) || (!format.is_compressed && start.saturating_sub(point_end) > MAX_METADATA) {
            return Err(invalid());
        }
        input.seek(SeekFrom::Start(start + 20)).map_err(io_error)?;
        let mut len = [0; 8];
        input.read_exact(&mut len).map_err(io_error)?;
        if u64::from_le_bytes(len) > MAX_METADATA.min(file_len - start - 60) {
            return Err(invalid());
        }
    }
    let table = if format.is_compressed {
        let vlr = laszip.ok_or_else(invalid)?;
        if vlr.items_size() != u64::from(header.point_data_record_length) {
            return Err(invalid());
        }
        Some(check_chunks(input, point_offset, file_len, count, &vlr)?)
    } else {
        None
    };
    input.rewind().map_err(io_error)?;
    Ok(table)
}

fn check_chunks(input: &mut (impl Read + Seek), start: u64, len: u64, count: u64, vlr: &laz::LazVlr) -> Result<laz::laszip::ChunkTable> {
    const CHUNK_LIMIT: u64 = 64 << 20;
    let invalid = || IoError::Format("LAZ chunk table is invalid or exceeds the supported size".into());
    let mut offset = [0; 8];
    input.seek(SeekFrom::Start(start)).map_err(|_| invalid())?;
    input.read_exact(&mut offset).map_err(|_| invalid())?;
    let mut table = i64::from_le_bytes(offset);
    if table <= start as i64 {
        input.seek(SeekFrom::End(-8)).map_err(|_| invalid())?;
        input.read_exact(&mut offset).map_err(|_| invalid())?;
        table = i64::from_le_bytes(offset);
    }
    let table = u64::try_from(table).map_err(|_| invalid())?;
    if table < start + 8 || table > len.saturating_sub(8) {
        return Err(invalid());
    }
    input.seek(SeekFrom::Start(table)).map_err(|_| invalid())?;
    let mut header = [0; 8];
    input.read_exact(&mut header).map_err(|_| invalid())?;
    let chunks = u64::from(u32::from_le_bytes([header[4], header[5], header[6], header[7]]));
    if header[..4] != [0; 4] || chunks == 0 || chunks > count.min(1_000_000) {
        return Err(invalid());
    }
    input.seek(SeekFrom::Start(start)).map_err(|_| invalid())?;
    let entries = laz::laszip::ChunkTable::read_from(input, vlr).map_err(|_| invalid())?;
    let mut bytes = 0u64;
    let mut points = 0u64;
    for entry in &entries {
        if entry.byte_count == 0
            || entry.byte_count > CHUNK_LIMIT
            || entry.point_count == 0
            || entry.point_count.saturating_mul(vlr.items_size()) > CHUNK_LIMIT
        {
            return Err(invalid());
        }
        bytes += entry.byte_count;
        points += entry.point_count;
    }
    let valid_count = if vlr.uses_variable_size_chunks() { points == count } else { points >= count && points - u64::from(vlr.chunk_size()) < count };
    if !valid_count || bytes != table - start - 8 {
        return Err(invalid());
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cadcraft_doc::{Common, EntityKind, Line, Space};

    #[test]
    fn tracing_a_cloud_can_be_saved_as_cad_geometry() {
        let [a, b] = [Vec3::new(50.0, 20.0, 10.0), Vec3::new(80.0, 40.0, 15.0)];
        let points = [a, b].map(|position| CloudPoint { position, rgb: [0; 3], intensity: 0, classification: 0 });
        let mut drawing = Drawing::new_metric();
        drawing.point_cloud = Some(Arc::new(PointCloud::new(points.to_vec(), 2, false, false)));
        assert!(crate::write(&drawing, "trace.dxf").is_err());
        drawing.add(&Space::Model, Common::default(), EntityKind::Line(Line { a, b })).unwrap();
        let bytes = crate::write(&drawing, "trace.dxf").unwrap();
        let read = crate::read_dxf(&bytes).unwrap();
        assert!(matches!(&read.model.iter().next().unwrap().kind, EntityKind::Line(line) if line.a == a && line.b == b));
        assert!(read.point_cloud.is_none());
        assert!(drawing.point_cloud.is_some());
        assert!(crate::write(&drawing, "trace.las").is_err());
    }
}
