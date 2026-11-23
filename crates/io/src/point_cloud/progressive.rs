//! Read sampled blocks first. Index the original blocks in the background and reopen only the
//! overlapping blocks for a detailed view. The worker retains the already authorized file handle.
use super::{CloudPoint, Drawing, IoError, MAX_FULL_POINTS, PointCloud, Result, preflight};
use cadcraft_doc::point_cloud::{CloudDetails, CloudView, DetailSource};
use cadcraft_geom::{Bounds2, Vec2, Vec3};
use las::{Header, PointData, PointDataBuilder};
use laz::laszip::{ChunkTable, ChunkTableEntry};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

const INITIAL_BLOCKS: usize = 40;
const OVERVIEW_BUDGET: usize = 1_000_000;
const DETAIL_BUDGET: usize = 500_000;

pub fn read_progressive(file: BufReader<File>) -> Result<Drawing> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| read_preview(file, MAX_FULL_POINTS, INITIAL_BLOCKS)))
        .map_err(|_| format_error("could not decode initial point cloud preview"))?
}

fn read_preview(mut file: BufReader<File>, full_limit: u64, preview_blocks: usize) -> Result<Drawing> {
    let table = preflight(&mut file)?;
    let header = Header::new(&mut file).map_err(format_error)?;
    if header.number_of_points() <= full_limit {
        file.rewind().map_err(format_error)?;
        return super::read(file);
    }
    let mut source = Source::new(file, &header, table)?;
    let mut quick = Sample::new(preview_blocks * 8_000);
    let blocks = preview_blocks.min(source.entries.len());
    for i in 0..blocks {
        let index = if blocks < 2 { 0 } else { i * (source.entries.len() - 1) / (blocks - 1) };
        let data = source.read(index..index + 1)?;
        visit(&data, 0..data.len(), 8, |point| quick.add(point))?;
    }
    let bounds = header.bounds();
    let (min, max) = (Vec2::new(bounds.min.x, bounds.min.y), Vec2::new(bounds.max.x, bounds.max.y));
    let mut initial = source.cloud(quick);
    if min.is_finite()
        && max.is_finite()
        && min.x <= max.x
        && min.y <= max.y
        && [min.x, min.y, max.x, max.y].iter().all(|v| v.abs() <= super::super::MAX_WINDOW_COORD)
    {
        initial.extent = initial.extent.union(&Bounds2::new(min, max));
    }
    if bounds.min.z.is_finite()
        && bounds.max.z.is_finite()
        && bounds.min.z <= bounds.max.z
        && bounds.min.z.abs().max(bounds.max.z.abs()) <= super::super::MAX_WINDOW_COORD
    {
        initial.z_range = [initial.z_range[0].min(bounds.min.z), initial.z_range[1].max(bounds.max.z)];
    }

    let (wake, rx) = sync_channel(1);
    let bridge = Arc::new(Bridge { state: Mutex::new(State::default()), wake, extent: initial.extent });
    let weak = Arc::downgrade(&bridge);
    std::thread::Builder::new()
        .name("cadcraft-point-cloud".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(source, &weak, rx)));
            if !matches!(result, Ok(Ok(())))
                && let Some(bridge) = weak.upgrade()
            {
                bridge.state().failed = true;
            }
            if let Ok(Err(e)) = result {
                log::warn!("{e}");
            }
        })
        .map_err(format_error)?;
    initial.detail = Some(DetailSource(bridge));
    let mut drawing = Drawing::new_metric();
    drawing.point_cloud = Some(Arc::new(initial));
    Ok(drawing)
}

fn format_error(e: impl std::fmt::Display) -> IoError {
    IoError::Format(format!("LAS/LAZ: {e}"))
}

struct Source {
    file: BufReader<File>,
    entries: Vec<ChunkTableEntry>,
    offsets: Vec<u64>,
    vlr: Option<laz::LazVlr>,
    builder: PointDataBuilder,
    record_len: usize,
    count: u64,
    rgb: bool,
}

impl Source {
    fn new(mut file: BufReader<File>, header: &Header, table: Option<ChunkTable>) -> Result<Self> {
        let count = header.number_of_points();
        let record_len = usize::from(header.point_format().len());
        let mut pos = file.stream_position().map_err(format_error)?;
        let (entries, vlr) = if let Some(table) = table {
            pos += ChunkTable::OFFSET_SIZE as u64;
            let mut remaining = count;
            let entries = table
                .as_ref()
                .iter()
                .map(|entry| {
                    let point_count = entry.point_count.min(remaining);
                    remaining -= point_count;
                    ChunkTableEntry { point_count, byte_count: entry.byte_count }
                })
                .collect();
            (entries, Some(header.laz_vlr().map_err(format_error)?))
        } else {
            let entries = (0..count)
                .step_by(50_000)
                .map(|i| {
                    let point_count = (count - i).min(50_000);
                    ChunkTableEntry { point_count, byte_count: point_count * record_len as u64 }
                })
                .collect();
            (entries, None)
        };
        let mut offsets = vec![pos];
        for entry in &entries {
            let entry: &ChunkTableEntry = entry;
            pos += entry.byte_count;
            offsets.push(pos);
        }
        Ok(Self {
            file,
            entries,
            offsets,
            vlr,
            builder: PointDataBuilder::new().for_header(header),
            record_len,
            count,
            rgb: header.point_format().has_color,
        })
    }

