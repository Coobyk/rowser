//! borderfx: border styles, corner radii, outlines, box-shadows and
//! overflow clipping — value parsing + display-item emission shared by
//! the block/float/inline-block/grid/table box paths.

use crate::{
    alpha, over, parse_color, parse_color_ctx, parse_len, BorderEdge, BorderStyle, Color, DisplayItem, Props,
    BLACK,
};

/// Whitespace-split keeping `fn(...)` groups whole (`rgb(0 0 0)`).
fn ws_tokens(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut st = 0usize;
    let b = s.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'(' => depth += 1,
            b')' => depth -= 1,
            c if c.is_ascii_whitespace() && depth <= 0 => {
                if st < i {
                    out.push(&s[st..i]);
                }
                st = i + 1;
            }
            _ => {}
        }
    }
    if st < b.len() {
        out.push(&s[st..]);
    }
    out
}

/// A `border-*-style`/`outline-style` keyword.
fn style_kw(t: &str) -> Option<BorderStyle> {
    Some(match t {
        "solid" => BorderStyle::Solid,
        "dotted" => BorderStyle::Dotted,
        "dashed" => BorderStyle::Dashed,
        "double" => BorderStyle::Double,
        "groove" => BorderStyle::Groove,
        "ridge" => BorderStyle::Ridge,
        "inset" => BorderStyle::Inset,
        "outset" => BorderStyle::Outset,
        _ => return None,
    })
}

/// Border/outline width: a length or `thin|medium|thick`.
fn width_val(t: &str, em: f32) -> Option<f32> {
    match t.trim() {
        "thin" => Some(1.0),
        "medium" => Some(3.0),
        "thick" => Some(6.0),
        other => parse_len(other, 16.0, em),
    }
}

/// Components of a `border`/`border-<side>` shorthand — or of a longhand
/// carrying the whole shorthand token list after `each_decl` expansion
/// (callers pick their own component). `none` = `none`/`hidden` seen.
#[derive(Default)]
pub struct BorderParts {
    pub none: bool,
    pub w: Option<f32>,
    pub style: Option<BorderStyle>,
    pub color: Option<String>,
}

pub fn border_parts(v: &str, em: f32) -> BorderParts {
    let mut out = BorderParts::default();
    let toks = ws_tokens(v);
    for (i, t) in toks.iter().enumerate() {
        let t = t.trim().to_lowercase();
        if t.is_empty() {
            continue;
        }
        if t == "none" || t == "hidden" {
            out.none = true;
        } else if let Some(s) = style_kw(&t) {
            out.style = Some(s);
        } else if let Some(w) = width_val(&t, em) {
            if out.w.is_none() {
                out.w = Some(w);
            }
        } else {
            out.color = Some(toks[i..].join(" "));
            break;
        }
    }
    out
}

fn parts_color(p: &BorderParts, cur: Color) -> Option<Color> {
    p.color
        .as_deref()
        .and_then(|s| {
            parse_color_ctx(s, Some(cur))
                .or_else(|| s.split_whitespace().find_map(|t| parse_color_ctx(t, Some(cur))))
        })
}

/// Per-side border widths/styles/colors — [top, right, bottom, left].
#[derive(Clone, Copy)]
pub struct Borders {
    pub w: [f32; 4],
    pub style: [BorderStyle; 4],
    pub color: [Color; 4],
}

