//! Minimal SVG rasterizer: viewBox + path (M/L/H/V/C/S/Q/T/Z + relative),
//! circle/rect/polygon/polyline/line, even-odd fill, `fill`/`currentColor`.
//! Strokes are approximated as thin fills only for `stroke="currentColor"`.

use rowser_dom::{Document, NodeId, NodeKind};

#[derive(Clone, Copy, Default)]
struct Pt {
    x: f32,
    y: f32,
}

type Poly = Vec<Pt>;

struct Shapes {
    polys: Vec<Poly>,
    fill: Option<u32>,
    vbx: f32,
    vby: f32,
    vbw: f32,
    vbh: f32,
}

fn parse_d(d: &str) -> Poly {
    let b = d.as_bytes();
    let mut i = 0usize;
    let mut pts: Poly = Vec::new();
    let mut cur = Pt::default();
    let mut start = Pt::default();
    let mut cmd = b' ';
    let mut rel = false;

    let num = |i: &mut usize| -> f32 {
        while *i < b.len() && (b[*i] as char).is_whitespace() || b.get(*i) == Some(&b',') {
            *i += 1;
        }
        let st = *i;
        if *i < b.len() && (b[*i] == b'-' || b[*i] == b'+') {
            *i += 1;
        }
        while *i < b.len()
            && (b[*i].is_ascii_digit() || b[*i] == b'.' || b[*i] == b'e' || b[*i] == b'E'
                || ((b[*i] == b'-' || b[*i] == b'+') && *i > 0 && (b[*i - 1] == b'e' || b[*i - 1] == b'E')))
        {
            *i += 1;
        }
        std::str::from_utf8(&b[st..*i])
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0)
    };

    while i < b.len() {
        let c = b[i];
        if c.is_ascii_alphabetic() {
            cmd = c;
            rel = c.is_ascii_lowercase();
            i += 1;
            if cmd == b'Z' || cmd == b'z' {
                pts.push(start);
                cur = start;
            }
            continue;
        }
        let p = |i: &mut usize, rel: bool, cur: Pt| -> Pt {
            let mut x = num(i);
            let mut y = num(i);
            if rel {
                x += cur.x;
                y += cur.y;
            }
            Pt { x, y }
        };
        match cmd.to_ascii_uppercase() {
            b'M' | b'L' => {
                cur = p(&mut i, rel, cur);
                if cmd == b'M' || cmd == b'm' {
                    start = cur;
                }
                pts.push(cur);
                if cmd == b'M' || cmd == b'm' {
                    cmd = if rel { b'l' } else { b'L' };
                }
            }
            b'H' => {
                let mut x = num(&mut i);
                if rel {
                    x += cur.x;
                }
                cur = Pt { x, y: cur.y };
                pts.push(cur);
            }
            b'V' => {
                let mut y = num(&mut i);
                if rel {
                    y += cur.y;
                }
                cur = Pt { x: cur.x, y };
                pts.push(cur);
            }
            b'C' | b'S' => {
                // cubic → flatten 16 steps (S mirrors prev control point)
                let (c1, c2) = if cmd == b'S' || cmd == b's' {
                    let c1 = cur; // mirror approx: reuse last point
                    let c2 = p(&mut i, rel, cur);
                    (c1, c2)
                } else {
                    let c1 = p(&mut i, rel, cur);
                    let c2 = p(&mut i, rel, cur);
                    (c1, c2)
                };
                let e = p(&mut i, rel, cur);
                let p0 = cur;
                for k in 1..=16 {
                    let t = k as f32 / 16.0;
                    let u = 1.0 - t;
                    let x = u * u * u * p0.x + 3.0 * u * u * t * c1.x + 3.0 * u * t * t * c2.x + t * t * t * e.x;
                    let y = u * u * u * p0.y + 3.0 * u * u * t * c1.y + 3.0 * u * t * t * c2.y + t * t * t * e.y;
                    pts.push(Pt { x, y });
                }
                cur = e;
            }
            b'Q' | b'T' => {
                let c1 = if cmd == b'Q' || cmd == b'q' {
                    p(&mut i, rel, cur)
                } else {
                    cur
                };
                let e = p(&mut i, rel, cur);
                let p0 = cur;
                for k in 1..=12 {
                    let t = k as f32 / 12.0;
                    let u = 1.0 - t;
                    pts.push(Pt {
                        x: u * u * p0.x + 2.0 * u * t * c1.x + t * t * e.x,
                        y: u * u * p0.y + 2.0 * u * t * c1.y + t * t * e.y,
                    });
                }
                cur = e;
            }
            _ => {
                // unknown command (A etc.) — consume numbers as line endpoints
                let mut e = p(&mut i, rel, cur);
                if cmd == b'A' || cmd == b'a' {
                    // arc: rx ry rot large sweep x y — skip first 5 params
                    let _ = num(&mut i);
                    let _ = num(&mut i);
                    let _ = num(&mut i);
                    let _ = num(&mut i);
                    let _ = num(&mut i);
                    e = p(&mut i, rel, cur);
                }
                cur = e;
                pts.push(cur);
            }
        }
    }
    pts
}

