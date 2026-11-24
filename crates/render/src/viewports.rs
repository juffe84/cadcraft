#[cfg(not(target_arch = "wasm32"))]
use super::display::append;
use super::display::{DImage, DPrim, DisplayList, Kind, NO_LAYER, PlotText, SpatialSnap, VIEWPORT_CONTENT};
use super::draw::entity;
#[cfg(not(target_arch = "wasm32"))]
use super::pipeline::PARALLEL_CHUNK;
use super::pipeline::{Options, style_table, top_ctx, with_point_view};
use super::{
    Bounds2, Builder, Ctx, Drawing, Entity, EntityKind, Handle, MAX_BLOCK_EXPANSION, MAX_DISPLAY_POINTS, Mat3, Pen, Space, Vec2, clip, paper, pen,
    plotstyle,
};

/// Upper bound on viewports drawn per layout (hostile files).
pub(crate) const MAX_VIEWPORTS: usize = 256;

/// A paper-space viewport: its border (as an ordinary entity) and model space seen through it.
/// Viewport id 1 is the paper-space view itself and is not drawn. A clipped viewport shows
/// model space through its clip boundary's outline (the boundary entity draws itself, so the
/// rectangle is not drawn); a viewport turned off shows only its border.
pub(crate) fn viewport(b: &mut Builder, d: &Drawing, e: &Entity, vp: &cadcraft_doc::Viewport) {
    if !viewport_shown(d, e, vp) {
        return;
    }
    let poly = viewport_clip(d, e.handle, vp);
    // Border (on the viewport's layer; layer off hides only the border).
    if poly.is_none() {
        entity(b, &top_ctx(d, Mat3::IDENTITY, e.handle, &[], &[]), e);
    }
    if b.opts.skip_viewport == Some(e.handle) {
        return;
    }
    let unit_mm = paper::unit_mm_of(d, e.handle);
    match &poly {
        Some(p) => contents(b, d, vp, p.bounds(), &Clip::Poly(p), unit_mm),
        None => {
            if let Some(rect) = viewport_rect(vp) {
                contents(b, d, vp, rect, &Clip::Rect(rect), unit_mm);
            }
        }
    }
}

/// Whether a viewport draws anything: id 1 is the paper-space view itself, and a hidden viewport
/// or one on a frozen layer shows neither its border nor its contents.
fn viewport_shown(d: &Drawing, e: &Entity, vp: &cadcraft_doc::Viewport) -> bool {
    vp.id != 1 && e.common.visible && !d.is_hidden_object(e.handle) && !d.layer(&e.common.layer).is_some_and(|l| l.frozen)
}

/// A viewport's rectangle on paper; `None` for an unusable size or centre.
fn viewport_rect(vp: &cadcraft_doc::Viewport) -> Option<Bounds2> {
    let center = vp.center.xy();
    let ok = |v: f64| v.is_finite() && v > 0.0;
    (ok(vp.width) && ok(vp.height) && center.is_finite()).then(|| {
        let half = Vec2::new(vp.width / 2.0, vp.height / 2.0);
        Bounds2::new(center - half, center + half)
    })
}

/// Model space seen through the layout viewport `handle` of `space`, in paper units, clipped to
/// `area` (paper units) instead of to the viewport: what [`build`] draws inside that viewport,
/// over an area the caller chooses. The canvas builds the active viewport this way for an area
/// around it and draws it clipped to the viewport, so panning and zooming inside it moves the
/// geometry instead of rebuilding the layout (see [`Options::skip_viewport`]). Empty when the
/// viewport shows nothing.
pub fn viewport_content(d: &Drawing, space: &Space, handle: Handle, area: Bounds2, opts: &Options) -> DisplayList {
    let Some(e) = d.space(space).and_then(|s| s.get(handle)) else { return DisplayList::default() };
    let EntityKind::Viewport(vp) = &e.kind else { return DisplayList::default() };
    if !viewport_shown(d, e, vp) || area.is_empty() || !(area.min.is_finite() && area.max.is_finite()) {
        return DisplayList::default();
    }
    let opts = &with_point_view(d, space, opts);
    let styler = style_table(d, space, opts, false).map(|t| plotstyle::Styler::new(d, t));
    let points = std::sync::atomic::AtomicUsize::new(0);
    let mut b = Builder {
        list: DisplayList::default(),
        opts,
        plotting: false,
        styles: styler.as_ref(),
        named: None,
        shared_budget: None,
        transparency: 0,
        points: &points,
        max_points: MAX_DISPLAY_POINTS,
        snaps_only: false,
    };
    contents(&mut b, d, vp, area, &Clip::Rect(area), paper::unit_mm_of(d, handle));
    b.list
}

