//! SVG rasterizer: viewBox + preserveAspectRatio, paths (M/L/H/V/C/S/Q/T/A/Z
//! + relative, multi-subpath), basic shapes, transforms, `style=` props,
//! even-odd + nonzero fills, paint inheritance through <g>, stroke
//! joins/caps, opacity, minimal <defs>/<use>. `currentColor` uses
//! `fallback_color`. Strokes flatten to polygon patches; output is BGRA,
//! internally supersampled and box-downsampled.

use rowser_dom::{Document, NodeId, NodeKind};
use std::collections::HashMap;

#[derive(Clone, Copy, Default)]
struct Pt {
    x: f32,
    y: f32,
}

type Poly = Vec<Pt>;

/// A flattened subpath; `closed` = ended with Z (stroke joins wrap, no caps).
struct Sub {
    pts: Poly,
    closed: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Rule {
    NonZero,
    EvenOdd,
}

#[derive(Clone, Copy, PartialEq)]
enum Cap {
    Butt,
    Round,
    Square,
}

#[derive(Clone, Copy, PartialEq)]
enum Join {
    Miter,
    Round,
    Bevel,
}

/// 2×3 affine: x' = a·x + c·y + e, y' = b·x + d·y + f.
#[derive(Clone, Copy)]
struct Mat {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

impl Mat {
    const ID: Mat = Mat {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    /// self ∘ o (o applied to the point first).
    fn mul(self, o: Mat) -> Mat {
        Mat {
            a: self.a * o.a + self.c * o.b,
            b: self.b * o.a + self.d * o.b,
            c: self.a * o.c + self.c * o.d,
            d: self.b * o.c + self.d * o.d,
            e: self.a * o.e + self.c * o.f + self.e,
            f: self.b * o.e + self.d * o.f + self.f,
        }
    }
    fn tr(x: f32, y: f32) -> Mat {
        Mat {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: x,
            f: y,
        }
    }
    fn sc(x: f32, y: f32) -> Mat {
        Mat {
            a: x,
            b: 0.0,
            c: 0.0,
            d: y,
            e: 0.0,
            f: 0.0,
        }
    }
    fn rot(deg: f32) -> Mat {
        let (s, c) = deg.to_radians().sin_cos();
        Mat {
            a: c,
            b: s,
            c: -s,
            d: c,
            e: 0.0,
            f: 0.0,
        }
    }
    fn map(self, p: Pt) -> Pt {
        Pt {
            x: self.a * p.x + self.c * p.y + self.e,
            y: self.b * p.x + self.d * p.y + self.f,
        }
    }
    /// √|det| — rough local→parent length scale, used for stroke width/tol.
    fn wscale(self) -> f32 {
        (self.a * self.d - self.b * self.c).abs().sqrt()
    }
}

/// A paint value — flat color or a paint server (gradient) by table index.
#[derive(Clone, Copy)]
enum Fill {
    Solid(u32),
    Srv(u32),
}

/// A parsed gradient. Coords stay as-authored (gradientUnits decides the
/// space); stops are (offset, ARGB) sorted by offset.
struct Grad {
    lin: bool,
    /// linear: x1 y1 x2 y2 — radial: cx cy r fx fy
    p: [f32; 6],
    /// gradientUnits=objectBoundingBox → coords are normalized 0..1 over the
    /// referencing shape's bbox; otherwise userSpaceOnUse
    bbox: bool,
    gtx: Mat, // gradientTransform
    spread: u8, // 0 pad, 1 reflect, 2 repeat
    stops: Vec<(f32, u32)>,
}

impl Default for Grad {
    fn default() -> Self {
        Grad {
            lin: true,
            p: [0.0; 6],
            bbox: true,
            gtx: Mat::ID,
            spread: 0,
            stops: Vec::new(),
        }
    }
}

/// Per-shape resolved fill: userSpaceOnUse endpoints are pre-mapped to
/// viewBox space; B* variants stay normalized for the shape bbox.
#[derive(Clone, Copy)]
enum SFill {
    Solid(u32),
    Lin {
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        g: u32,
        a: f32,
    },
    Rad {
        cx: f32,
        cy: f32,
        r: f32,
        g: u32,
        a: f32,
    },
    BLin {
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        g: u32,
        a: f32,
    },
    BRad {
        cx: f32,
        cy: f32,
        r: f32,
        g: u32,
        a: f32,
    },
}

/// A flattened shape in viewBox space, paint already resolved.
struct Shape {
    subs: Vec<Sub>,
    /// ARGB fill (opacity folded in), None = `fill="none"`
    fill: Option<SFill>,
    /// stroke paint + width in viewBox units
    stroke: Option<(SFill, f32)>,
    rule: Rule,
    cap: Cap,
    join: Join,
    miter: f32,
    /// optional clip rect in viewBox space (nested <svg> viewports)
    clip: Option<[f32; 4]>,
    /// shape bounds in viewBox space — objectBoundingBox gradient basis
    bbox: [f32; 4],
}

/// Inherited paint context down the <g>/<svg> tree — fill defaults to black,
/// stroke to none, fill-rule nonzero, per SVG defaults. `falpha`/`salpha` are
/// running products of `opacity`/`fill-opacity`/`stroke-opacity` (group
/// opacity approximated by multiplication, no offscreen pass).
#[derive(Clone, Copy)]
struct Paint {
    fill: Option<Fill>,
    falpha: f32,
    stroke: Option<(Fill, f32)>,
    salpha: f32,
    rule: Rule,
    cap: Cap,
    join: Join,
    miter: f32,
    cc: u32, // `color` property — supplies currentColor
    xf: Mat,
    /// optional clip rect in viewBox space (nested <svg> viewports)
    clip: Option<[f32; 4]>,
}

/// viewBox→output-pixel affine.
#[derive(Clone, Copy)]
struct VMap {
    sx: f32,
    sy: f32,
    ox: f32,
    oy: f32,
}

// ---------- low-level parsing ----------

/// Leading float of a length/string ("12px", ".5em", "-3") → f32.
fn fnum(v: &str) -> Option<f32> {
    let v = v.trim_start();
    let mut end = 0;
    for (k, c) in v.char_indices() {
        if c.is_ascii_digit() || matches!(c, '.' | '+' | '-' | 'e' | 'E') {
            end = k + c.len_utf8();
        } else {
            break;
        }
    }
    v[..end].parse().ok()
}

fn nums_of(s: &str) -> Vec<f32> {
    s.split(|c: char| c.is_whitespace() || c == ',' || c == '(' || c == ')')
        .filter_map(|t| t.parse().ok())
        .collect()
}

/// `fill:a; stroke:b` style string → prop list.
fn parse_style(s: &str) -> Vec<(String, String)> {
    s.split(';')
        .filter_map(|d| {
            let (k, v) = d.split_once(':')?;
            Some((k.trim().to_ascii_lowercase(), v.trim().to_string()))
        })
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

/// Element props: presentation attributes overridden by `style=` props.
fn elem_props(doc: &Document, ch: NodeId) -> HashMap<String, String> {
    let mut m = HashMap::new();
    if let NodeKind::Element { attrs, .. } = doc.kind(ch) {
        for (k, v) in attrs {
            m.insert(k.clone(), v.clone());
        }
    }
    if let Some(s) = doc.attr(ch, "style") {
        for (k, v) in parse_style(s) {
            m.insert(k, v);
        }
    }
    m
}

/// `transform="translate() scale() rotate(+cx cy) skewX skewY matrix()"`.
/// List composes left→right: p' = T₁·T₂·…·Tₙ·p.
fn parse_transform(s: &str) -> Mat {
    let mut m = Mat::ID;
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && !b[i].is_ascii_alphabetic() {
            i += 1;
        }
        let st = i;
        while i < b.len() && b[i].is_ascii_alphabetic() {
            i += 1;
        }
        if st == i {
            break;
        }
        let name = &s[st..i];
        while i < b.len() && b[i] != b'(' {
            i += 1;
        }
        let close = s[i..].find(')').map(|k| i + k).unwrap_or(b.len());
        let n = nums_of(&s[i..close]);
        let g = |k: usize, d: f32| n.get(k).copied().unwrap_or(d);
        let t = match name {
            "matrix" if n.len() >= 6 => Mat {
                a: g(0, 1.0),
                b: g(1, 0.0),
                c: g(2, 0.0),
                d: g(3, 1.0),
                e: g(4, 0.0),
                f: g(5, 0.0),
            },
            "translate" => Mat::tr(g(0, 0.0), g(1, 0.0)),
            "scale" => Mat::sc(g(0, 1.0), g(1, g(0, 1.0))),
            "rotate" if n.len() >= 3 => Mat::tr(g(1, 0.0), g(2, 0.0))
                .mul(Mat::rot(g(0, 0.0)))
                .mul(Mat::tr(-g(1, 0.0), -g(2, 0.0))),
            "rotate" => Mat::rot(g(0, 0.0)),
            "skewX" => Mat {
                a: 1.0,
                b: 0.0,
                c: g(0, 0.0).to_radians().tan(),
                d: 1.0,
                e: 0.0,
                f: 0.0,
            },
            "skewY" => Mat {
                a: 1.0,
                b: g(0, 0.0).to_radians().tan(),
                c: 0.0,
                d: 1.0,
                e: 0.0,
                f: 0.0,
            },
            _ => Mat::ID,
        };
        m = m.mul(t);
        i = close.saturating_add(1).min(b.len());
    }
    m
}

// ---------- path flattening ----------

fn cubic(pts: &mut Poly, p0: Pt, c1: Pt, c2: Pt, e: Pt) {
    for k in 1..=16 {
        let t = k as f32 / 16.0;
        let u = 1.0 - t;
        pts.push(Pt {
            x: u * u * u * p0.x + 3.0 * u * u * t * c1.x + 3.0 * u * t * t * c2.x + t * t * t * e.x,
            y: u * u * u * p0.y + 3.0 * u * u * t * c1.y + 3.0 * u * t * t * c2.y + t * t * t * e.y,
        });
    }
}

fn quad(pts: &mut Poly, p0: Pt, c1: Pt, e: Pt) {
    for k in 1..=12 {
        let t = k as f32 / 12.0;
        let u = 1.0 - t;
        pts.push(Pt {
            x: u * u * p0.x + 2.0 * u * t * c1.x + t * t * e.x,
            y: u * u * p0.y + 2.0 * u * t * c1.y + t * t * e.y,
        });
    }
}

/// Elliptical arc, endpoint→center parameterization (SVG spec F.6.5),
/// flattened with chord error ≤ `tol` local units.
#[allow(clippy::too_many_arguments)]
fn arc(
    out: &mut Poly,
    p0: Pt,
    rx0: f32,
    ry0: f32,
    rot: f32,
    large: bool,
    sweep: bool,
    p1: Pt,
    tol: f32,
) {
    if (p0.x - p1.x).abs() + (p0.y - p1.y).abs() < 1e-9 {
        return; // spec: coincident endpoints skip the arc
    }
    let mut rx = rx0.abs();
    let mut ry = ry0.abs();
    if rx < 1e-9 || ry < 1e-9 {
        out.push(p1); // zero radius → straight line
        return;
    }
    let (sp, cp) = rot.to_radians().sin_cos();
    let (hx, hy) = ((p0.x - p1.x) * 0.5, (p0.y - p1.y) * 0.5);
    let x1p = cp * hx + sp * hy;
    let y1p = -sp * hx + cp * hy;
    // out-of-range radii get scaled up until the arc can reach
    let lam = x1p * x1p / (rx * rx) + y1p * y1p / (ry * ry);
    if lam > 1.0 {
        let s = lam.sqrt();
        rx *= s;
        ry *= s;
    }
    let (rx2, ry2, x2, y2) = (rx * rx, ry * ry, x1p * x1p, y1p * y1p);
    let num = (rx2 * ry2 - rx2 * y2 - ry2 * x2).max(0.0);
    let den = (rx2 * y2 + ry2 * x2).max(1e-12);
    let co = (num / den).sqrt() * if large == sweep { -1.0 } else { 1.0 };
    let cxp = co * rx * y1p / ry;
    let cyp = -co * ry * x1p / rx;
    let cx = cp * cxp - sp * cyp + (p0.x + p1.x) * 0.5;
    let cy = sp * cxp + cp * cyp + (p0.y + p1.y) * 0.5;
    let ang = |ux: f32, uy: f32, vx: f32, vy: f32| -> f32 {
        let d = ((ux * vx + uy * vy) / ((ux * ux + uy * uy) * (vx * vx + vy * vy)).sqrt())
            .clamp(-1.0, 1.0);
        let mut a = d.acos();
        if ux * vy - uy * vx < 0.0 {
            a = -a;
        }
        a
    };
    let (ux, uy) = ((x1p - cxp) / rx, (y1p - cyp) / ry);
    let (vx, vy) = ((-x1p - cxp) / rx, (-y1p - cyp) / ry);
    let th0 = ang(1.0, 0.0, ux, uy);
    let mut dth = ang(ux, uy, vx, vy);
    if sweep && dth < 0.0 {
        dth += std::f32::consts::TAU;
    } else if !sweep && dth > 0.0 {
        dth -= std::f32::consts::TAU;
    }
    let r = rx.max(ry);
    let step = if tol < r {
        2.0 * (1.0 - tol / r).clamp(-1.0, 1.0).acos()
    } else {
        std::f32::consts::FRAC_PI_2
    };
    let n = ((dth.abs() / step).ceil() as usize).clamp(1, 512);
    for k in 1..=n {
        let t = th0 + dth * (k as f32 / n as f32);
        let (st, ct) = t.sin_cos();
        out.push(Pt {
            x: cx + rx * cp * ct - ry * sp * st,
            y: cy + rx * sp * ct + ry * cp * st,
        });
    }
}

/// `d` → flattened subpaths in local coordinates. A second M/m starts a new
/// subpath; Z marks closed.
fn parse_d(d: &str, tol: f32) -> Vec<Sub> {
    let b = d.as_bytes();
    let mut i = 0usize;
    let mut subs: Vec<Sub> = Vec::new();
    let mut pts: Poly = Vec::new();
    let mut cur = Pt::default();
    let mut start = Pt::default();
    let mut cmd = b' ';
    let mut rel = false;
    let mut last_c: Option<Pt> = None; // prev cubic ctrl2 (S reflection)
    let mut last_q: Option<Pt> = None; // prev quad ctrl (T reflection)

    let num = |i: &mut usize| -> f32 {
        while *i < b.len() && (b[*i].is_ascii_whitespace() || b[*i] == b',') {
            *i += 1;
        }
        let st = *i;
        if *i < b.len() && (b[*i] == b'-' || b[*i] == b'+') {
            *i += 1;
        }
        // mantissa: digits with at most one '.'; a second dot ends the number
        // ("0 .89.29" is two numbers per the SVG grammar)
        let mut seen_dot = false;
        while *i < b.len() {
            let c = b[*i];
            if c.is_ascii_digit() {
                *i += 1;
            } else if c == b'.' && !seen_dot {
                seen_dot = true;
                *i += 1;
            } else {
                break;
            }
        }
        // optional exponent; back out if it has no digits ("1e" is just 1)
        if *i < b.len() && (b[*i] == b'e' || b[*i] == b'E') {
            let save = *i;
            *i += 1;
            if *i < b.len() && (b[*i] == b'-' || b[*i] == b'+') {
                *i += 1;
            }
            if *i < b.len() && b[*i].is_ascii_digit() {
                while *i < b.len() && b[*i].is_ascii_digit() {
                    *i += 1;
                }
            } else {
                *i = save;
            }
        }
        std::str::from_utf8(&b[st..*i])
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0)
    };
    // large-arc/sweep flags are single chars and may touch neighbours ("0 01")
    let flag = |i: &mut usize| -> bool {
        while *i < b.len() && (b[*i].is_ascii_whitespace() || b[*i] == b',') {
            *i += 1;
        }
        let f = b.get(*i) == Some(&b'1');
        if matches!(b.get(*i), Some(c) if *c == b'0' || *c == b'1') {
            *i += 1;
        }
        f
    };

