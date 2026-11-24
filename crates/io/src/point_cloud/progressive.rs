//! Read sampled blocks first. Index the original blocks in the background and reopen only the
//! overlapping blocks for a detailed view. The worker retains the already authorized file handle.
use super::{CloudPoint, Drawing, IoError, MAX_FULL_POINTS, PointCloud, Result, preflight};
use cadcraft_doc::point_cloud::{CloudDetails, CloudView, DetailOptions, DetailSource, MAX_PREVIEW_STEP};
use cadcraft_geom::{Bounds2, Vec2, Vec3};
use las::{Header, PointData, PointDataBuilder};
use laz::laszip::{ChunkTable, ChunkTableEntry};
use rayon::prelude::*;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::File;
use std::io::{BufReader, Seek};
#[cfg(not(unix))]
use std::io::{Read, SeekFrom};
use std::path::Path;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

const INITIAL_BLOCKS: usize = 40;
const OVERVIEW_BUDGET: usize = 1_000_000;
const DETAIL_BUDGET: usize = 500_000;

mod cache;
use cache::IndexCache;

pub fn read_progressive(file: BufReader<File>) -> Result<Drawing> {
    read_progressive_cached(file, None)
}

/// Optionally reuse the most recent point-cloud index from the desktop profile.
pub fn read_progressive_cached(file: BufReader<File>, cache_directory: Option<&Path>) -> Result<Drawing> {
    read_progressive_with_preview(file, 0, cache_directory)
}

/// Load every `preview_step`th point in the overview, or use automatic sampling for zero.
pub fn read_progressive_with_preview(file: BufReader<File>, preview_step: u16, cache_directory: Option<&Path>) -> Result<Drawing> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| read_preview(file, MAX_FULL_POINTS, INITIAL_BLOCKS, cache_directory, preview_step)))
        .map_err(|_| format_error("could not decode initial point cloud preview"))?
}

