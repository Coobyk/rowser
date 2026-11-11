//! rowser-paint: display list → BGRA pixel buffer (premultiplied-friendly;
//! we emit alpha=255 everywhere for now).
//!
//! Text is drawn by blending cached glyph coverage bitmaps with the run
//! color. Underline/strikethrough are plain rects under/over the baseline.

use rowser_image::Image;
use rowser_layout::xform::{items_bbox, Mat};
use rowser_layout::{offset_item, Color, DisplayItem};
use std::collections::HashMap;
use rowser_text::FontCache;

pub struct Canvas {
    pub width: u32,
    pub height: u32,
    /// BGRA bytes
    pub pixels: Vec<u8>,
}

impl Canvas {
    pub fn new(width: u32, height: u32, bg: Color) -> Self {
        let mut px = vec![0u8; (width * height * 4) as usize];
        fill(&mut px, width, height, 0, 0, width, height, bg);
        Self {
            width,
            height,
            pixels: px,
        }
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: u32, h: u32, color: Color) {
        fill(&mut self.pixels, self.width, self.height, x, y, w, h, color);
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
        // four corner quarter-discs
        for (cx, cy) in [(x + ri, y + ri), (x + w as i32 - ri, y + ri), (x + ri, y + h as i32 - ri), (x + w as i32 - ri, y + h as i32 - ri)] {
            let mut dy = -ri;
            while dy <= ri {
                let mut dx = -ri;
                while dx <= ri {
                    let dist = ((dx * dx + dy * dy) as f32).sqrt();
                    if dist <= r {
                        self.fill_rect(cx + dx, cy + dy, 1, 1, color);
                    }
                    dx += 1;
                }
                dy += 1;
            }
        }
    }

    /// Blend a coverage bitmap with `color` onto the canvas at (x, y).
    pub fn blend_alpha(&mut self, x: i32, y: i32, w: u32, h: u32, alpha: &[u8], color: Color) {
        let (cw, ch) = (self.width as i32, self.height as i32);
        let [b, g, r, _] = color.to_le_bytes();
        for gy in 0..h as i32 {
            let dy = y + gy;
            if dy < 0 || dy >= ch {
                continue;
            }
            for gx in 0..w as i32 {
                let dx = x + gx;
                if dx < 0 || dx >= cw {
                    continue;
                }
                let a = alpha[(gy * w as i32 + gx) as usize] as u32;
                if a == 0 {
                    continue;
                }
                let i = ((dy * cw + dx) * 4) as usize;
                let inv = 255 - a;
                self.pixels[i] = ((b as u32 * a + self.pixels[i] as u32 * inv) / 255) as u8;
                self.pixels[i + 1] =
                    ((g as u32 * a + self.pixels[i + 1] as u32 * inv) / 255) as u8;
                self.pixels[i + 2] =
                    ((r as u32 * a + self.pixels[i + 2] as u32 * inv) / 255) as u8;
                self.pixels[i + 3] = 255;
            }
        }
    }
}

