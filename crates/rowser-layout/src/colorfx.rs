//! colorfx — modern CSS color mixing and paint effects: `filter`,
//! `backdrop-filter`, `clip-path`, `mix-blend-mode`, and non-sRGB
//! `color-mix()` interpolation.
//!
//! Layout wraps an element's subtree items in
//! `DisplayItem::PushEffect(e)` … `DisplayItem::PopEffect`; rowser-paint
//! renders each span to an offscreen tile (or edits the canvas in place
//! for backdrop filters). Push order for a single element is
//! [Backdrop, Blend, Filter, Clip] — content is clipped first, the
//! clipped result filtered, then blended over the backdrop.

use crate::{
    alpha, bgra_a, ch, fhue, fnum, fn_color, fn_color_srgb, hsl_rgb, hwb_rgb,
    lab_rgb, oklab_rgb, parse_color_ctx, parse_len, srgb_un, top_args, Color,
    DisplayItem, Props,
};

/// One `filter`/`backdrop-filter` function, resolved for the painter.
#[derive(Debug, Clone)]
pub enum FilterOp {
    /// Gaussian blur, sigma in px.
    Blur(f32),
    /// 4×5 RGBA color matrix, row-major, operating on 0..1 channels:
    /// `out[i] = Σ m[i*5+j]·in[j] + m[i*5+4]` (R,G,B,A order).
    Matrix([f32; 20]),
    /// Blurred alpha silhouette painted `dx,dy` behind the content.
    DropShadow {
        dx: f32,
        dy: f32,
        blur: f32,
        color: Color,
    },
}

/// `mix-blend-mode` keywords.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BlendMode {
    Normal,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    ColorDodge,
    ColorBurn,
    HardLight,
    SoftLight,
    Difference,
    Exclusion,
    Hue,
    Saturation,
    Color,
    Luminosity,
}

impl BlendMode {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_lowercase().as_str() {
            "normal" => Self::Normal,
            "multiply" => Self::Multiply,
            "screen" => Self::Screen,
            "overlay" => Self::Overlay,
            "darken" => Self::Darken,
            "lighten" => Self::Lighten,
            "color-dodge" => Self::ColorDodge,
            "color-burn" => Self::ColorBurn,
            "hard-light" => Self::HardLight,
            "soft-light" => Self::SoftLight,
            "difference" => Self::Difference,
            "exclusion" => Self::Exclusion,
            "hue" => Self::Hue,
            "saturation" => Self::Saturation,
            "color" => Self::Color,
            "luminosity" => Self::Luminosity,
            _ => return None,
        })
    }
}

/// A clip-path length: `px` absolute plus `frac` of the reference-box
/// axis (`left 10px`-style keyword+offset positions fold into both).
#[derive(Debug, Clone, Copy)]
pub struct CLen {
    pub frac: f32,
    pub px: f32,
}

impl CLen {
    fn resolve(self, base: f32) -> f32 {
        self.frac * base + self.px
    }
    fn parse(t: &str, em: f32) -> Option<CLen> {
        let t = t.trim();
        if let Some(p) = t.strip_suffix('%') {
            return p
                .trim()
                .parse::<f32>()
                .ok()
                .map(|n| CLen { frac: n / 100.0, px: 0.0 });
        }
        parse_len(t, 0.0, em).map(|v| CLen { frac: 0.0, px: v })
    }
}

/// `clip-path` basic shape in element-local coords (origin = border-box
/// top-left; `frac` components resolve against the reference box).
#[derive(Debug, Clone)]
pub enum ClipShape {
    /// `inset(t r b l [round radius])` — offsets from each box edge.
    Inset {
        t: CLen,
        r: CLen,
        b: CLen,
        l: CLen,
        radius: CLen,
    },
    /// `circle([r] [at cx cy])`
    Circle {
        r: Option<CLen>,
        cx: CLen,
        cy: CLen,
    },
    /// `ellipse([rx ry] [at cx cy])` — None = closest-side
    Ellipse {
        rx: Option<CLen>,
        ry: Option<CLen>,
        cx: CLen,
        cy: CLen,
    },
    /// `polygon(x y, ...)` vertex list.
    Polygon(Vec<(CLen, CLen)>),
}

/// A paint-time effect group. `rect` fields carry the element's border
/// box when layout knows it; `None` → the painter uses the span's own
/// item bbox as the reference box.
#[derive(Debug, Clone)]
pub enum Effect {
    /// `filter:` op list applied to the subtree's painted output.
    Filter(Vec<FilterOp>),
    /// `backdrop-filter:` ops applied to the canvas region *behind* the
    /// subtree before it paints.
    Backdrop {
        ops: Vec<FilterOp>,
        rect: Option<[f32; 4]>,
    },
    /// `clip-path:` shape mask limiting the subtree's painting.
    Clip {
        shape: ClipShape,
        rect: Option<[f32; 4]>,
    },
    /// `mix-blend-mode:` subtree blended over the backdrop.
    Blend(BlendMode),
}