    fn end(&self, start: usize, end: usize) -> usize {
        let mut next = start + 1;
        let mut points = self.entries[start].point_count;
        while next < end
            && self.offsets[next + 1] - self.offsets[start] <= 32 << 20
            && (points + self.entries[next].point_count) * self.record_len as u64 <= 32 << 20
        {
            points += self.entries[next].point_count;
            next += 1;
        }
        next
    }

    fn read(&mut self, range: std::ops::Range<usize>) -> Result<PointData> {
        let offset = self.offsets[range.start];
        let mut encoded = vec![0; (self.offsets[range.end] - offset) as usize];
        self.file.seek(SeekFrom::Start(offset)).map_err(format_error)?;
        self.file.read_exact(&mut encoded).map_err(format_error)?;
        let bytes = if let Some(vlr) = &self.vlr {
            let entries = &self.entries[range];
            let count = entries.iter().map(|e| e.point_count).sum::<u64>();
            let mut raw = vec![0; count as usize * self.record_len];
            laz::par_decompress(&encoded, &mut raw, vlr, entries).map_err(format_error)?;
            raw
        } else {
            encoded
        };
        self.builder.clone().build_from_bytes(bytes).map_err(format_error)
    }

    fn cloud(&self, sample: Sample) -> PointCloud {
        let max = sample.points.iter().flat_map(|p| p.rgb).max().unwrap_or(0);
        PointCloud::new(sample.points, self.count, self.rgb && max != 0, max <= 255)
    }
}

/// When the sample fills, keep alternate entries and double the interval for future points.
/// Small regions retain all their points. Large regions use bounded storage and full coverage.
struct Sample {
    points: Vec<CloudPoint>,
    capacity: usize,
    seen: usize,
    stride: usize,
}

impl Sample {
    fn new(capacity: usize) -> Self {
        Self { points: Vec::new(), capacity, seen: 0, stride: 1 }
    }

    fn add(&mut self, point: CloudPoint) {
        if self.points.len() == self.capacity && self.seen.is_multiple_of(self.stride) {
            let mut index = 0;
            self.points.retain(|_| {
                index += 1;
                index % 2 == 1
            });
            self.stride *= 2;
        }
        if self.seen.is_multiple_of(self.stride) {
            self.points.push(point);
        }
        self.seen += 1;
    }
}