/// Merge `border`, `border-<side>` shorthands and the per-component
/// longhands. A side with no explicit style reads `solid` (matches the
/// old lenient `border:2px` → painted edge behavior); `none`/`hidden`
/// zeroes that side's width.
pub fn borders(c: &Props, size: f32, cur: Color) -> Option<Borders> {
    let mut b = Borders {
        w: [0.0; 4],
        style: [BorderStyle::Solid; 4],
        color: [BLACK; 4],
    };
    if let Some(v) = c.get("border") {
        let p = border_parts(v, size);
        if !p.none {
            if let Some(w) = p.w {
                b.w = [w; 4];
            }
            if let Some(s) = p.style {
                b.style = [s; 4];
            }
            if let Some(col) = parts_color(&p, cur) {
                b.color = [col; 4];
            }
        }
    }
    const SIDES: [(&str, &str, &str); 4] = [
        ("border-top-width", "border-top-style", "border-top-color"),
        ("border-right-width", "border-right-style", "border-right-color"),
        ("border-bottom-width", "border-bottom-style", "border-bottom-color"),
        ("border-left-width", "border-left-style", "border-left-color"),
    ];
    for (i, (wn, sn, cn)) in SIDES.iter().copied().enumerate() {
        if let Some(v) = c.get(wn) {
            if let Some(w) = width_val(v, size) {
                b.w[i] = w;
            } else {
                let p = border_parts(v, size);
                if p.none {
                    b.w[i] = 0.0;
                } else if let Some(w) = p.w {
                    b.w[i] = w;
                }
            }
        }
        if let Some(v) = c.get(sn) {
            let t = v.trim();
            if t == "none" || t == "hidden" {
                b.w[i] = 0.0;
            } else if let Some(s) = style_kw(t) {
                b.style[i] = s;
            } else {
                let p = border_parts(v, size);
                if p.none {
                    b.w[i] = 0.0;
                } else if let Some(s) = p.style {
                    b.style[i] = s;
                }
            }
        }
        if let Some(v) = c.get(cn) {
            if let Some(col) = parse_color_ctx(v, Some(cur)) {
                b.color[i] = col;
            } else if let Some(col) = parts_color(&border_parts(v, size), cur) {
                b.color[i] = col;
            }
        }
    }
    if b.w.iter().all(|&w| w <= 0.0) {
        return None;
    }
    Some(b)
}

// ---------- corner radii ----------

/// Per-corner radius spec in raw tokens — resolved against the final box
/// size once known (percentages → of each axis). Corners are
/// [top-left, top-right, bottom-right, bottom-left], each `(h, v)`.
#[derive(Clone, Default)]
pub struct RadiusSpec(pub [Option<(String, String)>; 4]);

/// The i-th of a 1–4 value list in the usual corner pattern (tl, tr, br, bl).
fn nth4<'a>(t: &[&'a str], i: usize) -> &'a str {
    match t.len() {
        1 => t[0],
        2 => t[i % 2],
        3 => t[[0, 1, 2, 1][i]],
        _ => t[i.min(3)],
    }
}

/// `border-radius` shorthand → corner spec (the `each_decl` expansion
/// already emits corner longhands; this is the fallback for Props that
/// didn't pass through it).
fn split_radius(v: &str) -> [Option<(String, String)>; 4] {
    let (h, vert) = match v.find('/') {
        Some(i) => (&v[..i], &v[i + 1..]),
        None => (v, ""),
    };
    let hs = ws_tokens(h);
    let vs = if vert.trim().is_empty() {
        hs.clone()
    } else {
        ws_tokens(vert)
    };
    let mut out: [Option<(String, String)>; 4] = Default::default();
    if hs.is_empty() || vs.is_empty() {
        return out;
    }
    for i in 0..4 {
        out[i] = Some((nth4(&hs, i).to_string(), nth4(&vs, i).to_string()));
    }
    out
}

pub fn radius_spec(c: &Props) -> RadiusSpec {
    const CORNERS: [&str; 4] = [
        "border-top-left-radius",
        "border-top-right-radius",
        "border-bottom-right-radius",
        "border-bottom-left-radius",
    ];
    let mut out = RadiusSpec::default();
    for (i, k) in CORNERS.iter().enumerate() {
        if let Some(v) = c.get(*k) {
            let t = ws_tokens(v);
            if let Some(h) = t.first() {
                out.0[i] = Some((
                    h.to_string(),
                    t.get(1).map(|s| s.to_string()).unwrap_or_else(|| h.to_string()),
                ));
            }
        }
    }
    if out.none() {
        if let Some(v) = c.get("border-radius") {
            out.0 = split_radius(v);
        }
    }
    out
}

/// Resolved corner radii — [tl, tr, br, bl] each `(rx, ry)`.
#[derive(Clone, Copy, Default)]
pub struct Radii(pub [[f32; 2]; 4]);

