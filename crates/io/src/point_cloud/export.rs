//! Write surviving source records without requantizing coordinates or rebuilding attributes.
use super::{IoError, Result, preflight};
use cadcraft_doc::point_cloud::CloudRegion;
use cadcraft_geom::Vec3;
use las::{Builder, LazParallelism, PointDataBuilder, Reader, ReaderOptions, Writer, WriterOptions};
use std::io::{Read, Seek, Write};

pub fn export_laz(
    mut source: impl Read + Seek + Send + Sync + 'static,
    output: impl Write + Seek + Send + Sync + 'static,
    removals: &[CloudRegion],
) -> Result<u64> {
    preflight(&mut source)?;
    let error = |e: las::Error| IoError::Format(format!("LAS/LAZ: {e}"));
    let mut reader = Reader::with_options(source, ReaderOptions::default().with_laz_parallelism(LazParallelism::Yes)).map_err(error)?;
    let builder = PointDataBuilder::new().for_header(reader.header());
    let mut batch = builder.clone().build();
    let mut header = Builder::from(reader.header().clone());
    if header.point_format.has_waveform {
        return Err(IoError::Format("LAZ export does not support waveform packets".into()));
    }
    let keep_metadata = |v: &las::Vlr| v.user_id != laz::LazVlr::USER_ID && !v.user_id.eq_ignore_ascii_case("copc");
    header.vlrs.retain(keep_metadata);
    header.evlrs.retain(keep_metadata);
    header.point_format.is_compressed = true;
    let options = WriterOptions::default().with_laz_parallelism(LazParallelism::Yes);
    let mut writer = Writer::with_options(output, header.into_header().map_err(error)?, options).map_err(error)?;
    let batch_size = (16 * 1024 * 1024 / batch.record_len() as u64).min(super::BATCH);
    let mut remaining = reader.header().number_of_points();
    let mut kept = 0;
    let mut survivors = builder.build();
    while remaining != 0 {
        let read = reader.fill_points(batch_size.min(remaining), &mut batch).map_err(error)?;
        if read == 0 || read as usize != batch.len() || read > remaining {
            return Err(IoError::Format("LAS/LAZ ended before its declared point count".into()));
        }
        remaining -= read;
        if removals.is_empty() {
            writer.write_points(&batch).map_err(error)?;
            kept += read;
            continue;
        }
        let record_len = batch.record_len();
        let bytes = survivors.resize_for(batch.len());
        let mut count = 0;
        for (((x, y), z), record) in batch.x().zip(batch.y()).zip(batch.z()).zip(batch.raw_bytes().chunks_exact(batch.record_len())) {
            if !removals.iter().any(|region| region.contains(Vec3::new(x, y, z))) {
                bytes[count * record_len..(count + 1) * record_len].copy_from_slice(record);
                count += 1;
            }
        }
        survivors.resize_for(count);
        writer.write_points(&survivors).map_err(error)?;
        kept += survivors.len() as u64;
    }
    if kept == 0 {
        return Err(IoError::Format("No points remain to export".into()));
    }
    writer.into_inner().map_err(error)?.flush().map_err(|e| IoError::Format(e.to_string()))?;
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn export_preserves_surviving_source_records_and_metadata() {
        let mut header = las::Builder::from((1, 4));
        header.point_format = las::point::Format::new(7).unwrap();
        header.point_format.extra_bytes = 2;
        header.vlrs.push(las::Vlr {
            user_id: "LASF_Projection".into(),
            record_id: 2112,
            data: b"LOCAL_CS[\"Survey\",UNIT[\"metre\",1]]\0".to_vec(),
            ..Default::default()
        });
        let points = [10.01, 11.02, 11.03, 12.04].map(|x| las::Point {
            x,
            y: 100.0,
            z: 5.0,
            intensity: 1234,
            classification: las::point::Classification::Ground,
            return_number: 1,
            number_of_returns: 1,
            gps_time: Some(9876.0),
            color: Some(las::Color::new(65535, 1234, 5678)),
            extra_bytes: vec![11, 42],
            ..Default::default()
        });
        let removed = CloudRegion::plan(cadcraft_geom::Vec2::new(11.0, 99.0), cadcraft_geom::Vec2::new(12.0, 101.0));
        let path = std::env::temp_dir().join(format!("cadcraft-export-roundtrip-{}.laz", std::process::id()));
        for compressed in [false, true] {
            header.point_format.is_compressed = compressed;
            let mut source = Writer::new(Cursor::new(Vec::new()), header.clone().into_header().unwrap()).unwrap();
            for point in &points {
                source.write_point(point.clone()).unwrap();
            }
            let source = source.into_inner().unwrap().into_inner();
            assert_eq!(export_laz(Cursor::new(source), std::fs::File::create(&path).unwrap(), &[removed]).unwrap(), 2);
            let mut output = Reader::from_path(&path).unwrap();
            assert!(output.header().point_format().is_compressed);
            assert!(output.header().vlrs().contains(&header.vlrs[0]));
            assert_eq!(output.header().transforms(), &header.transforms);
            assert_eq!(output.header().number_of_points(), 2);
            let actual = output.read_all().unwrap().points().collect::<las::Result<Vec<_>>>().unwrap();
            for (read, expected) in actual.iter().zip([&points[0], &points[3]]) {
                assert!((read.x - expected.x).abs() < 1e-8);
                assert_eq!(
                    (read.y, read.z, read.intensity, read.classification, read.gps_time, read.color, &read.extra_bytes),
                    (expected.y, expected.z, expected.intensity, expected.classification, expected.gps_time, expected.color, &expected.extra_bytes)
                );
            }
            drop(output);
            std::fs::remove_file(&path).unwrap();
        }
    }
}