    while i < b.len() {
        let c = b[i];
        if c.is_ascii_alphabetic() {
            cmd = c;
            rel = c.is_ascii_lowercase();
            i += 1;
            if cmd == b'Z' || cmd == b'z' {
                subs.push(Sub {
                    pts: std::mem::take(&mut pts),
                    closed: true,
                });
                cur = start;
                last_c = None;
                last_q = None;
            }
            continue;
        }
        let i0 = i;
        let up = cmd.to_ascii_uppercase();
        let p = |i: &mut usize, rel: bool, cur: Pt| -> Pt {
            let mut x = num(i);
            let mut y = num(i);
            if rel {
                x += cur.x;
                y += cur.y;
            }
            Pt { x, y }
        };
        match up {
            b'M' => {
                if !pts.is_empty() {
                    subs.push(Sub {
                        pts: std::mem::take(&mut pts),
                        closed: false,
                    });
                }
                cur = p(&mut i, rel, cur);
                start = cur;
                pts.push(cur);
                cmd = if rel { b'l' } else { b'L' };
            }
            b'L' => {
                cur = p(&mut i, rel, cur);
                pts.push(cur);
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
            b'C' => {
                let c1 = p(&mut i, rel, cur);
                let c2 = p(&mut i, rel, cur);
                let e = p(&mut i, rel, cur);
                cubic(&mut pts, cur, c1, c2, e);
                cur = e;
                last_c = Some(c2);
            }
            b'S' => {
                let c1 = last_c
                    .map(|l| Pt {
                        x: 2.0 * cur.x - l.x,
                        y: 2.0 * cur.y - l.y,
                    })
                    .unwrap_or(cur);
                let c2 = p(&mut i, rel, cur);
                let e = p(&mut i, rel, cur);
                cubic(&mut pts, cur, c1, c2, e);
                cur = e;
                last_c = Some(c2);
            }
            b'Q' => {
                let c1 = p(&mut i, rel, cur);
                let e = p(&mut i, rel, cur);
                quad(&mut pts, cur, c1, e);
                cur = e;
                last_q = Some(c1);
            }
            b'T' => {
                let c1 = last_q
                    .map(|l| Pt {
                        x: 2.0 * cur.x - l.x,
                        y: 2.0 * cur.y - l.y,
                    })
                    .unwrap_or(cur);
                let e = p(&mut i, rel, cur);
                quad(&mut pts, cur, c1, e);
                cur = e;
                last_q = Some(c1);
            }
            b'A' => {
                let rx = num(&mut i);
                let ry = num(&mut i);
                let rot = num(&mut i);
                let large = flag(&mut i);
                let sweep = flag(&mut i);
                let e = p(&mut i, rel, cur);
                arc(&mut pts, cur, rx, ry, rot, large, sweep, e, tol);
                cur = e;
            }
            _ => {
                let _ = num(&mut i); // unknown command — drain one number
            }
        }
        if up != b'C' && up != b'S' {
            last_c = None;
        }
        if up != b'Q' && up != b'T' {
            last_q = None;
        }
        if i == i0 {
            i += 1; // malformed input must still make progress
        }
    }
    if !pts.is_empty() {
        subs.push(Sub { pts, closed: false });
    }
    subs
}

// ---------- paint ----------

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
            "lime" => 0xff00ff00,
            "blue" => 0xff0000ff,
            "gray" | "grey" => 0xff808080,
            "silver" => 0xffc0c0c0,
            "yellow" => 0xffffff00,
            "orange" => 0xffffa500,
            "purple" => 0xff800080,
            "brown" => 0xffa52a2a,
            "pink" => 0xffffc0cb,
            "cyan" | "aqua" => 0xff00ffff,
            "magenta" | "fuchsia" => 0xffff00ff,
            "navy" => 0xff000080,
            "teal" => 0xff008080,
            "olive" => 0xff808000,
            "maroon" => 0xff800000,
            "gold" => 0xffffd700,
            "transparent" => 0x00000000,
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
    if let Some(inner) = l
        .strip_prefix("rgb")
        .and_then(|s| s.strip_prefix('('))
        .and_then(|s| s.strip_suffix(')'))
    {
        let mut it = inner
            .split(|c: char| c == ',' || c == '/' || c.is_whitespace())
            .filter(|t| !t.is_empty());
        let chan = |t: Option<&str>| -> Option<u32> {
            let t = t?.trim();
            if let Some(p) = t.strip_suffix('%') {
                Some((p.parse::<f32>().ok()? * 2.55) as u32)
            } else {
                t.parse::<f32>().ok().map(|v| v as u32)
            }
        };
        if let (Some(r), Some(g), Some(b)) = (chan(it.next()), chan(it.next()), chan(it.next())) {
            let a = it
                .next()
                .and_then(|t| {
                    let t = t.trim();
                    if let Some(p) = t.strip_suffix('%') {
                        p.parse::<f32>().ok().map(|v| v * 2.55)
                    } else {
                        t.parse::<f32>()
                            .ok()
                            .map(|v| if v <= 1.0 { v * 255.0 } else { v })
                    }
                })
                .unwrap_or(255.0);
            return Some(Some(
                (a as u32).min(255) << 24 | r.min(255) << 16 | g.min(255) << 8 | b.min(255),
            ));
        }
    }
    named(&l).map(Some)
}

/// `paint:` — url(#id) resolves to a paint server (with an optional fallback
/// color after it); colors fill flat; `none`/unknown keep the inherited paint.
fn paint_of(
    v: Option<&str>,
    inherited: Option<Fill>,
    fallback: u32,
    gmap: &HashMap<String, usize>,
) -> Option<Fill> {
    let Some(v) = v else { return inherited };
    let t = v.trim();
    if let Some(l) = t.strip_prefix("url").and_then(|t| t.find('(')) {
        let (l, r) = (l + "url".len(), t.rfind(')').unwrap_or(t.len()));
        let id = t[l + 1..r]
            .trim()
            .trim_matches(|c| c == '\'' || c == '"')
            .trim_start_matches('#');
        if let Some(&i) = gmap.get(id) {
            return Some(Fill::Srv(i as u32));
        }
        // unresolved server → the fallback color after `)`, else inherit
        return match svg_color(t[r + 1..].trim(), fallback) {
            Some(c) => c.map(Fill::Solid),
            None => inherited,
        };
    }
    match svg_color(t, fallback) {
        Some(Some(c)) => Some(Fill::Solid(c)),
        Some(None) => None,
        None => inherited,
    }
}

fn with_alpha(c: u32, k: f32) -> u32 {
    let a = (((c >> 24) as f32) * k).round().clamp(0.0, 255.0) as u32;
    (a << 24) | (c & 0x00ff_ffff)
}

/// Combine element props (attrs + style) onto the inherited context.
fn elem_paint(
    props: &HashMap<String, String>,
    parent: Paint,
    gmap: &HashMap<String, usize>,
) -> Paint {
    let at = |n: &str| {
        props
            .get(n)
            .or_else(|| props.get(&n.to_ascii_lowercase()))
            .map(String::as_str)
    };
    let op = at("opacity").and_then(fnum).unwrap_or(1.0);
    let cc = at("color")
        .and_then(|v| svg_color(v, parent.cc))
        .and_then(|c| c)
        .unwrap_or(parent.cc);
    let stroke = {
        let c = paint_of(at("stroke"), parent.stroke.map(|s| s.0), cc, gmap);
        let w = at("stroke-width")
            .and_then(fnum)
            .or(parent.stroke.map(|s| s.1));
        // spec default stroke-width is 1 — a bare stroke= still paints
        c.map(|c| (c, w.unwrap_or(1.0)))
    };
    Paint {
        fill: paint_of(at("fill"), parent.fill, cc, gmap),
        falpha: parent.falpha * op * at("fill-opacity").and_then(fnum).unwrap_or(1.0),
        stroke,
        salpha: parent.salpha * op * at("stroke-opacity").and_then(fnum).unwrap_or(1.0),
        rule: match at("fill-rule").map(|v| v.trim()) {
            Some(v) if v.eq_ignore_ascii_case("evenodd") => Rule::EvenOdd,
            Some(v) if v.eq_ignore_ascii_case("nonzero") => Rule::NonZero,
            _ => parent.rule,
        },
        cap: match at("stroke-linecap").map(|v| v.trim()) {
            Some("round") => Cap::Round,
            Some("square") => Cap::Square,
            Some("butt") => Cap::Butt,
            _ => parent.cap,
        },
        join: match at("stroke-linejoin").map(|v| v.trim()) {
            Some("round") => Join::Round,
            Some("bevel") => Join::Bevel,
            Some("miter") | Some("miter-clip") => Join::Miter,
            _ => parent.join,
        },
        miter: at("stroke-miterlimit")
            .and_then(fnum)
            .unwrap_or(parent.miter),
        cc,
        xf: parent
            .xf
            .mul(parse_transform(at("transform").unwrap_or(""))),
        clip: parent.clip,
    }
}

// ---------- tree walk ----------

struct Ctx<'a> {
    doc: &'a Document,
    ids: &'a HashMap<String, NodeId>,
    grads: &'a [Grad],
    gmap: &'a HashMap<String, usize>,
    tol0: f32, // flatten tolerance in viewBox units
}

fn collect_ids(doc: &Document, n: NodeId, ids: &mut HashMap<String, NodeId>) {
    if let NodeKind::Element { .. } = doc.kind(n) {
        if let Some(id) = doc.attr(n, "id") {
            ids.insert(id.to_string(), n);
        }
        for &c in doc.children(n) {
            collect_ids(doc, c, ids);
        }
    }
}

/// `offset` accepts a fraction or percent.
fn grad_offset(v: &str) -> Option<f32> {
    let v = v.trim();
    if let Some(p) = v.strip_suffix('%') {
        p.parse::<f32>().ok().map(|n| n / 100.0)
    } else {
        v.parse::<f32>().ok()
    }
}

/// Parse one `<stop>`: offset + color/opacity from attrs or `style`.
fn grad_stop(doc: &Document, n: NodeId) -> Option<(f32, u32)> {
    let props = elem_props(doc, n);
    let at = |k: &str| props.get(k).map(String::as_str);
    let off = at("offset").and_then(grad_offset).unwrap_or(0.0);
    let col = at("stop-color")
        .and_then(|c| svg_color(c, 0xff000000))
        .unwrap_or(Some(0xff000000))
        .unwrap_or(0xff000000);
    let al = at("stop-opacity").and_then(fnum).unwrap_or(1.0);
    Some((off.clamp(0.0, 1.0), with_alpha(col, al)))
}

/// Parse a gradient element, following `href`/`xlink:href` chains so a child
/// gradient inherits missing attrs and stops from its template (per spec,
/// local attrs win; stops inherit only when the element has none).
fn parse_grad(doc: &Document, n: NodeId, ids: &HashMap<String, NodeId>, depth: u8) -> Grad {
    let NodeKind::Element { tag, .. } = doc.kind(n) else {
        return Grad::default();
    };
    let lin = tag == "lineargradient";
    let props = elem_props(doc, n);
    let at = |k: &str| props.get(k).map(String::as_str);

    // template lookup (defs → defs chains are common in sprites); spec
    // defaults apply only when there's no template to inherit from
    let href = at("xlink:href").or_else(|| at("href"));
    let tmpl = href
        .and_then(|h| h.trim().strip_prefix('#'))
        .and_then(|id| ids.get(id).copied())
        .filter(|&t| t != n && depth < 8)
        .map(|t| parse_grad(doc, t, ids, depth + 1));
    let mut base = tmpl.unwrap_or_else(|| {
        if lin {
            Grad {
                p: [0.0, 0.0, 1.0, 0.0, 0.0, 0.0],
                ..Grad::default()
            }
        } else {
            Grad {
                p: [0.5, 0.5, 0.5, 0.5, 0.5, 0.0],
                ..Grad::default()
            }
        }
    });
    base.lin = lin;

    // percent coords are bbox fractions (userSpace %, rare, approximates too)
    let num = |k: &str| {
        at(k).and_then(|v| {
            if let Some(p) = v.trim().strip_suffix('%') {
                p.parse::<f32>().ok().map(|n| n / 100.0)
            } else {
                fnum(v)
            }
        })
    };
    if lin {
        base.p[0] = num("x1").unwrap_or(base.p[0]);
        base.p[1] = num("y1").unwrap_or(base.p[1]);
        base.p[2] = num("x2").unwrap_or(base.p[2]);
        base.p[3] = num("y2").unwrap_or(base.p[3]);
    } else {
        base.p[0] = num("cx").unwrap_or(base.p[0]);
        base.p[1] = num("cy").unwrap_or(base.p[1]);
        base.p[2] = num("r").unwrap_or(base.p[2]);
        // fx/fy default to the resolved cx/cy
        base.p[3] = num("fx").unwrap_or(base.p[0]);
        base.p[4] = num("fy").unwrap_or(base.p[1]);
    }
    if let Some(u) = at("gradientunits") {
        base.bbox = !u.trim().eq_ignore_ascii_case("userspaceonuse");
    }
    if let Some(t) = at("gradienttransform") {
        base.gtx = parse_transform(t);
    }
    if let Some(s) = at("spreadmethod") {
        base.spread = match s.trim() {
            "reflect" => 1,
            "repeat" => 2,
            _ => 0,
        };
    }
    // stops: local `<stop>` children win, else inherit the template's
    let mut stops: Vec<(f32, u32)> = Vec::new();
    for &c in doc.children(n) {
        if let NodeKind::Element { tag, .. } = doc.kind(c) {
            if tag == "stop" {
                if let Some(s) = grad_stop(doc, c) {
                    stops.push(s);
                }
            }
        }
    }
    if !stops.is_empty() {
        // offsets are non-decreasing per spec
        let mut last = 0.0f32;
        for s in &mut stops {
            s.0 = s.0.max(last);
            last = s.0;
        }
        base.stops = stops;
    }
    base
}

/// Index every `linearGradient`/`radialGradient` in the tree; returns the
/// paint-server table plus an id → table-index map for `url(#id)` refs.
fn collect_grads(
    doc: &Document,
    root: NodeId,
    ids: &HashMap<String, NodeId>,
) -> (Vec<Grad>, HashMap<String, usize>) {
    let mut gnodes = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if let NodeKind::Element { tag, .. } = doc.kind(n) {
            if tag == "lineargradient" || tag == "radialgradient" {
                gnodes.push(n);
            }
        }
        stack.extend_from_slice(doc.children(n));
    }
    let mut grads = Vec::new();
    let mut gmap = HashMap::new();
    for n in gnodes {
        if let Some(id) = doc.attr(n, "id") {
            gmap.insert(id.to_string(), grads.len());
        }
        grads.push(parse_grad(doc, n, ids, 0));
    }
    (grads, gmap)
}