fn fill(px: &mut [u8], w: u32, h: u32, x: i32, y: i32, rw: u32, rh: u32, color: Color) {
    let (w, h) = (w as i32, h as i32);
    let [b, g, r, _] = color.to_le_bytes();
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + rw as i32).min(w);
    let y1 = (y + rh as i32).min(h);
    for yy in y0..y1 {
        for xx in x0..x1 {
            let i = ((yy * w + xx) * 4) as usize;
            px[i] = b;
            px[i + 1] = g;
            px[i + 2] = r;
            px[i + 3] = 255;
        }
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
    fonts: &mut FontCache,
    images: &HashMap<String, Image>,
    m: &Mat,
) {
    let ch = canvas.height as f32;
    let mut idx = 0;
    while idx < items.len() {
        let item = &items[idx];
        if let DisplayItem::PushTransform { m: inner } = item {
            let total = m.mul(inner);
            // the matching PopTransform (nesting counts)
            let mut depth = 1i32;
            let mut j = idx + 1;
            while j < items.len() && depth > 0 {
                match items[j] {
                    DisplayItem::PushTransform { .. } => depth += 1,
                    DisplayItem::PopTransform => depth -= 1,
                    _ => {}
                }
                j += 1;
            }
            paint_transformed(canvas, &items[idx + 1..j - 1], &total, fonts, images);
            idx = j;
            continue;
        }
        if matches!(item, DisplayItem::PopTransform) {
            idx += 1;
            continue;
        }
        // Vertical cull: items fully off-canvas can't write visible pixels.
        // Text gets a ±4em band — glyph ink can overshoot the em box, but
        // never by multiples of it.
        let (top, bot) = match item {
            DisplayItem::FillRect { y, h, .. }
            | DisplayItem::RoundedRect { y, h, .. }
            | DisplayItem::Raster { y, h, .. }
            | DisplayItem::Image { y, h, .. } => (*y, *y + *h),
            DisplayItem::Text { baseline, size, .. } => {
                (baseline - size * 4.0, baseline + size * 4.0)
            }
            DisplayItem::PushTransform { .. } | DisplayItem::PopTransform => {
                unreachable!()
            }
        };
        if bot < 0.0 || top > ch {
            idx += 1;
            continue;
        }
        match item {
            DisplayItem::RoundedRect { x, y, w, h, radius, color } => {
                canvas.fill_rounded(*x as i32, *y as i32, *w as u32, *h as u32, *radius, *color);
            }
            DisplayItem::Raster { x, y, w, h, pixels } => {
                canvas.blit_pixels(pixels, *x as i32, *y as i32, *w as i32, *h as i32);
            }
            DisplayItem::Image { x, y, w, h, src } => {
                if let Some(img) = images.get(src) {
                    canvas.blit(img, *x as i32, *y as i32, *w as i32, *h as i32);
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
            DisplayItem::FillRect { x, y, w, h, color } => {
                canvas.fill_rect(*x as i32, *y as i32, *w as u32, *h as u32, *color);
            }
            DisplayItem::Text {
                x,
                baseline,
                text,
                font,
                size,
                color,
                underline,
                strike,
                href: _,
            } => {
                let mut pen = *x;
                let mut prev = 0u16;
                for c in text.chars() {
                    let Some(f) = fonts.font(*font) else { break };
                    let gid = f.glyph_id(c);
                    let upem = f.units_per_em() as f32;
                    pen += f.kern(prev, gid) as f32 * size / upem;
                    let advance = f.advance(gid, *size);
                    if let Some(r) = fonts.raster(*font, gid, *size) {
                        let (rw, rh, bx, by) = (r.width, r.height, r.bearing_x, r.bearing_y);
                        let alpha = &r.alpha;
                        canvas.blend_alpha(
                            (pen + bx as f32).round() as i32,
                            (*baseline - by as f32).round() as i32,
                            rw,
                            rh,
                            alpha,
                            *color,
                        );
                    }
                    pen += advance;
                    prev = gid;
                }
                let end = pen;
                if *underline {
                    let uy = *baseline + 0.1 * size;
                    let thick = (size / 14.0).max(1.0);
                    canvas.fill_rect(*x as i32, uy as i32, (end - *x) as u32, thick as u32, *color);
                }
                if *strike {
                    let sy = *baseline - 0.28 * size;
                    let thick = (size / 16.0).max(1.0);
                    canvas.fill_rect(*x as i32, sy as i32, (end - *x) as u32, thick as u32, *color);
                }
            }
            DisplayItem::PushTransform { .. } | DisplayItem::PopTransform => {}
        }
        idx += 1;
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
    fonts: &mut FontCache,
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
    let (tw, th) = ((bw + 2.0 * PAD).ceil() as u32, (bh + 2.0 * PAD).ceil() as u32);
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
                underline: false,
                strike: false,
                href: None,
            },
            DisplayItem::Image {
                x: 0.0,
                y: 0.0,
                w: 10.0,
                h: 10.0,
                src: "missing.png".into(),
            },
            DisplayItem::FillRect {
                x: 0.0,
                y: 0.0,
                w: 5.0,
                h: 5.0,
                color: bgra(255, 0, 0),
            },
        ];
        let px = paint(&items, &mut fonts, 200, 100, rowser_layout::WHITE, &HashMap::new());
        // some dark pixels must exist (the text)
        let dark = px.chunks(4).filter(|p| p[2] < 100).count();
        assert!(dark > 50);
        // red rect top-left
        assert_eq!(px[2], 255);
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
        let flat = paint(&items, &mut fonts, w, h, rowser_layout::WHITE, &HashMap::new());
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
        let rot = paint(&items, &mut fonts, w, h, rowser_layout::WHITE, &HashMap::new());
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
        let px = paint(&items, &mut fonts, w, h, rowser_layout::WHITE, &HashMap::new());
        // rect (4,4,10,10) → painted at (19,14)-(29,24)
        assert_eq!(px_at(&px, w, 24, 19), [0, 0, 255, 255], "shifted rect");
        assert_eq!(px_at(&px, w, 5, 5), [255, 255, 255, 255], "original spot");
    }
}



