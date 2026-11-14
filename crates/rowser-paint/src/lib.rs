//! rowser-paint: display list → BGRA pixel buffer (premultiplied-friendly;
//! we emit alpha=255 everywhere for now).
//!
//! Text is drawn by blending cached glyph coverage bitmaps with the run
//! color. Underline/strikethrough are plain rects under/over the baseline.

use rowser_image::Image;
use rowser_layout::colorfx::Effect;
use rowser_layout::xform::{items_bbox, Mat};
use rowser_layout::{offset_item, Color, DisplayItem};
use rowser_text::{Font, FontCache, Raster};
use std::collections::HashMap;
use std::sync::Mutex;

pub mod fx;
pub mod gpu;
use fx::Clip;

pub struct Canvas {
    pub width: u32,
    pub height: u32,
    /// BGRA bytes
    pub pixels: Vec<u8>,
    /// Active overflow-clip stack (DisplayItem::PushClip/PopClip).
    pub clips: Vec<Clip>,
    /// Rows the canvas will actually write `[y0, y1)` — band workers set
    /// it to their strip; `(0, height)` for a normal full-canvas paint.
    /// Applied only at final per-pixel write sites, so intermediate
    /// buffers (shadow masks, effect tiles) stay byte-identical to a
    /// full serial render.
    pub band: (i32, i32),
}

impl Canvas {
    pub fn new(width: u32, height: u32, bg: Color) -> Self {
        let mut px = vec![0u8; (width * height * 4) as usize];
        fill(&mut px, width, height, 0, 0, width, height, bg, &[]);
        Self {
            width,
            height,
            pixels: px,
            clips: Vec::new(),
            band: (0, height as i32),
        }
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: u32, h: u32, color: Color) {
        // Band rows only: clamping the rect to `band` first is a pure
        // row-range intersection — painted pixels inside it are unchanged.
        let y1 = (y + h as i32).min(self.band.1);
        let y = y.max(self.band.0);
        if y1 <= y {
            return;
        }
        fill(
            &mut self.pixels,
            self.width,
            self.height,
            x,
            y,
            w,
            (y1 - y) as u32,
            color,
            &self.clips,
        );
    }

    /// Filled rect with uniform rounded corners (quarter-circle arcs).
    pub fn fill_rounded(&mut self, x: i32, y: i32, w: u32, h: u32, r: f32, color: Color) {
        let r = r.min(w as f32 / 2.0).min(h as f32 / 2.0).max(0.0);
        if r < 0.5 {
            return self.fill_rect(x, y, w, h, color);
        }
        // side strips + center span
        let ri = r.ceil() as i32;
        self.fill_rect(x + ri, y, w.saturating_sub(2 * ri as u32), h, color);
        self.fill_rect(x, y + ri, w, h.saturating_sub(2 * ri as u32), color);
        // four corner quarter-discs, one fill per scanline: dist<=r is
        // monotone in |dx|, so each row is a single centered run — nudge
        // the float estimate to the boundary the sqrt test picks
        for (cx, cy) in [
            (x + ri, y + ri),
            (x + w as i32 - ri, y + ri),
            (x + ri, y + h as i32 - ri),
            (x + w as i32 - ri, y + h as i32 - ri),
        ] {
            for dy in -ri..=ri {
                let mut dx = ((r * r - (dy * dy) as f32).max(0.0)).sqrt() as i32;
                while dx >= 0 && ((dx * dx + dy * dy) as f32).sqrt() > r {
                    dx -= 1;
                }
                while dx < ri && (((dx + 1) * (dx + 1) + dy * dy) as f32).sqrt() <= r {
                    dx += 1;
                }
                if dx >= 0 {
                    self.fill_rect(cx - dx, cy + dy, (2 * dx + 1) as u32, 1, color);
                }
            }
        }
    }

    /// Filled rect with per-corner elliptical radii — scanline fill: each
    /// row is shortened where a corner ellipse cuts it. Corner order
    /// [top-left, top-right, bottom-right, bottom-left], each (rx, ry).
    pub fn fill_rounded_radii(
        &mut self,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        radii: [[f32; 2]; 4],
        color: Color,
    ) {
        let (wf, hf) = (w as f32, h as f32);
        let mut r = radii;
        // scale corners so adjacent radii never overshoot a shared edge
        let mut f = 1.0f32;
        for (a, b, len, ax) in [(0, 1, wf, 0), (2, 3, wf, 0), (0, 3, hf, 1), (1, 2, hf, 1)] {
            let s = r[a][ax] + r[b][ax];
            if s > len && s > 0.0 {
                f = f.min(len / s);
            }
        }
        if f < 1.0 {
            for c in r.iter_mut() {
                c[0] *= f;
                c[1] *= f;
            }
        }
        if r.iter().all(|c| c[0] < 0.5 && c[1] < 0.5) {
            return self.fill_rect(x, y, w, h, color);
        }
        // consecutive rows sharing a span merge into one fill
        let mut run: Option<(i32, i32, i32)> = None; // (xl, xr, first dy)
        for dy in 0..h as i32 {
            let t = dy as f32 + 0.5;
            let (mut xl, mut xr) = (x, x + w as i32);
            // top corners cut rows [0, ry); bottom cut rows (h - ry, h)
            for (cx, corner) in [(0usize, 0usize), (1, 1)] {
                let [rx, ry] = r[corner];
                if rx > 0.0 && ry > 0.0 && t < ry {
                    let dyy = ry - t;
                    let cut = rx * (1.0 - (dyy * dyy) / (ry * ry)).max(0.0).sqrt();
                    if cx == 0 {
                        xl = xl.max(x + (rx - cut).ceil() as i32);
                    } else {
                        xr = xr.min(x + w as i32 - (rx - cut).ceil() as i32);
                    }
                }
            }
            let tb = hf - t;
            for (cx, corner) in [(0usize, 3usize), (1, 2)] {
                let [rx, ry] = r[corner];
                if rx > 0.0 && ry > 0.0 && tb < ry {
                    let dyy = ry - tb;
                    let cut = rx * (1.0 - (dyy * dyy) / (ry * ry)).max(0.0).sqrt();
                    if cx == 0 {
                        xl = xl.max(x + (rx - cut).ceil() as i32);
                    } else {
                        xr = xr.min(x + w as i32 - (rx - cut).ceil() as i32);
                    }
                }
            }
            match (run, xr > xl) {
                (Some((l, rr, _)), true) if l == xl && rr == xr => {}
                (_, true) => {
                    if let Some((l, rr, s)) = run.take() {
                        self.fill_rect(l, y + s, (rr - l) as u32, (dy - s) as u32, color);
                    }
                    run = Some((xl, xr, dy));
                }
                (Some((l, rr, s)), false) => {
                    run = None;
                    self.fill_rect(l, y + s, (rr - l) as u32, (dy - s) as u32, color);
                }
                _ => {}
            }
        }
        if let Some((l, rr, s)) = run {
            self.fill_rect(l, y + s, (rr - l) as u32, (h as i32 - s) as u32, color);
        }
    }

