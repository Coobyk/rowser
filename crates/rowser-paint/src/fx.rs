//! fx: per-side border styles, box-shadow blur, and the overflow clip
//! stack — the raster side of rowser-layout's borderfx items.

use crate::Canvas;
use rowser_image::Image;
use rowser_layout::colorfx::{
    blend_rgb, clip_covers, resolve_clip, BlendMode, ClipAbs, Effect, FilterOp,
};
use rowser_layout::{bgra, offset_item, DisplayItem};
use rowser_layout::{BorderEdge, BorderStyle, Color};
use rowser_text::FontCache;
use std::collections::HashMap;

/// An active clip region pushed by `DisplayItem::PushClip`.
#[derive(Clone, Copy)]
pub struct Clip {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// [top-left, top-right, bottom-right, bottom-left] (rx, ry)
    pub radii: [[f32; 2]; 4],
}

impl Clip {
    /// Point membership: inside the rect, and inside the corner ellipse
    /// when the point lands in a rounded corner square.
    pub fn contains(&self, px: f32, py: f32) -> bool {
        if px < self.x || px >= self.x + self.w || py < self.y || py >= self.y + self.h {
            return false;
        }
        for (i, &[rx, ry]) in self.radii.iter().enumerate() {
            if rx <= 0.0 || ry <= 0.0 {
                continue;
            }
            let (cx, cy) = match i {
                0 => (self.x + rx, self.y + ry),
                1 => (self.x + self.w - rx, self.y + ry),
                2 => (self.x + self.w - rx, self.y + self.h - ry),
                _ => (self.x + rx, self.y + self.h - ry),
            };
            let (dx, dy) = (px - cx, py - cy);
            let in_sq = match i {
                0 => dx < 0.0 && dy < 0.0,
                1 => dx > 0.0 && dy < 0.0,
                2 => dx > 0.0 && dy > 0.0,
                _ => dx < 0.0 && dy > 0.0,
            };
            if in_sq && (dx * dx) / (rx * rx) + (dy * dy) / (ry * ry) > 1.0 {
                return false;
            }
        }
        true
    }
}

/// Combined axis-aligned scissor of the clip stack ∩ canvas bounds.
pub fn scissor(clips: &[Clip], cw: i32, ch: i32) -> (i32, i32, i32, i32) {
    let (mut x0, mut y0, mut x1, mut y1) = (0, 0, cw, ch);
    for c in clips {
        x0 = x0.max(c.x.floor() as i32);
        y0 = y0.max(c.y.floor() as i32);
        x1 = x1.min((c.x + c.w).ceil() as i32);
        y1 = y1.min((c.y + c.h).ceil() as i32);
    }
    (x0, y0, x1.max(x0), y1.max(y0))
}

/// Filled disc (border dots) at (cx, cy).
fn disc(cv: &mut Canvas, cx: f32, cy: f32, r: f32, color: Color) {
    let ri = r.ceil() as i32;
    let (cx, cy) = (cx.round() as i32, cy.round() as i32);
    for dy in -ri..=ri {
        for dx in -ri..=ri {
            if (dx * dx + dy * dy) as f32 <= r * r {
                cv.fill_rect(cx + dx, cy + dy, 1, 1, color);
            }
        }
    }
}

/// Darken (`f` < 0, toward black) or lighten (`f` > 0, toward white).
fn shade(c: Color, f: f32) -> Color {
    let m = |v: u8| -> u8 {
        if f >= 0.0 {
            (v as f32 + (255.0 - v as f32) * f) as u8
        } else {
            (v as f32 * (1.0 + f)) as u8
        }
    };
    rowser_layout::bgra(
        m(((c >> 16) & 0xff) as u8),
        m(((c >> 8) & 0xff) as u8),
        m((c & 0xff) as u8),
    )
}