/// Model space seen through `vp`, in paper units, appended to `b`'s list clipped to `clip`;
/// `area` (the clip's bounds on paper) picks the model objects to draw.
fn contents(b: &mut Builder, d: &Drawing, vp: &cadcraft_doc::Viewport, area: Bounds2, clip: &Clip, unit_mm: f64) {
    if b.list.expanded >= MAX_BLOCK_EXPANSION || !vp.on {
        return;
    }
    let center = vp.center.xy();
    let ok = |v: f64| v.is_finite() && v > 0.0;
    if !(ok(vp.width) && ok(vp.height) && ok(vp.view_height) && center.is_finite() && vp.view_center.is_finite()) {
        return;
    }
    let s = vp.height / vp.view_height;
    if !ok(s) {
        return;
    }
    // Model space scaled onto the sheet and turned by the viewport's view twist.
    let xf = vp.model_to_paper_matrix();
    // The model box the area covers (turned, with a twist), with slack: entity bounds are
    // approximate (text, dimensions).
    let mbox = Bounds2::from_points(area.corners().map(|p| vp.paper_to_model(p)));
    let win = mbox.expand(mbox.width().max(mbox.height()) * 0.05);
    // PSLTSCALE on: model-space dashes are scaled so they measure the same on paper as in
    // paper space, whatever the viewport scale.
    let lt_factor = if d.header.i64("PSLTSCALE", 1) != 0 { 1.0 / s } else { 1.0 };
    let ctx = || Ctx { lt_factor, ..top_ctx(d, xf, VIEWPORT_CONTENT, &vp.frozen_layers, &vp.layer_colors) };
    let seen = |me: &Entity| {
        if matches!(me.kind, EntityKind::Viewport(_)) {
            return false;
        }
        if matches!(me.kind, EntityKind::Ray(_) | EntityKind::XLine(_)) {
            return true;
        }
        let eb = cadcraft_doc::entity_bounds(d, me, 0);
        eb.is_empty() || eb.intersects(&win)
    };
    // One expansion budget for the whole build: seed it with what the enclosing list has already
    // spent so a viewport's contents cannot start over with a fresh allowance and exceed the
    // documented limit (CWE-400). Both the serial sub-builder and the parallel chunks share it.
    let budget = std::sync::atomic::AtomicUsize::new(b.list.expanded);
    let mut sub = Builder {
        list: DisplayList::default(),
        opts: b.opts,
        plotting: b.plotting,
        styles: b.styles,
        named: None,
        shared_budget: Some(&budget),
        transparency: 0,
        points: b.points,
        max_points: b.max_points,
        snaps_only: false,
    };
    #[cfg(not(target_arch = "wasm32"))]
    let parallel = !b.plotting && b.styles.is_none() && d.model.len() >= 2 * PARALLEL_CHUNK;
    #[cfg(target_arch = "wasm32")]
    let parallel = false;
    if parallel {
        #[cfg(not(target_arch = "wasm32"))]
        {
            // As `build_parallel`: chunks built side by side, joined in drawing order.
            use rayon::prelude::*;
            let ents: Vec<&std::sync::Arc<Entity>> = d.model.iter().collect();
            let (opts, points, max_points) = (b.opts, b.points, b.max_points);
            let parts: Vec<DisplayList> = ents
                .par_chunks(PARALLEL_CHUNK)
                .map(|chunk| {
                    let mut part = Builder {
                        list: DisplayList::default(),
                        opts,
                        plotting: false,
                        styles: None,
                        named: None,
                        shared_budget: Some(&budget),
                        transparency: 0,
                        points,
                        max_points,
                        snaps_only: false,
                    };
                    let c = ctx();
                    for me in chunk.iter().filter(|me| seen(me)) {
                        entity(&mut part, &c, me);
                    }
                    part.list
                })
                .collect();
            for part in parts {
                append(&mut sub.list, part);
            }
        }
    } else {
        let c = ctx();
        for me in d.model.iter().filter(|me| seen(me)) {
            entity(&mut sub, &c, me);
        }
    }
    // Plotted text glyphs move to the layout's list; their outlines carry the glyph's index there
    // in their handle's high half until clipped (see `take_viewport_layers`).
    for t in std::mem::take(&mut sub.list.texts) {
        let k = b.list.texts.len() as u64 + 1;
        if let Some(p) = sub.list.prims.get_mut(t.prim as usize)
            && k < u64::from(u32::MAX)
        {
            p.handle = Handle(p.handle.0 | (k << 32));
            b.list.texts.push(PlotText { prim: PlotText::NONE, ..t });
        }
    }
    // The contents are in paper units now: draw their pens before clipping.
    pen::apply(&mut sub.list, unit_mm);
    b.list.expanded = b.list.expanded.saturating_add(sub.list.expanded);
    b.list.truncated |= sub.list.truncated;
    append_clipped(&mut b.list, &sub.list, clip);
}