/// Transform `subs` into viewBox space and record the shape.
/// Resolve an inherited paint to per-shape form: userSpaceOnUse endpoints map
/// to viewBox space now (×gradientTransform); objectBoundingBox stays
/// normalized for the shape's bbox at sample time.
fn sfill_of(f: Fill, a: f32, xf: Mat, gs: &[Grad]) -> Option<SFill> {
    match f {
        Fill::Solid(c) => Some(SFill::Solid(with_alpha(c, a))),
        Fill::Srv(i) => {
            let g = &gs[i as usize];
            let m = xf.mul(g.gtx);
            Some(if g.lin {
                if g.bbox {
                    SFill::BLin {
                        x1: g.p[0],
                        y1: g.p[1],
                        x2: g.p[2],
                        y2: g.p[3],
                        g: i,
                        a,
                    }
                } else {
                    let p1 = m.map(Pt { x: g.p[0], y: g.p[1] });
                    let p2 = m.map(Pt { x: g.p[2], y: g.p[3] });
                    SFill::Lin {
                        x1: p1.x,
                        y1: p1.y,
                        x2: p2.x,
                        y2: p2.y,
                        g: i,
                        a,
                    }
                }
            } else if g.bbox {
                SFill::BRad {
                    cx: g.p[0],
                    cy: g.p[1],
                    r: g.p[2],
                    g: i,
                    a,
                }
            } else {
                let c = m.map(Pt { x: g.p[0], y: g.p[1] });
                let f_ = m.map(Pt { x: g.p[3], y: g.p[4] });
                let (fx, fy) = if g.p[3] == g.p[0] && g.p[4] == g.p[1] {
                    (c.x, c.y)
                } else {
                    (f_.x, f_.y)
                };
                SFill::Rad {
                    cx: fx,
                    cy: fy,
                    r: g.p[2] * m.wscale(),
                    g: i,
                    a,
                }
            })
        }
    }
}