/// Paint one border/outline edge; `x,y,w,h` is the edge's own rect.
pub fn border_side(
    cv: &mut Canvas,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    side: BorderEdge,
    style: BorderStyle,
    color: Color,
) {
    let horiz = matches!(side, BorderEdge::Top | BorderEdge::Bottom);
    let thick = if horiz { h } else { w };
    let len = if horiz { w } else { h };
    if thick <= 0.0 || len <= 0.0 {
        return;
    }
    match style {
        BorderStyle::Solid => cv.fill_rect(x as i32, y as i32, w as u32, h as u32, color),
        BorderStyle::Dotted => {
            // round dots: diameter = edge thickness, gap = diameter
            let r = thick / 2.0;
            let step = (thick * 2.0).max(2.0);
            let mut s = 0.0f32;
            while s + r <= len {
                let (cx, cy) = if horiz {
                    (x + s + r, y + r)
                } else {
                    (x + r, y + s + r)
                };
                disc(cv, cx, cy, r, color);
                s += step;
            }
        }
        BorderStyle::Dashed => {
            // dash runs ≈3× the border width, equal gaps
            let dash = (3.0 * thick).max(1.0);
            let gap = dash;
            let mut s = 0.0f32;
            while s < len {
                let e = (s + dash).min(len);
                if horiz {
                    cv.fill_rect((x + s) as i32, y as i32, (e - s) as u32, h as u32, color);
                } else {
                    cv.fill_rect(x as i32, (y + s) as i32, w as u32, (e - s) as u32, color);
                }
                s += dash + gap;
            }
        }
        BorderStyle::Double => {
            // two lines + gap, each a third of the thickness
            let t = (thick / 3.0).floor().max(1.0);
            if thick < 3.0 {
                cv.fill_rect(x as i32, y as i32, w as u32, h as u32, color);
            } else if horiz {
                cv.fill_rect(x as i32, y as i32, w as u32, t as u32, color);
                cv.fill_rect(x as i32, (y + thick - t) as i32, w as u32, t as u32, color);
            } else {
                cv.fill_rect(x as i32, y as i32, t as u32, h as u32, color);
                cv.fill_rect((x + thick - t) as i32, y as i32, t as u32, h as u32, color);
            }
        }
        BorderStyle::Groove | BorderStyle::Ridge | BorderStyle::Inset | BorderStyle::Outset => {
            // two-tone bevel: edge halves across the thickness — the outer
            // band darkens on top/left for a sunken look (groove/inset),
            // or on bottom/right when raised (ridge/outset)
            let dark = shade(color, -0.45);
            let light = shade(color, 0.55);
            let sunken = matches!(style, BorderStyle::Groove | BorderStyle::Inset);
            let outer_dark = matches!(side, BorderEdge::Top | BorderEdge::Left) == sunken;
            let (outer, inner) = if outer_dark {
                (dark, light)
            } else {
                (light, dark)
            };
            let half = (thick / 2.0).max(0.0);
            if half < 0.5 {
                cv.fill_rect(x as i32, y as i32, w as u32, h as u32, outer);
                return;
            }
            match side {
                BorderEdge::Top => {
                    cv.fill_rect(x as i32, y as i32, w as u32, half as u32, outer);
                    cv.fill_rect(
                        x as i32,
                        (y + half) as i32,
                        w as u32,
                        (h - half) as u32,
                        inner,
                    );
                }
                BorderEdge::Bottom => {
                    cv.fill_rect(
                        x as i32,
                        (y + half) as i32,
                        w as u32,
                        (h - half) as u32,
                        outer,
                    );
                    cv.fill_rect(x as i32, y as i32, w as u32, half as u32, inner);
                }
                BorderEdge::Left => {
                    cv.fill_rect(x as i32, y as i32, half as u32, h as u32, outer);
                    cv.fill_rect(
                        (x + half) as i32,
                        y as i32,
                        (w - half) as u32,
                        h as u32,
                        inner,
                    );
                }
                BorderEdge::Right => {
                    cv.fill_rect(
                        (x + half) as i32,
                        y as i32,
                        (w - half) as u32,
                        h as u32,
                        outer,
                    );
                    cv.fill_rect(x as i32, y as i32, half as u32, h as u32, inner);
                }
            }
        }
    }
}

/// Separable sliding-window box blur pass (O(1) per pixel, edges clamp).
fn blur_pass(src: &[u8], dst: &mut [u8], w: usize, h: usize, r: usize, horiz: bool) {
    let (lines, len) = if horiz { (h, w) } else { (w, h) };
    for line in 0..lines {
        let at = |i: usize| -> u32 {
            if horiz {
                src[line * w + i] as u32
            } else {
                src[i * w + line] as u32
            }
        };
        let mut sum = 0u32;
        let hi0 = r.min(len - 1);
        for k in 0..=hi0 {
            sum += at(k);
        }
        let (mut lo, mut hi) = (0usize, hi0);
        for i in 0..len {
            let wl = i.saturating_sub(r);
            let wh = (i + r).min(len - 1);
            while lo < wl {
                sum -= at(lo);
                lo += 1;
            }
            while hi < wh {
                hi += 1;
                sum += at(hi);
            }
            while hi > wh {
                sum -= at(hi);
                hi -= 1;
            }
            let v = (sum / (hi - lo + 1) as u32) as u8;
            if horiz {
                dst[line * w + i] = v;
            } else {
                dst[i * w + line] = v;
            }
        }
    }
}

/// Two box-blur iterations per axis at radius `r` — a cheap gaussian
/// approximation (σ ≈ 0.8·r, halo spread ±2r).
fn blur_mask(m: &mut [u8], w: usize, h: usize, r: usize) {
    if r == 0 || w < 2 || h < 2 {
        return;
    }
    let mut a = m.to_vec();
    let mut b = vec![0u8; m.len()];
    let rr = r.max(1);
    for _ in 0..2 {
        blur_pass(&a, &mut b, w, h, rr, true);
        blur_pass(&b, &mut a, w, h, rr, false);
    }
    m.copy_from_slice(&a);
}

