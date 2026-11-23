use cadcraft_doc::Space;
use cadcraft_geom::{Bounds2, Vec2};
use std::io::Cursor;

// Fixture generated here from synthetic survey coordinates, with no external data.
fn survey(compressed: bool, extended: bool) -> Vec<u8> {
    let mut builder = las::Builder::from((1, if extended { 4 } else { 2 }));
    builder.point_format = las::point::Format::new(if extended { 7 } else { 2 }).unwrap();
    builder.transforms.y.offset = 6_700_000.0;
    builder.point_format.is_compressed = compressed;
    let mut writer = las::Writer::new(Cursor::new(Vec::new()), builder.into_header().unwrap()).unwrap();
    for i in 0..1_500 {
        writer
            .write_point(las::Point {
                x: 500_000.0 + f64::from(i) * 0.25,
                y: 6_700_000.0 + f64::from(i % 7),
                z: 92.25,
                color: Some(las::Color::new(32_768, 1024, 0)),
                intensity: 456,
                gps_time: extended.then_some(123.0),
                classification: las::point::Classification::Ground,
                ..las::Point::default()
            })
            .unwrap();
    }
    writer.into_inner().unwrap().into_inner()
}

#[test]
fn compressed_and_uncompressed_clouds_keep_coordinates_and_zoom_detail() {
    let las = cadcraft_io::read(&survey(false, false), "survey.las").unwrap();
    let laz = cadcraft_io::read(&survey(true, false), "survey.LAZ").unwrap();
    let extended = cadcraft_io::read(&survey(true, true), "extended.laz").unwrap();
    assert_eq!(extended.point_cloud, laz.point_cloud);
    let cloud = laz.point_cloud.as_ref().unwrap();
    assert_eq!(laz.point_cloud, las.point_cloud);
    assert!(laz.model.is_empty());
    assert!(!laz.extents(&Space::Model).is_empty());
    assert!(cloud.has_rgb && !cloud.rgb_is_8_bit);
    assert_eq!(cloud.points().len(), 1_500);
    let point = cloud.points().iter().find(|point| point.position.x == 500_000.0).unwrap();
    assert_eq!(point.position.y, 6_700_000.0);
    assert_eq!(point.position.z, 92.25);
    assert_eq!(point.rgb, [32_768, 1024, 0]);
    assert_eq!(point.intensity, 456);
    assert_eq!(point.classification, u8::from(las::point::Classification::Ground));

    let mut overview = 0;
    cloud.visible(cloud.bounds(), 30, |_| overview += 1);
    assert!(overview > 0 && overview <= 30);
    let detail = Bounds2::new(Vec2::new(500_000.0, 6_700_000.0), Vec2::new(500_002.0, 6_700_010.0));
    let mut visible = 0;
    cloud.visible(detail, 100, |point| {
        assert!(detail.contains(point.position.xy()));
        visible += 1;
    });
    assert_eq!(visible, cloud.points().iter().filter(|point| detail.contains(point.position.xy())).count());
    assert!(cadcraft_io::write(&laz, "survey.dxf").is_err());
}

#[test]
fn truncated_point_data_is_rejected() {
    let data = survey(false, false);
    assert!(cadcraft_io::read(&data[..data.len() - 1], "survey.las").is_err());
    let compressed = survey(true, false);
    assert!(cadcraft_io::read(&compressed[..compressed.len() / 2], "survey.laz").is_err());
}
