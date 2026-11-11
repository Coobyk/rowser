//! fx: per-side border styles, box-shadow blur, and the overflow clip
//! stack — the raster side of rowser-layout's borderfx items.

use crate::Canvas;
use rowser_layout::{BorderEdge, BorderStyle, Color};

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
                let (cx, cy) = if horiz { (x + s + r, y + r) } else { (x + r, y + s + r) };
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
            let (outer, inner) = if outer_dark { (dark, light) } else { (light, dark) };
            let half = (thick / 2.0).max(0.0);
            if half < 0.5 {
                cv.fill_rect(x as i32, y as i32, w as u32, h as u32, outer);
                return;
            }
            match side {
                BorderEdge::Top => {
                    cv.fill_rect(x as i32, y as i32, w as u32, half as u32, outer);
                    cv.fill_rect(x as i32, (y + half) as i32, w as u32, (h - half) as u32, inner);
                }
                BorderEdge::Bottom => {
                    cv.fill_rect(x as i32, (y + half) as i32, w as u32, (h - half) as u32, outer);
                    cv.fill_rect(x as i32, y as i32, w as u32, half as u32, inner);
                }
                BorderEdge::Left => {
                    cv.fill_rect(x as i32, y as i32, half as u32, h as u32, outer);
                    cv.fill_rect((x + half) as i32, y as i32, (w - half) as u32, h as u32, inner);
                }
                BorderEdge::Right => {
                    cv.fill_rect((x + half) as i32, y as i32, (w - half) as u32, h as u32, outer);
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
        let (x1, y1) = (((x + w).ceil() as i32).min(bx1), ((y + h).ceil() as i32).min(by1));
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
                blend(cv, &[255u8.saturating_sub(cov)], 1, x0 + i as i32, y0 + j as i32, 0, 0);
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