impl Effect {
    /// Shift absolute rects — used when a whole box is translated after
    /// layout (transform/relative offsets, flex/grid item placement).
    pub fn offset(self, dx: f32, dy: f32) -> Effect {
        let shift = |r: Option<[f32; 4]>| r.map(|r| [r[0] + dx, r[1] + dy, r[2], r[3]]);
        match self {
            Effect::Backdrop { ops, rect } => Effect::Backdrop {
                ops,
                rect: shift(rect),
            },
            Effect::Clip { shape, rect } => Effect::Clip {
                shape,
                rect: shift(rect),
            },
            other => other,
        }
    }

    /// In-place variant for `&mut` owners (e.g. the scroll fast path that
    /// nudges every item vertically without rebuilding the list).
    pub fn offset_mut(&mut self, dx: f32, dy: f32) {
        *self = std::mem::replace(self, Effect::Blend(BlendMode::Normal)).offset(dx, dy);
    }
}

/// Effects declared on an element, outermost-first (push order).
/// `cur` = element's resolved `color` (drop-shadow default); `rect` =
/// border box when the caller knows it.
pub fn element_fx(c: &Props, size: f32, cur: Color, rect: Option<[f32; 4]>) -> Vec<Effect> {
    let mut out = Vec::new();
    if let Some(v) = c.get("backdrop-filter") {
        let ops = parse_filter(v, size, cur);
        if !ops.is_empty() {
            out.push(Effect::Backdrop { ops, rect });
        }
    }
    if let Some(m) = c.get("mix-blend-mode").and_then(|v| BlendMode::parse(v)) {
        if m != BlendMode::Normal {
            out.push(Effect::Blend(m));
        }
    }
    if let Some(v) = c.get("filter") {
        let ops = parse_filter(v, size, cur);
        if !ops.is_empty() {
            out.push(Effect::Filter(ops));
        }
    }
    if let Some(v) = c.get("clip-path") {
        if let Some(shape) = parse_clip_path(v, size) {
            out.push(Effect::Clip { shape, rect });
        }
    }
    out
}

/// Splice Push/Pop markers around `items[start..]`, outermost first.
pub fn wrap_vec(items: &mut Vec<DisplayItem>, start: usize, fx: Vec<Effect>) {
    let n = fx.len();
    for (k, e) in fx.into_iter().enumerate() {
        items.insert(start + k, DisplayItem::PushEffect(e));
    }
    for _ in 0..n {
        items.push(DisplayItem::PopEffect);
    }
}

// ---------- filter parsing ----------

/// Split on whitespace outside any parens (`rgb(1, 2, 3)` stays whole).
fn ws_tokens(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut last = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            _ if c.is_whitespace() && depth <= 0 => {
                if i > last {
                    out.push(&s[last..i]);
                }
                last = i + 1;
            }
            _ => {}
        }
    }
    if s.len() > last {
        out.push(&s[last..]);
    }
    out
}

/// `<number>`|`<pct>` amount: `30%`/`0.3` → 0.3; empty → `def`.
fn amount(s: &str, def: f32) -> f32 {
    let s = s.trim();
    if s.is_empty() {
        def
    } else if let Some(p) = s.strip_suffix('%') {
        fnum(p) / 100.0
    } else {
        fnum(s)
    }
}

const IDENT: [f32; 20] = [
    1.0, 0.0, 0.0, 0.0, 0.0, //
    0.0, 1.0, 0.0, 0.0, 0.0, //
    0.0, 0.0, 1.0, 0.0, 0.0, //
    0.0, 0.0, 0.0, 1.0, 0.0,
];

fn m_lerp(a: [f32; 20], b: [f32; 20], t: f32) -> [f32; 20] {
    let t = t.clamp(0.0, 1.0);
    let mut m = [0.0f32; 20];
    for i in 0..20 {
        m[i] = a[i] + (b[i] - a[i]) * t;
    }
    m
}

fn m_brightness(k: f32) -> [f32; 20] {
    [
        k, 0.0, 0.0, 0.0, 0.0, //
        0.0, k, 0.0, 0.0, 0.0, //
        0.0, 0.0, k, 0.0, 0.0, //
        0.0, 0.0, 0.0, 1.0, 0.0,
    ]
}

fn m_contrast(k: f32) -> [f32; 20] {
    let o = 0.5 - 0.5 * k;
    [
        k, 0.0, 0.0, 0.0, o, //
        0.0, k, 0.0, 0.0, o, //
        0.0, 0.0, k, 0.0, o, //
        0.0, 0.0, 0.0, 1.0, 0.0,
    ]
}

/// SVG `saturate` matrix (Rec.601 luma).
fn m_saturate(s: f32) -> [f32; 20] {
    [
        0.213 + 0.787 * s,
        0.715 - 0.715 * s,
        0.072 - 0.072 * s,
        0.0,
        0.0, //
        0.213 - 0.213 * s,
        0.715 + 0.285 * s,
        0.072 - 0.072 * s,
        0.0,
        0.0, //
        0.213 - 0.213 * s,
        0.715 - 0.715 * s,
        0.072 + 0.928 * s,
        0.0,
        0.0, //
        0.0,
        0.0,
        0.0,
        1.0,
        0.0,
    ]
}