impl SFill {
    /// ARGB at viewBox-space point `v`; `bbox` = shape bounds in same space.
    fn at(&self, gs: &[Grad], bbox: &[f32; 4], vx: f32, vy: f32) -> u32 {
        match *self {
            SFill::Solid(c) => c,
            SFill::Lin { x1, y1, x2, y2, g, a } => {
                let (dx, dy) = (x2 - x1, y2 - y1);
                let t = ((vx - x1) * dx + (vy - y1) * dy) / (dx * dx + dy * dy).max(1e-9);
                with_alpha(grad_at(&gs[g as usize], t), a)
            }
            SFill::BLin { x1, y1, x2, y2, g, a } => {
                let g_ = &gs[g as usize];
                let (bw, bh) = ((bbox[2] - bbox[0]).max(1e-9), (bbox[3] - bbox[1]).max(1e-9));
                let p = g_.gtx.map(Pt {
                    x: (vx - bbox[0]) / bw,
                    y: (vy - bbox[1]) / bh,
                });
                let (dx, dy) = (x2 - x1, y2 - y1);
                let t = ((p.x - x1) * dx + (p.y - y1) * dy) / (dx * dx + dy * dy).max(1e-9);
                with_alpha(grad_at(g_, t), a)
            }
            SFill::Rad { cx, cy, r, g, a } => {
                let t = (vx - cx).hypot(vy - cy) / r.max(1e-9);
                with_alpha(grad_at(&gs[g as usize], t), a)
            }
            SFill::BRad { cx, cy, r, g, a } => {
                let g_ = &gs[g as usize];
                let (bw, bh) = ((bbox[2] - bbox[0]).max(1e-9), (bbox[3] - bbox[1]).max(1e-9));
                let p = g_.gtx.map(Pt {
                    x: (vx - bbox[0]) / bw,
                    y: (vy - bbox[1]) / bh,
                });
                let t = (p.x - cx).hypot(p.y - cy) / r.max(1e-9);
                with_alpha(grad_at(g_, t), a)
            }
        }
    }
}

