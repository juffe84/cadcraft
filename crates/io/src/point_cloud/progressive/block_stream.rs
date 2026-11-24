//! Keep decoded chunks alive across nearby views so the renderer can keep their buffers.
use super::{BlockExtent, Bridge, Request, Source, visit};
use cadcraft_doc::point_cloud::{CloudView, DetailOptions, PointCloud};
use cadcraft_geom::Bounds2;
use rayon::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Weak};

// Bound the decoded set by points, with room for close-ups of dense survey data.
const RESIDENT_POINTS: u64 = 42_000_000;
const MAX_GROUPS: u64 = 4096;

#[derive(Debug, Default)]
pub(super) struct Resident {
    chunks: BTreeMap<usize, Arc<PointCloud>>,
    settled: Option<Request>,
    limited: bool,
}

impl Resident {
    pub(super) fn snapshot(&self, wanted: Option<Request>, bounds: Bounds2, failed: bool) -> CloudView {
        let active = wanted.is_some();
        let pending = active && self.settled != wanted && !failed;
        let blocks = if active { self.chunks.values().filter(|cloud| cloud.bounds().intersects(&bounds)).cloned().collect() } else { Vec::new() };
        CloudView {
            blocks,
            block_streaming: true,
            detail_count: active.then(|| self.chunks.values().map(|cloud| cloud.points().len()).sum()),
            detail_limited: active && self.limited,
            pending,
            failed,
            ..Default::default()
        }
    }
}

struct Group {
    chunks: Range<usize>,
    bounds: Bounds2,
    points: u64,
}

fn groups(source: &Source, index: &[BlockExtent], block_points: u32) -> Vec<Group> {
    let count = (source.count / u64::from(block_points)).clamp(1, MAX_GROUPS);
    let span = source.count.div_ceil(count);
    let mut first = 0;
    source
        .point_starts
        .chunk_by(|a, b| a / span == b / span)
        .map(|starts| {
            let chunks = first..first + starts.len();
            first = chunks.end;
            let bounds = index[chunks.clone()].iter().fold(Bounds2::EMPTY, |bounds, block| bounds.union(&block.bounds));
            let points = source.entries[chunks.clone()].iter().map(|entry| entry.point_count).sum();
            Group { chunks, bounds, points }
        })
        .collect()
}

fn select(groups: &[Group], index: &[BlockExtent], request: Request) -> (Vec<usize>, u64, bool) {
    let center = request.priority.center();
    let mut matches: Vec<_> = groups
        .iter()
        .filter(|group| group.bounds.intersects(&request.region))
        .filter_map(|group| {
            let distance =
                index[group.chunks.clone()].iter().filter_map(|block| block.distance_to(&request.region, center)).min_by(f64::total_cmp)?;
            Some((group, distance))
        })
        .collect();
    matches.sort_unstable_by(|(a, da), (b, db)| da.total_cmp(db).then(a.chunks.start.cmp(&b.chunks.start)));
    let mut selected = Vec::new();
    let mut room = RESIDENT_POINTS;
    let mut limited = false;
    for (group, _) in matches {
        if group.points <= room {
            selected.extend(group.chunks.clone());
            room -= group.points;
        } else {
            limited = true;
        }
    }
    (selected, room, limited)
}

fn keep_zone(request: Request) -> Bounds2 {
    let region = request.region;
    let half = (region.max - region.min) * 0.5;
    Bounds2 { min: region.min - half, max: region.max + half }
}

pub(super) fn run(source: &mut Source, index: &[BlockExtent], bridge: &Weak<Bridge>, wake: Receiver<()>) -> super::Result<()> {
    let mut block_points = 0;
    let mut layout = Vec::new();
    while wake.recv().is_ok() {
        let Some(active) = bridge.upgrade() else { break };
        let request = {
            let mut state = active.state();
            if state.wanted.is_none() {
                state.blocks = Resident::default();
            }
            state.wanted
        };
        drop(active);
        let Some(request) = request else { continue };
        let target = request.options.block_points.max(DetailOptions::MIN_BLOCK_POINTS);
        if block_points != target {
            layout = groups(source, index, target);
            block_points = target;
        }
        let (selected, room, limited) = select(&layout, index, request);
        load_missing(source, index, bridge, request, selected, room, limited)?;
    }
    Ok(())
}

fn load_missing(
    source: &mut Source,
    index: &[BlockExtent],
    bridge: &Weak<Bridge>,
    request: Request,
    mut selected: Vec<usize>,
    mut room: u64,
    limited: bool,
) -> super::Result<()> {
    {
        let Some(active) = bridge.upgrade() else { return Ok(()) };
        let mut state = active.state();
        if state.wanted != Some(request) {
            return Ok(());
        }
        let wanted: BTreeSet<_> = selected.iter().copied().collect();
        let keep = keep_zone(request);
        state.blocks.chunks.retain(|id, block| {
            if wanted.contains(id) {
                return true;
            }
            let points = block.points().len() as u64;
            if points > room || !index[*id].bounds.intersects(&keep) {
                return false;
            }
            room -= points;
            true
        });
        selected.retain(|id| !state.blocks.chunks.contains_key(id));
        state.blocks.limited = limited;
    }
    let mut remaining = selected.into_iter().peekable();
    while let Some(first) = remaining.next() {
        let mut batch = source.batch(first, &mut remaining, request.options.batch_points);
        batch.sort_unstable();
        let decoded = source.read_blocks(&batch)?;
        let blocks = batch
            .into_par_iter()
            .zip(decoded)
            .map(|(id, data)| {
                let mut points = Vec::with_capacity(data.len());
                visit(&data, 0, 1, |_| true, |point| points.push(point))?;
                Ok((id, Arc::new(source.cloud(points))))
            })
            .collect::<super::Result<Vec<_>>>()?;
        let Some(active) = bridge.upgrade() else { return Ok(()) };
        let mut state = active.state();
        // Commit finished work. The next request can reuse any chunks it still needs.
        state.blocks.chunks.extend(blocks);
        if state.wanted != Some(request) {
            return Ok(());
        }
    }
    if let Some(active) = bridge.upgrade() {
        active.state().blocks.settled = Some(request);
    }
    Ok(())
}