fn m_invert(i: f32) -> [f32; 20] {
    let k = 1.0 - 2.0 * i;
    [
        k, 0.0, 0.0, 0.0, i, //
        0.0, k, 0.0, 0.0, i, //
        0.0, 0.0, k, 0.0, i, //
        0.0, 0.0, 0.0, 1.0, 0.0,
    ]
}

const SEPIA: [f32; 20] = [
    0.393, 0.769, 0.189, 0.0, 0.0, //
    0.349, 0.686, 0.168, 0.0, 0.0, //
    0.272, 0.534, 0.131, 0.0, 0.0, //
    0.0, 0.0, 0.0, 1.0, 0.0,
];

/// SVG `hueRotate` matrix (luma-preserving rotation).
fn m_hue_rotate(deg: f32) -> [f32; 20] {
    let (s, c) = deg.to_radians().sin_cos();
    [
        0.213 + c * 0.787 - s * 0.213,
        0.715 - c * 0.715 - s * 0.715,
        0.072 - c * 0.072 + s * 0.928,
        0.0,
        0.0, //
        0.213 - c * 0.213 + s * 0.143,
        0.715 + c * 0.285 + s * 0.140,
        0.072 - c * 0.072 - s * 0.283,
        0.0,
        0.0, //
        0.213 - c * 0.213 - s * 0.787,
        0.715 - c * 0.715 + s * 0.715,
        0.072 + c * 0.928 + s * 0.072,
        0.0,
        0.0, //
        0.0,
        0.0,
        0.0,
        1.0,
        0.0,
    ]
}

fn m_opacity(o: f32) -> [f32; 20] {
    [
        1.0, 0.0, 0.0, 0.0, 0.0, //
        0.0, 1.0, 0.0, 0.0, 0.0, //
        0.0, 0.0, 1.0, 0.0, 0.0, //
        0.0, 0.0, 0.0, o, 0.0,
    ]
}

/// `drop-shadow(<x> <y> [blur] [color])` — the color may precede the
/// lengths and may itself contain parens/commas.
fn parse_drop_shadow(inner: &str, em: f32, cur: Color) -> Option<FilterOp> {
    let mut lens = Vec::new();
    let mut col_toks: Vec<&str> = Vec::new();
    for t in ws_tokens(inner) {
        if lens.len() < 3 {
            if let Some(n) = parse_len(t, em, em) {
                lens.push(n);
                continue;
            }
        }
        col_toks.push(t);
    }
    if lens.len() < 2 {
        return None;
    }
    let color = if col_toks.is_empty() {
        cur
    } else {
        parse_color_ctx(&col_toks.join(" "), Some(cur)).unwrap_or(cur)
    };
    Some(FilterOp::DropShadow {
        dx: lens[0],
        dy: lens[1],
        blur: lens.get(2).copied().unwrap_or(0.0).max(0.0),
        color,
    })
}

/// `filter:`/`backdrop-filter:` value → op list. `none` and `url()`
/// yield nothing.
pub fn parse_filter(v: &str, size: f32, cur: Color) -> Vec<FilterOp> {
    let v = v.trim().to_lowercase();
    if v.is_empty() || v == "none" {
        return Vec::new();
    }
    let mut ops = Vec::new();
    for tok in ws_tokens(&v) {
        let Some(open) = tok.find('(') else { continue };
        let name = &tok[..open];
        let rest = &tok[open + 1..];
        // strip exactly one ')' — a nested call keeps its own
        let inner = rest.strip_suffix(')').unwrap_or(rest).trim();
        match name {
            "blur" => ops.push(FilterOp::Blur(
                parse_len(inner, size, size).unwrap_or(0.0).max(0.0),
            )),
            "brightness" => ops.push(FilterOp::Matrix(m_brightness(amount(inner, 1.0)))),
            "contrast" => ops.push(FilterOp::Matrix(m_contrast(amount(inner, 1.0)))),
            "drop-shadow" => {
                if let Some(op) = parse_drop_shadow(inner, size, cur) {
                    ops.push(op);
                }
            }
            "grayscale" => ops.push(FilterOp::Matrix(m_lerp(
                IDENT,
                m_saturate(0.0),
                amount(inner, 1.0),
            ))),
            "hue-rotate" => ops.push(FilterOp::Matrix(m_hue_rotate(fhue(inner)))),
            "invert" => ops.push(FilterOp::Matrix(m_invert(amount(inner, 1.0)))),
            "opacity" => ops.push(FilterOp::Matrix(m_opacity(amount(inner, 1.0)))),
            "saturate" => ops.push(FilterOp::Matrix(m_saturate(amount(inner, 1.0)))),
            "sepia" => ops.push(FilterOp::Matrix(m_lerp(IDENT, SEPIA, amount(inner, 1.0)))),
            _ => {} // url() and unknowns ignored
        }
    }
    ops
}

// ---------- clip-path parsing ----------