/// The clip region of a clipped viewport: the outline of its clip boundary entity, when that
/// is a closed shape in the drawing. `None` for a rectangular viewport (or a dangling,
/// unusable or self-referencing boundary, which falls back to the rectangle).
pub fn viewport_clip(d: &Drawing, handle: Handle, vp: &cadcraft_doc::Viewport) -> Option<clip::PolyClip> {
    let h = vp.clip.filter(|h| *h != handle)?;
    let be = d.entity(h)?;
    clip::PolyClip::new(&clip::boundary_polygon(&be.kind)?)
}

/// `list` clipped to `clip` (a clipped viewport's outline in the list's own units), for a list
/// drawn inside a clipped viewport apart from its contents.
pub fn clip_list(list: &DisplayList, clip: &clip::PolyClip) -> DisplayList {
    let mut out = DisplayList::new(list.heights.is_some());
    append_clipped(&mut out, list, &Clip::Poly(clip));
    out
}

/// What viewport contents are clipped to.
pub(crate) enum Clip<'a> {
    /// The viewport rectangle (the fast path).
    Rect(Bounds2),
    /// A clip boundary's outline.
    Poly(&'a clip::PolyClip),
    /// Outside a clip boundary's outline (an inverted XCLIP).
    Outside(&'a clip::PolyClip),
}

impl Clip<'_> {
    fn contains(&self, point: Vec2) -> bool {
        match self {
            Self::Rect(rect) => rect.contains(point),
            Self::Poly(poly) => poly.contains(point),
            Self::Outside(poly) => !poly.contains(point),
        }
    }

    fn line_pieces(&self, line: &[Vec2]) -> Vec<Vec<Vec2>> {
        match self {
            Self::Rect(rect) => clip::clip_polyline(line, rect),
            Self::Poly(poly) => poly.clip_polyline(line),
            Self::Outside(poly) => poly.clip_polyline_outside(line),
        }
    }
}

fn clip_snaps(dst: &mut DisplayList, src: &DisplayList, clip: &Clip) {
    for &snap in &src.spatial_snaps {
        let [a, b] = snap.positions().map(|p| p.xy());
        if let SpatialSnap::Segment { ends, range } = snap
            && a != b
        {
            let dir = b - a;
            let parameter = |p: Vec2| range[0] + (range[1] - range[0]) * ((p - a).dot(dir) / dir.dot(dir)).clamp(0.0, 1.0);
            for piece in clip.line_pieces(&[a, b]) {
                if let Some((first, last)) = piece.first().zip(piece.last()) {
                    dst.spatial_snaps.push(SpatialSnap::Segment { ends, range: [parameter(*first), parameter(*last)] });
                }
            }
        } else if clip.contains(a) {
            dst.spatial_snaps.push(snap);
        }
    }
}

/// Append `src` to `dst`, clipped to `clip`.
pub(crate) fn append_clipped(dst: &mut DisplayList, src: &DisplayList, clip: &Clip) {
    clip_snaps(dst, src, clip);
    for p in &src.prims {
        clip_prim(dst, src, p, clip);
    }
}