    /// Blend a coverage bitmap with `color` onto the canvas at (x, y).
    pub fn blend_alpha(&mut self, x: i32, y: i32, w: u32, h: u32, alpha: &[u8], color: Color) {
        let (cw, ch) = (self.width as i32, self.height as i32);
        let (sx0, sy0, sx1, sy1) = fx::scissor(&self.clips, cw, ch);
        // Corner-rounded clips need a per-pixel test; rect-only clips are
        // pure bounds and fold into the box exactly (see `fill`).
        let rounded = self
            .clips
            .iter()
            .any(|c| c.radii.iter().any(|r| r[0] > 0.0 && r[1] > 0.0));
        let (mut ex0, mut ey0, mut ex1, mut ey1) = (sx0, sy0, sx1, sy1);
        if !rounded {
            for c in &self.clips {
                ex0 = ex0.max((c.x - 0.5).ceil() as i32);
                ey0 = ey0.max((c.y - 0.5).ceil() as i32);
                ex1 = ex1.min((c.x + c.w - 0.5).ceil() as i32);
                ey1 = ey1.min((c.y + c.h - 0.5).ceil() as i32);
            }
        }
        // band rows only — same intersection as serial + row clamp
        ey0 = ey0.max(self.band.0);
        ey1 = ey1.min(self.band.1);
        // glyph box ∩ scissor, in source coordinates
        let (gx0, gx1) = ((ex0 - x).max(0), (w as i32).min(ex1 - x));
        let (gy0, gy1) = ((ey0 - y).max(0), (h as i32).min(ey1 - y));
        if gx1 <= gx0 || gy1 <= gy0 {
            return;
        }
        let [b, g, r, _] = color.to_le_bytes();
        let (w, stride) = (w as usize, cw as usize * 4);
        let (px, clips) = (&mut self.pixels, &self.clips);
        for gy in gy0..gy1 {
            let dy = y + gy;
            let arow = &alpha[gy as usize * w + gx0 as usize..gy as usize * w + gx1 as usize];
            let di = dy as usize * stride + (x + gx0) as usize * 4;
            let drow = &mut px[di..di + arow.len() * 4];
            for (k, &a) in arow.iter().enumerate() {
                if a == 0 {
                    continue;
                }
                if rounded
                    && !clips.iter().all(|c| {
                        c.contains((x + gx0 + k as i32) as f32 + 0.5, dy as f32 + 0.5)
                    })
                {
                    continue;
                }
                let d = &mut drow[k * 4..k * 4 + 4];
                if a == 255 {
                    d.copy_from_slice(&[b, g, r, 255]);
                    continue;
                }
                let (a, inv) = (a as u32, 255 - a as u32);
                d[0] = ((b as u32 * a + d[0] as u32 * inv) / 255) as u8;
                d[1] = ((g as u32 * a + d[1] as u32 * inv) / 255) as u8;
                d[2] = ((r as u32 * a + d[2] as u32 * inv) / 255) as u8;
                d[3] = 255;
            }
        }
    }
}

fn fill(
    px: &mut [u8],
    w: u32,
    h: u32,
    x: i32,
    y: i32,
    rw: u32,
    rh: u32,
    color: Color,
    clips: &[Clip],
) {
    let (w, h) = (w as i32, h as i32);
    let (sx0, sy0, sx1, sy1) = fx::scissor(clips, w, h);
    let (mut x0, mut y0) = (x.max(0).max(sx0), y.max(0).max(sy0));
    let (mut x1, mut y1) = ((x + rw as i32).min(w).min(sx1), (y + rh as i32).min(h).min(sy1));
    // Corner-rounded clips need a per-pixel test; rect-only clips are
    // pure bounds, so contains() folds into the box exactly.
    let rounded = clips
        .iter()
        .any(|c| c.radii.iter().any(|r| r[0] > 0.0 && r[1] > 0.0));
    if !rounded {
        for c in clips {
            x0 = x0.max((c.x - 0.5).ceil() as i32);
            y0 = y0.max((c.y - 0.5).ceil() as i32);
            x1 = x1.min((c.x + c.w - 0.5).ceil() as i32);
            y1 = y1.min((c.y + c.h - 0.5).ceil() as i32);
        }
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let mut pat = color.to_le_bytes();
        pat[3] = 255;
        let (stride, span) = (w as usize * 4, (x1 - x0) as usize * 4);
        let first = y0 as usize * stride + x0 as usize * 4;
        fill_span(&mut px[first..first + span], pat);
        if x0 == 0 && span == stride {
            // contiguous run — double-copy the painted row
            let total = (y1 - y0) as usize * span;
            let mut done = span;
            while done < total {
                let n = done.min(total - done);
                px.copy_within(first..first + n, first + done);
                done += n;
            }
        } else {
            for yy in y0 + 1..y1 {
                px.copy_within(first..first + span, yy as usize * stride + x0 as usize * 4);
            }
        }
        return;
    }
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let mut pat = color.to_le_bytes();
    pat[3] = 255;
    for yy in y0..y1 {
        for xx in x0..x1 {
            if !clips
                .iter()
                .all(|c| c.contains(xx as f32 + 0.5, yy as f32 + 0.5))
            {
                continue;
            }
            let i = ((yy * w + xx) * 4) as usize;
            px[i..i + 4].copy_from_slice(&pat);
        }
    }
}

/// Fill `row` (a multiple of 4 long) with the repeating pixel `pat`.
fn fill_span(row: &mut [u8], pat: [u8; 4]) {
    let mut block = [0u8; 1024];
    for c in block.chunks_exact_mut(4) {
        c.copy_from_slice(&pat);
    }
    for c in row.chunks_mut(1024) {
        let n = c.len();
        c.copy_from_slice(&block[..n]);
    }
}

/// Font access during painting. Serial paint borrows the cache directly
/// (zero overhead); band workers share it behind a mutex and memo glyph
/// rasters per band — alpha blits never hold the lock.
#[doc(hidden)]
pub enum Fonts<'a> {
    /// Serial paint: straight through to the caller's cache.
    Direct(&'a mut FontCache),
    /// Band worker: shaping/measure under the lock; rasters cloned once
    /// per band per (font, glyph, size) into the local map.
    Shared(
        &'a Mutex<&'a mut FontCache>,
        HashMap<(usize, u16, u32), Option<Raster>>,
    ),
}

impl Fonts<'_> {
    /// Run `f` with the resolved `&mut Font` — serially under the band
    /// mutex when shared.
    fn with_font<R>(&mut self, idx: usize, f: impl FnOnce(&mut Font) -> R) -> Option<R> {
        match self {
            Fonts::Direct(c) => c.font(idx).map(f),
            Fonts::Shared(m, _) => {
                let mut g = m.lock().unwrap();
                g.font(idx).map(f)
            }
        }
    }

    /// Cached glyph raster at `px` (quarter-pixel quantized). Shared
    /// workers clone it out of the shared cache into a per-band memo so
    /// the lock is held for a hash lookup, not the blit.
    fn raster(&mut self, idx: usize, gid: u16, px: f32) -> Option<&Raster> {
        match self {
            Fonts::Direct(c) => c.raster(idx, gid, px),
            Fonts::Shared(m, local) => local
                .entry((idx, gid, (px * 4.0) as u32))
                .or_insert_with(|| {
                    let mut g = m.lock().unwrap();
                    g.raster(idx, gid, px).map(|r| Raster {
                        width: r.width,
                        height: r.height,
                        bearing_x: r.bearing_x,
                        bearing_y: r.bearing_y,
                        alpha: r.alpha.clone(),
                    })
                })
                .as_ref(),
        }
    }
}

/// Hard ceiling on band workers — replay cost is linear in bands.
const MAX_BANDS: usize = 8;
/// Fork/join + per-band full-canvas alloc + full-list replay only pays off
/// on genuinely heavy frames (~2 MP *and* a long list — a 1 MP page with
/// ~3k small items paints in ~0.2 ms serially, well below band overhead).
const PAR_MIN_PIXELS: u64 = 2 * 1024 * 1024;
/// …and a long display list to split across the bands.
const PAR_MIN_ITEMS: usize = 2_000;
/// Thinner bands than this mostly re-pay replay overhead.
const MIN_BAND_ROWS: u32 = 64;

