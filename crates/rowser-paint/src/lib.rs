//! rowser-paint: display list → BGRA pixel buffer (premultiplied-friendly;
//! we emit alpha=255 everywhere for now).
//!
//! Text is drawn by blending cached glyph coverage bitmaps with the run
//! color. Underline/strikethrough are plain rects under/over the baseline.

use rowser_image::Image;
use rowser_layout::xform::{items_bbox, Mat};
use rowser_layout::{offset_item, Color, DisplayItem};
use rowser_text::FontCache;
use std::collections::HashMap;

pub mod fx;
use fx::Clip;

pub struct Canvas {
    pub width: u32,
    pub height: u32,
    /// BGRA bytes
    pub pixels: Vec<u8>,
    /// Active overflow-clip stack (DisplayItem::PushClip/PopClip).
    pub clips: Vec<Clip>,
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
        }
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: u32, h: u32, color: Color) {
        fill(
            &mut self.pixels,
            self.width,
            self.height,
            x,
            y,
            w,
            h,
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
        // four corner quarter-discs
        for (cx, cy) in [
            (x + ri, y + ri),
            (x + w as i32 - ri, y + ri),
            (x + ri, y + h as i32 - ri),
            (x + w as i32 - ri, y + h as i32 - ri),
        ] {
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
        for dy in 0..h as i32 {
            let py = y + dy;
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
            if xr > xl {
                self.fill_rect(xl, py, (xr - xl) as u32, 1, color);
            }
        }
    }

    /// Blend a coverage bitmap with `color` onto the canvas at (x, y).
    pub fn blend_alpha(&mut self, x: i32, y: i32, w: u32, h: u32, alpha: &[u8], color: Color) {
        let (cw, ch) = (self.width as i32, self.height as i32);
        let (sx0, sy0, sx1, sy1) = fx::scissor(&self.clips, cw, ch);
        let clipped = !self.clips.is_empty();
        let [b, g, r, _] = color.to_le_bytes();
        for gy in 0..h as i32 {
            let dy = y + gy;
            if dy < sy0 || dy >= sy1 {
                continue;
            }
            for gx in 0..w as i32 {
                let dx = x + gx;
                if dx < sx0 || dx >= sx1 {
                    continue;
                }
                if clipped
                    && !self
                        .clips
                        .iter()
                        .all(|c| c.contains(dx as f32 + 0.5, dy as f32 + 0.5))
                {
                    continue;
                }
                let a = alpha[(gy * w as i32 + gx) as usize] as u32;
                if a == 0 {
                    continue;
                }
                let i = ((dy * cw + dx) * 4) as usize;
                let inv = 255 - a;
                self.pixels[i] = ((b as u32 * a + self.pixels[i] as u32 * inv) / 255) as u8;
                self.pixels[i + 1] = ((g as u32 * a + self.pixels[i + 1] as u32 * inv) / 255) as u8;
                self.pixels[i + 2] = ((r as u32 * a + self.pixels[i + 2] as u32 * inv) / 255) as u8;
                self.pixels[i + 3] = 255;
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
    let [b, g, r, _] = color.to_le_bytes();
    let (sx0, sy0, sx1, sy1) = fx::scissor(clips, w, h);
    let x0 = x.max(0).max(sx0);
    let y0 = y.max(0).max(sy0);
    let x1 = (x + rw as i32).min(w).min(sx1);
    let y1 = (y + rh as i32).min(h).min(sy1);
    let clipped = !clips.is_empty();
    for yy in y0..y1 {
        for xx in x0..x1 {
            if clipped
                && !clips
                    .iter()
                    .all(|c| c.contains(xx as f32 + 0.5, yy as f32 + 0.5))
            {
                continue;
            }
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
        // Vertical cull: items fully off-canvas can't write visible pixels.
        // Text gets a ±4em band — glyph ink can overshoot the em box, but
        // never by multiples of it. Effect markers never cull.
        let (top, bot) = match item {
            DisplayItem::FillRect { y, h, .. }
            | DisplayItem::RoundedRect { y, h, .. }
            | DisplayItem::RoundedRect2 { y, h, .. }
            | DisplayItem::BorderSide { y, h, .. }
            | DisplayItem::Raster { y, h, .. }
            | DisplayItem::Image { y, h, .. }
            | DisplayItem::BgImage { y, h, .. } => (*y, *y + *h),
            DisplayItem::Text { baseline, size, .. } => {
                (baseline - size * 4.0, baseline + size * 4.0)
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
            | DisplayItem::PopTransform => (f32::MIN, f32::MAX),
        };
        if bot < 0.0 || top > ch {
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
            } => {
                if let Some(img) = images.get(src) {
                    canvas.blit_tiled(
                        img, *x, *y, *w, *h, *px, *py, *pw, *ph, *mode, *sw, *sh, *fx, *ax, *fy,
                        *ay, *rx, *ry,
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
                    pen += advance + ls;
                    prev = gid;
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
}

impl Canvas {
    /// Nearest-neighbor blit of a decoded BGRA image into a rect.
    /// Blit a raw BGRA buffer of `dw`×`dh` pixels (no scaling).
    pub fn blit_pixels(&mut self, pixels: &[u8], dx: i32, dy: i32, dw: i32, dh: i32) {
        let img = Image {
            width: dw.max(0) as u32,
            height: dh.max(0) as u32,
            pixels: pixels.to_vec(),
        };
        self.blit(&img, dx, dy, dw, dh);
    }

    fn blit(&mut self, img: &Image, dx: i32, dy: i32, dw: i32, dh: i32) {
        if img.width == 0 || img.height == 0 {
            return;
        }
        let (cw, ch) = (self.width as i32, self.height as i32);
        let (sx0, sy0, sx1, sy1) = fx::scissor(&self.clips, cw, ch);
        let clipped = !self.clips.is_empty();
        for y in dy.max(0).max(sy0)..(dy + dh).min(ch).min(sy1) {
            let sy = ((y - dy) as u32 * img.height / dh.max(1) as u32).min(img.height - 1);
            for x in dx.max(0).max(sx0)..(dx + dw).min(cw).min(sx1) {
                if clipped
                    && !self
                        .clips
                        .iter()
                        .all(|c| c.contains(x as f32 + 0.5, y as f32 + 0.5))
                {
                    continue;
                }
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
    ) {
        let (nw, nh) = (img.width as f32, img.height as f32);
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
    ) {
        if img.width == 0 || img.height == 0 || dw <= 0 || dh <= 0 {
            return;
        }
        let (cw, ch) = (self.width as i32, self.height as i32);
        let x_lo = dx.max(cx0).max(0);
        let x_hi = (dx + dw).min(cx1).min(cw);
        let y_lo = dy.max(cy0).max(0);
        let y_hi = (dy + dh).min(cy1).min(ch);
        for yy in y_lo..y_hi {
            let sy = ((yy - dy) as u32 * img.height / dh.max(1) as u32).min(img.height - 1);
            for xx in x_lo..x_hi {
                let sx = ((xx - dx) as u32 * img.width / dw.max(1) as u32).min(img.width - 1);
                let si = ((sy * img.width + sx) * 4) as usize;
                let di = ((yy * cw + xx) * 4) as usize;
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