impl Canvas {
    /// Nearest-neighbor blit of a decoded BGRA image into a rect.
    /// Blit a raw BGRA buffer of `dw`×`dh` pixels (no scaling).
    pub fn blit_pixels(&mut self, pixels: &[u8], dx: i32, dy: i32, dw: i32, dh: i32) {
        let img = Image { width: dw.max(0) as u32, height: dh.max(0) as u32, pixels: pixels.to_vec() };
        self.blit(&img, dx, dy, dw, dh);
    }

    fn blit(&mut self, img: &Image, dx: i32, dy: i32, dw: i32, dh: i32) {
        if img.width == 0 || img.height == 0 {
            return;
        }
        let (cw, ch) = (self.width as i32, self.height as i32);
        for y in dy.max(0)..(dy + dh).min(ch) {
            let sy = ((y - dy) as u32 * img.height / dh.max(1) as u32).min(img.height - 1);
            for x in dx.max(0)..(dx + dw).min(cw) {
                let sx = ((x - dx) as u32 * img.width / dw.max(1) as u32).min(img.width - 1);
                let si = ((sy * img.width + sx) * 4) as usize;
                let di = ((y * cw + x) * 4) as usize;
                let (b, g, r, a) = (
                    img.pixels[si],
                    img.pixels[si + 1],
                    img.pixels[si + 2],
                    img.pixels[si + 3] as u32,
                );
                if a == 255 {
                    self.pixels[di] = b;
                    self.pixels[di + 1] = g;
                    self.pixels[di + 2] = r;
                    self.pixels[di + 3] = 255;
                } else if a > 0 {
                    let inv = 255 - a;
                    self.pixels[di] = ((b as u32 * a + self.pixels[di] as u32 * inv) / 255) as u8;
                    self.pixels[di + 1] =
                        ((g as u32 * a + self.pixels[di + 1] as u32 * inv) / 255) as u8;
                    self.pixels[di + 2] =
                        ((r as u32 * a + self.pixels[di + 2] as u32 * inv) / 255) as u8;
                    self.pixels[di + 3] = 255;
                }
            }
        }
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
        let (x0, y0) = ((x0.floor() as i32).max(0), (y0.floor() as i32).max(0));
        let (x1, y1) = ((x1.ceil() as i32).min(cw), (y1.ceil() as i32).min(ch));
        for y in y0..y1 {
            for x in x0..x1 {
                // pixel-center sampling back into the tile
                let (sx, sy) = inv.apply(x as f32 + 0.5, y as f32 + 0.5);
                if sx < 0.0 || sy < 0.0 || sx >= sw as f32 || sy >= sh as f32 {
                    continue;
                }
                let (b, g, r, a) = bilinear(src, sw, sh, sx, sy);
                if a == 0 {
                    continue;
                }
                let i = ((y * cw + x) * 4) as usize;
                let ia = 255 - a;
                self.pixels[i] = ((b * a + self.pixels[i] as u32 * ia) / 255) as u8;
                self.pixels[i + 1] = ((g * a + self.pixels[i + 1] as u32 * ia) / 255) as u8;
                self.pixels[i + 2] = ((r * a + self.pixels[i + 2] as u32 * ia) / 255) as u8;
                self.pixels[i + 3] = 255;
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
    let mut acc = [0f32; 4];
    for oy in 0..2 {
        for ox in 0..2 {
            let wgt = (if ox == 0 { 1.0 - fx } else { fx })
                * (if oy == 0 { 1.0 - fy } else { fy });
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