/// box-shadow: build an alpha mask of the (spread ±, offset) box shape,
/// blur it, and blend `color` through it. Outer shadows knock out the
/// box interior (the shadow only shows around it); `inset` keeps the
/// shadow inside the box as a soft inner band.
#[allow(clippy::too_many_arguments)]
pub fn box_shadow(
    cv: &mut Canvas,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    dx: f32,
    dy: f32,
    blur: f32,
    spread: f32,
    radii: [[f32; 2]; 4],
    color: Color,
    inset: bool,
) {
    let [cb, cg, cr, ca] = color.to_le_bytes();
    if ca == 0 {
        return;
    }
    let clips = cv.clips.clone();
    let (bx0, by0, bx1, by1) = scissor(&clips, cv.width as i32, cv.height as i32);
    let blend = |cv: &mut Canvas, mask: &[u8], mw: usize, x0: i32, y0: i32, j: usize, i: usize| {
        let (px, py) = (x0 as f32 + i as f32 + 0.5, y0 as f32 + j as f32 + 0.5);
        if !clips.is_empty() && !clips.iter().all(|c| c.contains(px, py)) {
            return;
        }
        let cov = mask[j * mw + i] as u32 * ca as u32 / 255;
        if cov == 0 {
            return;
        }
        let inv = 255 - cov;
        let di = ((py as i32 * cv.width as i32 + px as i32) * 4) as usize;
        cv.pixels[di] = ((cb as u32 * cov + cv.pixels[di] as u32 * inv) / 255) as u8;
        cv.pixels[di + 1] = ((cg as u32 * cov + cv.pixels[di + 1] as u32 * inv) / 255) as u8;
        cv.pixels[di + 2] = ((cr as u32 * cov + cv.pixels[di + 2] as u32 * inv) / 255) as u8;
        cv.pixels[di + 3] = 255;
    };
    let br = blur.max(0.0).ceil() as usize;
    if inset {
        // inner shadow: inside the box — a soft band where the offset,
        // spread-shrunken "core" does not cover.
        let core = Clip {
            x: x + dx + spread,
            y: y + dy + spread,
            w: (w - 2.0 * spread).max(0.0),
            h: (h - 2.0 * spread).max(0.0),
            radii: {
                let mut r = radii;
                for c in r.iter_mut() {
                    c[0] = (c[0] - spread).max(0.0);
                    c[1] = (c[1] - spread).max(0.0);
                }
                r
            },
        };
        let (x0, y0) = ((x.floor() as i32).max(bx0), (y.floor() as i32).max(by0));
        let (x1, y1) = (
            ((x + w).ceil() as i32).min(bx1),
            ((y + h).ceil() as i32).min(by1),
        );
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let (mw, mh) = ((x1 - x0) as usize, (y1 - y0) as usize);
        let mut mask = vec![0u8; mw * mh];
        for j in 0..mh {
            for i in 0..mw {
                if core.contains(x0 as f32 + i as f32 + 0.5, y0 as f32 + j as f32 + 0.5) {
                    mask[j * mw + i] = 255;
                }
            }
        }
        if br > 0 {
            blur_mask(&mut mask, mw, mh, br);
        }
        let edge = Clip { x, y, w, h, radii };
        for j in 0..mh {
            for i in 0..mw {
                let (px, py) = (x0 as f32 + i as f32 + 0.5, y0 as f32 + j as f32 + 0.5);
                if !edge.contains(px, py) {
                    continue;
                }
                let cov = mask[j * mw + i];
                // invert: shadow where the core does not cover
                blend(
                    cv,
                    &[255u8.saturating_sub(cov)],
                    1,
                    x0 + i as i32,
                    y0 + j as i32,
                    0,
                    0,
                );
            }
        }
    } else {
        // outer shadow: blurred halo of the spread box, interior knocked out
        let shape = Clip {
            x: x + dx - spread,
            y: y + dy - spread,
            w: (w + 2.0 * spread).max(0.0),
            h: (h + 2.0 * spread).max(0.0),
            radii: {
                let mut r = radii;
                for c in r.iter_mut() {
                    c[0] = (c[0] + spread).max(0.0);
                    c[1] = (c[1] + spread).max(0.0);
                }
                r
            },
        };
        let pad = (blur.max(0.0) * 3.0).ceil() as i32 + 1;
        let (x0, y0) = (
            (shape.x.floor() as i32 - pad).max(bx0),
            (shape.y.floor() as i32 - pad).max(by0),
        );
        let (x1, y1) = (
            ((shape.x + shape.w).ceil() as i32 + pad).min(bx1),
            ((shape.y + shape.h).ceil() as i32 + pad).min(by1),
        );
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let (mw, mh) = ((x1 - x0) as usize, (y1 - y0) as usize);
        let mut mask = vec![0u8; mw * mh];
        for j in 0..mh {
            for i in 0..mw {
                if shape.contains(x0 as f32 + i as f32 + 0.5, y0 as f32 + j as f32 + 0.5) {
                    mask[j * mw + i] = 255;
                }
            }
        }
        if br > 0 {
            blur_mask(&mut mask, mw, mh, br);
        }
        let knock = Clip { x, y, w, h, radii };
        for j in 0..mh {
            for i in 0..mw {
                let (px, py) = (x0 as f32 + i as f32 + 0.5, y0 as f32 + j as f32 + 0.5);
                if knock.contains(px, py) {
                    continue;
                }
                blend(cv, &mask, mw, x0, y0, j, i);
            }
        }
    }
}

// ---------- colors/effects: filter, backdrop, blend ----------

/// Straight-alpha BGRA tile in canvas coordinates.
struct Tile {
    ox: i32,
    oy: i32,
    w: usize,
    h: usize,
    px: Vec<u8>,
}

