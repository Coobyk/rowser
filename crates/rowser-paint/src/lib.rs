//! rowser-paint: display list → BGRA pixel buffer (premultiplied-friendly;
//! we emit alpha=255 everywhere for now).
//!
//! Text is drawn by blending cached glyph coverage bitmaps with the run
//! color. Underline/strikethrough are plain rects under/over the baseline.

use rowser_image::Image;
use rowser_layout::{Color, DisplayItem};
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
    for item in items {
        match item {
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
}



impl Canvas {
    /// Nearest-neighbor blit of a decoded BGRA image into a rect.
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
}