impl Radii {
    pub fn any(&self) -> bool {
        self.0.iter().any(|r| r[0] > 0.0 || r[1] > 0.0)
    }
    /// Uniform circular radius when every corner matches (fast path).
    pub fn uniform(&self) -> Option<f32> {
        let [rx, ry] = self.0[0];
        if self.0[1..]
            .iter()
            .all(|r| (r[0] - rx).abs() < 0.01 && (r[1] - ry).abs() < 0.01)
            && (rx - ry).abs() < 0.01
        {
            Some(rx)
        } else {
            None
        }
    }
    /// Scale all corners so adjacent radii never overshoot a shared edge.
    pub fn fit(&mut self, w: f32, h: f32) {
        let mut f = 1.0f32;
        for (a, b, len, ax) in [(0, 1, w, 0), (2, 3, w, 0), (0, 3, h, 1), (1, 2, h, 1)] {
            let s = self.0[a][ax] + self.0[b][ax];
            if s > len && s > 0.0 {
                f = f.min(len / s);
            }
        }
        if f < 1.0 {
            for c in self.0.iter_mut() {
                c[0] *= f;
                c[1] *= f;
            }
        }
    }
    /// Radii of an inner edge (padding box): shrink each corner's rx by
    /// the adjacent left/right inset and ry by the top/bottom one.
    /// `sides` = [top, right, bottom, left].
    pub fn inset(&self, sides: [f32; 4]) -> Radii {
        let [t, r, b, l] = sides;
        let mut out = *self;
        for (i, (sx, sy)) in [(l, t), (r, t), (r, b), (l, b)].iter().copied().enumerate() {
            out.0[i][0] = (out.0[i][0] - sx).max(0.0);
            out.0[i][1] = (out.0[i][1] - sy).max(0.0);
        }
        out
    }
}

impl RadiusSpec {
    pub fn none(&self) -> bool {
        self.0.iter().all(|c| c.is_none())
    }
    /// Resolve against the final border-box size (w, h) and em base.
    pub fn resolve(&self, w: f32, h: f32, em: f32) -> Radii {
        let mut out = Radii::default();
        for (i, c) in self.0.iter().enumerate() {
            if let Some((hs, vs)) = c {
                out.0[i][0] = parse_len(hs, w, em).unwrap_or(0.0).max(0.0);
                out.0[i][1] = parse_len(vs, h, em).unwrap_or(0.0).max(0.0);
            }
        }
        out.fit(w, h);
        out
    }
}

// ---------- box-shadow ----------

/// One `box-shadow` term: `[inset] <x> <y> [blur] [spread] [color]`.
#[derive(Clone)]
pub struct BoxShadow {
    pub dx: f32,
    pub dy: f32,
    pub blur: f32,
    pub spread: f32,
    pub color: Color,
    pub inset: bool,
}

/// Full `box-shadow` grammar — comma-separated list, `inset` keyword,
/// optional blur/spread lengths, color last (may contain spaces via
/// parens). `bd` composites translucent colors over the backdrop; `def`
/// stands in for currentcolor.
pub fn parse_shadows(c: &Props, size: f32, bd: Color, def: Color) -> Vec<BoxShadow> {
    let mut out = Vec::new();
    let Some(v) = c.get("box-shadow") else {
        return out;
    };
    if v.trim() == "none" {
        return out;
    }
    let mut depth = 0i32;
    for sh in v.split(|ch| {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        ch == ',' && depth <= 0
    }) {
        // tokens: lengths first, then a (possibly space-containing) color
        let mut nums = Vec::new();
        let mut col_s = "";
        let mut inset = false;
        let b = sh.as_bytes();
        let mut i = 0;
        while i < b.len() {
            while i < b.len() && (b[i] as char).is_whitespace() {
                i += 1;
            }
            let st = i;
            let mut d = 0i32;
            while i < b.len() && (d > 0 || !(b[i] as char).is_whitespace()) {
                match b[i] {
                    b'(' => d += 1,
                    b')' => d -= 1,
                    _ => {}
                }
                i += 1;
            }
            let tok = &sh[st..i];
            if tok.is_empty() {
                break;
            }
            if tok == "inset" {
                inset = true;
                continue;
            }
            if let Some(n) = parse_len(tok, size, size) {
                nums.push(n);
            } else {
                col_s = sh[st..].trim();
                break;
            }
        }
        if nums.len() < 2 {
            continue;
        }
        let col = if col_s.is_empty() {
            def
        } else {
            parse_color(col_s).unwrap_or(def)
        };
        if alpha(col) == 0 {
            continue; // fully transparent shadow
        }
        out.push(BoxShadow {
            dx: nums[0],
            dy: nums[1],
            blur: nums.get(2).copied().unwrap_or(0.0).max(0.0),
            spread: nums.get(3).copied().unwrap_or(0.0),
            color: over(col, bd),
            inset,
        });
    }
    out
}