/// `<position>` tokens → (x, y). Handles keywords and one/two-value
/// offsets (`center`, `left`, `30% 60%`, `left 10px top 20%`).
fn clip_position(toks: &[&str], em: f32) -> (CLen, CLen) {
    let mut x = CLen { frac: 0.5, px: 0.0 };
    let mut y = CLen { frac: 0.5, px: 0.0 };
    let mut anchor: Option<bool> = None; // Some(false)=x side, Some(true)=y side
    let mut bare = 0;
    for &t in toks {
        match t {
            "left" | "right" => {
                x = CLen {
                    frac: if t == "right" { 1.0 } else { 0.0 },
                    px: 0.0,
                };
                anchor = Some(false);
            }
            "top" | "bottom" => {
                y = CLen {
                    frac: if t == "bottom" { 1.0 } else { 0.0 },
                    px: 0.0,
                };
                anchor = Some(true);
            }
            "center" => {
                if anchor.is_none() {
                    anchor = Some(false);
                }
            }
            _ => {
                if let Some(l) = CLen::parse(t, em) {
                    match anchor {
                        Some(false) => {
                            if x.frac > 0.5 {
                                x.frac -= l.frac;
                                x.px -= l.px;
                            } else {
                                x.frac += l.frac;
                                x.px += l.px;
                            }
                            anchor = None;
                        }
                        Some(true) => {
                            if y.frac > 0.5 {
                                y.frac -= l.frac;
                                y.px -= l.px;
                            } else {
                                y.frac += l.frac;
                                y.px += l.px;
                            }
                            anchor = None;
                        }
                        None => {
                            if bare == 0 {
                                x = l;
                            } else {
                                y = l;
                            }
                            bare += 1;
                        }
                    }
                }
            }
        }
    }
    (x, y)
}

/// `clip-path:` → basic shape. `none`, `url(...)`, `path(...)`, `xywh()`
/// and bare reference-box keywords yield None.
pub fn parse_clip_path(v: &str, size: f32) -> Option<ClipShape> {
    let v = v.trim().to_lowercase();
    let open = v.find('(')?;
    let name = v[..open].trim();
    let rest = &v[open + 1..];
    let inner = rest.strip_suffix(')').unwrap_or(rest).trim();
    match name {
        "inset" => {
            let (lens_s, rad_s) = match inner.find("round") {
                Some(i) => (&inner[..i], &inner[i + 5..]),
                None => (inner, ""),
            };
            let l: Vec<CLen> = ws_tokens(lens_s)
                .iter()
                .filter_map(|t| CLen::parse(t, size))
                .collect();
            if l.is_empty() {
                return None;
            }
            let g = |i: usize| l[i.min(l.len() - 1)];
            let (t, r, b, lf) = match l.len() {
                1 => (g(0), g(0), g(0), g(0)),
                2 => (g(0), g(1), g(0), g(1)),
                3 => (g(0), g(1), g(2), g(1)),
                _ => (g(0), g(1), g(2), g(3)),
            };
            let radius = ws_tokens(rad_s)
                .iter()
                .filter_map(|t| CLen::parse(t, size))
                .next()
                .unwrap_or(CLen { frac: 0.0, px: 0.0 });
            Some(ClipShape::Inset {
                t,
                r,
                b,
                l: lf,
                radius,
            })
        }
        "circle" | "ellipse" => {
            let (radii_s, pos_s) = match inner.find(" at ") {
                Some(i) => (&inner[..i], &inner[i + 4..]),
                None => (inner, ""),
            };
            let radii: Vec<CLen> = ws_tokens(radii_s)
                .iter()
                .filter_map(|t| CLen::parse(t, size))
                .collect();
            let (cx, cy) = clip_position(&ws_tokens(pos_s), size);
            if name == "circle" {
                Some(ClipShape::Circle {
                    r: radii.first().copied(),
                    cx,
                    cy,
                })
            } else {
                Some(ClipShape::Ellipse {
                    rx: radii.first().copied(),
                    ry: radii.get(1).copied(),
                    cx,
                    cy,
                })
            }
        }
        "polygon" => {
            let mut pts = Vec::new();
            for pair in top_args(inner) {
                let t = ws_tokens(pair.trim());
                if t.len() < 2 {
                    continue;
                }
                if let (Some(x), Some(y)) = (CLen::parse(t[0], size), CLen::parse(t[1], size)) {
                    pts.push((x, y));
                }
            }
            if pts.len() < 3 {
                return None;
            }
            Some(ClipShape::Polygon(pts))
        }
        _ => None,
    }
}

// ---------- shape rasterization ----------

/// ClipShape resolved to absolute px against the reference (border) box.
#[derive(Debug, Clone)]
pub enum ClipAbs {
    Rounded { x: f32, y: f32, w: f32, h: f32, r: f32 },
    Ellipse { cx: f32, cy: f32, rx: f32, ry: f32 },
    Polygon(Vec<(f32, f32)>),
}