/// Append `src` to `dst` clipped to `clip`, keeping each primitive's plot style pen and, for a
/// glyph that the clip leaves whole, its plotted text (an XCLIP-clipped block reference).
pub(crate) fn append_clipped_keep(dst: &mut DisplayList, src: &DisplayList, clip: &Clip) {
    clip_snaps(dst, src, clip);
    let texts: std::collections::HashMap<usize, &PlotText> =
        src.texts.iter().filter(|t| t.prim != PlotText::NONE && t.prim != PlotText::CLIPPED).map(|t| (t.prim as usize, t)).collect();
    for (i, p) in src.prims.iter().enumerate() {
        let first = dst.prims.len();
        clip_prim(dst, src, p, clip);
        // Each piece keeps its source's layer (block contents not on layer 0 keep theirs).
        if let Some(layer) = src.prim_layers.get(i).copied() {
            for k in first..dst.prims.len() {
                dst.prim_layers.resize(k, NO_LAYER);
                dst.prim_layers.push(layer);
            }
        }
        if let Some(pen) = src.pens.get(i).copied().filter(|pen| !pen.is_object()) {
            for k in first..dst.prims.len() {
                dst.pens.resize(k, Pen::default());
                dst.pens.push(pen);
            }
        }
        if dst.prims.len() == first + 1
            && let (Some(q), Some(t)) = (dst.prims.get(first), texts.get(&i))
            && q.kind == p.kind
            && dst.points(q) == src.points(p)
            && let Ok(prim) = u32::try_from(first)
        {
            dst.texts.push(PlotText { prim, ..(**t).clone() });
        }
    }
}

/// Append primitive `p` of `src` to `dst`, clipped to `clip`.
fn clip_prim(dst: &mut DisplayList, src: &DisplayList, p: &DPrim, clip: &Clip) {
    let z = src.elevations(p);
    if dst.heights.is_none() || z.is_empty() {
        clip_plan_prim(dst, src, p, clip);
        return;
    }
    let first = dst.prims.len();
    let xy = src.points(p);
    if matches!(p.kind, Kind::Image { .. }) {
        clip_plan_prim(dst, src, p, clip);
        if dst.prims.len() != first
            && let Some(heights) = &mut dst.heights
        {
            heights.verts.extend_from_slice(z);
        }
        return;
    }
    let inside = |q: Vec2| clip.contains(q);
    let line_height = |q: Vec2, a: Vec2, direction: Vec2, za: f64, dz: f64| {
        let den = direction.dot(direction);
        if den == 0.0 { za } else { za + (q - a).dot(direction) * dz / den }
    };
    match p.kind {
        Kind::Infinite { .. } if xy[1] == Vec2::ZERO => {
            if inside(xy[0]) {
                push_raw(dst, p, p.kind, xy);
                if let Some(h) = &mut dst.heights {
                    h.verts.extend_from_slice(z);
                }
            }
        }
        Kind::Infinite { .. } => {
            clip_plan_prim(dst, src, p, clip);
            extend_heights(dst, first, |q| line_height(q, xy[0], xy[1], z[0], z[1]));
        }
        Kind::Polyline if z.iter().any(|v| *v != z[0]) => {
            for i in 0..xy.len().saturating_sub(1) {
                let first = dst.prims.len();
                if xy[i] == xy[i + 1] && inside(xy[i]) {
                    push_raw(dst, p, p.kind, &xy[i..i + 2]);
                    if let Some(h) = &mut dst.heights {
                        h.verts.extend_from_slice(&z[i..i + 2]);
                    }
                    continue;
                }
                clip_plan_prim(dst, src, &DPrim { start: p.start + i as u32, len: 2, ..*p }, clip);
                extend_heights(dst, first, |q| line_height(q, xy[i], xy[i + 1] - xy[i], z[i], z[i + 1] - z[i]));
            }
        }
        Kind::Tris | Kind::Mask if z.iter().any(|v| *v != z[0]) => {
            for i in (0..xy.len()).step_by(3) {
                let Some((triangle, elevations)) = xy.get(i..i + 3).zip(z.get(i..i + 3)) else { break };
                let first = dst.prims.len();
                let (u, v) = (triangle[1] - triangle[0], triangle[2] - triangle[0]);
                let det = u.cross(v);
                if det == 0.0 {
                    clip_vertical_triangle(dst, p, triangle, elevations, clip, &inside);
                    continue;
                }
                clip_plan_prim(dst, src, &DPrim { start: p.start + i as u32, len: 3, ..*p }, clip);
                extend_heights(dst, first, |q| {
                    let w = q - triangle[0];
                    elevations[0] + (w.cross(v) * (elevations[1] - elevations[0]) + u.cross(w) * (elevations[2] - elevations[0])) / det
                });
            }
        }
        _ => {
            clip_plan_prim(dst, src, p, clip);
            extend_heights(dst, first, |_| z[0]);
        }
    }
}