/// Gradient sample at param `t` (spreadMethod applied, stops lerped ARGB).
fn grad_at(g: &Grad, t: f32) -> u32 {
    let t = match g.spread {
        1 => {
            let t2 = t.rem_euclid(2.0);
            if t2 > 1.0 { 2.0 - t2 } else { t2 }
        }
        2 => t.rem_euclid(1.0),
        _ => t.clamp(0.0, 1.0),
    };
    let s = &g.stops;
    let (Some(&first), Some(&last)) = (s.first(), s.last()) else {
        return 0;
    };
    if t <= first.0 {
        return first.1;
    }
    if t >= last.0 {
        return last.1;
    }
    for w in s.windows(2) {
        if t <= w[1].0 {
            let k = (t - w[0].0) / (w[1].0 - w[0].0).max(1e-9);
            let (a0, a1) = (w[0].1, w[1].1);
            let mix = |sh: u32| {
                let c0 = ((a0 >> sh) & 0xff) as f32;
                let c1 = ((a1 >> sh) & 0xff) as f32;
                ((c0 + (c1 - c0) * k).round().clamp(0.0, 255.0) as u32) << sh
            };
            return mix(24) | mix(16) | mix(8) | mix(0);
        }
    }
    last.1
}

fn push_shape(ctx: &Ctx, out: &mut Vec<Shape>, mut subs: Vec<Sub>, paint: &Paint) {
    subs.retain(|s| !s.pts.is_empty());
    if subs.is_empty() {
        return;
    }
    let xf = paint.xf;
    let mut bbox = [f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY];
    for s in &mut subs {
        for p in &mut s.pts {
            *p = xf.map(*p);
            bbox[0] = bbox[0].min(p.x);
            bbox[1] = bbox[1].min(p.y);
            bbox[2] = bbox[2].max(p.x);
            bbox[3] = bbox[3].max(p.y);
        }
    }
    let det = xf.wscale().max(1e-4);
    let fill = paint
        .fill
        .and_then(|f| sfill_of(f, paint.falpha, paint.xf, ctx.grads));
    let stroke = paint.stroke.and_then(|(f, w)| {
        sfill_of(f, paint.salpha, paint.xf, ctx.grads).map(|s| (s, w * det))
    });
    out.push(Shape {
        subs,
        fill,
        stroke,
        rule: paint.rule,
        cap: paint.cap,
        join: paint.join,
        miter: paint.miter,
        clip: paint.clip,
        bbox,
    });
}

/// Emit one element (and recurse for containers). `paint` is the inherited
/// context from the parent — the element's own props are layered on inside.
fn walk(ctx: &Ctx, ch: NodeId, out: &mut Vec<Shape>, paint: Paint, depth: u8) {
    if depth > 16 {
        return;
    }
    let NodeKind::Element { tag, .. } = ctx.doc.kind(ch) else {
        return;
    };
    let props = elem_props(ctx.doc, ch);
    if matches!(props.get("display").map(|v| v.trim()), Some("none"))
        || matches!(
            props.get("visibility").map(|v| v.trim()),
            Some("hidden") | Some("collapse")
        )
    {
        return;
    }
    let mine = elem_paint(&props, paint, ctx.gmap);
    let at = |n: &str| {
        props
            .get(n)
            .or_else(|| props.get(&n.to_ascii_lowercase()))
            .map(String::as_str)
    };
    let tol = ctx.tol0 / mine.xf.wscale().max(1e-4);
    match tag.as_str() {
        "path" => {
            if let Some(d) = at("d") {
                push_shape(ctx, out, parse_d(d, tol), &mine);
            }
        }
        "circle" | "ellipse" => {
            let cx = at("cx").and_then(fnum).unwrap_or(0.0);
            let cy = at("cy").and_then(fnum).unwrap_or(0.0);
            let rx = at("r").or_else(|| at("rx")).and_then(fnum).unwrap_or(0.0);
            let ry = if tag.as_str() == "circle" {
                rx
            } else {
                at("r").or_else(|| at("ry")).and_then(fnum).unwrap_or(0.0)
            };
            if rx > 0.0 && ry > 0.0 {
                let mut pts = Vec::with_capacity(33);
                for k in 0..32 {
                    let a = k as f32 / 32.0 * std::f32::consts::TAU;
                    pts.push(Pt {
                        x: cx + rx * a.cos(),
                        y: cy + ry * a.sin(),
                    });
                }
                push_shape(ctx, out, vec![Sub { pts, closed: true }], &mine);
            }
        }
        "rect" => {
            let x = at("x").and_then(fnum).unwrap_or(0.0);
            let y = at("y").and_then(fnum).unwrap_or(0.0);
            let w = at("width").and_then(fnum).unwrap_or(0.0);
            let h = at("height").and_then(fnum).unwrap_or(0.0);
            if w <= 0.0 || h <= 0.0 {
                return;
            }
            let mut rx = at("rx")
                .or_else(|| at("ry"))
                .and_then(fnum)
                .unwrap_or(0.0)
                .min(w * 0.5);
            let mut ry = at("ry")
                .or_else(|| at("rx"))
                .and_then(fnum)
                .unwrap_or(0.0)
                .min(h * 0.5);
            if rx <= 0.0 {
                rx = ry;
            }
            if ry <= 0.0 {
                ry = rx;
            }
            let pts = if rx <= 0.0 || ry <= 0.0 {
                vec![
                    Pt { x, y },
                    Pt { x: x + w, y },
                    Pt { x: x + w, y: y + h },
                    Pt { x, y: y + h },
                ]
            } else {
                // rounded rect: 4 corner ellipse quarter-arcs
                let mut pts = Vec::with_capacity(28);
                for &(ccx, ccy, a0) in &[
                    (x + w - rx, y + ry, -90.0f32),
                    (x + w - rx, y + h - ry, 0.0),
                    (x + rx, y + h - ry, 90.0),
                    (x + rx, y + ry, 180.0),
                ] {
                    for k in 0..=6 {
                        let a = (a0 + k as f32 * 15.0).to_radians();
                        pts.push(Pt {
                            x: ccx + rx * a.cos(),
                            y: ccy + ry * a.sin(),
                        });
                    }
                }
                pts
            };
            push_shape(ctx, out, vec![Sub { pts, closed: true }], &mine);
        }
        "polygon" | "polyline" => {
            if let Some(p) = at("points") {
                let nums = nums_of(p);
                let pts: Poly = nums
                    .chunks(2)
                    .filter_map(|c| c.get(0).zip(c.get(1)).map(|(&x, &y)| Pt { x, y }))
                    .collect();
                push_shape(
                    ctx,
                    out,
                    vec![Sub {
                        pts,
                        closed: tag.as_str() == "polygon",
                    }],
                    &mine,
                );
            }
        }
        "line" => {
            let g = |n: &str| at(n).and_then(fnum).unwrap_or(0.0);
            push_shape(
                ctx,
                out,
                vec![Sub {
                    pts: vec![
                        Pt {
                            x: g("x1"),
                            y: g("y1"),
                        },
                        Pt {
                            x: g("x2"),
                            y: g("y2"),
                        },
                    ],
                    closed: false,
                }],
                &mine,
            );
        }
        "use" => {
            let href = at("xlink:href").or_else(|| at("href"));
            if let Some(id) = href.and_then(|h| h.trim().strip_prefix('#')) {
                if let Some(&tgt) = ctx.ids.get(id) {
                    let x = at("x").and_then(fnum).unwrap_or(0.0);
                    let y = at("y").and_then(fnum).unwrap_or(0.0);
                    let mut m2 = mine;
                    m2.xf = mine.xf.mul(Mat::tr(x, y));
                    walk(ctx, tgt, out, m2, depth + 1);
                }
            }
        }
        "svg" => {
            // a nested <svg> is a new viewport: x/y position plus its own
            // viewBox→width/height map (preserveAspectRatio xMidYMid meet)
            let mut m2 = mine;
            if let Some(vb) = at("viewBox").map(|v| nums_of(v)) {
                if vb.len() == 4 && vb[2] > 0.0 && vb[3] > 0.0 {
                    let x = at("x").and_then(fnum).unwrap_or(0.0);
                    let y = at("y").and_then(fnum).unwrap_or(0.0);
                    let w = at("width").and_then(fnum).unwrap_or(vb[2]);
                    let h = at("height").and_then(fnum).unwrap_or(vb[3]);
                    let s = (w / vb[2]).min(h / vb[3]);
                    m2.xf = mine.xf.mul(Mat {
                        a: s,
                        b: 0.0,
                        c: 0.0,
                        d: s,
                        e: x + (w - vb[2] * s) / 2.0 - vb[0] * s,
                        f: y + (h - vb[3] * s) / 2.0 - vb[1] * s,
                    });
                    // the new viewport clips its content (overflow:hidden
                    // is the default) — only while the mapping stays
                    // axis-aligned, which every sprite stack is
                    if mine.xf.b.abs() < 1e-9 && mine.xf.c.abs() < 1e-9 {
                        let p0 = mine.xf.map(Pt { x, y });
                        let p1 = mine.xf.map(Pt { x: x + w, y: y + h });
                        let (x0, y0) = (p0.x, p0.y);
                        let (x1, y1) = (p1.x, p1.y);
                        let r = [x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)];
                        m2.clip = Some(match m2.clip {
                            Some(c) => [
                                c[0].max(r[0]),
                                c[1].max(r[1]),
                                c[2].min(r[2]),
                                c[3].min(r[3]),
                            ],
                            None => r,
                        });
                    }
                }
            }
            for &c in ctx.doc.children(ch) {
                walk(ctx, c, out, m2, depth + 1);
            }
        }
        "g" | "symbol" | "a" | "switch" => {
            for &c in ctx.doc.children(ch) {
                walk(ctx, c, out, mine, depth + 1);
            }
        }
        // defs/title/desc/metadata/clipPath/mask/pattern/marker/text…: skipped.
        // defs children stay reachable via <use> + the id map.
        _ => {}
    }
}