pub fn resolve_clip(s: &ClipShape, rect: [f32; 4]) -> ClipAbs {
    let [bx, by, bw, bh] = rect;
    match s {
        ClipShape::Inset { t, r, b, l, radius } => {
            let x = bx + l.resolve(bw);
            let y = by + t.resolve(bh);
            let x2 = bx + bw - r.resolve(bw);
            let y2 = by + bh - b.resolve(bh);
            ClipAbs::Rounded {
                x,
                y,
                w: (x2 - x).max(0.0),
                h: (y2 - y).max(0.0),
                r: radius.resolve(bw.min(bh)).clamp(0.0, bw.min(bh) / 2.0),
            }
        }
        ClipShape::Circle { r, cx, cy } => {
            let cx = bx + cx.resolve(bw);
            let cy = by + cy.resolve(bh);
            // % radii resolve vs sqrt(w²+h²)/√2; none → closest-side
            let rr = match r {
                Some(cl) => cl.resolve((bw * bw + bh * bh).sqrt() / std::f32::consts::SQRT_2),
                None => (cx - bx)
                    .min(bx + bw - cx)
                    .min(cy - by)
                    .min(by + bh - cy)
                    .max(0.0),
            };
            ClipAbs::Ellipse {
                cx,
                cy,
                rx: rr,
                ry: rr,
            }
        }
        ClipShape::Ellipse { rx: ex, ry: ey, cx, cy } => {
            let cx = bx + cx.resolve(bw);
            let cy = by + cy.resolve(bh);
            ClipAbs::Ellipse {
                cx,
                cy,
                rx: ex
                    .map(|c| c.resolve(bw))
                    .unwrap_or_else(|| (cx - bx).min(bx + bw - cx).max(0.0)),
                ry: ey
                    .map(|c| c.resolve(bh))
                    .unwrap_or_else(|| (cy - by).min(by + bh - cy).max(0.0)),
            }
        }
        ClipShape::Polygon(pts) => ClipAbs::Polygon(
            pts.iter()
                .map(|(a, b)| (bx + a.resolve(bw), by + b.resolve(bh)))
                .collect(),
        ),
    }
}

/// Is absolute point (x,y) inside the resolved clip shape?
pub fn clip_covers(c: &ClipAbs, x: f32, y: f32) -> bool {
    match c {
        ClipAbs::Rounded { x: rx, y: ry, w, h, r } => {
            if x < *rx || x >= rx + w || y < *ry || y >= ry + h {
                return false;
            }
            if *r <= 0.0 {
                return true;
            }
            let cx = x.clamp(*rx + r, rx + w - r);
            let cy = y.clamp(*ry + r, ry + h - r);
            (x - cx) * (x - cx) + (y - cy) * (y - cy) <= r * r
        }
        ClipAbs::Ellipse { cx, cy, rx, ry } => {
            if *rx <= 0.0 || *ry <= 0.0 {
                return false;
            }
            let dx = (x - cx) / rx;
            let dy = (y - cy) / ry;
            dx * dx + dy * dy <= 1.0
        }
        ClipAbs::Polygon(pts) => {
            let mut inside = false;
            let n = pts.len();
            for i in 0..n {
                let (x1, y1) = pts[i];
                let (x2, y2) = pts[(i + 1) % n];
                if (y1 > y) != (y2 > y) && x < (x2 - x1) * (y - y1) / (y2 - y1) + x1 {
                    inside = !inside;
                }
            }
            inside
        }
    }
}

// ---------- blend modes (CSS Compositing & Blending L1) ----------

fn lum(c: (f32, f32, f32)) -> f32 {
    0.3 * c.0 + 0.59 * c.1 + 0.11 * c.2
}
fn sat(c: (f32, f32, f32)) -> f32 {
    c.0.max(c.1).max(c.2) - c.0.min(c.1).min(c.2)
}
fn clip_color(c: (f32, f32, f32)) -> (f32, f32, f32) {
    let l = lum(c);
    let n = c.0.min(c.1).min(c.2);
    let x = c.0.max(c.1).max(c.2);
    let mut c = c;
    if n < 0.0 {
        c = (
            l + (c.0 - l) * l / (l - n),
            l + (c.1 - l) * l / (l - n),
            l + (c.2 - l) * l / (l - n),
        );
    }
    if x > 1.0 {
        c = (
            l + (c.0 - l) * (1.0 - l) / (x - l),
            l + (c.1 - l) * (1.0 - l) / (x - l),
            l + (c.2 - l) * (1.0 - l) / (x - l),
        );
    }
    c
}
fn set_lum(c: (f32, f32, f32), l: f32) -> (f32, f32, f32) {
    let d = l - lum(c);
    clip_color((c.0 + d, c.1 + d, c.2 + d))
}
fn set_sat(c: (f32, f32, f32), s: f32) -> (f32, f32, f32) {
    let (mn, mx) = (c.0.min(c.1).min(c.2), c.0.max(c.1).max(c.2));
    if mx <= mn {
        return (0.0, 0.0, 0.0);
    }
    // spec: min components → 0, max components → s, mid scales
    let f = |x: f32| {
        if x == mn {
            0.0
        } else if x == mx {
            s
        } else {
            (x - mn) * s / (mx - mn)
        }
    };
    (f(c.0), f(c.1), f(c.2))
}