/// Process the `PushEffect` at `items[push]`; returns the index just
/// past its matching `PopEffect`.
pub fn process(
    canvas: &mut Canvas,
    items: &[DisplayItem],
    push: usize,
    fonts: &mut FontCache,
    images: &HashMap<String, Image>,
) -> usize {
    let DisplayItem::PushEffect(eff) = &items[push] else {
        return push + 1;
    };
    let j = matching_pop(items, push);
    let inner = &items[push + 1..j];
    match eff {
        Effect::Backdrop { ops, rect } => {
            let base = rect.or_else(|| items_bbox(inner, fonts));
            if let Some(b) = base {
                backdrop_apply(canvas, b, ops);
            }
            crate::paint_into(canvas, inner, fonts, images);
        }
        _ => {
            let pad = match eff {
                Effect::Filter(ops) => ops_extent(ops),
                _ => [0.0; 4],
            };
            let base = items_bbox(inner, fonts);
            if let Some((x0, y0, w, h)) = base.and_then(|b| pad_int(b, pad, canvas)) {
                let mut tile = render_tile(inner, x0, y0, w, h, fonts, images);
                match eff {
                    Effect::Clip { shape, rect } => {
                        let refr = rect.or(base).unwrap_or([0.0; 4]);
                        clip_tile(&mut tile, &resolve_clip(shape, refr));
                    }
                    Effect::Filter(ops) => {
                        for op in ops {
                            apply_op(&mut tile, op);
                        }
                    }
                    Effect::Mask { src } => {
                        if let Some(m) = images.get(src) {
                            mask_tile(&mut tile, m);
                        }
                    }
                    _ => {}
                }
                match eff {
                    Effect::Blend(m) => blend_composite(canvas, &tile, *m),
                    _ => composite(canvas, &tile),
                }
            }
        }
    }
    j + 1
}