fn visit(data: &PointData, range: std::ops::Range<usize>, stride: usize, mut visit: impl FnMut(CloudPoint)) -> Result<()> {
    let (start, count) = (range.start, range.len());
    let position = data.x().zip(data.y()).zip(data.z()).skip(start).take(count).step_by(stride);
    let attributes = data.intensity().zip(data.classification()).skip(start).take(count).step_by(stride);
    let mut rgb = data.rgb().map(|c| c.skip(start).take(count).step_by(stride));
    for (((x, y), z), (intensity, classification)) in position.zip(attributes) {
        if [x, y, z].iter().any(|v| !v.is_finite() || v.abs() > super::super::MAX_WINDOW_COORD) {
            return Err(format_error("point coordinates outside the supported range"));
        }
        let (r, g, b) = rgb.as_mut().and_then(Iterator::next).unwrap_or_default();
        visit(CloudPoint { position: Vec3::new(x, y, z), rgb: [r, g, b], intensity, classification });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Request {
    region: Bounds2,
    width: f64,
    height: f64,
}

impl Request {
    fn covers(&self, view: Bounds2) -> bool {
        self.region.contains_box(&view) && self.width * 0.5 <= view.width() && self.height * 0.5 <= view.height()
    }
}

#[derive(Debug, Default)]
struct State {
    wanted: Option<Request>,
    detail: Option<(Request, Arc<PointCloud>)>,
    overview: Option<Arc<PointCloud>>,
    failed: bool,
}

#[derive(Debug)]
struct Bridge {
    state: Mutex<State>,
    wake: SyncSender<()>,
    extent: Bounds2,
}

impl Bridge {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn snapshot(&self, state: &State, bounds: Bounds2) -> CloudView {
        let detail = state.detail.as_ref().filter(|(request, _)| request.region.contains_box(&bounds));
        let completed = state.detail.as_ref().map(|(request, _)| *request);
        let points = detail.map(|(_, cloud)| cloud.clone()).or_else(|| state.overview.clone());
        CloudView {
            points,
            pending: !state.failed && (state.overview.is_none() || (state.wanted.is_some() && state.wanted != completed)),
            failed: state.failed,
        }
    }
}

impl CloudDetails for Bridge {
    fn view(&self, bounds: Bounds2) -> CloudView {
        let mut state = self.state();
        let close = bounds.intersects(&self.extent) && (bounds.width() < 0.65 * self.extent.width() || bounds.height() < 0.65 * self.extent.height());
        if !close {
            state.wanted = None;
        } else if !state.failed && !state.wanted.is_some_and(|request| request.covers(bounds)) {
            let padding = 0.2 * bounds.width().min(bounds.height());
            state.wanted = Some(Request { region: bounds.expand(padding), width: bounds.width(), height: bounds.height() });
            let _ = self.wake.try_send(());
        }
        self.snapshot(&state, bounds)
    }

    fn current(&self, bounds: Bounds2) -> CloudView {
        self.snapshot(&self.state(), bounds)
    }
}

fn work(mut source: Source, bridge: &Weak<Bridge>, wake: Receiver<()>) -> Result<()> {
    let mut index = Vec::with_capacity(source.entries.len());
    let mut overview = Sample::new(OVERVIEW_BUDGET);
    let stride = source.count.div_ceil(OVERVIEW_BUDGET as u64) as usize;
    let mut first = 0;
    let mut seen = 0usize;
    while first < source.entries.len() {
        if bridge.strong_count() == 0 {
            return Ok(());
        }
        let end = source.end(first, source.entries.len());
        // Index every XY point and retain a coarse overview across the entire scan.
        let data = source.read(first..end)?;
        let mut cursor = 0;
        let mut xy = data.x().zip(data.y());
        for entry in &source.entries[first..end] {
            let count = entry.point_count as usize;
            index.push(Bounds2::from_points(xy.by_ref().take(count).map(|(x, y)| Vec2::new(x, y))));
            cursor += count;
        }
        let start = (stride - seen % stride) % stride;
        seen += cursor;
        if start < cursor {
            visit(&data, start..cursor, stride, |point| overview.add(point))?;
        }
        first = end;
    }
    let Some(active) = bridge.upgrade() else { return Ok(()) };
    active.state().overview = Some(Arc::new(source.cloud(overview)));
    drop(active);

    while wake.recv().is_ok() {
        let Some(active) = bridge.upgrade() else { break };
        let request = active.state().wanted;
        let Some(request) = request.filter(|r| !active.state().detail.as_ref().is_some_and(|(done, _)| done == r)) else { continue };
        drop(active);
        if let Some(detail) = query(&mut source, &index, request, bridge)?
            && let Some(active) = bridge.upgrade()
        {
            let mut state = active.state();
            if state.wanted == Some(request) {
                state.detail = Some((request, Arc::new(detail)));
            }
        }
    }
    Ok(())
}

fn query(source: &mut Source, index: &[Bounds2], request: Request, bridge: &Weak<Bridge>) -> Result<Option<PointCloud>> {
    let mut hits = Sample::new(DETAIL_BUDGET);
    let mut block = 0;
    while block < index.len() {
        if !index[block].intersects(&request.region) {
            block += 1;
            continue;
        }
        if !bridge.upgrade().is_some_and(|b| b.state().wanted == Some(request)) {
            return Ok(None);
        }
        let run_end = index[block..].iter().take(128).take_while(|b| b.intersects(&request.region)).count() + block;
        let end = source.end(block, run_end);
        let data = source.read(block..end)?;
        visit(&data, 0..data.len(), 1, |point| {
            if request.region.contains(point.position.xy()) {
                hits.add(point);
            }
        })?;
        block = end;
    }
    Ok(Some(source.cloud(hits)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_view_recovers_original_points_absent_from_the_first_preview() {
        let path = std::env::temp_dir().join(format!("cadcraft-progressive-{}.laz", std::process::id()));
        let mut builder = las::Builder::from((1, 2));
        builder.point_format = las::point::Format::new(0).unwrap();
        {
            let mut writer = las::Writer::from_path(&path, builder.into_header().unwrap()).unwrap();
            for i in 0..100_100 {
                writer.write_point(las::Point { x: f64::from(i), y: f64::from(i % 10), z: 12.5, ..Default::default() }).unwrap();
            }
        }

        let file = BufReader::new(File::open(&path).unwrap());
        let d = read_preview(file, 10, 1).unwrap();
        let cloud = d.point_cloud.as_ref().unwrap();
        let area = Bounds2::new(Vec2::new(75_000.0, 0.0), Vec2::new(75_020.0, 9.0));
        let mut initial = 0;
        cloud.visible(area, usize::MAX, |_| initial += 1);
        assert_eq!(initial, 0);
        let source = &cloud.detail.as_ref().unwrap().0;
        assert!(source.view(area).pending);
        // Polling rather than a fixed sleep allows the worker as much time as it needs on CI.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let view = source.current(area);
            assert!(!view.failed);
            if !view.pending {
                let mut hits = Vec::new();
                view.points.unwrap().visible(area, usize::MAX, |p| hits.push(p.position));
                assert!(hits.iter().any(|p| p.x == 75_001.0 && p.z == 12.5));
                assert!(hits.iter().all(|p| area.contains(p.xy())));
                break;
            }
            assert!(std::time::Instant::now() < deadline, "detail worker stalled");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drop(d);
        std::fs::remove_file(&path).ok();
    }
}