/// Worker count for band-parallel paint: `min(available_parallelism, 8,
/// height/64)`, and 1 (serial) when the frame is too small to benefit.
fn band_count(width: u32, height: u32, items: usize) -> usize {
    // Parallel only when the serial cost plausibly exceeds band overhead:
    // each band allocates a full-canvas buffer (~w*h*4) and replays the
    // whole list, so a big canvas with few items is cheaper serially.
    // Both bars must be cleared.
    if (width as u64 * height as u64) < PAR_MIN_PIXELS || items < PAR_MIN_ITEMS {
        return 1;
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(MAX_BANDS)
        .min((height / MIN_BAND_ROWS).max(1) as usize)
}

/// Band-parallel [`paint`]: identical output, rasterized by up to
/// [`MAX_BANDS`] scoped threads, each owning a horizontal strip.
///
/// Every band replays the whole display list (painter order + clip stack
/// preserved) into a private full-size buffer with `Canvas::band`
/// confining final writes to its rows. Write sites are pure per-pixel
/// functions of painter-order state, so each strip comes out
/// byte-identical to a serial paint; ops with intermediate buffers
/// (box-shadow masks, effect tiles) compute the same result serial does
/// and their out-of-band writes are simply dropped.
///
/// `Effect::Backdrop` reads canvas rows another band may own — presence
/// of one falls back to serial. Small canvases/lists stay serial too.
pub fn paint_par(
    items: &[DisplayItem],
    fonts: &mut FontCache,
    width: u32,
    height: u32,
    bg: Color,
    images: &HashMap<String, Image>,
) -> Vec<u8> {
    paint_banded(
        items,
        fonts,
        width,
        height,
        bg,
        images,
        band_count(width, height, items.len()),
    )
}

/// `paint` with an explicit band count (tests force `bands` on small
/// canvases where the heuristic would stay serial).
pub(crate) fn paint_banded(
    items: &[DisplayItem],
    fonts: &mut FontCache,
    width: u32,
    height: u32,
    bg: Color,
    images: &HashMap<String, Image>,
    bands: usize,
) -> Vec<u8> {
    if width == 0
        || height == 0
        || bands <= 1
        || items.iter().any(|i| {
            matches!(i, DisplayItem::PushEffect(Effect::Backdrop { .. }))
        })
    {
        return paint(items, fonts, width, height, bg, images);
    }
    prewarm(items, fonts);
    // `&mut *fonts` reborrows so the mutex's inner lifetime can shrink
    // to the scope below (Mutex<T> is invariant in T).
    let fonts = Mutex::new(&mut *fonts);
    let stride = width as usize * 4;
    let bh = height.div_ceil(bands as u32).max(1);
    let mut out = vec![0u8; width as usize * height as usize * 4];
    std::thread::scope(|s| {
        for (b, chunk) in out.chunks_mut(bh as usize * stride).enumerate() {
            let fonts = &fonts;
            s.spawn(move || {
                let y0 = b as u32 * bh;
                let y1 = (y0 + bh).min(height);
                // Private full-size buffer (calloc-lazy zeros): only the
                // band's rows are initialised to bg; anything written
                // outside the strip — unclipped shadow halos, tile
                // composites — is correct-but-discarded work.
                let mut cv = Canvas {
                    width,
                    height,
                    pixels: vec![0; width as usize * height as usize * 4],
                    clips: Vec::new(),
                    band: (y0 as i32, y1 as i32),
                };
                fill(&mut cv.pixels, width, height, 0, y0 as i32, width, y1 - y0, bg, &[]);
                let mut f = Fonts::Shared(fonts, HashMap::new());
                paint_span(&mut cv, items, &mut f, images, &Mat::IDENTITY);
                chunk.copy_from_slice(
                    &cv.pixels[y0 as usize * stride..y1 as usize * stride],
                );
            });
        }
    });
    out
}

/// Warm the shared glyph/measure caches single-threaded before the bands
/// fork, so a worker's raster lookup hits an occupied entry instead of
/// serializing the others on a fresh swash rasterization.
fn prewarm(items: &[DisplayItem], fonts: &mut FontCache) {
    let mut gids: Vec<u16> = Vec::new();
    for it in items {
        let DisplayItem::Text {
            text, font, size, ..
        } = it
        else {
            continue;
        };
        if let Some(f) = fonts.font(*font) {
            gids.extend(text.chars().map(|c| f.glyph_id(c)));
            f.measure(text, *size);
        }
        for &gid in &gids {
            fonts.raster(*font, gid, *size);
        }
        gids.clear();
    }
}

/// Paint a display list onto a fresh white canvas.
pub fn paint(
    items: &[DisplayItem],
    fonts: &mut FontCache,
    width: u32,
    height: u32,
    bg: Color,
    images: &HashMap<String, Image>,
) -> Vec<u8> {
    let mut canvas = Canvas::new(width, height, bg);
    paint_into(&mut canvas, items, fonts, images);
    canvas.pixels
}

pub fn paint_into(
    canvas: &mut Canvas,
    items: &[DisplayItem],
    fonts: &mut FontCache,
    images: &HashMap<String, Image>,
) {
    paint_list(canvas, items, &mut Fonts::Direct(fonts), images);
}

/// Paint `items` through an already-wrapped font handle — the inner
/// entry point for serial and band workers alike.
pub(crate) fn paint_list(
    canvas: &mut Canvas,
    items: &[DisplayItem],
    fonts: &mut Fonts<'_>,
    images: &HashMap<String, Image>,
) {
    paint_span(canvas, items, fonts, images, &Mat::IDENTITY);
}

/// Paint one display-item span. `m` maps the span's coordinates onto the
/// canvas — identity at the top level and inside each offscreen tile.
/// PushTransform consumes its whole sub-span: it renders through an
/// offscreen tile + affine blit, so nested transforms compose by
/// matrix-multiplying onto `m`.
fn paint_span(
    canvas: &mut Canvas,
    items: &[DisplayItem],
    fonts: &mut Fonts<'_>,
    images: &HashMap<String, Image>,
    m: &Mat,
) {
    let cw = canvas.width as f32;
    // vertical cull bounds: the writable rows — the band strip on band
    // canvases, the whole canvas otherwise (band ⊆ [0, h], so this is a
    // strict superset of the old `bot < 0 || top > ch` check and still
    // never culls state items — they get unbounded extents below)
    let (cb0, cb1) = (canvas.band.0 as f32, canvas.band.1 as f32);
    // scratch for shaped glyph runs — avoids a Vec per text item
    let mut glyphs: Vec<(u16, f32)> = Vec::new();
    let mut i = 0;
    while i < items.len() {
        let item = &items[i];
        if let DisplayItem::PushTransform { m: inner } = item {
            let total = m.mul(inner);
            // the matching PopTransform (nesting counts)
            let mut depth = 1i32;
            let mut j = i + 1;
            while j < items.len() && depth > 0 {
                match items[j] {
                    DisplayItem::PushTransform { .. } => depth += 1,
                    DisplayItem::PopTransform => depth -= 1,
                    _ => {}
                }
                j += 1;
            }
            paint_transformed(canvas, &items[i + 1..j - 1], &total, fonts, images);
            i = j;
            continue;
        }
        if matches!(item, DisplayItem::PopTransform) {
            i += 1;
            continue;
        }
        // Cull: items fully off-canvas can't write visible pixels. Text
        // gets a ±4em vertical band — glyph ink can overshoot the em box,
        // but never by multiples of it; its right edge is unbounded (pen
        // advances accumulate kerning). Effect markers never cull.
        let (top, bot, lft, rgt) = match item {
            DisplayItem::FillRect { x, y, w, h, .. }
            | DisplayItem::RoundedRect { x, y, w, h, .. }
            | DisplayItem::RoundedRect2 { x, y, w, h, .. }
            | DisplayItem::BorderSide { x, y, w, h, .. }
            | DisplayItem::Raster { x, y, w, h, .. }
            | DisplayItem::Image { x, y, w, h, .. }
            | DisplayItem::BgImage { x, y, w, h, .. } => (*y, *y + *h, *x, *x + *w),
            DisplayItem::Text { baseline, size, .. } => {
                (baseline - size * 4.0, baseline + size * 4.0, f32::MIN, f32::MAX)
            }
            // PushClip/PopClip/Shadow/PushEffect/PopEffect are never culled
            // — a skipped PushClip would un-clip its children, a shadow's
            // halo reaches beyond its box, and an effect group's bounds
            // can't be derived from a marker item.
            DisplayItem::PushClip { .. }
            | DisplayItem::PopClip
            | DisplayItem::Shadow { .. }
            | DisplayItem::PushEffect(_)
            | DisplayItem::PopEffect
            | DisplayItem::PushTransform { .. }
            | DisplayItem::PopTransform => (f32::MIN, f32::MAX, f32::MIN, f32::MAX),
        };
        if bot < cb0 || top > cb1 || rgt < 0.0 || lft > cw {
            i += 1;
            continue;
        }
        match item {
            DisplayItem::PushEffect(_) => {
                i = fx::process(canvas, items, i, fonts, images);
                continue;
            }
            DisplayItem::PopEffect => {
                i += 1;
                continue;
            }
            DisplayItem::RoundedRect {
                x,
                y,
                w,
                h,
                radius,
                color,
            } => {
                canvas.fill_rounded(*x as i32, *y as i32, *w as u32, *h as u32, *radius, *color);
            }
            DisplayItem::Raster { x, y, w, h, pixels } => {
                canvas.blit_pixels(pixels, *x as i32, *y as i32, *w as i32, *h as i32);
            }
            DisplayItem::Image {
                x,
                y,
                w,
                h,
                src,
                near,
            } => {
                if let Some(img) = images.get(src) {
                    canvas.blit(img, *x as i32, *y as i32, *w as i32, *h as i32, *near);
                } else {
                    canvas.fill_rect(
                        *x as i32,
                        *y as i32,
                        w.max(1.0) as u32,
                        h.max(1.0) as u32,
                        0xffe8e8e8,
                    );
                }
            }
            DisplayItem::BgImage {
                x,
                y,
                w,
                h,
                px,
                py,
                pw,
                ph,
                mode,
                sw,
                sh,
                fx,
                ax,
                fy,
                ay,
                rx,
                ry,
                src,
                near,
            } => {
                if let Some(img) = images.get(src) {
                    canvas.blit_tiled(
                        img, *x, *y, *w, *h, *px, *py, *pw, *ph, *mode, *sw, *sh, *fx, *ax, *fy,
                        *ay, *rx, *ry, *near,
                    );
                }
                // undecoded/failed image: paint nothing — the element's
                // background-color still shows through
            }
            DisplayItem::FillRect { x, y, w, h, color } => {
                canvas.fill_rect(*x as i32, *y as i32, *w as u32, *h as u32, *color);
            }
            DisplayItem::BorderSide {
                x,
                y,
                w,
                h,
                side,
                style,
                color,
            } => {
                fx::border_side(canvas, *x, *y, *w, *h, *side, *style, *color);
            }
            DisplayItem::RoundedRect2 {
                x,
                y,
                w,
                h,
                radii,
                color,
            } => {
                canvas
                    .fill_rounded_radii(*x as i32, *y as i32, *w as u32, *h as u32, *radii, *color);
            }
            DisplayItem::Shadow {
                x,
                y,
                w,
                h,
                dx,
                dy,
                blur,
                spread,
                radii,
                color,
                inset,
            } => {
                fx::box_shadow(
                    canvas, *x, *y, *w, *h, *dx, *dy, *blur, *spread, *radii, *color, *inset,
                );
            }
            DisplayItem::PushClip { x, y, w, h, radii } => {
                canvas.clips.push(Clip {
                    x: *x,
                    y: *y,
                    w: *w,
                    h: *h,
                    radii: *radii,
                });
            }
            DisplayItem::PopClip => {
                canvas.clips.pop();
            }
            DisplayItem::Text {
                x,
                baseline,
                text,
                font,
                size,
                color,
                deco,
                ls,
                href: _,
                sel: _,
            } => {
                // shape the whole run with one font lookup, then blit each
                // glyph — splits the &mut Font borrow from the raster cache
                glyphs.clear();
                let mut pen = *x;
                fonts.with_font(*font, |f| {
                    let upem = f.units_per_em() as f32;
                    let mut prev = 0u16;
                    for c in text.chars() {
                        let gid = f.glyph_id(c);
                        pen += f.kern(prev, gid) as f32 * size / upem;
                        glyphs.push((gid, pen));
                        pen += f.advance(gid, *size) + ls;
                        prev = gid;
                    }
                });
                for &(gid, gp) in &glyphs {
                    if let Some(r) = fonts.raster(*font, gid, *size) {
                        canvas.blend_alpha(
                            (gp + r.bearing_x as f32).round() as i32,
                            (*baseline - r.bearing_y as f32).round() as i32,
                            r.width,
                            r.height,
                            &r.alpha,
                            *color,
                        );
                    }
                }
                let end = pen;
                let dcol = deco.color.unwrap_or(*color);
                let thick = deco.thick.unwrap_or((size / 14.0).max(1.0));
                // line y centers: underline ~0.1em below baseline (+offset),
                // overline on the ascent, strike ~0.28em above baseline
                let lines = [
                    deco.underline()
                        .then_some(*baseline + 0.1 * size + deco.offset.unwrap_or(0.0)),
                    (deco.lines & 2 != 0).then_some(*baseline - size * 0.85),
                    deco.strike().then_some(*baseline - 0.28 * size),
                ];
                for ly in lines.into_iter().flatten() {
                    paint_deco_line(canvas, *x, ly, end - *x, thick, deco.style, dcol);
                }
            }
            DisplayItem::PushTransform { .. } | DisplayItem::PopTransform => {}
        }
        i += 1;
    }
}

/// Paint `span` (pre-transform coordinates) through matrix `m`: render it
/// into a transparent BGRA tile sized to its laid-out bounding box, then
/// blit the tile onto the canvas through `m`. Singular matrices paint
/// nothing; nested PushTransform inside the tile composes recursively.
fn paint_transformed(
    canvas: &mut Canvas,
    span: &[DisplayItem],
    m: &Mat,
    fonts: &mut Fonts<'_>,
    images: &HashMap<String, Image>,
) {
    if m.is_identity() {
        return paint_span(canvas, span, fonts, images, &Mat::IDENTITY);
    }
    if m.inverse().is_none() {
        return; // singular transform — the element has no painted area
    }
    // leaf-item bbox in pre-transform space (+ bleed for glyph ink/shadows)
    let Some((bx, by, bw, bh)) = items_bbox(span) else {
        return;
    };
    const PAD: f32 = 8.0;
    let (bx, by) = (bx - PAD, by - PAD);
    let (tw, th) = (
        (bw + 2.0 * PAD).ceil() as u32,
        (bh + 2.0 * PAD).ceil() as u32,
    );
    const MAX_TILE_PX: u64 = 64 * 1024 * 1024;
    if tw == 0 || th == 0 || tw as u64 * th as u64 > MAX_TILE_PX {
        return;
    }
    // tile pixel (x,y) ↔ pre-transform point (x+bx, y+by): shift the
    // span's items into tile space and paint them there
    let mut tile = Canvas {
        width: tw,
        height: th,
        pixels: vec![0; tw as usize * th as usize * 4],
        clips: Vec::new(),
        band: (0, th as i32),
    };
    let shifted: Vec<DisplayItem> = span
        .iter()
        .cloned()
        .map(|it| offset_item(it, -bx, -by))
        .collect();
    paint_span(&mut tile, &shifted, fonts, images, &Mat::IDENTITY);
    // canvas_pt = m(pre_pt) = m(tile_pt + (bx,by)) → m ∘ T(bx,by)
    let bm = m.mul(&Mat::translate(bx, by));
    canvas.blit_affine(&tile.pixels, tw, th, &bm);
}

/// One `text-decoration` line — solid/double/dotted/dashed/wavy at the
/// given y center. Wavy approximates the sine with a ±thick zigzag.
fn paint_deco_line(
    canvas: &mut Canvas,
    x0: f32,
    y: f32,
    w: f32,
    thick: f32,
    style: rowser_layout::DecoStyle,
    color: u32,
) {
    use rowser_layout::DecoStyle::*;
    let (x0, w, t) = (x0 as i32, w.max(0.0) as u32, thick.max(1.0) as u32);
    if w == 0 {
        return;
    }
    let yi = y as i32;
    match style {
        Solid => canvas.fill_rect(x0, yi - t as i32 / 2, w, t, color),
        Double => {
            let g = t.max(1);
            canvas.fill_rect(x0, yi - t as i32 / 2 - g as i32, w, t, color);
            canvas.fill_rect(x0, yi + t as i32 / 2 + g as i32, w, t, color);
        }
        Dotted => {
            let step = (t * 3).max(2);
            let mut x = x0;
            while (x - x0) < w as i32 {
                canvas.fill_rect(x, yi - t as i32 / 2, t, t, color);
                x += step as i32;
            }
        }
        Dashed => {
            let dash = (t * 4).max(4);
            let step = (t * 7).max(6);
            let mut x = x0;
            while (x - x0) < w as i32 {
                let dw = (dash as i32).min(w as i32 - (x - x0)).max(0) as u32;
                canvas.fill_rect(x, yi - t as i32 / 2, dw, t, color);
                x += step as i32;
            }
        }
        Wavy => {
            let step = (t * 2).max(2) as i32;
            let mut x = x0;
            let mut up = false;
            while (x - x0) < w as i32 {
                let yy = if up { yi - t as i32 } else { yi };
                canvas.fill_rect(x, yy, step.max(1) as u32, t, color);
                up = !up;
                x += step;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rowser_layout::{bgra, BLACK};

    #[test]
    fn paints_text() {
        let mut fonts = FontCache::new();
        let font = fonts.resolve("liberation sans", false, false).unwrap();
        let items = vec![
            DisplayItem::Text {
                x: 10.0,
                baseline: 50.0,
                text: "Hello".into(),
                font,
                size: 24.0,
                color: BLACK,
                deco: rowser_layout::TextDeco::NONE,
                ls: 0.0,
                href: None,
                sel: true,
            },
            DisplayItem::Image {
                x: 0.0,
                y: 0.0,
                w: 10.0,
                h: 10.0,
                src: "missing.png".into(),
                near: false,
            },
            DisplayItem::FillRect {
                x: 0.0,
                y: 0.0,
                w: 5.0,
                h: 5.0,
                color: bgra(255, 0, 0),
            },
        ];
        let px = paint(
            &items,
            &mut fonts,
            200,
            100,
            rowser_layout::WHITE,
            &HashMap::new(),
        );
        // some dark pixels must exist (the text)
        let dark = px.chunks(4).filter(|p| p[2] < 100).count();
        assert!(dark > 50);
        // red rect top-left
        assert_eq!(px[2], 255);
    }

    #[test]
    fn push_clip_clips_fill() {
        let mut fonts = FontCache::new();
        let items = vec![
            DisplayItem::PushClip {
                x: 0.0,
                y: 0.0,
                w: 10.0,
                h: 10.0,
                radii: [[0.0; 2]; 4],
            },
            DisplayItem::FillRect {
                x: 0.0,
                y: 0.0,
                w: 50.0,
                h: 50.0,
                color: bgra(255, 0, 0),
            },
            DisplayItem::PopClip,
        ];
        let px = paint(
            &items,
            &mut fonts,
            100,
            100,
            rowser_layout::WHITE,
            &HashMap::new(),
        );
        let at = |x: usize, y: usize| &px[(y * 100 + x) * 4..(y * 100 + x) * 4 + 3];
        // red pixel has blue channel 0; white bg has 255
        assert_eq!(at(5, 5)[0], 0, "inside clip painted red");
        assert_eq!(at(5, 5)[2], 255);
        assert_eq!(at(30, 30)[0], 255, "outside clip stays white");
        assert_eq!(at(30, 30)[2], 255);
    }

    #[test]
    fn dashed_border_has_gaps() {
        let mut fonts = FontCache::new();
        let items = vec![DisplayItem::BorderSide {
            x: 0.0,
            y: 0.0,
            w: 30.0,
            h: 4.0,
            side: rowser_layout::BorderEdge::Top,
            style: rowser_layout::BorderStyle::Dashed,
            color: bgra(0, 0, 0),
        }];
        let px = paint(
            &items,
            &mut fonts,
            40,
            10,
            rowser_layout::WHITE,
            &HashMap::new(),
        );
        // pixel dark when all channels < 128 (black dash vs white bg)
        let dark = |x: usize| (0..3).all(|ch| px[(2 * 40 + x) * 4 + ch] < 128);
        let (mut painted, mut empty) = (0, 0);
        for x in 0..30 {
            if dark(x) {
                painted += 1
            } else {
                empty += 1
            }
        }
        assert!(painted > 0 && empty > 0, "dashes and gaps both present");
    }

    #[test]
    fn double_border_two_bands() {
        let mut fonts = FontCache::new();
        let items = vec![DisplayItem::BorderSide {
            x: 0.0,
            y: 0.0,
            w: 30.0,
            h: 6.0,
            side: rowser_layout::BorderEdge::Top,
            style: rowser_layout::BorderStyle::Double,
            color: bgra(0, 0, 0),
        }];
        let px = paint(
            &items,
            &mut fonts,
            40,
            10,
            rowser_layout::WHITE,
            &HashMap::new(),
        );
        let dark = |y: usize| (0..3).all(|ch| px[(y * 40 + 15) * 4 + ch] < 128);
        let light = |y: usize| (0..3).all(|ch| px[(y * 40 + 15) * 4 + ch] > 200);
        assert!(dark(0) && dark(5), "two lines painted");
        assert!(light(2) || light(3), "gap between lines");
    }

    #[test]
    fn outer_shadow_blurs() {
        let mut fonts = FontCache::new();
        let items = vec![DisplayItem::Shadow {
            x: 20.0,
            y: 20.0,
            w: 20.0,
            h: 20.0,
            dx: 0.0,
            dy: 0.0,
            blur: 5.0,
            spread: 0.0,
            radii: [[0.0; 2]; 4],
            color: bgra(0, 0, 0),
            inset: false,
        }];
        let px = paint(
            &items,
            &mut fonts,
            80,
            80,
            rowser_layout::WHITE,
            &HashMap::new(),
        );
        let edge = px[(30 * 80 + 14) * 4 + 2]; // 6px left of the box — halo
        assert!(edge < 250 && edge > 100, "soft shadow halo present: {edge}");
        let far = px[(30 * 80 + 5) * 4 + 2]; // beyond blur reach — stays white
        assert!(far > 240, "no shadow beyond the halo: {far}");
    }

    #[test]
    fn rounded2_clips_corner() {
        let mut fonts = FontCache::new();
        let items = vec![DisplayItem::RoundedRect2 {
            x: 0.0,
            y: 0.0,
            w: 40.0,
            h: 40.0,
            radii: [[20.0, 20.0], [0.0, 0.0], [0.0, 0.0], [0.0, 0.0]],
            color: bgra(255, 0, 0),
        }];
        let px = paint(
            &items,
            &mut fonts,
            50,
            50,
            rowser_layout::WHITE,
            &HashMap::new(),
        );
        // blue channel: white bg 255, red fill 0
        assert_eq!(px[0], 255, "top-left corner cut to bg");
        let center = px[(20 * 50 + 20) * 4];
        assert_eq!(center, 0, "interior painted red");
    }

    #[test]
    fn rounded_rect_paints_disc_pixels() {
        // fill_rounded's batched corner rows must cover exactly the pixels
        // of the old per-pixel dist<=r disc loop
        let (x, y, w, h, r) = (5, 5, 30, 20, 6.3f32);
        let mut c = Canvas::new(60, 60, rowser_layout::WHITE);
        c.fill_rounded(x, y, w, h, r, bgra(255, 0, 0));
        let ri = r.ceil() as i32;
        let centers = [
            (x + ri, y + ri),
            (x + w as i32 - ri, y + ri),
            (x + ri, y + h as i32 - ri),
            (x + w as i32 - ri, y + h as i32 - ri),
        ];
        for (cx, cy) in centers {
            for dy in -ri..=ri {
                for dx in -ri..=ri {
                    if ((dx * dx + dy * dy) as f32).sqrt() <= r {
                        assert_eq!(
                            px_at(&c.pixels, 60, (cx + dx) as u32, (cy + dy) as u32),
                            [0, 0, 255, 255],
                            "disc pixel ({},{}) painted",
                            cx + dx,
                            cy + dy
                        );
                    }
                }
            }
        }
        // the disc square's far corner is outside the disc and outside the
        // side strips — must stay bg
        assert_eq!(px_at(&c.pixels, 60, x as u32, y as u32), [255, 255, 255, 255]);
        assert_eq!(
            px_at(&c.pixels, 60, (x + w as i32 - 1) as u32, y as u32),
            [255, 255, 255, 255]
        );
    }

    fn px_at(px: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        [px[i], px[i + 1], px[i + 2], px[i + 3]]
    }

    #[test]
    fn rotated_span_paints_shifted_pixels() {
        let mut fonts = FontCache::new();
        let red = bgra(255, 0, 0);
        let items = vec![DisplayItem::FillRect {
            x: 10.0,
            y: 10.0,
            w: 20.0,
            h: 20.0,
            color: red,
        }];
        let (w, h) = (60u32, 60u32);
        let flat = paint(
            &items,
            &mut fonts,
            w,
            h,
            rowser_layout::WHITE,
            &HashMap::new(),
        );
        // 45° about the square's center: a diamond — tips reach outside the
        // original square, its corners empty out
        let m = Mat::translate(20.0, 20.0)
            .mul(&Mat::rotate(45.0))
            .mul(&Mat::translate(-20.0, -20.0));
        let items = vec![
            DisplayItem::PushTransform { m },
            DisplayItem::FillRect {
                x: 10.0,
                y: 10.0,
                w: 20.0,
                h: 20.0,
                color: red,
            },
            DisplayItem::PopTransform,
        ];
        let rot = paint(
            &items,
            &mut fonts,
            w,
            h,
            rowser_layout::WHITE,
            &HashMap::new(),
        );
        assert_ne!(flat, rot, "transformed output must differ");
        // non-empty + shifted: the diamond's top tip paints above where the
        // square ended (pixel-center 20.5,8.5 sits ~2px inside the tip)
        let tip = px_at(&rot, w, 20, 8);
        assert!(tip[2] > 128 && tip[0] < 128, "diamond tip is red: {tip:?}");
        let ctr = px_at(&rot, w, 20, 20);
        assert_eq!(ctr, [0, 0, 255, 255], "center stays solid red");
        // the original square's corner is outside the diamond → stays white
        assert_eq!(px_at(&rot, w, 11, 11), [255, 255, 255, 255], "corner bg");
    }

    #[test]
    fn translated_span_via_matrix_offsets() {
        // a pure-translate PushTransform paints the same rect, moved —
        // sanity for the offscreen+blit path (not only direct offsets)
        let mut fonts = FontCache::new();
        let red = bgra(255, 0, 0);
        let m = Mat::translate(15.0, 10.0);
        let items = vec![
            DisplayItem::PushTransform { m },
            DisplayItem::FillRect {
                x: 4.0,
                y: 4.0,
                w: 10.0,
                h: 10.0,
                color: red,
            },
            DisplayItem::PopTransform,
        ];
        let (w, h) = (60u32, 60u32);
        let px = paint(
            &items,
            &mut fonts,
            w,
            h,
            rowser_layout::WHITE,
            &HashMap::new(),
        );
        // rect (4,4,10,10) → painted at (19,14)-(29,24)
        assert_eq!(px_at(&px, w, 24, 19), [0, 0, 255, 255], "shifted rect");
        assert_eq!(px_at(&px, w, 5, 5), [255, 255, 255, 255], "original spot");
    }

    #[test]
    fn banded_matches_serial() {
        // Mixed list with state + ops straddling the 40px band seams of a
        // 200×160 canvas painted with 4 bands: clip stack replay,
        // rounded fills, a blurred box-shadow (intermediate mask must not
        // be band-clipped), a transform tile, a dashed border, a rounded
        // clip with per-pixel contains, and text when a font resolves.
        let mut fonts = FontCache::new();
        let (red, green, blue, black) = (
            bgra(255, 0, 0),
            bgra(0, 255, 0),
            bgra(0, 0, 255),
            bgra(0, 0, 0),
        );
        let mut items = vec![
            DisplayItem::PushClip {
                x: 0.0,
                y: 10.0,
                w: 100.0,
                h: 80.0,
                radii: [[0.0; 2]; 4],
            },
            DisplayItem::FillRect {
                x: 0.0,
                y: 0.0,
                w: 200.0,
                h: 160.0,
                color: red,
            },
            DisplayItem::PopClip,
            DisplayItem::RoundedRect {
                x: 20.0,
                y: 30.0,
                w: 60.0,
                h: 40.0,
                radius: 8.0,
                color: blue,
            },
            DisplayItem::PushClip {
                x: 0.0,
                y: 40.0,
                w: 200.0,
                h: 60.0,
                radii: [[12.0, 12.0]; 4],
            },
            DisplayItem::FillRect {
                x: 0.0,
                y: 0.0,
                w: 200.0,
                h: 160.0,
                color: green,
            },
            DisplayItem::PopClip,
            DisplayItem::Shadow {
                x: 50.0,
                y: 35.0,
                w: 30.0,
                h: 30.0,
                dx: 4.0,
                dy: 4.0,
                blur: 6.0,
                spread: 2.0,
                radii: [[4.0, 4.0]; 4],
                color: black,
                inset: false,
            },
            DisplayItem::PushTransform {
                m: Mat::translate(5.0, 3.0),
            },
            DisplayItem::FillRect {
                x: 10.0,
                y: 36.0,
                w: 40.0,
                h: 12.0,
                color: black,
            },
            DisplayItem::PopTransform,
            DisplayItem::BorderSide {
                x: 0.0,
                y: 78.0,
                w: 200.0,
                h: 4.0,
                side: rowser_layout::BorderEdge::Top,
                style: rowser_layout::BorderStyle::Dashed,
                color: black,
            },
            DisplayItem::RoundedRect2 {
                x: 140.0,
                y: 118.0,
                w: 50.0,
                h: 30.0,
                radii: [[10.0, 10.0], [0.0, 0.0], [14.0, 6.0], [0.0, 0.0]],
                color: red,
            },
        ];
        if let Some(font) = fonts.resolve("liberation sans", false, false) {
            items.push(DisplayItem::Text {
                x: 4.0,
                baseline: 42.0,
                text: "Bands!".into(),
                font,
                size: 20.0,
                color: black,
                deco: rowser_layout::TextDeco::NONE,
                ls: 0.0,
                href: None,
                sel: false,
            });
        }
        let (w, h) = (200u32, 160u32);
        let serial = paint(
            &items,
            &mut fonts,
            w,
            h,
            rowser_layout::WHITE,
            &HashMap::new(),
        );
        // 4 bands → 40px strips; several items cross the seams
        let banded = paint_banded(
            &items,
            &mut fonts,
            w,
            h,
            rowser_layout::WHITE,
            &HashMap::new(),
            4,
        );
        assert_eq!(serial, banded, "banded output must equal serial");
        // and the public entry point agrees (small canvas → serial path)
        let par = paint_par(
            &items,
            &mut fonts,
            w,
            h,
            rowser_layout::WHITE,
            &HashMap::new(),
        );
        assert_eq!(serial, par, "paint_par must equal serial");
    }
}

impl Canvas {
    /// Nearest-neighbor blit of a decoded BGRA image into a rect.
    /// Blit a raw BGRA buffer of `dw`×`dh` pixels (no scaling).
    pub fn blit_pixels(&mut self, pixels: &[u8], dx: i32, dy: i32, dw: i32, dh: i32) {
        // raw rasters are 1:1 content — nearest keeps them crisp
        self.blit_src(pixels, dw.max(0) as u32, dh.max(0) as u32, dx, dy, dw, dh, true);
    }

    /// `image-rendering`: `near` = pixelated nearest-neighbor, else the
    /// pixel center is bilinear-sampled.
    fn blit(&mut self, img: &Image, dx: i32, dy: i32, dw: i32, dh: i32, near: bool) {
        self.blit_src(&img.pixels, img.width, img.height, dx, dy, dw, dh, near);
    }

    /// Blit a `sw`×`sh` BGRA source into dest rect (dx,dy,dw,dh) under the
    /// canvas clip stack.
    #[allow(clippy::too_many_arguments)]
    fn blit_src(
        &mut self,
        spx: &[u8],
        sw: u32,
        sh: u32,
        dx: i32,
        dy: i32,
        dw: i32,
        dh: i32,
        near: bool,
    ) {
        if sw == 0 || sh == 0 || dw <= 0 || dh <= 0 {
            return;
        }
        let (cw, ch) = (self.width as i32, self.height as i32);
        let (sx0, sy0, sx1, sy1) = fx::scissor(&self.clips, cw, ch);
        let (x0, x1) = (dx.max(0).max(sx0), (dx + dw).min(cw).min(sx1));
        let (y0, y1) = (
            dy.max(0).max(sy0).max(self.band.0),
            (dy + dh).min(ch).min(sy1).min(self.band.1),
        );
        blit_run(
            &mut self.pixels,
            cw,
            &self.clips,
            spx,
            sw,
            sh,
            dx,
            dy,
            dw,
            dh,
            x0,
            x1,
            y0,
            y1,
            near,
        );
    }

    /// Tiled `background-image`/object-fit blit. `img` scales to a (tw,th)
    /// tile resolved here from the natural size, then tiles grid over the
    /// positioning area and clip to (x,y,w,h). See `DisplayItem::BgImage`
    /// for the field semantics; `space` approximates as `repeat`.
    #[allow(clippy::too_many_arguments)]
    pub fn blit_tiled(
        &mut self,
        img: &Image,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        px: f32,
        py: f32,
        pw: f32,
        ph: f32,
        mode: u8,
        sw: f32,
        sh: f32,
        fx: f32,
        ax: f32,
        fy: f32,
        ay: f32,
        rx: u8,
        ry: u8,
        near: bool,
    ) {
        let d = img.density.max(1e-3);
        let (nw, nh) = (img.width as f32 / d, img.height as f32 / d);
        if nw <= 0.0 || nh <= 0.0 || w <= 0.0 || h <= 0.0 {
            return;
        }
        // tile size: explicit axes fall back to natural size / aspect ratio
        let (mut tw, mut th) = match mode {
            1 => {
                let s = (pw / nw).max(ph / nh);
                (nw * s, nh * s)
            }
            2 => {
                let s = (pw / nw).min(ph / nh);
                (nw * s, nh * s)
            }
            3 => {
                let s = (pw / nw).min(ph / nh).min(1.0);
                (nw * s, nh * s)
            }
            _ => {
                let tw = if sw >= 0.0 {
                    sw
                } else if sh >= 0.0 {
                    nw * sh / nh
                } else {
                    nw
                };
                let th = if sh >= 0.0 {
                    sh
                } else if sw >= 0.0 {
                    nh * tw / nw
                } else {
                    nh
                };
                (tw, th)
            }
        };
        if tw <= 0.0 || th <= 0.0 {
            return;
        }
        // `round`: the tile stretches so a whole number fits the area
        if rx == 3 {
            tw = pw / (pw / tw).round().max(1.0);
        }
        if ry == 3 {
            th = ph / (ph / th).round().max(1.0);
        }
        let ox = px + fx * (pw - tw) + ax;
        let oy = py + fy * (ph - th) + ay;
        let (cx0, cy0) = (x as i32, y as i32);
        let (cx1, cy1) = ((x + w).ceil() as i32, (y + h).ceil() as i32);
        // first tile index that can intersect the clip rect
        let kx0 = if rx != 0 {
            ((x - ox) / tw).floor() as i32
        } else {
            0
        };
        let ky0 = if ry != 0 {
            ((y - oy) / th).floor() as i32
        } else {
            0
        };
        let mut ti = ky0;
        loop {
            let ty = oy + ti as f32 * th;
            if ty >= y + h {
                break;
            }
            if ry != 0 || ti == 0 {
                let mut tj = kx0;
                loop {
                    let tx = ox + tj as f32 * tw;
                    if tx >= x + w {
                        break;
                    }
                    if tx + tw > x && ty + th > y {
                        self.blit_clip(
                            img,
                            tx as i32,
                            ty as i32,
                            tw.ceil() as i32,
                            th.ceil() as i32,
                            cx0,
                            cy0,
                            cx1,
                            cy1,
                            near,
                        );
                    }
                    if rx == 0 {
                        break;
                    }
                    tj += 1;
                }
            }
            if ry == 0 {
                break;
            }
            ti += 1;
        }
    }

    /// `blit` clipped to a destination rect — used for `background-clip`.
    #[allow(clippy::too_many_arguments)]
    fn blit_clip(
        &mut self,
        img: &Image,
        dx: i32,
        dy: i32,
        dw: i32,
        dh: i32,
        cx0: i32,
        cy0: i32,
        cx1: i32,
        cy1: i32,
        near: bool,
    ) {
        if img.width == 0 || img.height == 0 || dw <= 0 || dh <= 0 {
            return;
        }
        let (cw, ch) = (self.width as i32, self.height as i32);
        let x_lo = dx.max(cx0).max(0);
        let x_hi = (dx + dw).min(cx1).min(cw);
        let y_lo = dy.max(cy0).max(0).max(self.band.0);
        let y_hi = (dy + dh).min(cy1).min(ch).min(self.band.1);
        blit_run(
            &mut self.pixels,
            cw,
            &[],
            &img.pixels,
            img.width,
            img.height,
            dx,
            dy,
            dw,
            dh,
            x_lo,
            x_hi,
            y_lo,
            y_hi,
            near,
        );
    }
    /// Blit a BGRA tile through affine matrix `m` (tile coords → canvas
    /// coords): each destination pixel inside the transformed tile bbox is
    /// inverse-mapped back into the tile and bilinear-sampled, then
    /// source-over composited (transparent tile pixels keep the backdrop).
    pub fn blit_affine(&mut self, src: &[u8], sw: u32, sh: u32, m: &Mat) {
        let Some(inv) = m.inverse() else { return };
        // destination bbox = m applied to the tile's corners
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for (cx, cy) in [
            (0.0, 0.0),
            (sw as f32, 0.0),
            (0.0, sh as f32),
            (sw as f32, sh as f32),
        ] {
            let (px, py) = m.apply(cx, cy);
            x0 = x0.min(px);
            y0 = y0.min(py);
            x1 = x1.max(px);
            y1 = y1.max(py);
        }
        let (cw, ch) = (self.width as i32, self.height as i32);
        let (x0, y0) = (
            (x0.floor() as i32).max(0),
            (y0.floor() as i32).max(0).max(self.band.0),
        );
        let (x1, y1) = (
            (x1.ceil() as i32).min(cw),
            (y1.ceil() as i32).min(ch).min(self.band.1),
        );
        let [a, b, c, d, e, f] = inv.0;
        for y in y0..y1 {
            let yv = y as f32 + 0.5;
            // inv.apply's y terms hoist out of the row (same value, same
            // op order — bit-identical to per-pixel apply)
            let (cy, dy_) = (c * yv, d * yv);
            for x in x0..x1 {
                // pixel-center sampling back into the tile
                let xv = x as f32 + 0.5;
                let (sx, sy) = (a * xv + cy + e, b * xv + dy_ + f);
                if sx < 0.0 || sy < 0.0 || sx >= sw as f32 || sy >= sh as f32 {
                    continue;
                }
                let (b, g, r, a) = bilinear(src, sw, sh, sx, sy);
                if a == 0 {
                    continue;
                }
                let i = ((y * cw + x) * 4) as usize;
                if a == 255 {
                    self.pixels[i..i + 4].copy_from_slice(&[b as u8, g as u8, r as u8, 255]);
                    continue;
                }
                let ia = 255 - a;
                self.pixels[i] = ((b * a + self.pixels[i] as u32 * ia) / 255) as u8;
                self.pixels[i + 1] = ((g * a + self.pixels[i + 1] as u32 * ia) / 255) as u8;
                self.pixels[i + 2] = ((r * a + self.pixels[i + 2] as u32 * ia) / 255) as u8;
                self.pixels[i + 3] = 255;
            }
        }
    }
}

/// Inner blit loop: paint the x0..x1 × y0..y1 dest range of the
/// (dx,dy,dw,dh) blit of a `sw`×`sh` BGRA source. `clips` adds the
/// rounded-corner point test; pass `&[]` for a pure rect clip.
#[allow(clippy::too_many_arguments)]
fn blit_run(
    px: &mut [u8],
    cw: i32,
    clips: &[Clip],
    spx: &[u8],
    sw: u32,
    sh: u32,
    dx: i32,
    dy: i32,
    dw: i32,
    dh: i32,
    mut x0: i32,
    mut x1: i32,
    mut y0: i32,
    mut y1: i32,
    near: bool,
) {
    // Corner-rounded clips test per pixel; rect-only clips fold into the
    // dest range exactly (see `fill`).
    let rounded = clips
        .iter()
        .any(|c| c.radii.iter().any(|r| r[0] > 0.0 && r[1] > 0.0));
    if !rounded {
        for c in clips {
            x0 = x0.max((c.x - 0.5).ceil() as i32);
            y0 = y0.max((c.y - 0.5).ceil() as i32);
            x1 = x1.min((c.x + c.w - 0.5).ceil() as i32);
            y1 = y1.min((c.y + c.h - 0.5).ceil() as i32);
        }
    }
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let (sw64, sh64, dw64, dh64) = (sw as u64, sh as u64, dw as u64, dh as u64);
    // `near` source-x stepper: sx = (x - dx)·sw/dw tracked exactly as
    // quotient + remainder — no per-pixel divide
    let (qx0, rx0) = ((x0 - dx) as u64 * sw64 / dw64, (x0 - dx) as u64 * sw64 % dw64);
    let (qstep, rstep) = (sw64 / dw64, sw64 % dw64);
    let iw = sw as usize;
    let (dwf, swf) = (dw as f32, sw as f32);
    let (dhf, shf) = (dh as f32, sh as f32);
    for y in y0..y1 {
        let di = (y * cw + x0) as usize * 4;
        let drow = &mut px[di..di + (x1 - x0) as usize * 4];
        if near {
            let sy = ((y - dy) as u64 * sh64 / dh64).min(sh64 - 1) as usize;
            let srow = &spx[sy * iw * 4..];
            let (mut q, mut r) = (qx0, rx0);
            for (k, d) in drow.chunks_exact_mut(4).enumerate() {
                let sx = (q as usize).min(iw - 1);
                q += qstep;
                r += rstep;
                if r >= dw64 {
                    r -= dw64;
                    q += 1;
                }
                if rounded
                    && !clips
                        .iter()
                        .all(|c| c.contains(x0 as f32 + k as f32 + 0.5, y as f32 + 0.5))
                {
                    continue;
                }
                let s = &srow[sx * 4..sx * 4 + 4];
                if s[3] == 255 {
                    d.copy_from_slice(s);
                } else if s[3] > 0 {
                    let (a, inv) = (s[3] as u32, 255 - s[3] as u32);
                    d[0] = ((s[0] as u32 * a + d[0] as u32 * inv) / 255) as u8;
                    d[1] = ((s[1] as u32 * a + d[1] as u32 * inv) / 255) as u8;
                    d[2] = ((s[2] as u32 * a + d[2] as u32 * inv) / 255) as u8;
                    d[3] = 255;
                }
            }
        } else {
            let sy = (y - dy) as f32 / dhf * shf;
            for (k, d) in drow.chunks_exact_mut(4).enumerate() {
                if rounded
                    && !clips
                        .iter()
                        .all(|c| c.contains(x0 as f32 + k as f32 + 0.5, y as f32 + 0.5))
                {
                    continue;
                }
                let sx = (x0 + k as i32 - dx) as f32 / dwf * swf;
                let (b, g, r, a) = bilinear(spx, sw, sh, sx + 0.5, sy + 0.5);
                if a == 255 {
                    d.copy_from_slice(&[b as u8, g as u8, r as u8, 255]);
                } else if a > 0 {
                    let inv = 255 - a;
                    d[0] = ((b * a + d[0] as u32 * inv) / 255) as u8;
                    d[1] = ((g * a + d[1] as u32 * inv) / 255) as u8;
                    d[2] = ((r * a + d[2] as u32 * inv) / 255) as u8;
                    d[3] = 255;
                }
            }
        }
    }
}

/// Bilinear sample of a straight-alpha BGRA buffer at fractional (x, y);
/// out-of-range neighbors contribute zero.
fn bilinear(px: &[u8], w: u32, h: u32, x: f32, y: f32) -> (u32, u32, u32, u32) {
    let (x, y) = (x - 0.5, y - 0.5);
    let (x0, y0) = (x.floor() as i32, y.floor() as i32);
    let (fx, fy) = (x - x0 as f32, y - y0 as f32);
    // interior tap: all four neighbours exist — skipping the bounds/zero
    // checks is exact (a skipped tap contributed +0.0 anyway)
    if x0 >= 0 && y0 >= 0 && x0 + 1 < w as i32 && y0 + 1 < h as i32 {
        let i = ((y0 * w as i32 + x0) * 4) as usize;
        let row2 = w as usize * 4;
        let (fxi, fyi) = (1.0 - fx, 1.0 - fy);
        let mut acc = [0f32; 4];
        for c in 0..4 {
            acc[c] = px[i + c] as f32 * (fxi * fyi)
                + px[i + 4 + c] as f32 * (fx * fyi)
                + px[i + row2 + c] as f32 * (fxi * fy)
                + px[i + row2 + 4 + c] as f32 * (fx * fy);
        }
        return (acc[0] as u32, acc[1] as u32, acc[2] as u32, acc[3] as u32);
    }
    let mut acc = [0f32; 4];
    for oy in 0..2 {
        for ox in 0..2 {
            let wgt = (if ox == 0 { 1.0 - fx } else { fx }) * (if oy == 0 { 1.0 - fy } else { fy });
            let (sx, sy) = (x0 + ox, y0 + oy);
            if wgt == 0.0 || sx < 0 || sy < 0 || sx >= w as i32 || sy >= h as i32 {
                continue;
            }
            let i = ((sy * w as i32 + sx) * 4) as usize;
            for c in 0..4 {
                acc[c] += px[i + c] as f32 * wgt;
            }
        }
    }
    (acc[0] as u32, acc[1] as u32, acc[2] as u32, acc[3] as u32)
}