/// Blend `s` (source) over `d` (backdrop) — 0..1 sRGB channels.
pub fn blend_rgb(mode: BlendMode, s: (f32, f32, f32), d: (f32, f32, f32)) -> (f32, f32, f32) {
    let f = |s: f32, d: f32| -> f32 {
        match mode {
            BlendMode::Multiply => s * d,
            BlendMode::Screen => s + d - s * d,
            BlendMode::Overlay => {
                if d <= 0.5 {
                    2.0 * s * d
                } else {
                    1.0 - 2.0 * (1.0 - s) * (1.0 - d)
                }
            }
            BlendMode::Darken => s.min(d),
            BlendMode::Lighten => s.max(d),
            BlendMode::ColorDodge => {
                if s >= 1.0 {
                    1.0
                } else {
                    (d / (1.0 - s)).min(1.0)
                }
            }
            BlendMode::ColorBurn => {
                if s <= 0.0 {
                    0.0
                } else {
                    1.0 - ((1.0 - d) / s).min(1.0)
                }
            }
            BlendMode::HardLight => {
                if s <= 0.5 {
                    2.0 * s * d
                } else {
                    1.0 - 2.0 * (1.0 - s) * (1.0 - d)
                }
            }
            BlendMode::SoftLight => {
                if s <= 0.5 {
                    d - (1.0 - 2.0 * s) * d * (1.0 - d)
                } else {
                    let dd = if d <= 0.25 {
                        ((16.0 * d - 12.0) * d + 4.0) * d
                    } else {
                        d.sqrt()
                    };
                    d + (2.0 * s - 1.0) * (dd - d)
                }
            }
            BlendMode::Difference => (d - s).abs(),
            BlendMode::Exclusion => d + s - 2.0 * d * s,
            _ => s, // Normal + non-separable handled below
        }
    };
    match mode {
        BlendMode::Hue => set_lum(set_sat(s, sat(d)), lum(d)),
        BlendMode::Saturation => set_lum(set_sat(d, sat(s)), lum(d)),
        BlendMode::Color => set_lum(s, lum(d)),
        BlendMode::Luminosity => set_lum(d, lum(s)),
        _ => (
            f(s.0, d.0).clamp(0.0, 1.0),
            f(s.1, d.1).clamp(0.0, 1.0),
            f(s.2, d.2).clamp(0.0, 1.0),
        ),
    }
}

// ---------- color-mix in non-srgb spaces ----------

/// `color-mix(in <space>, c1 w1, c2 w2)` for spaces other than plain
/// `srgb` (which keeps the fast path in `parse_color_ctx`). Weights are
/// the caller's normalized percentages; a missing weight drains alpha.
pub fn color_mix(space: &str, ca: Color, cb: Color, wa: f32, wb: f32) -> Option<Color> {
    let polar = matches!(space, "lch" | "oklch" | "hsl" | "hwb");
    let (aa, ab) = (
        alpha(ca) as f32 / 255.0 * wa,
        alpha(cb) as f32 / 255.0 * wb,
    );
    let oa = aa + ab;
    if oa <= 0.0 {
        return Some(bgra_a(0, 0, 0, 0));
    }
    // arm → (x, y, z) in the interpolation space; polar spaces put hue
    // last so the same mixing code fits all spaces
    let coords = |c: Color| -> (f32, f32, f32) {
        let (r, g, b) = (
            ch(c, 16) as f32 / 255.0,
            ch(c, 8) as f32 / 255.0,
            ch(c, 0) as f32 / 255.0,
        );
        let (lr, lg, lb) = (srgb_un(r), srgb_un(g), srgb_un(b));
        match space {
            "srgb-linear" => (lr, lg, lb),
            "lab" | "lch" => {
                let (l, a, bb) = lin_lab(lr, lg, lb);
                if space == "lab" {
                    (l, a, bb)
                } else {
                    (l, (a * a + bb * bb).sqrt(), bb.atan2(a).to_degrees())
                }
            }
            "oklab" | "oklch" => {
                let (l, a, bb) = lin_oklab(lr, lg, lb);
                if space == "oklab" {
                    (l, a, bb)
                } else {
                    (l, (a * a + bb * bb).sqrt(), bb.atan2(a).to_degrees())
                }
            }
            "hsl" => {
                let (h, s, l) = rgb_hsl(r, g, b);
                (s, l, h)
            }
            "hwb" => {
                let (h, _, _) = rgb_hsl(r, g, b);
                (r.min(g).min(b), 1.0 - r.max(g).max(b), h)
            }
            _ => (r, g, b),
        }
    };
    let (x1, y1, z1) = coords(ca);
    let (x2, y2, z2) = coords(cb);
    let mix2 = |a1: f32, a2: f32| (a1 * aa + a2 * ab) / oa;
    let (x, y) = (mix2(x1, x2), mix2(y1, y2));
    let z = if polar {
        // shorter-arc hue interpolation, weighted by each color's mass
        let d = (z2 - z1 + 540.0).rem_euclid(360.0) - 180.0;
        z1 + d * (ab / oa)
    } else {
        mix2(z1, z2)
    };
    match space {
        "srgb-linear" => Some(fn_color(x, y, z, oa)),
        "lab" => {
            let (r, g, b) = lab_rgb(x, y, z);
            Some(fn_color(r, g, b, oa))
        }
        "lch" => {
            let h = z.to_radians();
            let (r, g, b) = lab_rgb(x, y * h.cos(), y * h.sin());
            Some(fn_color(r, g, b, oa))
        }
        "oklab" => {
            let (r, g, b) = oklab_rgb(x, y, z);
            Some(fn_color(r, g, b, oa))
        }
        "oklch" => {
            let h = z.to_radians();
            let (r, g, b) = oklab_rgb(x, y * h.cos(), y * h.sin());
            Some(fn_color(r, g, b, oa))
        }
        "hsl" => {
            let (r, g, b) = hsl_rgb(z, x, y);
            Some(fn_color_srgb(r, g, b, oa))
        }
        "hwb" => {
            let (r, g, b) = hwb_rgb(z, x, y);
            Some(fn_color_srgb(r, g, b, oa))
        }
        _ => Some(fn_color_srgb(x, y, z, oa)),
    }
}