// ---------- stroking ----------

fn disc(v: Pt, r: f32, out: &mut Vec<Poly>) {
    let mut p = Vec::with_capacity(9);
    for k in 0..8 {
        let a = k as f32 / 8.0 * std::f32::consts::TAU;
        p.push(Pt {
            x: v.x + r * a.cos(),
            y: v.y + r * a.sin(),
        });
    }
    out.push(p);
}

/// Arc-fan polygon centred at `v`, radius `r`, from offset `o0` sweeping
/// `sweep` radians (signed).
fn fan(v: Pt, o0: Pt, r: f32, sweep: f32, tol: f32) -> Poly {
    let step = if tol < r {
        2.0 * (1.0 - tol / r).clamp(-1.0, 1.0).acos()
    } else {
        std::f32::consts::FRAC_PI_2
    };
    let n = ((sweep.abs() / step).ceil() as usize).clamp(1, 64);
    let a0 = o0.y.atan2(o0.x);
    let mut poly = Vec::with_capacity(n + 2);
    poly.push(v);
    for k in 0..=n {
        let a = a0 + sweep * (k as f32 / n as f32);
        poly.push(Pt {
            x: v.x + r * a.cos(),
            y: v.y + r * a.sin(),
        });
    }
    poly
}

/// Stroke `subs` into filled polygon patches of width `w` (viewBox units),
/// honoring `cap`/`join`/`miter` (miter limit in multiples of width).
fn stroke_shape(
    subs: &[Sub],
    w: f32,
    cap: Cap,
    join: Join,
    mlim: f32,
    tol: f32,
    out: &mut Vec<Poly>,
) {
    let hw = w * 0.5;
    for sub in subs {
        // drop consecutive duplicates; unwrap the repeated closure point
        let mut pts: Vec<Pt> = Vec::with_capacity(sub.pts.len());
        for &p in &sub.pts {
            if pts
                .last()
                .map_or(true, |l: &Pt| (l.x - p.x).abs() + (l.y - p.y).abs() > 1e-6)
            {
                pts.push(p);
            }
        }
        if sub.closed && pts.len() > 1 {
            let (f, l) = (pts[0], *pts.last().unwrap());
            if (f.x - l.x).abs() + (f.y - l.y).abs() <= 1e-6 {
                pts.pop();
            }
        }
        let n = pts.len();
        if n < 2 {
            // zero-length subpath: caps still draw a dot (round) or square
            if n == 1 && cap != Cap::Butt {
                if cap == Cap::Round {
                    disc(pts[0], hw, out);
                } else {
                    let v = pts[0];
                    out.push(vec![
                        Pt {
                            x: v.x - hw,
                            y: v.y - hw,
                        },
                        Pt {
                            x: v.x + hw,
                            y: v.y - hw,
                        },
                        Pt {
                            x: v.x + hw,
                            y: v.y + hw,
                        },
                        Pt {
                            x: v.x - hw,
                            y: v.y + hw,
                        },
                    ]);
                }
            }
            continue;
        }
        let segs = if sub.closed { n } else { n - 1 };
        let mut dirs: Vec<Pt> = Vec::with_capacity(segs);
        for i in 0..segs {
            let (a, b) = (pts[i], pts[(i + 1) % n]);
            let (dx, dy) = (b.x - a.x, b.y - a.y);
            let l = (dx * dx + dy * dy).sqrt().max(1e-9);
            dirs.push(Pt {
                x: dx / l,
                y: dy / l,
            });
        }
        for i in 0..segs {
            let (a, b, d) = (pts[i], pts[(i + 1) % n], dirs[i]);
            let (nx, ny) = (-d.y * hw, d.x * hw);
            out.push(vec![
                Pt {
                    x: a.x + nx,
                    y: a.y + ny,
                },
                Pt {
                    x: b.x + nx,
                    y: b.y + ny,
                },
                Pt {
                    x: b.x - nx,
                    y: b.y - ny,
                },
                Pt {
                    x: a.x - nx,
                    y: a.y - ny,
                },
            ]);
        }
        // joins at every vertex (all of them for closed paths)
        let first = if sub.closed { 0 } else { 1 };
        let last = if sub.closed { n } else { n - 1 };
        for j in first..last {
            let d0 = dirs[(j + segs - 1) % segs];
            let d1 = dirs[j % segs];
            let cross = d0.x * d1.y - d0.y * d1.x;
            let dot = d0.x * d1.x + d0.y * d1.y;
            if cross.abs() < 1e-4 && dot > 0.0 {
                continue; // straight-through, quads already meet
            }
            let v = pts[j];
            // outer side is -sign(cross) for the (-dy,dx) normal convention
            let os = if cross < 0.0 { 1.0 } else { -1.0 };
            let o0 = Pt {
                x: -d0.y * hw * os,
                y: d0.x * hw * os,
            };
            let o1 = Pt {
                x: -d1.y * hw * os,
                y: d1.x * hw * os,
            };
            let th = cross.atan2(dot); // signed turn angle, |th| ≤ π
            match join {
                Join::Round => out.push(fan(v, o0, hw, th, tol)),
                Join::Miter => {
                    // miter offset = (o0+o1)/(1+cos θ); length hw/cos(θ/2)
                    let half = (th * 0.5).cos();
                    if half.abs() > 1e-3 && 1.0 / half.abs() <= mlim {
                        let k = 1.0 / (1.0 + dot);
                        let m = Pt {
                            x: v.x + (o0.x + o1.x) * k,
                            y: v.y + (o0.y + o1.y) * k,
                        };
                        out.push(vec![
                            Pt {
                                x: v.x + o0.x,
                                y: v.y + o0.y,
                            },
                            m,
                            Pt {
                                x: v.x + o1.x,
                                y: v.y + o1.y,
                            },
                        ]);
                    } else {
                        out.push(vec![
                            v,
                            Pt {
                                x: v.x + o0.x,
                                y: v.y + o0.y,
                            },
                            Pt {
                                x: v.x + o1.x,
                                y: v.y + o1.y,
                            },
                        ]);
                    }
                }
                Join::Bevel => out.push(vec![
                    v,
                    Pt {
                        x: v.x + o0.x,
                        y: v.y + o0.y,
                    },
                    Pt {
                        x: v.x + o1.x,
                        y: v.y + o1.y,
                    },
                ]),
            }
        }
        // caps on open ends
        if !sub.closed {
            for &(vi, di, sgn) in &[(0usize, 0usize, -1.0f32), (n - 1, segs - 1, 1.0)] {
                let v = pts[vi];
                let d = dirs[di];
                let (nx, ny) = (-d.y * hw, d.x * hw);
                match cap {
                    Cap::Butt => {}
                    Cap::Square => {
                        let (ex, ey) = (d.x * hw * sgn, d.y * hw * sgn);
                        out.push(vec![
                            Pt {
                                x: v.x + nx,
                                y: v.y + ny,
                            },
                            Pt {
                                x: v.x + nx + ex,
                                y: v.y + ny + ey,
                            },
                            Pt {
                                x: v.x - nx + ex,
                                y: v.y - ny + ey,
                            },
                            Pt {
                                x: v.x - nx,
                                y: v.y - ny,
                            },
                        ]);
                    }
                    Cap::Round => {
                        // semicircle starting at the clockwise-normal of the
                        // outward dir so the +π sweep passes through it
                        let o0 = Pt {
                            x: sgn * d.y * hw,
                            y: -sgn * d.x * hw,
                        };
                        out.push(fan(v, o0, hw, std::f32::consts::PI, tol));
                    }
                }
            }
        }
    }
}

