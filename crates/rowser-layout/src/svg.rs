//! Minimal SVG rasterizer: viewBox + path (M/L/H/V/C/S/Q/T/Z + relative),
//! circle/rect/polygon/polyline/line, even-odd fill, per-element
//! `fill`/`stroke`/`stroke-width` with `<g>` inheritance. Strokes rasterize
//! as quadrilateral segments; `currentColor` uses `fallback_color`.

use rowser_dom::{Document, NodeId, NodeKind};

#[derive(Clone, Copy, Default)]
struct Pt {
    x: f32,
    y: f32,
}

type Poly = Vec<Pt>;

/// A filled or stroked shape — coordinates are in viewBox space.
struct Shape {
    poly: Poly,
    /// ARGB fill, None = `fill="none"`
    fill: Option<u32>,
    /// ARGB stroke color + width (viewBox units)
    stroke: Option<(u32, f32)>,
    closed: bool,
}

/// Inherited paint context down the <g>/<svg> tree — fill defaults to black,
/// stroke to none, per SVG defaults.
#[derive(Clone, Copy)]
struct Paint {
    fill: Option<u32>,
    stroke: Option<(u32, f32)>,
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

/// `#rgb`, `#rrggbb`, `#rrggbbaa`, a small named set, `none`/`currentColor`.
fn svg_color(v: &str, fallback: u32) -> Option<Option<u32>> {
    let v = v.trim();
    if v.is_empty() {
        return None;
    }
    if v.eq_ignore_ascii_case("none") {
        return Some(None);
    }
    if v.eq_ignore_ascii_case("currentcolor") {
        return Some(Some(fallback));
    }
    let named = |n: &str| -> Option<u32> {
        Some(match n {
            "black" => 0xff000000,
            "white" => 0xffffffff,
            "red" => 0xffff0000,
            "green" => 0xff008000,
            "blue" => 0xff0000ff,
            "gray" | "grey" => 0xff808080,
            "yellow" => 0xffffff00,
            "orange" => 0xffffa500,
            "purple" => 0xff800080,
            "brown" => 0xffa52a2a,
            _ => return None,
        })
    };
    if let Some(h) = v.strip_prefix('#') {
        let d = u32::from_str_radix(h, 16).ok()?;
        let c = match h.len() {
            3 => {
                let r = (d >> 8) & 0xf;
                let g = (d >> 4) & 0xf;
                let b = d & 0xf;
                0xff000000 | (r * 17) << 16 | (g * 17) << 8 | b * 17
            }
            4 => {
                let a = ((d >> 12) & 0xf) * 17;
                let r = ((d >> 8) & 0xf) * 17;
                let g = ((d >> 4) & 0xf) * 17;
                let b = (d & 0xf) * 17;
                a << 24 | r << 16 | g << 8 | b
            }
            6 => 0xff000000 | d,
            8 => d,
            _ => return None,
        };
        return Some(Some(c));
    }
    // rgb()/rgba() functional notation
    let l = v.to_ascii_lowercase();
    if let Some(inner) = l.strip_prefix("rgb").and_then(|s| s.strip_prefix('(')).and_then(|s| s.strip_suffix(')')) {
        let mut it = inner.split(|c: char| c == ',' || c.is_whitespace()).filter(|t| !t.is_empty());
        let chan = |t: Option<&str>| -> Option<u32> {
            let t = t?.trim();
            if let Some(p) = t.strip_suffix('%') {
                Some((p.parse::<f32>().ok()? * 2.55) as u32)
            } else {
                t.parse::<f32>().ok().map(|v| v as u32)
            }
        };
        if let (Some(r), Some(g), Some(b)) = (chan(it.next()), chan(it.next()), chan(it.next())) {
            return Some(Some(0xff000000 | r.min(255) << 16 | g.min(255) << 8 | b.min(255)));
        }
    }
    named(&l).map(Some)
}

/// `paint: url(...)`/`inherit`/unknown → keep inherited (return None so the
/// caller keeps the parent value).
fn paint_of(v: Option<&str>, inherited: Option<u32>, fallback: u32) -> Option<u32> {
    match v.and_then(|s| svg_color(s, fallback)) {
        Some(c) => c,
        None => inherited,
    }
}

fn walk_svg(doc: &Document, node: NodeId, out: &mut Vec<Shape>, paint: Paint, color: u32) {
    for &ch in doc.children(node) {
        let NodeKind::Element { tag, .. } = doc.kind(ch) else { continue };
        let attr = |n: &str| doc.attr(ch, n);
        let fill = paint_of(attr("fill"), paint.fill, color);
        let stroke_c = paint_of(attr("stroke"), paint.stroke.map(|s| s.0), color);
        let stroke_w = attr("stroke-width")
            .and_then(|v| v.parse().ok())
            .or(paint.stroke.map(|s| s.1));
        let stroke = stroke_c.zip(stroke_w);
        let mine = Paint { fill, stroke };
        let push = |out: &mut Vec<Shape>, poly: Poly, closed: bool| {
            if poly.len() > 1 {
                out.push(Shape { poly, fill, stroke, closed });
            }
        };
        match tag.as_str() {
            "path" => {
                if let Some(d) = attr("d") {
                    // Z-terminated subpaths share one Poly — the even-odd
                    // fill treats them as one shape which is what the cburnett
                    // pieces intend.
                    push(out, parse_d(d), true);
                }
            }
            "circle" | "ellipse" => {
                let cx: f32 = attr("cx").and_then(|v| v.parse().ok()).unwrap_or(0.0);
                let cy: f32 = attr("cy").and_then(|v| v.parse().ok()).unwrap_or(0.0);
                let rx: f32 = attr("r").or_else(|| attr("rx")).and_then(|v| v.parse().ok()).unwrap_or(0.0);
                let ry: f32 = attr("r").or_else(|| attr("ry")).and_then(|v| v.parse().ok()).unwrap_or(0.0);
                let mut poly = Vec::with_capacity(33);
                for k in 0..=32 {
                    let a = k as f32 / 32.0 * std::f32::consts::TAU;
                    poly.push(Pt { x: cx + rx * a.cos(), y: cy + ry * a.sin() });
                }
                push(out, poly, true);
            }
            "rect" => {
                let x: f32 = attr("x").and_then(|v| v.parse().ok()).unwrap_or(0.0);
                let y: f32 = attr("y").and_then(|v| v.parse().ok()).unwrap_or(0.0);
                let w: f32 = attr("width").and_then(|v| v.parse().ok()).unwrap_or(0.0);
                let h: f32 = attr("height").and_then(|v| v.parse().ok()).unwrap_or(0.0);
                push(out, vec![
                    Pt { x, y },
                    Pt { x: x + w, y },
                    Pt { x: x + w, y: y + h },
                    Pt { x, y: y + h },
                    Pt { x, y },
                ], true);
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
                    push(out, poly, tag.as_str() == "polygon");
                }
            }
            "g" | "svg" | "symbol" | "defs" | "a" => {
                walk_svg(doc, ch, out, mine, color);
            }
            _ => {}
        }
    }
}