fn walk_svg(doc: &Document, node: NodeId, out: &mut Vec<Poly>, inherited_fill: Option<u32>, color: u32) {
    for &ch in doc.children(node) {
        let NodeKind::Element { tag, .. } = doc.kind(ch) else { continue };
        let attr = |n: &str| doc.attr(ch, n);
        match tag.as_str() {
            "path" => {
                if let Some(d) = attr("d") {
                    out.push(parse_d(d));
                }
            }
            "circle" => {
                let cx: f32 = attr("cx").and_then(|v| v.parse().ok()).unwrap_or(0.0);
                let cy: f32 = attr("cy").and_then(|v| v.parse().ok()).unwrap_or(0.0);
                let r: f32 = attr("r").and_then(|v| v.parse().ok()).unwrap_or(0.0);
                let mut poly = Vec::with_capacity(33);
                for k in 0..=32 {
                    let a = k as f32 / 32.0 * std::f32::consts::TAU;
                    poly.push(Pt { x: cx + r * a.cos(), y: cy + r * a.sin() });
                }
                out.push(poly);
            }
            "rect" => {
                let x: f32 = attr("x").and_then(|v| v.parse().ok()).unwrap_or(0.0);
                let y: f32 = attr("y").and_then(|v| v.parse().ok()).unwrap_or(0.0);
                let w: f32 = attr("width").and_then(|v| v.parse().ok()).unwrap_or(0.0);
                let h: f32 = attr("height").and_then(|v| v.parse().ok()).unwrap_or(0.0);
                out.push(vec![
                    Pt { x, y },
                    Pt { x: x + w, y },
                    Pt { x: x + w, y: y + h },
                    Pt { x, y: y + h },
                    Pt { x, y },
                ]);
            }
            "polygon" | "polyline" | "line" => {
                if let Some(p) = attr("points") {
                    let nums: Vec<f32> = p
                        .split(|c: char| c.is_whitespace() || c == ',')
                        .filter_map(|t| t.parse().ok())
                        .collect();
                    let poly: Poly = nums
                        .chunks(2)
                        .filter_map(|c| c.get(0).zip(c.get(1)).map(|(&x, &y)| Pt { x, y }))
                        .collect();
                    if poly.len() > 1 {
                        out.push(poly);
                    }
                }
            }
            "g" | "svg" | "symbol" | "defs" => {
                walk_svg(doc, ch, out, inherited_fill, color);
            }
            _ => {}
        }
    }
    let _ = color;
}

/// Rasterize the <svg> element `node` to `w`×`h` BGRA pixels.
/// `fallback_color` supplies `currentColor`.
pub fn render(doc: &Document, node: NodeId, w: u32, h: u32, fallback_color: u32) -> Vec<u8> {
    let mut polys: Vec<Poly> = Vec::new();
    walk_svg(doc, node, &mut polys, None, fallback_color);
    let w = w.max(1) as usize;
    let h = h.max(1) as usize;
    let mut out = vec![0u8; w * h * 4];

    let (vbx, vby, vbw, vbh) = doc
        .attr(node, "viewBox")
        .and_then(|v| {
            let nums: Vec<f32> = v
                .split(|c: char| c.is_whitespace() || c == ',')
                .filter_map(|t| t.parse().ok())
                .collect();
            (nums.len() >= 4).then(|| (nums[0], nums[1], nums[2], nums[3]))
        })
        .unwrap_or((0.0, 0.0, w as f32, h as f32));
    let s = (w as f32 / vbw.max(0.01)).min(h as f32 / vbh.max(0.01));

    // even-odd scanline fill
    for y in 0..h {
        let sy = (y as f32 + 0.5) / s + vby;
        let mut xs: Vec<f32> = Vec::new();
        for poly in &polys {
            for seg in poly.windows(2) {
                let (a, b) = (seg[0], seg[1]);
                if (a.y <= sy) != (b.y <= sy) {
                    let t = (sy - a.y) / (b.y - a.y + f32::EPSILON);
                    xs.push(a.x + t * (b.x - a.x));
                }
            }
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        for pair in xs.chunks(2) {
            let (x0, x1) = (pair[0], *pair.get(1).unwrap_or(&pair[0]));
            let px0 = ((x0 - vbx) * s).floor().max(0.0) as usize;
            let px1 = ((x1 - vbx) * s).ceil().min(w as f32) as usize;
            for x in px0..px1 {
                let j = (y * w + x) * 4;
                let (r, g, b, a) = (
                    (fallback_color >> 16) & 0xff,
                    (fallback_color >> 8) & 0xff,
                    fallback_color & 0xff,
                    fallback_color >> 24,
                );
                out[j] = b as u8;
                out[j + 1] = g as u8;
                out[j + 2] = r as u8;
                out[j + 3] = a as u8;
            }
        }
    }
    out
}