/// Index of the `PopEffect` matching `items[push]` (nesting-aware);
/// `items.len()` if unbalanced.
fn matching_pop(items: &[DisplayItem], push: usize) -> usize {
    let mut depth = 0i32;
    for (i, it) in items.iter().enumerate().skip(push) {
        match it {
            DisplayItem::PushEffect(_) => depth += 1,
            DisplayItem::PopEffect => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
    }
    items.len()
}

/// Union bbox of the leaf items in a span (markers contribute nothing).
fn items_bbox(items: &[DisplayItem], fonts: &mut FontCache) -> Option<[f32; 4]> {
    let mut bb: Option<[f32; 4]> = None;
    let mut grow = |x: f32, y: f32, w: f32, h: f32| {
        let (x2, y2) = (x + w, y + h);
        bb = Some(match bb {
            None => [x, y, x2, y2],
            Some(b) => [b[0].min(x), b[1].min(y), b[2].max(x2), b[3].max(y2)],
        });
    };
    for it in items {
        match it {
            DisplayItem::FillRect { x, y, w, h, .. }
            | DisplayItem::RoundedRect { x, y, w, h, .. }
            | DisplayItem::Raster { x, y, w, h, .. }
            | DisplayItem::Image { x, y, w, h, .. } => grow(*x, *y, *w, *h),
            DisplayItem::Text {
                x,
                baseline,
                text,
                font,
                size,
                ..
            } => {
                let w = fonts
                    .font(*font)
                    .map(|f| f.measure(text, *size))
                    .unwrap_or(text.len() as f32 * size * 0.5);
                grow(*x, baseline - size * 1.2, w, size * 1.6);
            }
            _ => {}
        }
    }
    bb.map(|b| [b[0], b[1], b[2] - b[0], b[3] - b[1]])
}

/// Extra paint extent a filter list needs around the span bbox
/// [left, top, right, bottom].
fn ops_extent(ops: &[FilterOp]) -> [f32; 4] {
    let mut e = [0.0f32; 4];
    for op in ops {
        match op {
            FilterOp::Blur(s) => {
                for k in e.iter_mut() {
                    *k = k.max(s * 3.0);
                }
            }
            FilterOp::DropShadow { dx, dy, blur, .. } => {
                let p = blur * 3.0;
                e[0] = e[0].max(p - dx);
                e[1] = e[1].max(p - dy);
                e[2] = e[2].max(p + dx);
                e[3] = e[3].max(p + dy);
            }
            _ => {}
        }
    }
    e
}

/// Float bbox + per-side padding → int rect clamped to the canvas.
fn pad_int(b: [f32; 4], pad: [f32; 4], canvas: &Canvas) -> Option<(i32, i32, u32, u32)> {
    let x0 = ((b[0] - pad[0]).floor() as i32).max(0);
    let y0 = ((b[1] - pad[1]).floor() as i32).max(0);
    let x1 = ((b[0] + b[2] + pad[2]).ceil() as i32).min(canvas.width as i32);
    let y1 = ((b[1] + b[3] + pad[3]).ceil() as i32).min(canvas.height as i32);
    if x1 <= x0 || y1 <= y0 {
        None
    } else {
        Some((x0, y0, (x1 - x0) as u32, (y1 - y0) as u32))
    }
}

/// Paint the span twice (black/white backdrops) → straight-alpha tile.
fn render_tile(
    inner: &[DisplayItem],
    x0: i32,
    y0: i32,
    w: u32,
    h: u32,
    fonts: &mut FontCache,
    images: &HashMap<String, Image>,
) -> Tile {
    let off: Vec<DisplayItem> = inner
        .iter()
        .cloned()
        .map(|it| offset_item(it, -x0 as f32, -y0 as f32))
        .collect();
    let mut cb = Canvas::new(w, h, bgra(0, 0, 0));
    crate::paint_into(&mut cb, &off, fonts, images);
    let mut cw = Canvas::new(w, h, bgra(255, 255, 255));
    crate::paint_into(&mut cw, &off, fonts, images);
    let n = (w * h) as usize;
    let mut px = vec![0u8; n * 4];
    for i in 0..n {
        let a = (0..3)
            .map(|k| 255i32 - (cw.pixels[i * 4 + k] as i32 - cb.pixels[i * 4 + k] as i32))
            .sum::<i32>()
            / 3;
        let a = a.clamp(0, 255) as u32;
        for k in 0..3 {
            px[i * 4 + k] = if a > 0 {
                (cb.pixels[i * 4 + k] as u32 * 255 / a).min(255) as u8
            } else {
                0
            };
        }
        px[i * 4 + 3] = a as u8;
    }
    Tile {
        ox: x0,
        oy: y0,
        w: w as usize,
        h: h as usize,
        px,
    }
}

fn apply_op(tile: &mut Tile, op: &FilterOp) {
    match op {
        FilterOp::Matrix(m) => apply_matrix(&mut tile.px, m),
        FilterOp::Blur(s) => blur_tile(&mut tile.px, tile.w, tile.h, *s),
        FilterOp::DropShadow {
            dx,
            dy,
            blur,
            color,
        } => drop_shadow(tile, *dx, *dy, *blur, *color),
    }
}

/// 4×5 RGBA matrix (0..1 domain) over straight-alpha BGRA pixels.
fn apply_matrix(px: &mut [u8], m: &[f32; 20]) {
    for p in px.chunks_exact_mut(4) {
        let c = [
            p[2] as f32 / 255.0,
            p[1] as f32 / 255.0,
            p[0] as f32 / 255.0,
            p[3] as f32 / 255.0,
        ];
        for row in 0..3 {
            let v = m[row * 5] * c[0]
                + m[row * 5 + 1] * c[1]
                + m[row * 5 + 2] * c[2]
                + m[row * 5 + 3] * c[3]
                + m[row * 5 + 4];
            p[2 - row] = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
        let a = m[15] * c[0] + m[16] * c[1] + m[17] * c[2] + m[18] * c[3] + m[19];
        p[3] = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
    }
}

/// Gaussian ≈ 3 passes of a separable box blur, on premultiplied colors
/// (avoids dark halos at alpha edges).
fn blur_tile(px: &mut Vec<u8>, w: usize, h: usize, sigma: f32) {
    let r = sigma.round().max(0.0) as usize;
    if r == 0 {
        return;
    }
    for p in px.chunks_exact_mut(4) {
        let a = p[3] as u32;
        for k in 0..3 {
            p[k] = (p[k] as u32 * a / 255) as u8;
        }
    }
    for _ in 0..3 {
        *px = box_blur_h(px, w, h, r);
        *px = box_blur_v(px, w, h, r);
    }
    for p in px.chunks_exact_mut(4) {
        let a = p[3] as u32;
        if a == 0 {
            p[0] = 0;
            p[1] = 0;
            p[2] = 0;
            continue;
        }
        for k in 0..3 {
            p[k] = (p[k] as u32 * 255 / a).min(255) as u8;
        }
    }
}

fn box_blur_h(px: &[u8], w: usize, h: usize, r: usize) -> Vec<u8> {
    let mut out = px.to_vec();
    for y in 0..h {
        let row = y * w * 4;
        let mut acc = [0u32; 4];
        let mut cnt = 0u32;
        for x in 0..=r.min(w - 1) {
            let i = row + x * 4;
            for k in 0..4 {
                acc[k] += px[i + k] as u32;
            }
            cnt += 1;
        }
        for x in 0..w {
            let i = row + x * 4;
            for k in 0..4 {
                out[i + k] = (acc[k] / cnt) as u8;
            }
            let xr = x + r + 1;
            if xr < w {
                for k in 0..4 {
                    acc[k] += px[row + xr * 4 + k] as u32;
                }
                cnt += 1;
            }
            if x >= r {
                for k in 0..4 {
                    acc[k] -= px[row + (x - r) * 4 + k] as u32;
                }
                cnt -= 1;
            }
        }
    }
    out
}

fn box_blur_v(px: &[u8], w: usize, h: usize, r: usize) -> Vec<u8> {
    let mut out = px.to_vec();
    for x in 0..w {
        let mut acc = [0u32; 4];
        let mut cnt = 0u32;
        for y in 0..=r.min(h - 1) {
            let i = (y * w + x) * 4;
            for k in 0..4 {
                acc[k] += px[i + k] as u32;
            }
            cnt += 1;
        }
        for y in 0..h {
            let i = (y * w + x) * 4;
            for k in 0..4 {
                out[i + k] = (acc[k] / cnt) as u8;
            }
            let yr = y + r + 1;
            if yr < h {
                for k in 0..4 {
                    acc[k] += px[(yr * w + x) * 4 + k] as u32;
                }
                cnt += 1;
            }
            if y >= r {
                for k in 0..4 {
                    acc[k] -= px[((y - r) * w + x) * 4 + k] as u32;
                }
                cnt -= 1;
            }
        }
    }
    out
}

/// Single-channel blur for the drop-shadow alpha mask.
fn blur_mask_sigma(mask: &mut Vec<u8>, w: usize, h: usize, sigma: f32) {
    let r = sigma.round().max(0.0) as usize;
    if r == 0 {
        return;
    }
    for _ in 0..3 {
        // horizontal
        let src = mask.clone();
        for y in 0..h {
            let mut acc = 0u32;
            let mut cnt = 0u32;
            for x in 0..=r.min(w - 1) {
                acc += src[y * w + x] as u32;
                cnt += 1;
            }
            for x in 0..w {
                mask[y * w + x] = (acc / cnt) as u8;
                if x + r + 1 < w {
                    acc += src[y * w + x + r + 1] as u32;
                    cnt += 1;
                }
                if x >= r {
                    acc -= src[y * w + x - r] as u32;
                    cnt -= 1;
                }
            }
        }
        // vertical
        let src = mask.clone();
        for x in 0..w {
            let mut acc = 0u32;
            let mut cnt = 0u32;
            for y in 0..=r.min(h - 1) {
                acc += src[y * w + x] as u32;
                cnt += 1;
            }
            for y in 0..h {
                mask[y * w + x] = (acc / cnt) as u8;
                if y + r + 1 < h {
                    acc += src[(y + r + 1) * w + x] as u32;
                    cnt += 1;
                }
                if y >= r {
                    acc -= src[(y - r) * w + x] as u32;
                    cnt -= 1;
                }
            }
        }
    }
}

/// `s` over `d`, both straight-alpha BGRA pixels.
fn over_px(d: &mut [u8], s: &[u8]) {
    let sa = s[3] as u32;
    if sa == 0 {
        return;
    }
    if sa >= 255 {
        d[..4].copy_from_slice(&s[..4]);
        return;
    }
    let da = d[3] as u32;
    let inv = 255 - sa;
    let oa = sa + da * inv / 255;
    if oa == 0 {
        d[3] = 0;
        return;
    }
    for k in 0..3 {
        d[k] = ((s[k] as u32 * sa + d[k] as u32 * da * inv / 255) / oa) as u8;
    }
    d[3] = oa as u8;
}

/// Blur the tile's alpha, tint it `color`, offset by (dx,dy) and put the
/// content back on top — growing the tile to fit the shadow.
fn drop_shadow(tile: &mut Tile, dx: f32, dy: f32, blur: f32, color: u32) {
    let pad = (blur * 3.0).ceil() as i32;
    let (dx, dy) = (dx.round() as i32, dy.round() as i32);
    let nx0 = 0.min(dx - pad);
    let ny0 = 0.min(dy - pad);
    let nx1 = (tile.w as i32).max(dx + tile.w as i32 + pad);
    let ny1 = (tile.h as i32).max(dy + tile.h as i32 + pad);
    let (nw, nh) = ((nx1 - nx0) as usize, (ny1 - ny0) as usize);
    let mut mask: Vec<u8> = tile.px.chunks_exact(4).map(|p| p[3]).collect();
    blur_mask_sigma(&mut mask, tile.w, tile.h, blur);
    let [sb, sg, sr, sa] = color.to_le_bytes();
    let mut out = vec![0u8; nw * nh * 4];
    for y in 0..tile.h {
        for x in 0..tile.w {
            let a = mask[y * tile.w + x] as u32 * sa as u32 / 255;
            if a == 0 {
                continue;
            }
            let (px_, py_) = (x as i32 + dx - nx0, y as i32 + dy - ny0);
            let i = (py_ as usize * nw + px_ as usize) * 4;
            out[i] = sb;
            out[i + 1] = sg;
            out[i + 2] = sr;
            out[i + 3] = a as u8;
        }
    }
    // content over shadow, shifted by the canvas-space origin delta
    for y in 0..tile.h {
        let gy = y as i32 - ny0;
        for x in 0..tile.w {
            let gx = x as i32 - nx0;
            let si = (y * tile.w + x) * 4;
            let di = (gy as usize * nw + gx as usize) * 4;
            over_px(&mut out[di..di + 4], &tile.px[si..si + 4]);
        }
    }
    *tile = Tile {
        ox: tile.ox + nx0,
        oy: tile.oy + ny0,
        w: nw,
        h: nh,
        px: out,
    };
}

/// Zero the alpha of every pixel outside the clip shape.
fn clip_tile(tile: &mut Tile, abs: &ClipAbs) {
    for y in 0..tile.h {
        let cy = tile.oy as f32 + y as f32 + 0.5;
        for x in 0..tile.w {
            let cx = tile.ox as f32 + x as f32 + 0.5;
            if !clip_covers(abs, cx, cy) {
                tile.px[(y * tile.w + x) * 4 + 3] = 0;
            }
        }
    }
}

/// `mask-mode:alpha` — each tile pixel's alpha is scaled by the mask
/// image's alpha at the corresponding point (mask stretched to tile).
fn mask_tile(tile: &mut Tile, m: &Image) {
    if m.width == 0 || m.height == 0 {
        return;
    }
    for y in 0..tile.h {
        let sy =
            (y as u64 * m.height as u64 / tile.h.max(1) as u64).min(m.height as u64 - 1) as usize;
        for x in 0..tile.w {
            let sx =
                (x as u64 * m.width as u64 / tile.w.max(1) as u64).min(m.width as u64 - 1) as usize;
            let ma = m.pixels[(sy * m.width as usize + sx) * 4 + 3] as u32;
            let i = (y * tile.w + x) * 4;
            tile.px[i + 3] = (tile.px[i + 3] as u32 * ma / 255) as u8;
        }
    }
}

/// Source-over composite of a straight-alpha tile onto the opaque canvas.
fn composite(canvas: &mut Canvas, tile: &Tile) {
    let (cw, ch) = (canvas.width as i32, canvas.height as i32);
    for y in 0..tile.h as i32 {
        let gy = tile.oy + y;
        if gy < 0 || gy >= ch {
            continue;
        }
        for x in 0..tile.w as i32 {
            let gx = tile.ox + x;
            if gx < 0 || gx >= cw {
                continue;
            }
            let si = ((y * tile.w as i32 + x) * 4) as usize;
            let di = ((gy * cw + gx) * 4) as usize;
            let a = tile.px[si + 3] as u32;
            if a == 0 {
                continue;
            }
            if a == 255 {
                canvas.pixels[di..di + 4].copy_from_slice(&tile.px[si..si + 4]);
            } else {
                let inv = 255 - a;
                for k in 0..3 {
                    canvas.pixels[di + k] = ((tile.px[si + k] as u32 * a
                        + canvas.pixels[di + k] as u32 * inv)
                        / 255) as u8;
                }
                canvas.pixels[di + 3] = 255;
            }
        }
    }
}

/// Blend-mode composite: out = a·B(s,d) + (1−a)·d per channel.
fn blend_composite(canvas: &mut Canvas, tile: &Tile, mode: BlendMode) {
    let (cw, ch) = (canvas.width as i32, canvas.height as i32);
    for y in 0..tile.h as i32 {
        let gy = tile.oy + y;
        if gy < 0 || gy >= ch {
            continue;
        }
        for x in 0..tile.w as i32 {
            let gx = tile.ox + x;
            if gx < 0 || gx >= cw {
                continue;
            }
            let si = ((y * tile.w as i32 + x) * 4) as usize;
            let di = ((gy * cw + gx) * 4) as usize;
            let a = tile.px[si + 3] as f32 / 255.0;
            if a == 0.0 {
                continue;
            }
            let s = (
                tile.px[si + 2] as f32 / 255.0,
                tile.px[si + 1] as f32 / 255.0,
                tile.px[si] as f32 / 255.0,
            );
            let d = (
                canvas.pixels[di + 2] as f32 / 255.0,
                canvas.pixels[di + 1] as f32 / 255.0,
                canvas.pixels[di] as f32 / 255.0,
            );
            let (br, bg, bb) = blend_rgb(mode, s, d);
            canvas.pixels[di + 2] = ((br * a + d.0 * (1.0 - a)) * 255.0).round() as u8;
            canvas.pixels[di + 1] = ((bg * a + d.1 * (1.0 - a)) * 255.0).round() as u8;
            canvas.pixels[di] = ((bb * a + d.2 * (1.0 - a)) * 255.0).round() as u8;
        }
    }
}

/// `backdrop-filter`: apply the ops to the canvas region behind the
/// element, in place (region extracted, processed, written back).
fn backdrop_apply(canvas: &mut Canvas, rect: [f32; 4], ops: &[FilterOp]) {
    let Some((x0, y0, w, h)) = pad_int(rect, [0.0; 4], canvas) else {
        return;
    };
    let (w, h) = (w as usize, h as usize);
    let stride = canvas.width as usize * 4;
    let mut px = vec![0u8; w * h * 4];
    for y in 0..h {
        let src = (y0 as usize + y) * stride + x0 as usize * 4;
        px[y * w * 4..y * w * 4 + w * 4].copy_from_slice(&canvas.pixels[src..src + w * 4]);
    }
    for op in ops {
        match op {
            FilterOp::Matrix(m) => apply_matrix(&mut px, m),
            FilterOp::Blur(s) => {
                let r = s.round().max(0.0) as usize;
                for _ in 0..3 {
                    px = box_blur_h(&px, w, h, r);
                    px = box_blur_v(&px, w, h, r);
                }
            }
            // a backdrop has no silhouette to shadow — skip
            FilterOp::DropShadow { .. } => {}
        }
    }
    for y in 0..h {
        let dst = (y0 as usize + y) * stride + x0 as usize * 4;
        canvas.pixels[dst..dst + w * 4].copy_from_slice(&px[y * w * 4..y * w * 4 + w * 4]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ops(v: &str) -> Vec<FilterOp> {
        rowser_layout::colorfx::parse_filter(v, 16.0, bgra(0, 0, 0))
    }

    fn px_at(px: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        [px[i], px[i + 1], px[i + 2], px[i + 3]]
    }

    #[test]
    fn filter_invert_rect() {
        let mut f = FontCache::new();
        let items = vec![
            DisplayItem::PushEffect(Effect::Filter(parse_ops("invert(1)"))),
            DisplayItem::FillRect {
                x: 0.0,
                y: 0.0,
                w: 10.0,
                h: 10.0,
                color: bgra(255, 0, 0),
            },
            DisplayItem::PopEffect,
        ];
        let px = crate::paint(
            &items,
            &mut f,
            20,
            20,
            rowser_layout::WHITE,
            &HashMap::new(),
        );
        let p = px_at(&px, 20, 5, 5);
        assert_eq!(&p[..3], &[255, 255, 0]); // inverted red → cyan (b,g,r)
    }

    #[test]
    fn clip_circle_rect() {
        let mut f = FontCache::new();
        let shape =
            rowser_layout::colorfx::parse_clip_path("circle(50% at 50% 50%)", 16.0).unwrap();
        let items = vec![
            DisplayItem::PushEffect(Effect::Clip {
                shape,
                rect: Some([0.0, 0.0, 100.0, 100.0]),
            }),
            DisplayItem::FillRect {
                x: 0.0,
                y: 0.0,
                w: 100.0,
                h: 100.0,
                color: bgra(255, 0, 0),
            },
            DisplayItem::PopEffect,
        ];
        let px = crate::paint(
            &items,
            &mut f,
            100,
            100,
            rowser_layout::WHITE,
            &HashMap::new(),
        );
        // center inside circle → red; corner clipped → white
        assert_eq!(px_at(&px, 100, 50, 50)[2], 255);
        assert_eq!(px_at(&px, 100, 2, 2), [255, 255, 255, 255]);
    }

    #[test]
    fn blend_multiply() {
        let mut f = FontCache::new();
        // white bg rect, then a red blend-multiply rect on top
        let items = vec![
            DisplayItem::FillRect {
                x: 0.0,
                y: 0.0,
                w: 20.0,
                h: 20.0,
                color: bgra(255, 255, 255),
            },
            DisplayItem::PushEffect(Effect::Blend(BlendMode::Multiply)),
            DisplayItem::FillRect {
                x: 5.0,
                y: 5.0,
                w: 10.0,
                h: 10.0,
                color: bgra(255, 0, 0),
            },
            DisplayItem::PopEffect,
        ];
        let px = crate::paint(
            &items,
            &mut f,
            20,
            20,
            rowser_layout::WHITE,
            &HashMap::new(),
        );
        assert_eq!(&px_at(&px, 20, 10, 10)[..3], &[0, 0, 255]);
    }

    #[test]
    fn blur_diffuses() {
        let mut f = FontCache::new();
        let items = vec![
            DisplayItem::PushEffect(Effect::Filter(parse_ops("blur(2px)"))),
            DisplayItem::FillRect {
                x: 8.0,
                y: 8.0,
                w: 4.0,
                h: 4.0,
                color: bgra(0, 0, 0),
            },
            DisplayItem::PopEffect,
        ];
        let px = crate::paint(
            &items,
            &mut f,
            20,
            20,
            rowser_layout::WHITE,
            &HashMap::new(),
        );
        // blur smears beyond the rect's hard edge
        let edge = px_at(&px, 20, 5, 10);
        assert!(edge[0] < 250, "blur should smear beyond the rect");
        // a tiny rect under σ=2 mostly washes out, but its center is still
        // the darkest point — darker than the fringe
        let center = px_at(&px, 20, 10, 10);
        assert!(center[0] < 240 && center[0] < edge[0]);
    }

    #[test]
    fn drop_shadow_offset() {
        let mut f = FontCache::new();
        let items = vec![
            DisplayItem::PushEffect(Effect::Filter(parse_ops("drop-shadow(4px 0px 0px blue)"))),
            DisplayItem::FillRect {
                x: 0.0,
                y: 0.0,
                w: 10.0,
                h: 10.0,
                color: bgra(255, 0, 0),
            },
            DisplayItem::PopEffect,
        ];
        let px = crate::paint(
            &items,
            &mut f,
            20,
            20,
            rowser_layout::WHITE,
            &HashMap::new(),
        );
        // shadow sticks out to the right of the red rect → blue channel
        assert_eq!(&px_at(&px, 20, 12, 5)[..3], &[255, 0, 0]);
        // the rect itself still red
        assert_eq!(&px_at(&px, 20, 5, 5)[..3], &[0, 0, 255]);
    }

    #[test]
    fn backdrop_blur_region() {
        let mut f = FontCache::new();
        // left half black over a white canvas; a backdrop-blur over the
        // whole strip smears the boundary; the element's own small white
        // rect then paints on top
        let items = vec![
            DisplayItem::FillRect {
                x: 0.0,
                y: 0.0,
                w: 10.0,
                h: 20.0,
                color: bgra(0, 0, 0),
            },
            DisplayItem::PushEffect(Effect::Backdrop {
                ops: parse_ops("blur(3px)"),
                rect: Some([0.0, 0.0, 20.0, 20.0]),
            }),
            DisplayItem::FillRect {
                x: 0.0,
                y: 0.0,
                w: 2.0,
                h: 2.0,
                color: bgra(255, 255, 255),
            },
            DisplayItem::PopEffect,
        ];
        let px = crate::paint(
            &items,
            &mut f,
            20,
            20,
            rowser_layout::WHITE,
            &HashMap::new(),
        );
        // the element's own rect painted over the blurred backdrop
        assert_eq!(&px_at(&px, 20, 0, 0)[..3], &[255, 255, 255]);
        // the black/white boundary at x=10 smeared into a grey band
        let p = px_at(&px, 20, 10, 10);
        assert!(
            p[0] > 60 && p[0] < 200,
            "boundary should blur to grey, got {p:?}"
        );
        // far from the boundary the colors survive
        assert!(px_at(&px, 20, 2, 10)[0] < 40);
        assert!(px_at(&px, 20, 18, 10)[0] > 215);
    }
}