/// Stroke `poly` into filled quadrilateral segments of width `w`, appended to
/// `out`. Square caps/joins — fine at chess-piece scale.
fn stroke_quads(poly: &[Pt], w: f32, out: &mut Vec<Poly>) {
    let hw = w / 2.0;
    for seg in poly.windows(2) {
        let (a, b) = (seg[0], seg[1]);
        let dx = b.x - a.x;
        let dy = b.y - a.y;
        let len = (dx * dx + dy * dy).sqrt();
        if len < f32::EPSILON {
            continue;
        }
        let (nx, ny) = (-dy / len * hw, dx / len * hw);
        out.push(vec![
            Pt { x: a.x + nx, y: a.y + ny },
            Pt { x: b.x + nx, y: b.y + ny },
            Pt { x: b.x - nx, y: b.y - ny },
            Pt { x: a.x - nx, y: a.y - ny },
            Pt { x: a.x + nx, y: a.y + ny },
        ]);
    }
}

/// Fill `polys` (even-odd) into `out` at scale `s` offset by (vbx, vby).
fn fill_polys(polys: &[Poly], color: u32, out: &mut [u8], w: usize, h: usize, s: f32, vbx: f32, vby: f32) {
    if color >> 24 == 0 {
        return;
    }
    for y in 0..h {
        let sy = (y as f32 + 0.5) / s + vby;
        let mut xs: Vec<f32> = Vec::new();
        for poly in polys {
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
            for x in px0..px1.min(w) {
                let j = (y * w + x) * 4;
                let (r, g, b, a) = (
                    (color >> 16) & 0xff,
                    (color >> 8) & 0xff,
                    color & 0xff,
                    color >> 24,
                );
                // source-over composite so strokes layer over fills
                let da = out[j + 3] as u32;
                let sa = a;
                let oa = (sa + da * (255 - sa) / 255).min(255);
                if oa == 0 {
                    continue;
                }
                for (k, sc) in [b, g, r].iter().enumerate() {
                    let dc = out[j + k] as u32;
                    out[j + k] = ((sc * sa + dc * da * (255 - sa) / 255) / oa.max(1)) as u8;
                }
                out[j + 3] = oa as u8;
            }
        }
    }
}

/// Rasterize the <svg> element `node` to `w`×`h` BGRA pixels.
/// `fallback_color` supplies `currentColor`.
pub fn render(doc: &Document, node: NodeId, w: u32, h: u32, fallback_color: u32) -> Vec<u8> {
    let mut shapes: Vec<Shape> = Vec::new();
    let root_paint = Paint {
        fill: Some(0xff000000), // SVG default fill is black
        stroke: None,
        // root fill/stroke attrs still override below via walk on children
    };
    // the <svg> element's own paint attrs seed the context
    let root_paint = Paint {
        fill: paint_of(doc.attr(node, "fill"), root_paint.fill, fallback_color),
        stroke: {
            let c = paint_of(doc.attr(node, "stroke"), None, fallback_color);
            let w0 = doc.attr(node, "stroke-width").and_then(|v| v.parse().ok());
            c.zip(w0)
        },
    };
    walk_svg(doc, node, &mut shapes, root_paint, fallback_color);
    let w = w.max(1) as usize;
    let h = h.max(1) as usize;
    let mut out = vec![0u8; w * h * 4];

    let (vbx, vby, vbw, vbh) = doc
        .attr(node, "viewBox")
        .or_else(|| doc.attr(node, "viewbox"))
        .and_then(|v| {
            let nums: Vec<f32> = v
                .split(|c: char| c.is_whitespace() || c == ',')
                .filter_map(|t| t.parse().ok())
                .collect();
            (nums.len() >= 4).then(|| (nums[0], nums[1], nums[2], nums[3]))
        })
        .unwrap_or((0.0, 0.0, w as f32, h as f32));
    let s = (w as f32 / vbw.max(0.01)).min(h as f32 / vbh.max(0.01));

    // fills first, then strokes — per shape, in document order
    for sh in &shapes {
        if let Some(f) = sh.fill {
            fill_polys(std::slice::from_ref(&sh.poly), f, &mut out, w, h, s, vbx, vby);
        }
        if let Some((sc, sw)) = sh.stroke {
            let mut quads = Vec::new();
            stroke_quads(&sh.poly, sw, &mut quads);
            fill_polys(&quads, sc, &mut out, w, h, s, vbx, vby);
        }
    }
    out
}