/// linear sRGB → CIE Lab (D50 white).
fn lin_lab(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    // lin sRGB → XYZ (D50), CSS Color 4 combined matrix
    let x = 0.4360657428 * r + 0.3851514688 * g + 0.1430784544 * b;
    let y = 0.2224931918 * r + 0.7168870538 * g + 0.0606197905 * b;
    let z = 0.0139239045 * r + 0.0970812857 * g + 0.7140993580 * b;
    const E: f32 = 216.0 / 24389.0;
    const K: f32 = 24389.0 / 27.0;
    let f = |t: f32| {
        if t > E {
            t.cbrt()
        } else {
            (K * t + 16.0) / 116.0
        }
    };
    let (fx, fy, fz) = (f(x / 0.96422), f(y), f(z / 0.82521));
    (116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz))
}

/// linear sRGB → Oklab.
fn lin_oklab(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let l = (0.8189330101 * r + 0.3618667424 * g - 0.1288597137 * b).cbrt();
    let m = (0.0329845436 * r + 0.9293118715 * g + 0.0361456387 * b).cbrt();
    let s = (0.0482003018 * r + 0.2643662691 * g + 0.6338517070 * b).cbrt();
    (
        0.2104542553 * l + 0.7936177850 * m - 0.0040720468 * s,
        1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s,
        0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s,
    )
}

