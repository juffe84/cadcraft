//! Read the original XYZ values of a point-cloud selection for the CAD clipboard.
use super::{IoError, Result, preflight};
use cadcraft_doc::point_cloud::CloudRegion;
use cadcraft_geom::Vec3;
use las::{LazParallelism, PointDataBuilder, Reader, ReaderOptions};
use std::io::{Read, Seek};

pub fn read_selection(
    mut input: impl Read + Seek + Send + Sync + 'static,
    region: CloudRegion,
    removals: &[CloudRegion],
    limit: usize,
) -> Result<Vec<Vec3>> {
    preflight(&mut input)?;
    let error = |e: las::Error| IoError::Format(format!("LAS/LAZ: {e}"));
    let options = ReaderOptions::default().with_laz_parallelism(LazParallelism::Yes);
    let mut reader = Reader::with_options(input, options).map_err(error)?;
    let mut batch = PointDataBuilder::new().for_header(reader.header()).build();
    let batch_size = (16 * 1024 * 1024 / batch.record_len() as u64).min(super::BATCH);
    let mut remaining = reader.header().number_of_points();
    let mut points = Vec::new();
    while remaining != 0 {
        let read = reader.fill_points(batch_size.min(remaining), &mut batch).map_err(error)?;
        if read == 0 || read as usize != batch.len() || read > remaining {
            return Err(IoError::Format("LAS/LAZ ended before its declared point count".into()));
        }
        remaining -= read;
        for ((x, y), z) in batch.x().zip(batch.y()).zip(batch.z()) {
            let point = Vec3::new(x, y, z);
            if region.contains(point) && !removals.iter().any(|area| area.contains(point)) {
                if !point.is_finite() || x.abs().max(y.abs()).max(z.abs()) > super::super::MAX_WINDOW_COORD {
                    return Err(IoError::Format("The selected points contain coordinates outside the drawing range".into()));
                }
                if points.len() == limit {
                    return Err(IoError::Format(format!("The selection exceeds {limit} CAD points. Select a smaller area.")));
                }
                points.push(point);
            }
        }
    }
    Ok(points)
}