fn read_preview(
    mut file: BufReader<File>,
    full_limit: u64,
    preview_blocks: usize,
    cache_directory: Option<&Path>,
    preview_step: u16,
) -> Result<Drawing> {
    if preview_step > MAX_PREVIEW_STEP {
        return Err(format_error("preview step must be between 0 and 1000"));
    }
    let table = preflight(&mut file)?;
    let header = Header::new(&mut file).map_err(format_error)?;
    let count = header.number_of_points();
    if preview_step != 0 && count.div_ceil(u64::from(preview_step)) > MAX_FULL_POINTS {
        return Err(format_error(format!(
            "the selected overview exceeds {MAX_FULL_POINTS} points. Increase the sampling interval and reopen the file."
        )));
    }
    if preview_step == 1 || (preview_step == 0 && count <= full_limit) {
        file.rewind().map_err(format_error)?;
        return super::read(file);
    }
    let stride = if preview_step == 0 { count.div_ceil(OVERVIEW_BUDGET as u64) as usize } else { usize::from(preview_step) };
    let quick_stride = if preview_step == 0 { 8 } else { stride * 8usize.div_ceil(stride) };
    let cache = cache_directory.and_then(|directory| IndexCache::new(directory, file.get_ref(), stride));
    let mut source = Source::new(file, &header, table)?;
    let mut quick = Sample::new(preview_blocks * 8_000);
    let blocks = preview_blocks.min(source.entries.len());
    let preview_ids: Vec<_> = (0..blocks).map(|i| if blocks < 2 { 0 } else { i * (source.entries.len() - 1) / (blocks - 1) }).collect();
    let mut preview_index = Vec::with_capacity(blocks);
    let mut preview = preview_ids.iter().copied().peekable();
    while let Some(first) = preview.next() {
        let ids = source.batch(first, &mut preview, 0);
        let data = source.read_blocks(&ids)?;
        let sampled: Vec<_> = ids
            .into_par_iter()
            .zip(data)
            .map(|(id, data)| {
                let mut points = Vec::new();
                let start = if preview_step == 0 { 0 } else { source.sample_start(id, quick_stride) };
                visit(&data, start, quick_stride, |_| true, |point| points.push(point))?;
                Ok((id, BlockExtent::new(&data), points))
            })
            .collect::<Result<_>>()?;
        for (id, extent, points) in sampled {
            preview_index.push((id, extent));
            for point in points {
                quick.add(point);
            }
        }
    }
    let bounds = header.bounds();
    let (min, max) = (Vec2::new(bounds.min.x, bounds.min.y), Vec2::new(bounds.max.x, bounds.max.y));
    let mut initial = source.cloud(quick.points);
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
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(source, &preview_index, &weak, rx, cache, stride)));
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
    point_starts: Vec<u64>,
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
        let (entries, vlr): (Vec<ChunkTableEntry>, _) = if let Some(table) = table {
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
        let mut point_starts = Vec::with_capacity(entries.len());
        let mut point_start = 0;
        for entry in &entries {
            point_starts.push(point_start);
            point_start += entry.point_count;
            pos += entry.byte_count;
            offsets.push(pos);
        }
        Ok(Self {
            file,
            entries,
            offsets,
            point_starts,
            vlr,
            builder: PointDataBuilder::new().for_header(header),
            record_len,
            count,
            rgb: header.point_format().has_color,
        })
    }

    fn sample_start(&self, id: usize, stride: usize) -> usize {
        (stride - self.point_starts[id] as usize % stride) % stride
    }

    fn batch(&self, first: usize, ids: &mut std::iter::Peekable<impl Iterator<Item = usize>>, batch_points: u32) -> Vec<usize> {
        let workers = rayon::current_num_threads();
        let points_per_round = u64::from(batch_points) * workers as u64;
        let mut batch = vec![first];
        let mut bytes = self.entries[first].byte_count;
        let mut points = self.entries[first].point_count;
        while let Some(&id) = ids.peek() {
            let entry = self.entries[id];
            if (batch_points != 0 && batch.len() >= workers && points + entry.point_count > points_per_round)
                || bytes + entry.byte_count > 32 << 20
                || (points + entry.point_count) * self.record_len as u64 > 32 << 20
            {
                break;
            }
            let _ = ids.next();
            batch.push(id);
            bytes += entry.byte_count;
            points += entry.point_count;
        }
        batch
    }

    /// Keep each decoded block separate. Column iterators advance one record at a time,
    /// so indexing or filtering a shared slab would walk its earlier blocks repeatedly.
    fn read_blocks(&mut self, ids: &[usize]) -> Result<Vec<PointData>> {
        self.read_block_prefixes(ids.iter().map(|&id| (id, self.entries[id].point_count)).collect())
    }

    /// A LAZ chunk must decode from its start, but can stop after the last overlapping stripe.
    fn read_block_prefixes(&mut self, blocks: Vec<(usize, u64)>) -> Result<Vec<PointData>> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            // Workers read and decode their own chunks from the same authorized handle.
            // Positional reads never move the shared file cursor.
            blocks
                .into_par_iter()
                .map(|(id, point_count)| {
                    let entry = self.prefix(id, point_count);
                    let mut bytes = vec![0; entry.byte_count as usize];
                    self.file.get_ref().read_exact_at(&mut bytes, self.offsets[id]).map_err(format_error)?;
                    self.decode(entry, bytes)
                })
                .collect()
        }
        #[cfg(not(unix))]
        {
            let mut encoded = Vec::with_capacity(blocks.len());
            let mut last_offset = None;
            for (id, point_count) in blocks {
                let entry = self.prefix(id, point_count);
                let offset = self.offsets[id];
                if last_offset != Some(offset) {
                    self.file.seek(SeekFrom::Start(offset)).map_err(format_error)?;
                }
                let mut bytes = vec![0; entry.byte_count as usize];
                self.file.read_exact(&mut bytes).map_err(format_error)?;
                encoded.push((entry, bytes));
                last_offset = Some(offset + entry.byte_count);
            }
            encoded.into_par_iter().map(|(entry, bytes)| self.decode(entry, bytes)).collect()
        }
    }

    fn prefix(&self, id: usize, point_count: u64) -> ChunkTableEntry {
        let byte_count = if self.vlr.is_some() { self.entries[id].byte_count } else { point_count * self.record_len as u64 };
        ChunkTableEntry { byte_count, point_count }
    }

    fn decode(&self, entry: ChunkTableEntry, encoded: Vec<u8>) -> Result<PointData> {
        let bytes = if let Some(vlr) = &self.vlr {
            let selection = laz::DecompressionSelection::base().decompress_z().decompress_rgb().decompress_classification().decompress_intensity();
            let mut raw = vec![0; entry.point_count as usize * self.record_len];
            laz::par_decompress_selective(&encoded, &mut raw, vlr, &[entry], selection).map_err(format_error)?;
            raw
        } else {
            encoded
        };
        self.builder.clone().build_from_bytes(bytes).map_err(format_error)
    }

    fn cloud(&self, points: Vec<CloudPoint>) -> PointCloud {
        let max = points.iter().flat_map(|p| p.rgb).max().unwrap_or(0);
        PointCloud::new(points, self.count, self.rgb && max != 0, max <= 255)
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

fn visit(data: &PointData, start: usize, stride: usize, contains: impl Fn(Vec2) -> bool, mut visit: impl FnMut(CloudPoint)) -> Result<()> {
    let xy = data.x().zip(data.y()).skip(start).step_by(stride);
    let mut attributes = data.z().zip(data.intensity()).zip(data.classification()).skip(start).step_by(stride);
    let mut rgb = data.rgb().map(|colors| colors.skip(start).step_by(stride));
    let mut skipped = 0;
    for (x, y) in xy {
        if !contains(Vec2::new(x, y)) {
            skipped += 1;
            continue;
        }
        let Some(((z, intensity), classification)) = attributes.nth(skipped) else {
            return Err(format_error("incomplete point record"));
        };
        if [x, y, z].iter().any(|v| !v.is_finite() || v.abs() > super::super::MAX_WINDOW_COORD) {
            return Err(format_error("point coordinates outside the supported range"));
        }
        let (r, g, b) = rgb.as_mut().and_then(|colors| colors.nth(skipped)).unwrap_or_default();
        visit(CloudPoint { position: Vec3::new(x, y, z), rgb: [r, g, b], intensity, classification });
        skipped = 0;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Request {
    region: Bounds2,
    visible: Bounds2,
    priority: Bounds2,
    options: DetailOptions,
}

impl Request {
    fn new(bounds: Bounds2, options: DetailOptions) -> Self {
        if options.radius_factor > 0.0 {
            // A relative load zone must remain useful even at deep zoom. Retain a
            // little extra area so a small pan can reuse the same request.
            let factor = options.radius_factor;
            let center = bounds.center();
            let half = Vec2::new((bounds.width() / 2.0).max(300.0), (bounds.height() / 2.0).max(300.0)) * factor;
            let region = Bounds2::new(center - half, center + half);
            let inner = half * 0.75;
            let visible = Bounds2::new(center - inner, center + inner);
            let priority = Bounds2 { min: bounds.min.max(region.min), max: bounds.max.min(region.max) };
            return Self { region, visible, priority, options };
        }
        let cap = |region: Bounds2| {
            if options.radius > 0.0 {
                let center = bounds.center();
                let radius = Vec2::new(options.radius, options.radius);
                Bounds2 { min: region.min.max(center - radius), max: region.max.min(center + radius) }
            } else {
                region
            }
        };
        let visible = cap(bounds);
        let padding = 0.2 * visible.width().min(visible.height());
        Self { region: cap(visible.expand(padding)), visible, priority: visible, options }
    }

    fn covers_request(&self, next: Self) -> bool {
        // Radius changes affect where detail may appear. Batch changes only change pending work.
        self.same_radius(next) && self.options.block_points == next.options.block_points && self.covers(next.visible)
    }

    fn same_radius(&self, other: Self) -> bool {
        self.options.radius == other.options.radius && self.options.radius_factor == other.options.radius_factor
    }

    fn covers(&self, view: Bounds2) -> bool {
        self.region.contains_box(&view) && self.visible.width() * 0.5 <= view.width() && self.visible.height() * 0.5 <= view.height()
    }
}

#[derive(Debug, Default)]
struct State {
    wanted: Option<Request>,
    details: VecDeque<(Request, Arc<PointCloud>)>,
    partial: Option<(Request, Arc<PointCloud>)>,
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
        let details = || state.details.iter().rev().filter(|(request, _)| state.wanted.is_none_or(|wanted| request.same_radius(wanted)));
        let detail = details()
            .find(|(request, _)| state.wanted == Some(*request) && request.region.intersects(&bounds))
            .or_else(|| details().find(|(request, _)| request.covers(bounds)))
            .or_else(|| details().find(|(request, _)| request.region.contains_box(&bounds)))
            .or_else(|| state.wanted.and_then(|_| details().find(|(request, _)| request.region.intersects(&bounds))));
        let detail_count =
            state.wanted.and_then(|wanted| state.details.iter().find(|(request, _)| *request == wanted).map(|(_, cloud)| cloud.points().len()));
        let complete = state.wanted.is_none() || detail_count.is_some();
        let partial = state.partial.as_ref().filter(|(request, _)| state.wanted == Some(*request) && request.region.intersects(&bounds));
        let overlay = partial.or(detail).map(|(_, cloud)| cloud.clone());
        let overview = overlay.as_ref().and(state.overview.clone());
        let points = overlay.or_else(|| state.overview.clone());
        let indexing = state.overview.is_none();
        CloudView { points, overview, detail_count, indexing, pending: !state.failed && (indexing || !complete), failed: state.failed }
    }
}

impl CloudDetails for Bridge {
    fn view_with(&self, bounds: Bounds2, options: DetailOptions) -> CloudView {
        let mut state = self.state();
        let close = bounds.intersects(&self.extent) && (bounds.width() < 0.65 * self.extent.width() || bounds.height() < 0.65 * self.extent.height());
        if !close {
            state.wanted = None;
            state.partial = None;
        } else if !state.failed {
            let next = Request::new(bounds, options);
            let cached = state.details.iter().rposition(|(old, _)| old.covers_request(next));
            if let Some(index) = cached
                && let Some(detail) = state.details.remove(index)
            {
                if state.wanted != Some(detail.0) {
                    state.partial = None;
                }
                state.wanted = Some(detail.0);
                state.details.push_back(detail);
            } else if !state.wanted.is_some_and(|old| old.options == options && old.covers_request(next)) {
                state.partial = None;
                state.wanted = Some(next);
                let _ = self.wake.try_send(());
            }
        }
        self.snapshot(&state, bounds)
    }

    fn current(&self, bounds: Bounds2) -> CloudView {
        self.snapshot(&self.state(), bounds)
    }
}

/// Flight lines can make a single LAZ block's bounding box much larger than the area
/// its points occupy. Smaller consecutive ranges reject gaps without missing any point.
const STRIPE_POINTS: usize = 256;

struct BlockExtent {
    bounds: Bounds2,
    stripes: Vec<Bounds2>,
}

impl BlockExtent {
    fn new(data: &PointData) -> Self {
        let mut xy = data.x().zip(data.y());
        let stripes: Vec<_> = (0..data.len())
            .step_by(STRIPE_POINTS)
            .map(|_| Bounds2::from_points(xy.by_ref().take(STRIPE_POINTS).map(|(x, y)| Vec2::new(x, y))))
            .collect();
        Self::from_stripes(stripes)
    }

    fn from_stripes(stripes: Vec<Bounds2>) -> Self {
        let bounds = stripes.iter().fold(Bounds2::EMPTY, |extent, stripe| extent.union(stripe));
        Self { bounds, stripes }
    }

    fn distance_to(&self, region: &Bounds2, focus: Vec2) -> Option<f64> {
        if !self.bounds.intersects(region) {
            return None;
        }
        self.stripes
            .iter()
            .filter(|bounds| bounds.intersects(region))
            .map(|bounds| {
                let nearest = focus.max(bounds.min).min(bounds.max);
                let delta = nearest - focus;
                delta.dot(delta)
            })
            .min_by(f64::total_cmp)
    }

    fn points_through(&self, region: &Bounds2) -> u64 {
        self.stripes.iter().rposition(|stripe| stripe.intersects(region)).map_or(0, |last| ((last + 1) * STRIPE_POINTS) as u64)
    }
}

struct DetailJob {
    request: Request,
    indexed_before_request: usize,
    seeded: Vec<usize>,
    hits: Sample,
    published: Option<(Instant, usize)>,
}

impl DetailJob {
    fn new(request: Request, indexed_before_request: usize) -> Self {
        Self { request, indexed_before_request, seeded: Vec::new(), hits: Sample::new(DETAIL_BUDGET), published: None }
    }

    fn seed(&mut self, source: &mut Source, preview: &[(usize, BlockExtent)], index: &[BlockExtent], bridge: &Weak<Bridge>) -> Result<()> {
        let region = self.request.region;
        let priority = self.request.priority;
        let mut nearby: Vec<_> = index
            .iter()
            .enumerate()
            .chain(preview.iter().filter(|(id, _)| *id >= index.len()).map(|(id, extent)| (*id, extent)))
            .filter_map(|(id, extent)| extent.distance_to(&priority, priority.center()).map(|distance| (id, distance)))
            .collect();
        nearby.sort_unstable_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        nearby.truncate(rayon::current_num_threads());
        nearby.sort_unstable_by_key(|(id, _)| *id);
        let mut matches = nearby.into_iter().map(|(id, _)| id).peekable();
        if let Some(id) = matches.next() {
            // Recover the nearest known blocks immediately, including where the scan
            // had already passed before the user zoomed in.
            self.seeded = source.batch(id, &mut matches, 0);
            let blocks = source.read_blocks(&self.seeded)?;
            for data in blocks {
                visit(&data, 0, 1, |xy| region.contains(xy), |point| self.hits.add(point))?;
            }
            publish_partial(source, bridge, self);
        }
        Ok(())
    }
}

fn build_index(
    source: &mut Source,
    preview: &[(usize, BlockExtent)],
    bridge: &Weak<Bridge>,
    stride: usize,
) -> Result<Option<(Vec<BlockExtent>, Vec<CloudPoint>, Option<DetailJob>)>> {
    log::info!("point cloud indexing: {} Rayon workers", rayon::current_num_threads());
    let mut index = Vec::with_capacity(source.entries.len());
    let mut overview = Vec::new();
    let mut ids = (0..source.entries.len()).peekable();
    let mut live: Option<DetailJob> = None;
    while let Some(first) = ids.next() {
        let Some(active) = bridge.upgrade() else { return Ok(None) };
        let request = active.state().wanted;
        drop(active);
        if live.as_ref().map(|job| job.request) != request {
            live = request.map(|request| DetailJob::new(request, first));
            if let Some(job) = &mut live {
                job.seed(source, preview, &index, bridge)?;
            }
        }
        let batch = source.batch(first, &mut ids, 0);
        let data = source.read_blocks(&batch)?;
        let seeded = live.as_ref().map_or(&[][..], |job| job.seeded.as_slice());
        let blocks: Vec<_> = data
            .par_iter()
            .zip(batch)
            .map(|(data, id)| {
                let extent = BlockExtent::new(data);
                let mut coarse = Vec::new();
                visit(data, source.sample_start(id, stride), stride, |_| true, |point| coarse.push(point))?;
                let mut hits = Vec::new();
                if let Some(request) =
                    request.filter(|request| !seeded.contains(&id) && extent.distance_to(&request.region, request.region.center()).is_some())
                {
                    visit(data, 0, 1, |xy| request.region.contains(xy), |point| hits.push(point))?;
                }
                Ok((extent, coarse, hits))
            })
            .collect::<Result<_>>()?;
        for (extent, coarse, hits) in blocks {
            index.push(extent);
            overview.extend(coarse);
            if let Some(job) = &mut live {
                for point in hits {
                    job.hits.add(point);
                }
            }
        }
        if let Some(job) = &mut live {
            publish_partial(source, bridge, job);
        }
    }
    Ok(Some((index, overview, live)))
}

fn work(
    mut source: Source,
    preview: &[(usize, BlockExtent)],
    bridge: &Weak<Bridge>,
    wake: Receiver<()>,
    index_cache: Option<IndexCache>,
    stride: usize,
) -> Result<()> {
    let restored = index_cache.as_ref().and_then(|cache| cache.load(&source));
    let (index, overview, mut live, save) = match restored {
        Some((index, overview)) => {
            log::info!("point cloud index restored from cache");
            (index, overview, None, false)
        }
        None => {
            let Some((index, overview, live)) = build_index(&mut source, preview, bridge, stride)? else { return Ok(()) };
            (index, overview, live, true)
        }
    };
    let index = Arc::new(index);
    let overview = Arc::new(source.cloud(overview));
    let Some(active) = bridge.upgrade() else { return Ok(()) };
    active.state().overview = Some(overview.clone());
    drop(active);
    if save && let Some(cache) = index_cache {
        cache.save_in_background(source.file.get_ref(), index.clone(), overview);
    }

    let mut cache = BlockCache::default();
    while wake.recv().is_ok() {
        let Some(active) = bridge.upgrade() else { break };
        let request = active.state().wanted;
        let Some(request) = request.filter(|r| !active.state().details.iter().any(|(done, _)| done == r)) else { continue };
        drop(active);
        let job = live.take().filter(|job| job.request == request).unwrap_or_else(|| DetailJob::new(request, index.len()));
        if let Some(detail) = query(&mut source, &index[..job.indexed_before_request], job, bridge, &mut cache)?
            && let Some(active) = bridge.upgrade()
        {
            let mut state = active.state();
            if state.wanted == Some(request) {
                const MAX_DETAIL_VIEWS: usize = 4;
                if state.details.len() == MAX_DETAIL_VIEWS {
                    state.details.pop_front();
                }
                state.partial = None;
                state.details.push_back((request, Arc::new(detail)));
            }
        }
    }
    Ok(())
}

// Keep decoded blocks from nearby views. A block can be larger than the usual budget, so
// limit the cache by the actual number of retained points, not by a number of blocks.
const CACHE_POINTS: usize = 10_000_000;
// When a region exceeds the cache, remember the first (closest) blocks without evicting
// every previously cached block as the query crosses the rest of the file.
const LARGE_QUERY_CACHE_POINTS: u64 = 3_000_000;

#[derive(Default)]
struct BlockCache {
    blocks: BTreeMap<usize, (u64, Vec<CloudPoint>)>,
    clock: u64,
    points: usize,
}

impl BlockCache {
    fn get(&mut self, id: usize) -> Option<&[CloudPoint]> {
        let (last, points) = self.blocks.get_mut(&id)?;
        self.clock += 1;
        *last = self.clock;
        Some(points)
    }

    fn insert(&mut self, id: usize, points: Vec<CloudPoint>) {
        if points.len() > CACHE_POINTS {
            return;
        }
        while self.points + points.len() > CACHE_POINTS {
            let Some((&old, _)) = self.blocks.iter().min_by_key(|(_, (last, _))| *last) else { return };
            if let Some((_, removed)) = self.blocks.remove(&old) {
                self.points -= removed.len();
            }
        }
        self.clock += 1;
        self.points += points.len();
        self.blocks.insert(id, (self.clock, points));
    }
}

fn query(
    source: &mut Source,
    index: &[BlockExtent],
    mut job: DetailJob,
    bridge: &Weak<Bridge>,
    cache: &mut BlockCache,
) -> Result<Option<PointCloud>> {
    let request = job.request;
    let center = request.priority.center();
    let mut matches: Vec<_> = index
        .iter()
        .enumerate()
        .filter(|(id, _)| !job.seeded.contains(id))
        .filter_map(|(id, extent)| {
            if let Some(distance) = extent.distance_to(&request.priority, center) {
                Some((id, false, distance))
            } else {
                extent.distance_to(&request.region, center).map(|distance| (id, true, distance))
            }
        })
        .collect();
    // Give the actual view priority over the surrounding radius. Within each group
    // preserve file order after the first wave to avoid unnecessary disk seeks.
    let first_wave = rayon::current_num_threads().min(matches.len());
    if first_wave < matches.len() {
        matches.select_nth_unstable_by(first_wave, |a, b| a.1.cmp(&b.1).then_with(|| a.2.total_cmp(&b.2)).then_with(|| a.0.cmp(&b.0)));
    }
    matches[..first_wave].sort_unstable_by_key(|entry| entry.0);
    matches[first_wave..].sort_unstable_by_key(|&(id, padding, _)| (padding, id));
    let candidate_points = matches.iter().map(|&(id, _, _)| source.entries[id].point_count).sum::<u64>();
    let mut cache_budget = if candidate_points <= CACHE_POINTS as u64 { CACHE_POINTS as u64 } else { LARGE_QUERY_CACHE_POINTS };
    log::debug!("point cloud detail: {} candidate blocks, {candidate_points} source points", matches.len());
    let retain = |hits: &mut Sample, points: &[CloudPoint]| {
        for &point in points {
            if request.region.contains(point.position.xy()) {
                hits.add(point);
            }
        }
    };
    // Reuse all matching cached data before admitting new blocks that could evict it.
    let mut unread = Vec::with_capacity(matches.len());
    let neighbors = neighboring_chunks(source, index.len(), matches.iter().map(|&(id, _, _)| id), request.options.block_points);
    for (id, _, _) in matches {
        if let Some(points) = cache.get(id) {
            retain(&mut job.hits, points);
        } else {
            unread.push(id);
        }
    }
    // Extend the disk reads to the selected block boundaries when the decoded cache
    // can retain the additional chunks. The viewport still receives only its own points.
    let mut remaining = cache_budget.saturating_sub(candidate_points);
    for id in neighbors {
        let points = source.entries[id].point_count;
        if points <= remaining && !job.seeded.contains(&id) && cache.get(id).is_none() {
            unread.push(id);
            remaining -= points;
        }
    }
    publish_partial(source, bridge, &mut job);
    let mut ids = unread.into_iter().peekable();
    while let Some(id) = ids.next() {
        if !bridge.upgrade().is_some_and(|b| b.state().wanted == Some(request)) {
            return Ok(None);
        }
        let mut group = source.batch(id, &mut ids, request.options.batch_points);
        group.sort_unstable();
        let reads: Vec<_> = group
            .iter()
            .map(|&id| {
                let count = source.entries[id].point_count;
                let keep = cache_budget >= count;
                if keep {
                    cache_budget -= count;
                }
                let prefix = if keep { count } else { count.min(index[id].points_through(&request.region)) };
                (keep, (id, prefix))
            })
            .collect();
        let data = source.read_block_prefixes(reads.iter().map(|(_, read)| *read).collect())?;
        let decoded: Vec<_> = group
            .into_par_iter()
            .zip(reads)
            .zip(data)
            .map(|((id, (keep, _)), data)| {
                let mut points = Vec::new();
                if keep {
                    points.reserve(data.len());
                }
                visit(&data, 0, 1, |xy| keep || request.region.contains(xy), |point| points.push(point))?;
                Ok((id, keep, points))
            })
            .collect::<Result<_>>()?;
        for (id, keep, points) in decoded {
            if keep {
                retain(&mut job.hits, &points);
                cache.insert(id, points);
            } else {
                for point in points {
                    job.hits.add(point);
                }
            }
        }
        publish_partial(source, bridge, &mut job);
    }
    Ok(Some(source.cloud(job.hits.points)))
}

/// LAZ chunks are indivisible on disk. Group adjacent chunks by their source point
/// offsets so changing the loading block size can reuse the same spatial index.
fn neighboring_chunks(source: &Source, index_len: usize, matches: impl Iterator<Item = usize>, block_points: u32) -> Vec<usize> {
    if block_points == 0 {
        return Vec::new();
    }
    let block = |id: usize| source.point_starts[id] / u64::from(block_points);
    let matches: BTreeSet<_> = matches.collect();
    let selected: BTreeSet<_> = matches.iter().map(|&id| block(id)).collect();
    (0..index_len).filter(|&id| selected.contains(&block(id)) && !matches.contains(&id)).collect()
}

/// Reveal points from the first useful batches while the rest are still being read.
fn publish_partial(source: &Source, bridge: &Weak<Bridge>, job: &mut DetailJob) {
    let DetailJob { request, hits, published, .. } = job;
    if hits.points.is_empty() || published.is_some_and(|(t, seen)| seen == hits.seen || t.elapsed() < Duration::from_millis(350)) {
        return;
    }
    let Some(active) = bridge.upgrade() else { return };
    if active.state().wanted != Some(*request) {
        return;
    }
    let mut shown = Sample::new(100_000);
    let stride = hits.points.len().div_ceil(100_000).max(1);
    for point in hits.points.iter().step_by(stride) {
        shown.add(*point);
    }
    let cloud = Arc::new(source.cloud(shown.points));
    let mut state = active.state();
    if state.wanted == Some(*request) {
        state.partial = Some((*request, cloud));
        *published = Some((Instant::now(), hits.seen));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_radius_keeps_a_useful_load_zone_at_deep_zoom() {
        let center = Vec2::new(550_000.0, 6_700_000.0);
        let close = Bounds2::new(center, center).expand(0.1);
        let options = DetailOptions { radius_factor: 0.1, ..Default::default() };
        let detail = Request::new(close, options);
        assert!(detail.region.contains(center + Vec2::new(29.0, 29.0)));
        assert!(!detail.region.contains(center + Vec2::new(31.0, 0.0)));
        let moved = Bounds2::new(close.min + Vec2::X, close.max + Vec2::X);
        assert!(detail.covers_request(Request::new(moved, options)));
    }

    #[test]
    fn stripe_index_rejects_the_gap_and_keeps_the_last_short_range() {
        let points = (0..520).map(|i| {
            let x = if i < 256 {
                0.0
            } else if i < 513 {
                900.0
            } else {
                1_800.0
            };
            las::Point { x, y: f64::from(i % 10), ..Default::default() }
        });
        let data = PointDataBuilder::new().build_from_points(points.take(513)).unwrap();
        let extent = BlockExtent::new(&data);
        let square = |x| Bounds2::new(Vec2::new(x, 0.0), Vec2::new(x, 0.0)).expand(20.0);
        assert!(extent.bounds.intersects(&square(450.0)));
        assert!(extent.distance_to(&square(450.0), Vec2::new(450.0, 0.0)).is_none());
        assert!(extent.distance_to(&square(0.0), Vec2::ZERO).is_some());
        assert!(extent.distance_to(&square(900.0), Vec2::new(900.0, 0.0)).is_some());
        assert!(extent.distance_to(&square(1_800.0), Vec2::new(1_800.0, 0.0)).is_none());
    }

    #[test]
    fn indexed_blocks_can_seed_detail_outside_the_preview() {
        let path = std::env::temp_dir().join(format!("cadcraft-indexed-detail-{}.laz", std::process::id()));
        let mut builder = las::Builder::from((1, 4));
        builder.point_format = las::point::Format::new(7).unwrap();
        {
            let mut writer = las::Writer::from_path(&path, builder.into_header().unwrap()).unwrap();
            for x in 0..50_500 {
                writer
                    .write_point(las::Point {
                        x: f64::from(x),
                        y: f64::from(x % 10),
                        z: 12.5,
                        gps_time: Some(0.0),
                        color: Some(las::Color::new(300, 400, 500)),
                        ..Default::default()
                    })
                    .unwrap();
            }
        }
        let mut file = BufReader::new(File::open(&path).unwrap());
        let table = preflight(&mut file).unwrap();
        let header = Header::new(&mut file).unwrap();
        let mut source = Source::new(file, &header, table).unwrap();
        let blocks = source.read_blocks(&[0, 1]).unwrap();
        let index = blocks.iter().map(BlockExtent::new).collect::<Vec<_>>();
        let area = Bounds2::new(Vec2::new(50_000.0, 0.0), Vec2::new(50_020.0, 9.0));
        let prefix = index[1].points_through(&area);
        let decoded = source.read_block_prefixes(vec![(1, prefix)]).unwrap().pop().unwrap();
        assert!(decoded.len() < blocks[1].len());
        assert_eq!(decoded.raw_bytes(), &blocks[1].raw_bytes()[..decoded.raw_bytes().len()]);

        let (wake, _rx) = sync_channel(1);
        let bridge = Arc::new(Bridge { state: Mutex::new(State::default()), wake, extent: index[0].bounds.union(&index[1].bounds) });
        let _ = bridge.view(area);
        let mut job = DetailJob::new(bridge.state().wanted.unwrap(), index.len());
        job.seed(&mut source, &[], &index, &Arc::downgrade(&bridge)).unwrap();
        let view = bridge.current(area);
        assert!(view.indexing);
        assert!(view.points.unwrap().points().iter().any(|point| point.position == Vec3::new(50_001.0, 1.0, 12.5) && point.rgb == [300, 400, 500]));

        let inner = Bounds2::new(Vec2::new(50_100.0, 0.0), Vec2::new(50_120.0, 9.0));
        let mut read = |block_points| {
            let _ = bridge.view_with(inner, DetailOptions { block_points, ..Default::default() });
            let request = bridge.state().wanted.unwrap();
            let mut cache = BlockCache::default();
            let cloud = query(&mut source, &index, DetailJob::new(request, index.len()), &Arc::downgrade(&bridge), &mut cache).unwrap().unwrap();
            (cloud, cache)
        };
        // Keep the two chunks in the same logical block only with the larger setting.
        let (small, mut small_cache) = read(50_000);
        let (large, mut large_cache) = read(100_000);
        assert!(small_cache.get(0).is_none());
        assert!(large_cache.get(0).is_some());
        assert_eq!(small.points(), large.points());
        drop(source);
        std::fs::remove_file(path).unwrap();
    }

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

        let opened = read_progressive_with_preview(BufReader::new(File::open(&path).unwrap()), 997, None).unwrap();
        let cloud = opened.point_cloud.unwrap();
        assert!(cloud.points().iter().all(|point| (point.position.x.round() as u64).is_multiple_of(997)));
        let source = &cloud.detail.as_ref().unwrap().0;
        let deadline = Instant::now() + Duration::from_secs(10);
        let overview = loop {
            let view = source.current(cloud.bounds());
            assert!(!view.failed);
            if let Some(overview) = view.points {
                break overview;
            }
            assert!(Instant::now() < deadline, "the selected interval never finished loading");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(overview.points().len(), 100_100_usize.div_ceil(997));
        assert!(overview.points().iter().all(|point| (point.position.x.round() as u64).is_multiple_of(997)));
        drop(cloud);

        // The preview can seed real detail even before the full index exists.
        let mut file = BufReader::new(File::open(&path).unwrap());
        let table = preflight(&mut file).unwrap();
        let header = Header::new(&mut file).unwrap();
        let mut source = Source::new(file, &header, table).unwrap();
        let first = source.read_blocks(&[0]).unwrap().pop().unwrap();
        let preview = [(0, BlockExtent::new(&first))];
        let (wake, rx) = sync_channel(1);
        let bridge = Arc::new(Bridge { state: Mutex::new(State::default()), wake, extent: Bounds2::new(Vec2::ZERO, Vec2::new(100_099.0, 9.0)) });
        let seed_area = Bounds2::new(Vec2::new(11.0, 1.0), Vec2::new(15.0, 5.0));
        let options = DetailOptions { radius: 2.0, ..Default::default() };
        let _ = bridge.view_with(seed_area, options);
        let request = bridge.state().wanted.unwrap();
        let worker = Arc::downgrade(&bridge);
        let mut job = DetailJob::new(request, 0);
        job.seed(&mut source, &preview, &[], &worker).unwrap();
        let early = bridge.current(seed_area);
        assert!(early.indexing && early.pending);
        let early_points = early.points.unwrap();
        assert!(early_points.points().iter().any(|p| p.position.x == 13.0));
        let thread = std::thread::spawn(move || work(source, &preview, &worker, rx, None, 1).unwrap());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let view = bridge.current(seed_area);
            if !view.pending {
                assert_eq!(view.detail_count, Some(early_points.points().len()), "the index must not add the seeded points a second time");
                break;
            }
            assert!(Instant::now() < deadline, "the seeded view never finished");
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(bridge);
        thread.join().unwrap();

        let file = BufReader::new(File::open(&path).unwrap());
        let d = read_preview(file, 10, 1, None, 0).unwrap();
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
        let area = Bounds2::new(area.min, Vec2::new(area.max.x, 6.0));
        let options = DetailOptions { radius: 2.0, batch_points: 50_000, ..Default::default() };
        let mut view = source.view_with(area, options);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while view.pending && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
            view = source.current(area);
        }
        assert!(!view.pending && !view.failed);
        assert!(view.overview.is_some());
        let selected = view.points.unwrap();
        assert!(!selected.points().is_empty());
        assert!(selected.points().iter().all(|p| Bounds2::new(area.center(), area.center()).expand(2.0).contains(p.position.xy())));
        for _ in 0..2 {
            assert!(!source.view_with(area, options).pending, "the same capped view reuses its detail");
        }
        drop(d);
        std::fs::remove_file(&path).ok();
    }
}