// ---------- filling ----------

/// Scanline fill of closed rings `polys` under `rule`, compositing `color`
/// (ARGB) source-over into `out` (BGRA).
fn fill_polys(
    polys: &[&Poly],
    rule: Rule,
    sf: &SFill,
    gs: &[Grad],
    bbox: &[f32; 4],
    out: &mut [u8],
    w: usize,
    h: usize,
    m: &VMap,
    clip: Option<(f32, f32, f32, f32)>,
) {
    if matches!(sf, SFill::Solid(c) if c >> 24 == 0) || m.sy.abs() < 1e-9 || m.sx.abs() < 1e-9
    {
        return;
    }
    let (cx0, cy0, cx1, cy1) = clip.unwrap_or((0.0, 0.0, w as f32, h as f32));
    let y_start = (cy0 - 0.5).ceil().clamp(0.0, h as f32) as usize;
    let y_end = (cy1 - 0.5).ceil().clamp(0.0, h as f32) as usize;
    // crossing list is reused across rows — one alloc per shape instead of
    // one per scanline
    let mut xs: Vec<(f32, i32)> = Vec::new();
    for y in y_start..y_end {
        let wy = (y as f32 + 0.5 - m.oy) / m.sy;
        xs.clear();
        for poly in polys {
            let n = poly.len();
            if n < 2 {
                continue;
            }
            for k in 0..n {
                let (a, b) = (poly[k], poly[(k + 1) % n]);
                if (a.y <= wy) != (b.y <= wy) {
                    let t = (wy - a.y) / (b.y - a.y);
                    let wx = a.x + t * (b.x - a.x);
                    xs.push((wx * m.sx + m.ox, if b.y > a.y { 1 } else { -1 }));
                }
            }
        }
        if xs.is_empty() {
            continue;
        }
        xs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let mut wind = 0i32;
        let mut parity = false;
        let mut from: Option<f32> = None;
        for &(x, d) in &xs {
            wind += d;
            parity = !parity;
            let inside = match rule {
                Rule::EvenOdd => parity,
                Rule::NonZero => wind != 0,
            };
            match (from, inside) {
                (None, true) => from = Some(x),
                (Some(x0), false) => {
                    // pixel centres inside [x0, x) and the clip range
                    let px0 = (x0 - 0.5).ceil().max((cx0 - 0.5).ceil()).max(0.0) as usize;
                    let px1 = (x - 0.5)
                        .ceil()
                        .min((cx1 - 0.5).ceil())
                        .clamp(0.0, w as f32) as usize;
                    for px in px0..px1.min(w) {
                        let wx = (px as f32 + 0.5 - m.ox) / m.sx;
                        let color = sf.at(gs, bbox, wx, wy);

                        let (r, g, b, sa) = (
                            (color >> 16) & 0xff,
                            (color >> 8) & 0xff,
                            color & 0xff,
                            color >> 24,
                        );
                        if sa == 0 {
                            continue;
                        }
                        let j = (y * w + px) * 4;
                        let da = out[j + 3] as u32;
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
                    from = None;
                }
                _ => {}
            }
        }
    }
}

/// Premultiplied-average box downsample from `bw`-wide buffer to `w`×`h`.
fn downsample(buf: &[u8], bw: usize, ss: usize, w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let (mut sa, mut sb, mut sg, mut sr) = (0u32, 0u32, 0u32, 0u32);
            for dy in 0..ss {
                for dx in 0..ss {
                    let j = ((y * ss + dy) * bw + x * ss + dx) * 4;
                    let a = buf[j + 3] as u32;
                    sa += a;
                    sb += buf[j] as u32 * a;
                    sg += buf[j + 1] as u32 * a;
                    sr += buf[j + 2] as u32 * a;
                }
            }
            let i = (y * w + x) * 4;
            if sa > 0 {
                out[i] = (sb / sa) as u8;
                out[i + 1] = (sg / sa) as u8;
                out[i + 2] = (sr / sa) as u8;
            }
            out[i + 3] = (sa / (ss * ss) as u32) as u8;
        }
    }
    out
}

// ---------- entry ----------