// ---------- outline ----------

#[derive(Clone)]
pub struct Outline {
    pub w: f32,
    pub style: BorderStyle,
    pub color: Color,
    pub offset: f32,
}

/// `outline` + longhands + `outline-offset`. `auto` → solid medium ring;
/// `invert`/missing color → `def` (currentcolor). Paints outside the
/// border edge; zero layout cost.
pub fn outline(c: &Props, size: f32, bd: Color, def: Color) -> Option<Outline> {
    if c.get("outline")
        .is_some_and(|v| matches!(v.trim(), "none" | "hidden"))
    {
        return None;
    }
    if c.get("outline-style")
        .is_some_and(|v| matches!(v.trim(), "none" | "hidden"))
    {
        return None;
    }
    let mut o = Outline {
        w: 0.0,
        style: BorderStyle::Solid,
        color: def,
        offset: 0.0,
    };
    let (mut found, mut w_set) = (false, false);
    if let Some(v) = c.get("outline-width") {
        let t = v.trim();
        if t == "auto" {
            o.w = 3.0;
            w_set = true;
            found = true;
        } else if let Some(w) = width_val(t, size).or_else(|| border_parts(t, size).w) {
            o.w = w;
            w_set = true;
            found = true;
        }
    }
    if let Some(v) = c.get("outline-style") {
        let t = v.trim();
        if t == "auto" {
            found = true;
        } else if let Some(s) = style_kw(t).or_else(|| border_parts(t, size).style) {
            o.style = s;
            found = true;
        }
    }
    if let Some(v) = c.get("outline-color") {
        let t = v.trim();
        if !matches!(t, "auto" | "invert") {
            if let Some(col) = parse_color_ctx(t, Some(def)).or_else(|| parts_color(&border_parts(t, size), def)) {
                o.color = over(col, bd);
                found = true;
            }
        } else {
            found = true;
        }
    }
    if let Some(v) = c.get("outline-offset") {
        if let Some(n) = parse_len(v, size, size) {
            o.offset = n;
            found = true;
        }
    }
    if !found {
        return None;
    }
    if !w_set {
        o.w = 3.0;
    }
    Some(o)
}

// ---------- overflow ----------

/// `overflow[-x|-y]: hidden|clip|scroll|auto` on any axis → descendants
/// clip to the padding box (`clip` = hidden sans scroll affordance; no
/// scrollbar UI needed for scroll/auto).
pub fn clips_overflow(c: &Props) -> bool {
    ["overflow", "overflow-x", "overflow-y"].iter().any(|k| {
        c.get(*k).is_some_and(|v| {
            v.split_whitespace()
                .any(|t| matches!(t, "hidden" | "clip" | "scroll" | "auto"))
        })
    })
}

/// Padding-box rect (the overflow clip) inside a border box.
/// `bw` = [top, right, bottom, left] border widths.
pub fn padding_box(x: f32, y: f32, w: f32, h: f32, bw: [f32; 4]) -> (f32, f32, f32, f32) {
    (
        x + bw[3],
        y + bw[0],
        (w - bw[3] - bw[1]).max(0.0),
        (h - bw[0] - bw[2]).max(0.0),
    )
}

// ---------- display-item emission ----------

/// bg fill honoring per-corner radii — uniform circular corners keep the
/// RoundedRect fast path (and the existing item shape).
pub fn bg_item(x: f32, y: f32, w: f32, h: f32, color: Color, r: &Radii) -> DisplayItem {
    if let Some(u) = r.uniform() {
        if u > 0.5 {
            return DisplayItem::RoundedRect { x, y, w, h, radius: u, color };
        }
    } else if r.any() {
        return DisplayItem::RoundedRect2 { x, y, w, h, radii: r.0, color };
    }
    DisplayItem::FillRect { x, y, w, h, color }
}