fn extend_heights(dst: &mut DisplayList, first: usize, value: impl Fn(Vec2) -> f64) {
    let Some(heights) = &mut dst.heights else { return };
    for p in &dst.prims[first..] {
        let verts = if matches!(p.kind, Kind::Tris | Kind::Mask) { &dst.tris } else { &dst.verts };
        if let Some(points) = verts.get(p.start as usize..(p.start + p.len) as usize) {
            heights.pool_mut(p.kind).extend(points.iter().copied().map(&value));
        }
    }
}

fn clip_vertical_triangle(dst: &mut DisplayList, p: &DPrim, xy: &[Vec2], z: &[f64], clip: &Clip, inside: &impl Fn(Vec2) -> bool) {
    use cadcraft_geom::Vec3;
    let points = [Vec3::new(xy[0].x, xy[0].y, z[0]), Vec3::new(xy[1].x, xy[1].y, z[1]), Vec3::new(xy[2].x, xy[2].y, z[2])];
    let emit = |dst: &mut DisplayList, points: &[Vec3]| {
        if points.len() < 3 {
            return;
        }
        let first = dst.prims.len();
        push_raw(dst, p, p.kind, &points.iter().map(|v| v.xy()).collect::<Vec<_>>());
        if dst.prims.len() != first
            && let Some(h) = &mut dst.heights
        {
            h.tris.extend(points.iter().map(|v| v.z));
        }
    };
    let a = xy[0].min(xy[1]).min(xy[2]);
    let c = xy[0].max(xy[1]).max(xy[2]);
    let along_x = c.x - a.x >= c.y - a.y;
    let axis = |v: Vec2| if along_x { v.x } else { v.y };
    if a == c {
        if inside(a) {
            emit(dst, &points);
        }
        return;
    }
    let low = xy.iter().copied().min_by(|a, b| axis(*a).total_cmp(&axis(*b))).unwrap_or(a);
    let high = xy.iter().copied().max_by(|a, b| axis(*a).total_cmp(&axis(*b))).unwrap_or(c);
    let line = [low, high];
    let pieces = clip.line_pieces(&line);
    for piece in pieces {
        let Some((a, c)) = piece.first().zip(piece.last()) else { continue };
        let mut polygon = points.to_vec();
        for (edge, side) in [(axis(*a).min(axis(*c)), 1.0), (axis(*a).max(axis(*c)), -1.0)] {
            let mut next = Vec::new();
            for (a, c) in polygon.iter().zip(polygon.iter().cycle().skip(1)) {
                let da = (axis(a.xy()) - edge) * side;
                let dc = (axis(c.xy()) - edge) * side;
                if da >= 0.0 {
                    next.push(*a);
                }
                if (da < 0.0) != (dc < 0.0) {
                    next.push(*a + (*c - *a) * (da / (da - dc)));
                }
            }
            polygon = next;
        }
        if let Some(a) = polygon.first().copied() {
            for edge in polygon[1..].windows(2) {
                emit(dst, &[a, edge[0], edge[1]]);
            }
        }
    }
}