/// sRGB 0..1 → (h°, s, l).
fn rgb_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let (mx, mn) = (r.max(g).max(b), r.min(g).min(b));
    let l = (mx + mn) / 2.0;
    let d = mx - mn;
    if d <= 0.0 {
        return (0.0, 0.0, l);
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs());
    let h = if mx == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if mx == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (h.rem_euclid(360.0), s.clamp(0.0, 1.0), l)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bgra, parse_color};

    #[test]
    fn filter_spec_parse() {
        let ops = parse_filter(
            "blur(4px) brightness(1.2) grayscale(50%) drop-shadow(2px 3px 4px rgb(1,2,3))",
            16.0,
            bgra(0, 0, 0),
        );
        assert_eq!(ops.len(), 4);
        assert!(matches!(ops[0], FilterOp::Blur(b) if (b - 4.0).abs() < 1e-4));
        match &ops[3] {
            FilterOp::DropShadow { dx, dy, blur, color } => {
                assert_eq!((*dx, *dy, *blur), (2.0, 3.0, 4.0));
                assert_eq!(*color, bgra(1, 2, 3));
            }
            _ => panic!("expected drop-shadow"),
        }
        assert!(parse_filter("none", 16.0, 0).is_empty());
        // grayscale() no-arg = 100%
        match &parse_filter("grayscale()", 16.0, 0)[0] {
            FilterOp::Matrix(m) => assert!((m[0] - 0.213).abs() < 1e-3),
            _ => panic!(),
        }
        // color before lengths parses too
        match &parse_filter("drop-shadow(red 1px 1px)", 16.0, 0)[0] {
            FilterOp::DropShadow { color, .. } => assert_eq!(*color, bgra(255, 0, 0)),
            _ => panic!(),
        }
    }

    #[test]
    fn clip_path_shapes() {
        match parse_clip_path("inset(10px 20px round 5px)", 16.0).unwrap() {
            ClipShape::Inset { t, r, b, l, radius } => {
                assert_eq!(
                    (t.px, r.px, b.px, l.px, radius.px),
                    (10.0, 20.0, 10.0, 20.0, 5.0)
                );
            }
            _ => panic!(),
        }
        match parse_clip_path("circle(40% at 25% 75%)", 16.0).unwrap() {
            ClipShape::Circle { r, cx, cy } => {
                assert!((r.unwrap().frac - 0.4).abs() < 1e-4);
                assert!((cx.frac - 0.25).abs() < 1e-4);
                assert!((cy.frac - 0.75).abs() < 1e-4);
            }
            _ => panic!(),
        }
        match parse_clip_path("ellipse(at center)", 16.0).unwrap() {
            ClipShape::Ellipse { rx, ry, cx, .. } => {
                assert!(rx.is_none() && ry.is_none());
                assert!((cx.frac - 0.5).abs() < 1e-4);
            }
            _ => panic!(),
        }
        assert!(matches!(
            parse_clip_path("polygon(0 0, 100% 0, 50% 100%)", 16.0).unwrap(),
            ClipShape::Polygon(ref p) if p.len() == 3
        ));
        assert!(parse_clip_path("none", 16.0).is_none());
        assert!(parse_clip_path("url(#c)", 16.0).is_none());
    }

    #[test]
    fn clip_cover_points() {
        let s = resolve_clip(
            &ClipShape::Circle {
                r: Some(CLen { frac: 0.5, px: 0.0 }),
                cx: CLen { frac: 0.5, px: 0.0 },
                cy: CLen { frac: 0.5, px: 0.0 },
            },
            [0.0, 0.0, 100.0, 100.0],
        );
        // 50% r resolves vs sqrt(20000)/√2 = 100 → inscribed disc
        assert!(clip_covers(&s, 50.0, 50.0));
        assert!(clip_covers(&s, 90.0, 50.0));
        assert!(!clip_covers(&s, 5.0, 5.0));
        let s = resolve_clip(
            &parse_clip_path("inset(10px)", 16.0).unwrap(),
            [0.0, 0.0, 100.0, 100.0],
        );
        assert!(clip_covers(&s, 50.0, 50.0));
        assert!(!clip_covers(&s, 5.0, 50.0));
        let s = resolve_clip(
            &parse_clip_path("polygon(0 0, 100px 0, 0 100px)", 16.0).unwrap(),
            [0.0, 0.0, 100.0, 100.0],
        );
        assert!(clip_covers(&s, 10.0, 10.0));
        assert!(!clip_covers(&s, 90.0, 90.0));
    }

    #[test]
    fn blend_modes_math() {
        let (r, g, b) = blend_rgb(BlendMode::Multiply, (1.0, 0.0, 0.0), (0.5, 0.5, 0.5));
        assert!((r - 0.5).abs() < 1e-4 && g == 0.0 && b == 0.0);
        let (v, _, _) = blend_rgb(BlendMode::Screen, (0.5, 0.0, 0.0), (0.5, 0.0, 0.0));
        assert!((v - 0.75).abs() < 1e-4);
        let (v, _, _) = blend_rgb(BlendMode::Difference, (0.4, 0.0, 0.0), (0.4, 0.0, 0.0));
        assert!(v.abs() < 1e-4);
        // hue keeps backdrop luminance; luminosity keeps source luminance
        let out = blend_rgb(BlendMode::Hue, (1.0, 0.0, 0.0), (0.2, 0.4, 0.8));
        assert!((lum(out) - lum((0.2, 0.4, 0.8))).abs() < 0.02);
        let out = blend_rgb(BlendMode::Luminosity, (0.9, 0.9, 0.9), (0.2, 0.4, 0.8));
        assert!((lum(out) - 0.9).abs() < 0.02);
    }

    #[test]
    fn modern_color_fns() {
        use crate::{ch, parse_color_ctx};
        assert_eq!(parse_color("hsl(0,100%,50%)"), Some(bgra(255, 0, 0)));
        assert_eq!(parse_color("hsl(0.5turn 100% 50%)"), Some(bgra(0, 255, 255)));
        assert_eq!(
            parse_color("hsl(2.0943951rad,100%,50%)"),
            Some(bgra(0, 255, 0))
        );
        assert_eq!(parse_color("hwb(240 0% 0%)"), Some(bgra(0, 0, 255)));
        assert_eq!(parse_color("color(srgb 1 0 0)"), Some(bgra(255, 0, 0)));
        assert_eq!(
            parse_color("color(srgb 1 0 0 / 50%)").map(|c| ch(c, 24)),
            Some(128)
        );
        // xyz mid-tone (hand-computed)
        let c = parse_color("color(xyz 0.25 0.4 0.1)").unwrap();
        assert!((ch(c, 16) as i32 - 106).abs() <= 2);
        assert!((ch(c, 8) as i32 - 190).abs() <= 2);
        assert!((ch(c, 0) as i32 - 55).abs() <= 2);
        assert_eq!(parse_color("light-dark(red, blue)"), Some(bgra(255, 0, 0)));
        assert_eq!(
            parse_color_ctx("currentColor", Some(bgra(1, 2, 3))),
            Some(bgra(1, 2, 3))
        );
        assert_eq!(parse_color("currentcolor"), None);
        // srgb mix unchanged; non-srgb spaces go through color_mix
        assert_eq!(
            parse_color("color-mix(in srgb, red 50%, blue 50%)"),
            Some(bgra(128, 0, 128))
        );
        let c = parse_color("color-mix(in oklch, red, blue)").unwrap();
        assert!(ch(c, 16) > 120 && ch(c, 0) > 120); // vivid magenta-ish
        let c2 = parse_color("color-mix(in lab, white, black)").unwrap();
        // lab midpoint of black/white ≈ mid grey
        assert!((ch(c2, 16) as i32 - ch(c2, 8) as i32).abs() <= 2);
        assert!(ch(c2, 16) > 90 && ch(c2, 16) < 170);
        // named audit spot checks
        assert_eq!(parse_color("aliceblue"), Some(bgra(240, 248, 255)));
        assert_eq!(parse_color("mediumvioletred"), Some(bgra(199, 21, 133)));
        assert_eq!(parse_color("darkgray"), Some(bgra(169, 169, 169)));
    }
}