/// One item per non-zero border side. `Solid` keeps the FillRect edge
/// (cheap, and consumers scanning for bg/border rects still see them);
/// the fancier styles carry side+style for the painter.
pub fn emit_borders(out: &mut Vec<DisplayItem>, x: f32, y: f32, w: f32, h: f32, b: &Borders) {
    let e = [
        (x, y, w, b.w[0], BorderEdge::Top),
        (x + w - b.w[1], y, b.w[1], h, BorderEdge::Right),
        (x, y + h - b.w[2], w, b.w[2], BorderEdge::Bottom),
        (x, y, b.w[3], h, BorderEdge::Left),
    ];
    for (ex, ey, ew, eh, side) in e {
        if ew <= 0.0 || eh <= 0.0 {
            continue;
        }
        let i = match side {
            BorderEdge::Top => 0,
            BorderEdge::Right => 1,
            BorderEdge::Bottom => 2,
            BorderEdge::Left => 3,
        };
        if b.w[i] <= 0.0 {
            continue;
        }
        if b.style[i] == BorderStyle::Solid {
            out.push(DisplayItem::FillRect { x: ex, y: ey, w: ew, h: eh, color: b.color[i] });
        } else {
            out.push(DisplayItem::BorderSide {
                x: ex,
                y: ey,
                w: ew,
                h: eh,
                side,
                style: b.style[i],
                color: b.color[i],
            });
        }
    }
}

/// A `DisplayItem::Shadow` for one parsed box-shadow term.
pub fn shadow_item(s: &BoxShadow, x: f32, y: f32, w: f32, h: f32, r: &Radii) -> DisplayItem {
    DisplayItem::Shadow {
        x,
        y,
        w,
        h,
        dx: s.dx,
        dy: s.dy,
        blur: s.blur,
        spread: s.spread,
        radii: r.0,
        color: s.color,
        inset: s.inset,
    }
}

/// An outline ring `offset`+`w` outside the border box — four edge items.
pub fn emit_outline(out: &mut Vec<DisplayItem>, x: f32, y: f32, w: f32, h: f32, o: &Outline) {
    if o.w <= 0.0 {
        return;
    }
    let d = o.offset + o.w;
    let (ox, oy, ow, oh) = (x - d, y - d, w + 2.0 * d, h + 2.0 * d);
    if ow <= 0.0 || oh <= 0.0 {
        return;
    }
    for (ex, ey, ew, eh, side) in [
        (ox, oy, ow, o.w, BorderEdge::Top),
        (ox + ow - o.w, oy, o.w, oh, BorderEdge::Right),
        (ox, oy + oh - o.w, ow, o.w, BorderEdge::Bottom),
        (ox, oy, o.w, oh, BorderEdge::Left),
    ] {
        if o.style == BorderStyle::Solid {
            out.push(DisplayItem::FillRect { x: ex, y: ey, w: ew, h: eh, color: o.color });
        } else {
            out.push(DisplayItem::BorderSide {
                x: ex,
                y: ey,
                w: ew,
                h: eh,
                side,
                style: o.style,
                color: o.color,
            });
        }
    }
}

/// Full box chrome in one pass: outer shadows (reversed — the first CSS
/// shadow is topmost) → bg → `layers` (bg-image/gradient items) → inset
/// shadows → borders → [clip →] `inner` [→ pop] → outline. `inner` holds
/// the box's laid children in the same coordinate space (grid/table/
/// inline-block payloads).
#[allow(clippy::too_many_arguments)]
pub fn emit_box(
    out: &mut Vec<DisplayItem>,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    bg: Option<Color>,
    border: Option<&Borders>,
    radii: &Radii,
    shadows: &[BoxShadow],
    clip: bool,
    outline: Option<&Outline>,
    layers: Vec<DisplayItem>,
    inner: Vec<DisplayItem>,
) {
    for s in shadows.iter().filter(|s| !s.inset).rev() {
        out.push(shadow_item(s, x, y, w, h, radii));
    }
    if let Some(col) = bg {
        out.push(bg_item(x, y, w, h, col, radii));
    }
    out.extend(layers);
    for s in shadows.iter().filter(|s| s.inset).rev() {
        out.push(shadow_item(s, x, y, w, h, radii));
    }
    if let Some(b) = border {
        emit_borders(out, x, y, w, h, b);
    }
    if clip {
        let bw = border.map(|b| b.w).unwrap_or([0.0; 4]);
        let (cx, cy, cw, ch) = padding_box(x, y, w, h, bw);
        let pr = radii.inset(bw);
        out.push(DisplayItem::PushClip { x: cx, y: cy, w: cw, h: ch, radii: pr.0 });
    }
    out.extend(inner);
    if clip {
        out.push(DisplayItem::PopClip);
    }
    if let Some(o) = outline {
        emit_outline(out, x, y, w, h, o);
    }
}