fn clip_plan_prim(dst: &mut DisplayList, src: &DisplayList, p: &DPrim, clip: &Clip) {
    {
        let pts = src.points(p);
        match (p.kind, clip) {
            (Kind::Polyline, Clip::Rect(rect)) => {
                for piece in clip::clip_polyline(pts, rect) {
                    push_raw(dst, p, Kind::Polyline, &piece);
                }
            }
            (Kind::Polyline, Clip::Poly(c)) => {
                for piece in c.clip_polyline(pts) {
                    push_raw(dst, p, Kind::Polyline, &piece);
                }
            }
            (Kind::Polyline, Clip::Outside(c)) => {
                for piece in c.clip_polyline_outside(pts) {
                    push_raw(dst, p, Kind::Polyline, &piece);
                }
            }
            (Kind::Tris | Kind::Mask, Clip::Rect(rect)) => {
                let t = clip::clip_triangles(pts, rect);
                push_raw(dst, p, p.kind, &t);
            }
            (Kind::Tris | Kind::Mask, Clip::Poly(c)) => {
                let t = c.clip_triangles(pts);
                push_raw(dst, p, p.kind, &t);
            }
            (Kind::Tris | Kind::Mask, Clip::Outside(c)) => {
                let t = c.clip_triangles_outside(pts);
                push_raw(dst, p, p.kind, &t);
            }
            (Kind::Point, _) => {
                let inside = |q: Vec2| match clip {
                    Clip::Rect(rect) => rect.contains(q),
                    Clip::Poly(c) => c.contains(q),
                    Clip::Outside(c) => !c.contains(q),
                };
                if let Some(q) = pts.first()
                    && inside(*q)
                {
                    push_raw(dst, p, Kind::Point, &[*q]);
                }
            }
            (Kind::Image { index }, _) => {
                if let Some(img) = src.images.get(index as usize) {
                    push_clipped_image(dst, p, img, pts, clip);
                }
            }
            (Kind::Infinite { ray }, Clip::Rect(rect)) => {
                if let (Some(base), Some(dir)) = (pts.first(), pts.get(1))
                    && let Some((a, c)) = clip::clip_infinite(*base, *dir, ray, rect)
                {
                    push_raw(dst, p, Kind::Polyline, &[a, c]);
                }
            }
            (Kind::Infinite { ray }, Clip::Poly(c)) => {
                if let (Some(base), Some(dir)) = (pts.first(), pts.get(1)) {
                    for piece in c.clip_infinite(*base, *dir, ray) {
                        push_raw(dst, p, Kind::Polyline, &piece);
                    }
                }
            }
            (Kind::Infinite { ray }, Clip::Outside(c)) => {
                if let (Some(base), Some(dir)) = (pts.first(), pts.get(1)) {
                    for piece in c.clip_infinite_outside(*base, *dir, ray) {
                        push_raw(dst, p, Kind::Polyline, &piece);
                    }
                }
            }
        }
    }
}

/// Append image `img` (primitive `p`, corners `corners`) to `dst`, to be cut to `clip` when it
/// is drawn: the picture is not cut here, the outline is added to those it carries.
fn push_clipped_image(dst: &mut DisplayList, p: &DPrim, img: &DImage, corners: &[Vec2], clip: &Clip) {
    let (outline, area) = match clip {
        Clip::Rect(rect) => (vec![rect.min, Vec2::new(rect.max.x, rect.min.y), rect.max, Vec2::new(rect.min.x, rect.max.y)], *rect),
        Clip::Poly(c) => (c.polygon().to_vec(), c.bounds()),
        // An image can only be cut to the inside of an outline: under an inverted XCLIP it is
        // drawn whole, which shows too much rather than losing any of it.
        Clip::Outside(_) => {
            let Ok(index) = u32::try_from(dst.images.len()) else { return };
            dst.images.push(img.clone());
            push_raw(dst, p, Kind::Image { index }, corners);
            return;
        }
    };
    let own = Bounds2::from_points(corners.iter().copied());
    if !own.intersects(&area) {
        return;
    }
    let Ok(index) = u32::try_from(dst.images.len()) else { return };
    let mut img = img.clone();
    img.clips.push(outline);
    dst.images.push(img);
    // The list's bounds take only the part inside the clip, as for the other primitives.
    let before = dst.bounds;
    push_raw(dst, p, Kind::Image { index }, corners);
    dst.bounds = before;
    dst.bounds.add(own.min.max(area.min));
    dst.bounds.add(own.max.min(area.max));
}

fn push_raw(dst: &mut DisplayList, p: &DPrim, kind: Kind, pts: &[Vec2]) {
    let min = match kind {
        Kind::Polyline => 2,
        Kind::Tris | Kind::Mask => 3,
        _ => 1,
    };
    if pts.len() < min {
        return;
    }
    let tris = matches!(kind, Kind::Tris | Kind::Mask);
    let start = if tris { dst.tris.len() } else { dst.verts.len() } as u32;
    for q in pts {
        dst.bounds.add(*q);
    }
    if tris {
        dst.tris.extend_from_slice(pts);
    } else {
        dst.verts.extend_from_slice(pts);
    }
    dst.prims.push(DPrim {
        handle: p.handle,
        color: p.color,
        aci7: p.aci7,
        lw: p.lw,
        transparency: p.transparency,
        kind,
        start,
        len: pts.len() as u32,
    });
}
