//! A bounded, replaceable cache for the most recently indexed desktop point cloud.
use super::{BlockExtent, CloudPoint, PointCloud, STRIPE_POINTS, Source};
use cadcraft_geom::{Bounds2, Vec2, Vec3};
use std::fs::{self, File, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const MAGIC: &[u8; 8] = b"CCPCIDX1";
const HEADER_SIZE: usize = 32;
const STRIPE_SIZE: usize = 32;
const POINT_SIZE: usize = 33;
const MAX_BYTES: usize = 192 << 20;

pub(super) struct IndexCache {
    path: PathBuf,
    fingerprint: u64,
    stride: usize,
}

impl IndexCache {
    pub(super) fn new(directory: &Path, source: &File, stride: usize) -> Option<Self> {
        Some(Self { path: directory.join("index-v1.bin"), fingerprint: fingerprint(source, stride)?, stride })
    }

    pub(super) fn load(&self, source: &Source) -> Option<(Vec<BlockExtent>, Vec<CloudPoint>)> {
        if !fs::symlink_metadata(&self.path).ok()?.is_file() {
            return None;
        }
        let mut file = File::open(&self.path).ok()?;
        let len = usize::try_from(file.metadata().ok()?.len()).ok()?;
        let mut header = [0; HEADER_SIZE];
        file.read_exact(&mut header).ok()?;
        if &header[..8] != MAGIC || integer(&header[8..]) != self.fingerprint {
            return None;
        }
        let count = usize::try_from(integer(&header[16..])).ok()?;
        let stripe_counts = source.entries.iter().map(|entry| entry.point_count.div_ceil(STRIPE_POINTS as u64) as usize);
        let index_bytes = stripe_counts.clone().sum::<usize>() * STRIPE_SIZE;
        if count as u64 != source.count.div_ceil(self.stride as u64) || len != HEADER_SIZE + index_bytes + count * POINT_SIZE || len > MAX_BYTES {
            return None;
        }
        let mut payload = vec![0; len - HEADER_SIZE];
        file.read_exact(&mut payload).ok()?;
        if checksum(&payload) != integer(&header[24..]) || fingerprint(source.file.get_ref(), self.stride)? != self.fingerprint {
            return None;
        }
        let (bounds, points) = payload.split_at(index_bytes);
        let mut stripes = bounds.as_chunks::<STRIPE_SIZE>().0.iter();
        let mut index = Vec::with_capacity(source.entries.len());
        for count in stripe_counts {
            let stripes = stripes
                .by_ref()
                .take(count)
                .map(|bytes| {
                    let min = Vec2::new(coordinate(bytes)?, coordinate(&bytes[8..])?);
                    let max = Vec2::new(coordinate(&bytes[16..])?, coordinate(&bytes[24..])?);
                    (min.x <= max.x && min.y <= max.y).then_some(Bounds2 { min, max })
                })
                .collect::<Option<_>>()?;
            index.push(BlockExtent::from_stripes(stripes));
        }
        let points = points
            .as_chunks::<POINT_SIZE>()
            .0
            .iter()
            .map(|bytes| {
                let position = Vec3::new(coordinate(bytes)?, coordinate(&bytes[8..])?, coordinate(&bytes[16..])?);
                let short = |start| u16::from_le_bytes([bytes[start], bytes[start + 1]]);
                Some(CloudPoint { position, rgb: [short(24), short(26), short(28)], intensity: short(30), classification: bytes[32] })
            })
            .collect::<Option<_>>()?;
        Some((index, points))
    }

    pub(super) fn save_in_background(self, source: &File, index: Arc<Vec<BlockExtent>>, overview: Arc<PointCloud>) {
        let Ok(source) = source.try_clone() else { return };
        let _ = std::thread::Builder::new().name("cadcraft-point-cloud-cache".into()).spawn(move || {
            static WRITERS: Mutex<()> = Mutex::new(());
            let Ok(_writer) = WRITERS.try_lock() else { return };
            let _ = self.save(&source, &index, &overview);
        });
    }

    fn save(&self, source: &File, index: &[BlockExtent], overview: &PointCloud) -> std::io::Result<()> {
        let points = overview.points();
        let len = index.iter().map(|block| block.stripes.len() * STRIPE_SIZE).sum::<usize>() + points.len() * POINT_SIZE;
        if len > MAX_BYTES - HEADER_SIZE || fingerprint(source, self.stride) != Some(self.fingerprint) {
            return Ok(());
        }
        let mut payload = Vec::with_capacity(len);
        for stripe in index.iter().flat_map(|block| &block.stripes) {
            for coord in [stripe.min.x, stripe.min.y, stripe.max.x, stripe.max.y] {
                payload.extend_from_slice(&coord.to_le_bytes());
            }
        }
        for point in points {
            for coord in [point.position.x, point.position.y, point.position.z] {
                payload.extend_from_slice(&coord.to_le_bytes());
            }
            for value in point.rgb.into_iter().chain([point.intensity]) {
                payload.extend_from_slice(&value.to_le_bytes());
            }
            payload.push(point.classification);
        }
        let Some(directory) = self.path.parent() else { return Ok(()) };
        let mut directory_options = fs::DirBuilder::new();
        directory_options.recursive(true);
        let mut file_options = OpenOptions::new();
        file_options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
            directory_options.mode(0o700);
            file_options.mode(0o600);
        }
        directory_options.create(directory)?;
        let temporary = self.path.with_extension(format!("{}.tmp", std::process::id()));
        let mut file = file_options.open(&temporary)?;
        let result = (|| {
            file.write_all(MAGIC)?;
            file.write_all(&self.fingerprint.to_le_bytes())?;
            file.write_all(&(points.len() as u64).to_le_bytes())?;
            file.write_all(&checksum(&payload).to_le_bytes())?;
            file.write_all(&payload)?;
            drop(file);
            fs::rename(&temporary, &self.path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }
}

fn fingerprint(file: &File, stride: usize) -> Option<u64> {
    let metadata = file.metadata().ok()?;
    let mut hash = DefaultHasher::new();
    metadata.len().hash(&mut hash);
    metadata.modified().ok()?.hash(&mut hash);
    stride.hash(&mut hash);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (metadata.dev(), metadata.ino(), metadata.ctime(), metadata.ctime_nsec()).hash(&mut hash);
    }
    #[cfg(not(unix))]
    metadata.created().ok()?.hash(&mut hash);
    Some(hash.finish())
}

fn checksum(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    hasher.write(bytes);
    hasher.finish()
}

fn integer(bytes: &[u8]) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(word)
}

fn coordinate(bytes: &[u8]) -> Option<f64> {
    let value = f64::from_bits(integer(bytes));
    (value.is_finite() && value.abs() <= super::super::super::MAX_WINDOW_COORD).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Seek, SeekFrom};

    #[test]
    fn saved_index_is_reused_only_while_source_and_cache_are_intact() {
        let directory = std::env::temp_dir().join(format!("cadcraft-index-test-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("points.las");
        let point = CloudPoint { position: Vec3::new(25_000_000.125, 6_700_000.25, 42.5), rgb: [10, 20, 30], intensity: 120, classification: 2 };
        let transform = |offset| las::Transform { scale: 0.001, offset };
        let mut builder = las::Builder::from((1, 2));
        builder.transforms = las::Vector { x: transform(25_000_000.0), y: transform(6_700_000.0), z: transform(0.0) };
        {
            let mut writer = las::Writer::from_path(&path, builder.into_header().unwrap()).unwrap();
            writer.write_point(las::Point { x: point.position.x, y: point.position.y, z: point.position.z, ..Default::default() }).unwrap();
        }
        let mut file = BufReader::new(File::open(&path).unwrap());
        let header = las::Header::new(&mut file).unwrap();
        let mut source = Source::new(file, &header, None).unwrap();
        let index: Vec<_> = source.read_blocks(&[0]).unwrap().iter().map(BlockExtent::new).collect();
        let overview = source.cloud(vec![point]);
        let cache = IndexCache::new(&directory, source.file.get_ref(), 1).unwrap();
        cache.save(source.file.get_ref(), &index, &overview).unwrap();
        let (restored, points) = cache.load(&source).unwrap();
        assert_eq!(points, overview.points());
        assert_eq!(restored[0].stripes, index[0].stripes);

        let mut corrupted = OpenOptions::new().write(true).open(&cache.path).unwrap();
        corrupted.seek(SeekFrom::End(-1)).unwrap();
        corrupted.write_all(&[255]).unwrap();
        drop(corrupted);
        assert!(cache.load(&source).is_none());
        cache.save(source.file.get_ref(), &index, &overview).unwrap();
        OpenOptions::new().append(true).open(&path).unwrap().write_all(&[0]).unwrap();
        assert!(cache.load(&source).is_none());
        drop(source);
        fs::remove_dir_all(directory).unwrap();
    }
}