/// Rasterize the <svg> element `node` to `w`×`h` BGRA pixels.
/// `fallback_color` supplies `currentColor`.
pub fn render(doc: &Document, node: NodeId, w: u32, h: u32, fallback_color: u32) -> Vec<u8> {
    let w = w.max(1) as usize;
    let h = h.max(1) as usize;

    let (vbx, vby, vbw, vbh) = doc
        .attr(node, "viewBox")
        .or_else(|| doc.attr(node, "viewbox"))
        .and_then(|v| {
            let nums = nums_of(v);
            (nums.len() >= 4).then(|| (nums[0], nums[1], nums[2], nums[3].max(0.01)))
        })
        .map(|(a, b, c, d)| (a, b, c.max(0.01), d))
        .unwrap_or((0.0, 0.0, w as f32, h as f32));

    // preserveAspectRatio: none → stretch; else meet/slice + align
    let par = doc
        .attr(node, "preserveaspectratio")
        .or_else(|| doc.attr(node, "preserveAspectRatio"))
        .unwrap_or("");
    let mut it = par.split_whitespace().filter(|t| *t != "defer");
    let align = it.next().unwrap_or("xMidYMid");
    let mos = it.next().unwrap_or("meet");
    let map = if align.eq_ignore_ascii_case("none") {
        VMap {
            sx: w as f32 / vbw,
            sy: h as f32 / vbh,
            ox: -vbx * w as f32 / vbw,
            oy: -vby * h as f32 / vbh,
        }
    } else {
        let s = if mos.eq_ignore_ascii_case("slice") {
            (w as f32 / vbw).max(h as f32 / vbh)
        } else {
            (w as f32 / vbw).min(h as f32 / vbh)
        };
        let ax = if align.contains("xMin") {
            0.0
        } else if align.contains("xMax") {
            w as f32 - vbw * s
        } else {
            (w as f32 - vbw * s) * 0.5
        };
        let ay = if align.contains("YMin") {
            0.0
        } else if align.contains("YMax") {
            h as f32 - vbh * s
        } else {
            (h as f32 - vbh * s) * 0.5
        };
        VMap {
            sx: s,
            sy: s,
            ox: ax - vbx * s,
            oy: ay - vby * s,
        }
    };

    // supersample small outputs for antialiasing
    let ss = if w.max(h) < 64 {
        4
    } else if w.max(h) <= 256 {
        2
    } else {
        1
    };
    let (bw, bh) = (w * ss, h * ss);
    let f = ss as f32;
    let smap = VMap {
        sx: map.sx * f,
        sy: map.sy * f,
        ox: map.ox * f,
        oy: map.oy * f,
    };
    let tol0 = 0.3 / smap.sx.abs().max(0.01); // ~0.3 output px in viewBox units

    // paint tree → flattened shapes
    let mut ids = HashMap::new();
    collect_ids(doc, node, &mut ids);
    let (grads, gmap) = collect_grads(doc, node, &ids);
    let ctx = Ctx {
        doc,
        ids: &ids,
        grads: &grads,
        gmap: &gmap,
        tol0,
    };
    let base = Paint {
        fill: Some(Fill::Solid(0xff000000)), // SVG default fill is black
        falpha: 1.0,
        stroke: None,
        salpha: 1.0,
        rule: Rule::NonZero,
        cap: Cap::Butt,
        join: Join::Miter,
        miter: 4.0,
        cc: fallback_color,
        xf: Mat::ID,
        clip: None,
    };
    let root = elem_paint(&elem_props(doc, node), base, &gmap);
    let mut shapes: Vec<Shape> = Vec::new();
    for &c in doc.children(node) {
        walk(&ctx, c, &mut shapes, root, 0);
    }

    // fills first then strokes per shape, in document order
    let mut buf = vec![0u8; bw * bh * 4];
    for sh in &shapes {
        let clip = sh.clip.map(|r| {
            let (ax, ay) = (r[0] * smap.sx + smap.ox, r[1] * smap.sy + smap.oy);
            let (bx, by) = (r[2] * smap.sx + smap.ox, r[3] * smap.sy + smap.oy);
            (ax.min(bx), ay.min(by), ax.max(bx), ay.max(by))
        });
        if let Some(c) = &sh.fill {
            let polys: Vec<&Poly> = sh.subs.iter().map(|s| &s.pts).collect();
            fill_polys(&polys, sh.rule, c, &grads, &sh.bbox, &mut buf, bw, bh, &smap, clip);
        }
        if let Some((c, sw)) = &sh.stroke {
            let mut patches = Vec::new();
            stroke_shape(&sh.subs, *sw, sh.cap, sh.join, sh.miter, tol0, &mut patches);
            for p in &patches {
                fill_polys(&[p], Rule::NonZero, c, &grads, &sh.bbox, &mut buf, bw, bh, &smap, clip);
            }
        }
    }
    if ss > 1 {
        downsample(&buf, bw, ss, w, h)
    } else {
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(svg: &str) -> (Document, NodeId) {
        let d = rowser_html::parse(svg);
        let s = d.find_deep(d.root, "svg").expect("svg element");
        (d, s)
    }
    fn covered(px: &[u8]) -> usize {
        px.chunks(4).filter(|p| p[3] > 8).count()
    }
    fn pix(pxs: &[u8], w: usize, x: usize, y: usize) -> [u8; 4] {
        let j = (y * w + x) * 4;
        [pxs[j], pxs[j + 1], pxs[j + 2], pxs[j + 3]]
    }

    #[test]
    fn arcs_make_circles() {
        // full circle from two 180° arcs
        let (d, s) = doc(
            r##"<svg viewBox="0 0 24 24"><path d="M4 12 A8 8 0 1 0 20 12 A8 8 0 1 0 4 12" fill="#fff"/></svg>"##,
        );
        let px = render(&d, s, 48, 48, 0xff000000);
        let cov = covered(&px);
        let expect = std::f64::consts::PI * 16.0 * 16.0; // r=16px circle
        assert!(
            cov > (expect * 0.8) as usize && cov < (expect * 1.2) as usize,
            "cov={cov} expect~{expect}"
        );
        // arc endpoint must land on (20,12) in vb coords
        let subs = parse_d("M4 12 A8 8 0 1 0 20 12", 0.01);
        let last = subs[0].pts.last().unwrap();
        assert!((last.x - 20.0).abs() < 0.01 && (last.y - 12.0).abs() < 0.01);
    }

    #[test]
    fn multi_subpath_evenodd_hole() {
        let (d, s) = doc(
            r##"<svg viewBox="0 0 10 10"><path fill-rule="evenodd" fill="#fff"
            d="M0 0h10v10h-10z M2 2h6v6h-6z"/></svg>"##,
        );
        let px = render(&d, s, 20, 20, 0);
        assert_eq!(pix(&px, 20, 10, 10)[3], 0, "hole must be transparent");
        assert!(pix(&px, 20, 1, 1)[3] > 200, "ring must be filled");
    }

    #[test]
    fn multi_subpath_nonzero_hole() {
        // inner ring wound opposite → hole under nonzero
        let (d, s) = doc(r##"<svg viewBox="0 0 10 10"><path fill="#fff"
            d="M0 0h10v10h-10z M2 2v6h6v-6z"/></svg>"##);
        let px = render(&d, s, 20, 20, 0);
        assert_eq!(pix(&px, 20, 10, 10)[3], 0, "counter-wound hole transparent");
        // same winding → no hole
        let (d2, s2) = doc(r##"<svg viewBox="0 0 10 10"><path fill="#fff"
            d="M0 0h10v10h-10z M2 2h6v6h-6z"/></svg>"##);
        let px2 = render(&d2, s2, 20, 20, 0);
        assert!(pix(&px2, 20, 10, 10)[3] > 200, "same-winding stays solid");
    }

    #[test]
    fn transform_translate_group() {
        let (d, s) = doc(
            r#"<svg viewBox="0 0 20 20"><g transform="translate(10 0)" style="fill:#ff0000">
            <rect width="5" height="5"/></g></svg>"#,
        );
        let px = render(&d, s, 20, 20, 0);
        let p = pix(&px, 20, 11, 2);
        assert!(
            p[3] > 200 && p[2] > 200 && p[1] < 40,
            "translated red rect: {p:?}"
        );
        assert_eq!(pix(&px, 20, 2, 2)[3], 0);
    }

    #[test]
    fn lucide_line_icon_not_a_box() {
        let (d, s) = doc(
            r#"<svg fill="none" stroke="currentColor" viewBox="0 0 24 24" width="24" height="24">
            <path d="M5 12h14"/></svg>"#,
        );
        let px = render(&d, s, 24, 24, 0xffffffff);
        let cov = covered(&px);
        assert!(cov > 8, "line should paint some pixels, got {cov}");
        assert!(
            cov < 24 * 24 / 3,
            "line icon must not fill the box, cov={cov}"
        );
        assert_eq!(pix(&px, 24, 0, 0)[3], 0, "corner stays transparent");
        // and via style= instead of attributes
        let (d2, s2) = doc(
            r#"<svg style="fill:none;stroke:currentColor" viewBox="0 0 24 24">
            <path d="M5 12h14"/></svg>"#,
        );
        let px2 = render(&d2, s2, 24, 24, 0xffffffff);
        assert!(covered(&px2) > 8 && covered(&px2) < 24 * 24 / 3);
    }

    #[test]
    fn defs_use_and_opacity() {
        let (d, s) = doc(r##"<svg viewBox="0 0 10 10"><defs>
            <rect id="r" width="4" height="4"/></defs>
            <use href="#r" x="3" y="3" fill="#00ff00" opacity="0.5"/></svg>"##);
        let px = render(&d, s, 10, 10, 0);
        let p = pix(&px, 10, 4, 4);
        assert!(
            p[3] > 100 && p[3] < 160 && p[1] > 200,
            "half-alpha green via use: {p:?}"
        );
    }

    #[test]
    fn round_join_covers_corner() {
        // 90° bend, round join must cover the outer corner pixel
        let (d, s) = doc(r##"<svg viewBox="0 0 20 20"><path d="M4 16 L10 16 L10 4"
            fill="none" stroke="#fff" stroke-width="4" stroke-linejoin="round"/></svg>"##);
        let px = render(&d, s, 20, 20, 0);
        // outer corner of the bend — the quad arms leave a gap that the
        // round join's arc fan (r=hw=2 about (10,16)) must cover
        let p = pix(&px, 20, 10, 17);
        assert!(p[3] > 60, "round join covers corner, got {p:?}");
    }

    #[test]
    fn stroke_only_no_fill_box() {
        // the reported lucide bug: bare stroke + fill none must not paint a box
        let (d, s) = doc(
            r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor"
            stroke-width="2"><circle cx="12" cy="12" r="9"/></svg>"#,
        );
        let px = render(&d, s, 48, 48, 0xffffffff);
        let cov = covered(&px);
        assert!(
            cov > 24 && cov < 48 * 48 / 3,
            "ring outline only, cov={cov}"
        );
        assert_eq!(pix(&px, 48, 24, 24)[3], 0, "centre stays empty");
    }
}
