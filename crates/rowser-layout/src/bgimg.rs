//! `background-image` layers + CSS gradients.
//!
//! `parse_box` folds an element's `background-*` longhands (produced by the
//! `background` shorthand expansion in rowser-css, or set directly) into
//! per-layer specs. `items_for` then turns them into display items for a
//! concrete border box: `url()` layers become `DisplayItem::BgImage` whose
//! tile geometry resolves in paint once the decoded bitmap size is known,
//! while gradient layers rasterize straight into `DisplayItem::Raster`
//! tiles covering the clip area.

use crate::{alpha, bgra_a, ch, fhue, parse_color, parse_len_vp, top_args, DisplayItem, Props};

/// One `background-image` layer with its per-layer sub-properties merged.
pub(crate) struct Layer {
    img: Img,
    /// tile offset per axis = f*(pa - tile) + a (CSS <position> semantics:
    /// percentage aligns that point of the tile with the same point of the
    /// positioning area)
    pos: [(f32, f32); 2],
    /// background-size per axis (mode == 0)
    size: (Sz, Sz),
    /// 0 = sized tiles, 1 = cover, 2 = contain
    mode: u8,
    /// repeat per axis: 0 none, 1 repeat, 2 space (≈repeat), 3 round
    rep: (u8, u8),
    /// background-origin / background-clip: 0 border, 1 padding, 2 content
    origin: u8,
    clip: u8,
    /// background-attachment: fixed pins the positioning area to the viewport
    fixed: bool,
}

enum Img {
    Url(String),
    Grad(Box<Grad>),
    None,
}

#[derive(Clone, Copy)]
enum Sz {
    Auto,
    Px(f32),
    /// fraction of the positioning-area axis
    Pct(f32),
}

/// Parsed layers + the box metrics `items_for` needs to map
/// origin/clip keywords to rects.
pub(crate) struct BoxBgs {
    layers: Vec<Layer>,
    /// padding widths [t,r,b,l]
    pad: [f32; 4],
    /// border widths [t,r,b,l]
    brd: [f32; 4],
}

/// Does the computed style carry a paintable background image layer?
pub(crate) fn has_layers(c: &Props) -> bool {
    if let Some(v) = c.get("background-image") {
        if top_args(v.trim())
            .iter()
            .any(|s| !s.trim().is_empty() && s.trim() != "none")
        {
            return true;
        }
    }
    c.get("background")
        .is_some_and(|v| v.contains("url(") || v.contains("gradient("))
}

/// Parse the element's `background-*` longhands into a `BoxBgs`; `pad`/`brd`
/// are the element's resolved padding/border edge widths [t,r,b,l] (already
/// computed by the caller for layout), `em` the used font size and `current`
/// the used `color` (for `currentcolor` stops).
pub(crate) fn parse_box(
    c: &Props,
    pad: [f32; 4],
    brd: [f32; 4],
    em: f32,
    vh: f32,
    vw: f32,
    current: u32,
) -> Option<BoxBgs> {
    let layers = parse_layers(c, em, vh, vw, current);
    if layers.is_empty() {
        return None;
    }
    Some(BoxBgs { layers, pad, brd })
}

/// Emit display items for the layers, painted back-to-front (last layer
/// first), clipped/positioned against the border-box rect (x,y,w,h).
/// `viewport` supplies the fixed-attachment positioning area.
pub(crate) fn items_for(
    b: &BoxBgs,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    viewport: (f32, f32),
) -> Vec<DisplayItem> {
    let mut out = Vec::new();
    if w <= 0.0 || h <= 0.0 {
        return out;
    }
    let mut budget: usize = 24_000_000; // raster pixel budget across layers
    for l in b.layers.iter().rev() {
        let clip = area(x, y, w, h, &b.pad, &b.brd, l.clip);
        let pa = if l.fixed {
            (0.0, 0.0, viewport.0, viewport.1)
        } else {
            area(x, y, w, h, &b.pad, &b.brd, l.origin)
        };
        match &l.img {
            Img::None => {}
            Img::Url(src) => {
                if clip.2 <= 0.0 || clip.3 <= 0.0 {
                    continue;
                }
                let (sw, sh) = if l.mode == 0 {
                    (size_axis(&l.size.0, pa.2), size_axis(&l.size.1, pa.3))
                } else {
                    (-1.0, -1.0)
                };
                out.push(DisplayItem::BgImage {
                    x: clip.0,
                    y: clip.1,
                    w: clip.2,
                    h: clip.3,
                    px: pa.0,
                    py: pa.1,
                    pw: pa.2,
                    ph: pa.3,
                    mode: l.mode,
                    sw,
                    sh,
                    fx: l.pos[0].0,
                    ax: l.pos[0].1,
                    fy: l.pos[1].0,
                    ay: l.pos[1].1,
                    rx: l.rep.0,
                    ry: l.rep.1,
                    src: src.clone(),
                });
            }
            Img::Grad(g) => emit_gradient(&mut out, g, l, clip, pa, &mut budget),
        }
    }
    out
}

/// `object-fit` for replaced elements: returns (mode, x-pos, y-pos) for a
/// `BgImage` item, or `None` when `object-fit:fill` (default) applies and the
/// caller should emit a plain `DisplayItem::Image`.
pub(crate) fn object_fit(
    c: &Props,
    em: f32,
    vh: f32,
    vw: f32,
) -> Option<(u8, (f32, f32), (f32, f32))> {
    let v = c.get("object-fit")?;
    let mode = match v.trim().to_ascii_lowercase().as_str() {
        "cover" => 1u8,
        "contain" => 2,
        "scale-down" => 3,
        "none" => 0,
        _ => return None, // "fill" (default) and invalid values
    };
    let pos = c
        .get("object-position")
        .map(|v| parse_pos(v, em, vh, vw))
        .unwrap_or([(0.5, 0.0), (0.5, 0.0)]);
    Some((mode, pos[0], pos[1]))
}

/// Border/padding/content-box rect for a bg area keyword.
fn area(
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    pad: &[f32; 4],
    brd: &[f32; 4],
    which: u8,
) -> (f32, f32, f32, f32) {
    let (il, ir, it, ib) = match which {
        0 => (0.0, 0.0, 0.0, 0.0),             // border-box
        1 => (brd[3], brd[1], brd[0], brd[2]), // padding-box
        _ => (
            brd[3] + pad[3],
            brd[1] + pad[1],
            brd[0] + pad[0],
            brd[2] + pad[2],
        ), // content-box
    };
    (
        x + il,
        y + it,
        (w - il - ir).max(0.0),
        (h - it - ib).max(0.0),
    )
}

fn size_axis(s: &Sz, pa: f32) -> f32 {
    match s {
        Sz::Px(v) => *v,
        Sz::Pct(p) => p * pa,
        Sz::Auto => -1.0,
    }
}

// ---------- layer parsing ----------

/// Whitespace-separated tokens keeping `fn(...)` groups whole; top-level `/`
/// is its own token.
fn ws_tokens(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut st = 0usize;
    let b = s.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'/' if depth <= 0 => {
                if st < i {
                    out.push(&s[st..i]);
                }
                out.push(&s[i..i + 1]);
                i += 1;
                st = i;
                continue;
            }
            c if (c as char).is_whitespace() && depth <= 0 => {
                if st < i {
                    out.push(&s[st..i]);
                }
                i += 1;
                st = i;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    if st < s.len() {
        out.push(&s[st..]);
    }
    out
}

fn is_lenpct(t: &str) -> bool {
    let t = t.trim().to_ascii_lowercase();
    if t.is_empty() {
        return false;
    }
    if let Some(p) = t.strip_suffix('%') {
        return p.trim().parse::<f32>().is_ok();
    }
    if ["calc(", "min(", "max(", "clamp(", "var("]
        .iter()
        .any(|p| t.starts_with(p))
    {
        return true;
    }
    for suf in [
        "px", "em", "rem", "vw", "vh", "svw", "svh", "dvw", "dvh", "lvw", "lvh", "pt", "pc", "in",
        "cm", "mm", "q", "ex", "ch", "lh", "rlh", "vmin", "vmax",
    ] {
        if let Some(p) = t.strip_suffix(suf) {
            if p.parse::<f32>().is_ok() {
                return true;
            }
        }
    }
    t.parse::<f32>().is_ok()
}

/// Length/% token → (frac, add_px). Percentages become the frac term;
/// calc() sums % terms into frac and length terms into add (an
/// approximation, since %-in-position resolves against the positioning
/// area which isn't known at parse time).
fn len_pct_fa(t: &str, em: f32, vh: f32, vw: f32) -> Option<(f32, f32)> {
    let t = t.trim().to_ascii_lowercase();
    if let Some(p) = t.strip_suffix('%') {
        return p.trim().parse::<f32>().ok().map(|n| (n / 100.0, 0.0));
    }
    if let Some(inner) = t.strip_prefix("calc(").and_then(|s| s.strip_suffix(')')) {
        return calc_fa(inner, em, vh, vw);
    }
    parse_len_vp(&t, 0.0, em, vh, vw).map(|v| (0.0, v))
}

fn calc_fa(inner: &str, em: f32, vh: f32, vw: f32) -> Option<(f32, f32)> {
    let mut df = 0.0f32;
    let mut da = 0.0f32;
    let mut any = false;
    let mut sign = 1.0f32;
    let b = inner.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i] as char;
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '+' => {
                sign = 1.0;
                i += 1;
                continue;
            }
            '-' => {
                sign = -1.0;
                i += 1;
                continue;
            }
            '(' | ')' => {
                i += 1;
                continue;
            }
            _ => {}
        }
        let st = i;
        while i < b.len()
            && !matches!(b[i] as char, '+' | '-' | '(' | ')' | '*' | '/')
            && !(b[i] as char).is_whitespace()
        {
            i += 1;
        }
        let t = &inner[st..i];
        if let Some(p) = t.strip_suffix('%') {
            if let Ok(n) = p.parse::<f32>() {
                df += sign * n / 100.0;
                any = true;
            }
        } else if let Some(v) = parse_len_vp(t, 0.0, em, vh, vw) {
            da += sign * v;
            any = true;
        }
        sign = 1.0;
    }
    any.then_some((df, da))
}

/// Split a <position> token list into (x tokens, y tokens): an edge keyword
/// binds a following length token; bare values fill the first unset axis.
fn split_pos<'t>(toks: &[&'t str]) -> (Vec<&'t str>, Vec<&'t str>) {
    let (mut xs, mut ys) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i < toks.len() {
        let tl = toks[i].to_ascii_lowercase();
        match tl.as_str() {
            "left" | "right" => {
                xs.push(toks[i]);
                if let Some(t2) = toks.get(i + 1) {
                    if is_lenpct(t2) {
                        xs.push(t2);
                        i += 1;
                    }
                }
            }
            "top" | "bottom" => {
                ys.push(toks[i]);
                if let Some(t2) = toks.get(i + 1) {
                    if is_lenpct(t2) {
                        ys.push(t2);
                        i += 1;
                    }
                }
            }
            _ => {
                if xs.is_empty() {
                    xs.push(toks[i]);
                } else {
                    ys.push(toks[i]);
                }
            }
        }
        i += 1;
    }
    (xs, ys)
}

/// One axis of a <position>: returns (frac, add_px).
fn axis_fa(toks: &[&str], em: f32, vh: f32, vw: f32) -> (f32, f32) {
    let Some(&first) = toks.first() else {
        return (0.0, 0.0);
    };
    let t0 = first.to_ascii_lowercase();
    match t0.as_str() {
        "center" => (0.5, 0.0),
        "left" | "top" => match toks.get(1).and_then(|t| len_pct_fa(t, em, vh, vw)) {
            Some((df, da)) => (df, da),
            None => (0.0, 0.0),
        },
        "right" | "bottom" => match toks.get(1).and_then(|t| len_pct_fa(t, em, vh, vw)) {
            Some((df, da)) => (1.0 - df, -da),
            None => (1.0, 0.0),
        },
        _ => len_pct_fa(&t0, em, vh, vw).unwrap_or((0.0, 0.0)),
    }
}

fn axis_fa_or(toks: &[&str], d: (f32, f32), em: f32, vh: f32, vw: f32) -> (f32, f32) {
    if toks.is_empty() {
        d
    } else {
        axis_fa(toks, em, vh, vw)
    }
}

/// Full <position> value → [(f,a); 2]. A single value fills x; the
/// missing axis defaults to center per css-backgrounds-3 §3.6.
fn parse_pos(v: &str, em: f32, vh: f32, vw: f32) -> [(f32, f32); 2] {
    let toks = ws_tokens(v.trim());
    if toks.is_empty() {
        return [(0.0, 0.0), (0.0, 0.0)];
    }
    if toks.len() == 1 {
        return [axis_fa(&toks, em, vh, vw), (0.5, 0.0)];
    }
    let (xs, ys) = split_pos(&toks);
    [
        axis_fa_or(&xs, (0.5, 0.0), em, vh, vw),
        axis_fa_or(&ys, (0.5, 0.0), em, vh, vw),
    ]
}

fn layer_list<'a>(c: &'a Props, name: &str) -> Vec<&'a str> {
    c.get(name).map(|v| top_args(v.trim())).unwrap_or_default()
}

fn parse_size(v: &str, em: f32, vh: f32, vw: f32) -> (u8, Sz, Sz) {
    let toks = ws_tokens(v);
    let axis = |t: &&str| -> Option<Sz> {
        let tl = t.to_ascii_lowercase();
        if tl == "auto" {
            Some(Sz::Auto)
        } else if let Some(p) = tl.strip_suffix('%') {
            p.trim().parse::<f32>().ok().map(|n| Sz::Pct(n / 100.0))
        } else {
            parse_len_vp(&tl, 0.0, em, vh, vw).map(Sz::Px)
        }
    };
    match toks.as_slice() {
        [t] if t.eq_ignore_ascii_case("cover") => (1, Sz::Auto, Sz::Auto),
        [t] if t.eq_ignore_ascii_case("contain") => (2, Sz::Auto, Sz::Auto),
        _ => (
            0,
            toks.first().and_then(axis).unwrap_or(Sz::Auto),
            toks.get(1).and_then(axis).unwrap_or(Sz::Auto),
        ),
    }
}

fn parse_repeat(v: &str) -> (u8, u8) {
    let toks = ws_tokens(v);
    let map = |t: &str| -> u8 {
        match t.to_ascii_lowercase().as_str() {
            "repeat" => 1,
            "space" => 2,
            "round" => 3,
            _ => 0,
        }
    };
    if toks.len() == 1 {
        match toks[0].to_ascii_lowercase().as_str() {
            "repeat-x" => return (1, 0),
            "repeat-y" => return (0, 1),
            _ => {}
        }
    }
    let a = toks.first().map(|t| map(t)).unwrap_or(1);
    let b = toks.get(1).map(|t| map(t)).unwrap_or(a);
    (a, b)
}

fn box_idx(v: &str, default: u8) -> u8 {
    match v.trim().to_ascii_lowercase().as_str() {
        "padding-box" => 1,
        "content-box" => 2,
        "border-box" => 0,
        _ => default,
    }
}

fn parse_layers(c: &Props, em: f32, vh: f32, vw: f32, current: u32) -> Vec<Layer> {
    let mut imgs: Vec<String> = c
        .get("background-image")
        .map(|v| top_args(v.trim()).iter().map(|s| s.to_string()).collect())
        .unwrap_or_default();
    if imgs.is_empty() {
        // `background` that didn't go through each_decl expansion — pull
        // the image token out of each shorthand layer.
        if let Some(v) = c.get("background") {
            if v.contains("url(")
                || v.contains("gradient(")
                || v.contains("image-set(")
                || v.contains("cross-fade(")
                || v.contains("element(")
            {
                for lay in top_args(v.trim()) {
                    let mut found = "none".to_string();
                    for t in ws_tokens(lay) {
                        let tl = t.to_ascii_lowercase();
                        if tl.contains("url(")
                            || tl.contains("gradient(")
                            || tl.contains("image-set(")
                            || tl.contains("cross-fade(")
                            || tl.contains("element(")
                            || tl == "none"
                        {
                            found = t.to_string();
                            break;
                        }
                    }
                    imgs.push(found);
                }
            }
        }
    }
    if imgs.is_empty() || imgs.iter().all(|s| matches!(s.trim(), "" | "none")) {
        return Vec::new();
    }
    let pos = layer_list(c, "background-position");
    let pos_x = layer_list(c, "background-position-x");
    let pos_y = layer_list(c, "background-position-y");
    let sizes = layer_list(c, "background-size");
    let reps = layer_list(c, "background-repeat");
    let origins = layer_list(c, "background-origin");
    let clips = layer_list(c, "background-clip");
    let atts = layer_list(c, "background-attachment");
    fn pick<'a>(l: &[&'a str], i: usize, d: &'a str) -> &'a str {
        if l.is_empty() {
            d
        } else {
            l[i % l.len()]
        }
    }
    let mut out = Vec::with_capacity(imgs.len());
    for (i, im) in imgs.iter().enumerate() {
        let img = parse_image(im.trim(), em, vh, vw, current).unwrap_or(Img::None);
        let p = if pos_x.is_empty() && pos_y.is_empty() {
            parse_pos(pick(&pos, i, "0% 0%"), em, vh, vw)
        } else {
            let xt = ws_tokens(pick(&pos_x, i, "0%"));
            let yt = ws_tokens(pick(&pos_y, i, "center"));
            [
                axis_fa(&xt, em, vh, vw),
                axis_fa_or(&yt, (0.5, 0.0), em, vh, vw),
            ]
        };
        let (mode, sw, sh) = parse_size(pick(&sizes, i, "auto"), em, vh, vw);
        out.push(Layer {
            img,
            pos: p,
            size: (sw, sh),
            mode,
            rep: parse_repeat(pick(&reps, i, "repeat")),
            origin: box_idx(pick(&origins, i, "padding-box"), 1),
            clip: box_idx(pick(&clips, i, "border-box"), 0),
            fixed: pick(&atts, i, "scroll")
                .to_ascii_lowercase()
                .contains("fixed"),
        });
    }
    out
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches(|c| c == '"' || c == '\'').to_string()
}

/// Parse an <image> token: url(), gradient fn, or a nested image fn reduced
/// to its first argument.
fn parse_image(t: &str, em: f32, vh: f32, vw: f32, current: u32) -> Option<Img> {
    let t = t.trim();
    if t.is_empty() {
        return None;
    }
    let tl = t.to_ascii_lowercase();
    if tl == "none" {
        return Some(Img::None);
    }
    if tl.contains("gradient(") {
        return parse_gradient(t, em, vh, vw, current).map(|g| Img::Grad(Box::new(g)));
    }
    if let Some(open) = t.find('(') {
        let name = t[..open].trim().to_ascii_lowercase();
        let inner = t[open + 1..].trim_end_matches(')').trim();
        match name.as_str() {
            "url" => return Some(Img::Url(unquote(inner))),
            "image-set" | "-webkit-image-set" | "cross-fade" | "element" => {
                let first = top_args(inner).into_iter().next().unwrap_or("").trim();
                let ft = ws_tokens(first).into_iter().next().unwrap_or("");
                if ft.is_empty() || ft.starts_with('#') {
                    return None;
                }
                return parse_image(ft, em, vh, vw, current).or_else(|| {
                    let u = unquote(ft);
                    if u.is_empty() {
                        None
                    } else {
                        Some(Img::Url(u))
                    }
                });
            }
            _ => {}
        }
    }
    if !t.contains(char::is_whitespace) && !t.contains('(') {
        return Some(Img::Url(t.to_string()));
    }
    None
}

// ---------- gradients ----------

enum Grad {
    Linear {
        dir: LinDir,
        stops: Vec<Stop>,
        rep: bool,
    },
    Radial {
        circle: bool,
        size: RadSize,
        /// center as (f,a) per axis: point = f*tile_axis + a
        c: [(f32, f32); 2],
        stops: Vec<Stop>,
        rep: bool,
    },
    Conic {
        from: f32,
        c: [(f32, f32); 2],
        stops: Vec<Stop>,
        rep: bool,
    },
}

enum LinDir {
    /// angle in degrees (0 = toward top, clockwise)
    Deg(f32),
    /// direction toward a corner: (x sign, up sign) — the angle depends on
    /// the tile's aspect ratio so it resolves per tile at raster time
    Corner(f32, f32),
}

enum RadSize {
    ClosestSide,
    FarthestSide,
    ClosestCorner,
    FarthestCorner,
    Explicit(RLen, RLen),
}

#[derive(Clone, Copy)]
enum RLen {
    Px(f32),
    Pct(f32),
}

struct Stop {
    c: u32,
    pos: Option<PosV>,
}

#[derive(Clone, Copy)]
enum PosV {
    Frac(f32),
    Px(f32),
}

fn is_angle(t: &str) -> bool {
    for suf in ["deg", "grad", "rad", "turn"] {
        if let Some(p) = t.strip_suffix(suf) {
            return p.parse::<f32>().is_ok();
        }
    }
    t.parse::<f32>().is_ok()
}

fn stop_color(t: &str, current: u32) -> Option<u32> {
    let tl = t.trim().to_ascii_lowercase();
    if tl == "transparent" {
        return Some(bgra_a(0, 0, 0, 0));
    }
    if tl == "currentcolor" {
        return Some(current);
    }
    parse_color(t)
}

fn stop_pos(t: &str, conic: bool) -> Option<PosV> {
    let tl = t.trim().to_ascii_lowercase();
    if let Some(p) = tl.strip_suffix('%') {
        return p.trim().parse::<f32>().ok().map(|n| PosV::Frac(n / 100.0));
    }
    if conic && is_angle(&tl) {
        return Some(PosV::Frac(fhue(&tl) / 360.0));
    }
    parse_len_vp(&tl, 0.0, 16.0, 768.0, 1024.0).map(PosV::Px)
}

fn parse_stops(args: &[&str], conic: bool, current: u32) -> Vec<Stop> {
    let mut out = Vec::new();
    for a in args {
        let toks = ws_tokens(a);
        let Some(&first) = toks.first() else { continue };
        let Some(col) = stop_color(first, current) else {
            continue; // unparseable token (e.g. a color hint) — skip
        };
        let pos: Vec<Option<PosV>> = toks[1..].iter().map(|t| stop_pos(t, conic)).collect();
        match pos.as_slice() {
            [] => out.push(Stop { c: col, pos: None }),
            [p] => out.push(Stop { c: col, pos: *p }),
            [p1, p2, ..] => {
                // double-position syntax: one color, two stops
                out.push(Stop { c: col, pos: *p1 });
                out.push(Stop { c: col, pos: *p2 });
            }
        }
    }
    out
}

fn lin_dir(dx: f32, dy: f32) -> LinDir {
    match (dx != 0.0, dy != 0.0) {
        (true, true) => LinDir::Corner(dx.signum(), dy.signum()),
        (true, false) => LinDir::Deg(if dx > 0.0 { 90.0 } else { 270.0 }),
        (false, true) => LinDir::Deg(if dy > 0.0 { 0.0 } else { 180.0 }),
        (false, false) => LinDir::Deg(180.0),
    }
}

fn parse_gradient(spec: &str, em: f32, vh: f32, vw: f32, current: u32) -> Option<Grad> {
    let sl = spec.trim().to_ascii_lowercase();
    let mut s = sl.as_str();
    let mut rep = false;
    for pre in ["repeating-", "-webkit-repeating-"] {
        if let Some(r) = s.strip_prefix(pre) {
            s = r;
            rep = true;
            break;
        }
    }
    s = s.strip_prefix("-webkit-").unwrap_or(s);
    let open = s.find('(')?;
    let fname = s[..open].trim();
    let inner = s[open + 1..s.rfind(')')?].trim();
    let args = top_args(inner);
    if args.len() < 2 {
        return None;
    }
    let first_tok = ws_tokens(args[0])
        .first()
        .map(|t| t.to_ascii_lowercase())
        .unwrap_or_default();
    match fname {
        "linear-gradient" => {
            let (dir, stops_from) = if first_tok == "to" {
                let toks = ws_tokens(args[0]);
                let (mut dx, mut dy) = (0.0f32, 0.0f32);
                for t in &toks[1..] {
                    match t.to_ascii_lowercase().as_str() {
                        "top" => dy += 1.0,
                        "bottom" => dy -= 1.0,
                        "left" => dx -= 1.0,
                        "right" => dx += 1.0,
                        _ => {}
                    }
                }
                (lin_dir(dx, dy), 1)
            } else if is_angle(&first_tok) {
                (LinDir::Deg(fhue(&first_tok)), 1)
            } else if matches!(first_tok.as_str(), "top" | "bottom" | "left" | "right") {
                // legacy -webkit from-direction keywords
                let toks = ws_tokens(args[0]);
                let (mut dx, mut dy) = (0.0f32, 0.0f32);
                for t in &toks {
                    match t.to_ascii_lowercase().as_str() {
                        "top" => dy -= 1.0,
                        "bottom" => dy += 1.0,
                        "left" => dx -= 1.0,
                        "right" => dx += 1.0,
                        _ => {}
                    }
                }
                (lin_dir(dx, dy), 1)
            } else {
                (LinDir::Deg(180.0), 0)
            };
            let stops = parse_stops(&args[stops_from..], false, current);
            if stops.is_empty() {
                return None;
            }
            Some(Grad::Linear { dir, stops, rep })
        }
        "radial-gradient" => {
            let prelude = matches!(
                first_tok.as_str(),
                "at" | "circle"
                    | "ellipse"
                    | "closest-side"
                    | "farthest-side"
                    | "closest-corner"
                    | "farthest-corner"
            ) || is_lenpct(&first_tok);
            let mut circle = false;
            let mut size = RadSize::FarthestCorner;
            let mut c = [(0.5f32, 0.0f32), (0.5, 0.0)];
            let mut stops_from = 0usize;
            if prelude {
                stops_from = 1;
                let toks = ws_tokens(args[0]);
                let mut lens: Vec<RLen> = Vec::new();
                let mut i = 0;
                while i < toks.len() {
                    match toks[i].to_ascii_lowercase().as_str() {
                        "circle" => circle = true,
                        "ellipse" => circle = false,
                        "closest-side" => size = RadSize::ClosestSide,
                        "farthest-side" => size = RadSize::FarthestSide,
                        "closest-corner" => size = RadSize::ClosestCorner,
                        "farthest-corner" => size = RadSize::FarthestCorner,
                        "at" => {
                            let (xs, ys) = split_pos(&toks[i + 1..]);
                            c[0] = axis_fa_or(&xs, (0.5, 0.0), em, vh, vw);
                            c[1] = axis_fa_or(&ys, (0.5, 0.0), em, vh, vw);
                            break;
                        }
                        _ => {
                            let tl = toks[i].to_ascii_lowercase();
                            if let Some(p) =
                                tl.strip_suffix('%').and_then(|p| p.parse::<f32>().ok())
                            {
                                lens.push(RLen::Pct(p / 100.0));
                            } else if let Some(v) = parse_len_vp(&tl, 0.0, em, vh, vw) {
                                lens.push(RLen::Px(v));
                            }
                        }
                    }
                    i += 1;
                }
                if let Some(&a) = lens.first() {
                    let b = lens.get(1).copied().unwrap_or(a);
                    size = RadSize::Explicit(a, b);
                }
            }
            let stops = parse_stops(&args[stops_from..], false, current);
            if stops.is_empty() {
                return None;
            }
            Some(Grad::Radial {
                circle,
                size,
                c,
                stops,
                rep,
            })
        }
        "conic-gradient" => {
            let mut from = 0.0f32;
            let mut c = [(0.5f32, 0.0f32), (0.5, 0.0)];
            let mut stops_from = 0usize;
            if first_tok == "from" || first_tok == "at" {
                stops_from = 1;
                let toks = ws_tokens(args[0]);
                let mut i = 0;
                while i < toks.len() {
                    match toks[i].to_ascii_lowercase().as_str() {
                        "from" => {
                            if let Some(t) = toks.get(i + 1) {
                                from = fhue(t);
                                i += 1;
                            }
                        }
                        "at" => {
                            let (xs, ys) = split_pos(&toks[i + 1..]);
                            c[0] = axis_fa_or(&xs, (0.5, 0.0), em, vh, vw);
                            c[1] = axis_fa_or(&ys, (0.5, 0.0), em, vh, vw);
                            break;
                        }
                        _ => {}
                    }
                    i += 1;
                }
            }
            let stops = parse_stops(&args[stops_from..], true, current);
            if stops.is_empty() {
                return None;
            }
            Some(Grad::Conic {
                from,
                c,
                stops,
                rep,
            })
        }
        _ => None,
    }
}

/// Resolve stop positions to (t, bgra_a): implicit ends pin at 0/1,
/// unspecified positions distribute evenly, and positions clamp
/// non-decreasing (css-images-4 §3.5).
fn resolve_stops(stops: &[Stop], axis_px: f32) -> Vec<(f32, u32)> {
    let mut pos: Vec<Option<f32>> = stops
        .iter()
        .map(|s| {
            s.pos.map(|p| match p {
                PosV::Frac(f) => f,
                PosV::Px(px) => {
                    if axis_px > 0.0 {
                        px / axis_px
                    } else {
                        0.0
                    }
                }
            })
        })
        .collect();
    if let Some(f) = pos.first_mut() {
        if f.is_none() {
            *f = Some(0.0);
        }
    }
    if let Some(l) = pos.last_mut() {
        if l.is_none() {
            *l = Some(1.0);
        }
    }
    let mut i = 0;
    while i < pos.len() {
        if pos[i].is_some() {
            i += 1;
            continue;
        }
        let a = pos[i - 1].unwrap_or(0.0);
        let mut j = i;
        while j < pos.len() && pos[j].is_none() {
            j += 1;
        }
        let b = pos.get(j).copied().flatten().unwrap_or(1.0);
        let n = (j - i + 1) as f32;
        for k in i..j {
            pos[k] = Some(a + (b - a) * (k - i + 1) as f32 / n);
        }
        i = j;
    }
    let mut prev = f32::NEG_INFINITY;
    let mut out = Vec::with_capacity(stops.len());
    for (s, p) in stops.iter().zip(pos) {
        let p = p.unwrap_or(0.0).max(prev);
        prev = p;
        out.push((p, s.c));
    }
    out
}

/// Premultiplied sRGB lerp of two BGRA colors.
fn lerp_premul(a: u32, b: u32, t: f32) -> u32 {
    let (aa, ab) = (alpha(a) as f32 / 255.0, alpha(b) as f32 / 255.0);
    let r = ch(a, 16) as f32 * aa + (ch(b, 16) as f32 * ab - ch(a, 16) as f32 * aa) * t;
    let g = ch(a, 8) as f32 * aa + (ch(b, 8) as f32 * ab - ch(a, 8) as f32 * aa) * t;
    let bl = ch(a, 0) as f32 * aa + (ch(b, 0) as f32 * ab - ch(a, 0) as f32 * aa) * t;
    let al = aa + (ab - aa) * t;
    if al <= 0.0 {
        return 0;
    }
    let un = |v: f32| (v / al).round().clamp(0.0, 255.0) as u8;
    bgra_a(un(r), un(g), un(bl), (al * 255.0).round() as u8)
}

fn sample(stops: &[(f32, u32)], t: f32, rep: bool) -> u32 {
    if stops.is_empty() {
        return 0;
    }
    let mut t = t;
    if rep {
        let (p0, pl) = (stops[0].0, stops[stops.len() - 1].0);
        let span = pl - p0;
        if span > 1e-6 {
            t = p0 + (t - p0).rem_euclid(span);
        }
    }
    if t <= stops[0].0 {
        return stops[0].1;
    }
    if t >= stops[stops.len() - 1].0 {
        return stops[stops.len() - 1].1;
    }
    let mut i = 0;
    while i + 1 < stops.len() && stops[i + 1].0 < t {
        i += 1;
    }
    let (p0, c0) = stops[i];
    let (p1, c1) = stops[i + 1];
    let f = if p1 - p0 > 1e-9 {
        (t - p0) / (p1 - p0)
    } else {
        0.0
    };
    lerp_premul(c0, c1, f)
}

/// Gradient resolved against concrete tile dimensions.
enum RGrad {
    Linear {
        dx: f32,
        dy: f32,
        len: f32,
        rep: bool,
    },
    Radial {
        circle: bool,
        cx: f32,
        cy: f32,
        rx: f32,
        ry: f32,
        rep: bool,
    },
    Conic {
        cx: f32,
        cy: f32,
        from: f32,
        rep: bool,
    },
}

fn radial_radii(size: &RadSize, circle: bool, cx: f32, cy: f32, tw: f32, th: f32) -> (f32, f32) {
    match size {
        RadSize::Explicit(a, b) => {
            let rx = match a {
                RLen::Px(v) => *v,
                RLen::Pct(p) => p * tw,
            };
            let ry = match b {
                RLen::Px(v) => *v,
                RLen::Pct(p) => p * th,
            };
            if circle {
                let r = rx.max(0.1);
                (r, r)
            } else {
                (rx.max(0.1), ry.max(0.1))
            }
        }
        _ => {
            let near = matches!(size, RadSize::ClosestSide | RadSize::ClosestCorner);
            let pick = |a: f32, b: f32| if near { a.min(b) } else { a.max(b) };
            let hs = pick(cx, tw - cx).max(0.0);
            let vs = pick(cy, th - cy).max(0.0);
            if matches!(size, RadSize::ClosestSide | RadSize::FarthestSide) {
                if circle {
                    let r = pick(hs, vs).max(0.1);
                    (r, r)
                } else {
                    (hs.max(0.1), vs.max(0.1))
                }
            } else if circle {
                let r = hs.hypot(vs).max(0.1);
                (r, r)
            } else {
                // ellipse through the corner (hs,vs) with aspect hs:vs —
                // simplifies to √2·hs / √2·vs
                ((hs * 1.41421356).max(0.1), (vs * 1.41421356).max(0.1))
            }
        }
    }
}

fn resolve_grad(g: &Grad, tw: f32, th: f32) -> RGrad {
    match g {
        Grad::Linear { dir, rep, .. } => {
            let (dx, dy) = match dir {
                LinDir::Deg(d) => {
                    let r = d.to_radians();
                    (r.sin(), -r.cos())
                }
                LinDir::Corner(sx, sy) => {
                    let (vx, vy) = (tw * sx, -th * sy);
                    let n = vx.hypot(vy).max(1e-6);
                    (vx / n, vy / n)
                }
            };
            let len = (tw * dx.abs() + th * dy.abs()).max(1e-6);
            RGrad::Linear {
                dx,
                dy,
                len,
                rep: *rep,
            }
        }
        Grad::Radial {
            circle,
            size,
            c,
            rep,
            ..
        } => {
            let cx = c[0].0 * tw + c[0].1;
            let cy = c[1].0 * th + c[1].1;
            let (rx, ry) = radial_radii(size, *circle, cx, cy, tw, th);
            RGrad::Radial {
                circle: *circle,
                cx,
                cy,
                rx,
                ry,
                rep: *rep,
            }
        }
        Grad::Conic { from, c, rep, .. } => RGrad::Conic {
            cx: c[0].0 * tw + c[0].1,
            cy: c[1].0 * th + c[1].1,
            from: *from,
            rep: *rep,
        },
    }
}

/// Stop parameter t at tile-local point (x,y) in a tw×th tile.
fn t_at(g: &RGrad, x: f32, y: f32, tw: f32, th: f32) -> f32 {
    match g {
        RGrad::Linear { dx, dy, len, .. } => {
            ((x - tw / 2.0) * dx + (y - th / 2.0) * dy) / len + 0.5
        }
        RGrad::Radial {
            circle,
            cx,
            cy,
            rx,
            ry,
            ..
        } => {
            if *circle {
                (x - cx).hypot(y - cy) / rx
            } else {
                (((x - cx) / rx).powi(2) + ((y - cy) / ry).powi(2)).sqrt()
            }
        }
        RGrad::Conic { cx, cy, from, .. } => {
            let a = (x - cx).atan2(-(y - cy)).to_degrees();
            (a - from).rem_euclid(360.0) / 360.0
        }
    }
}

fn rep_flag(g: &RGrad) -> bool {
    match g {
        RGrad::Linear { rep, .. } | RGrad::Radial { rep, .. } | RGrad::Conic { rep, .. } => *rep,
    }
}

/// Rasterize a pw×ph band starting at absolute (x0,y0): for each pixel the
/// tile-local (u,v) = (abs - tile_origin), wrapped for repeat modes and
/// transparent outside the tile for no-repeat axes.
#[allow(clippy::too_many_arguments)]
fn raster_band(
    g: &RGrad,
    stops: &[(f32, u32)],
    x0: f32,
    y0: f32,
    pw: usize,
    ph: usize,
    ox: f32,
    oy: f32,
    tw: f32,
    th: f32,
    tile_rep: (u8, u8),
) -> Vec<u8> {
    let rep = rep_flag(g);
    let mut px = vec![0u8; pw * ph * 4];
    for j in 0..ph {
        let v = y0 + j as f32 + 0.5 - oy;
        let Some(v) = (if tile_rep.1 != 0 {
            Some(v.rem_euclid(th))
        } else if (0.0..th).contains(&v) {
            Some(v)
        } else {
            None
        }) else {
            continue;
        };
        for i in 0..pw {
            let u = x0 + i as f32 + 0.5 - ox;
            let Some(u) = (if tile_rep.0 != 0 {
                Some(u.rem_euclid(tw))
            } else if (0.0..tw).contains(&u) {
                Some(u)
            } else {
                None
            }) else {
                continue;
            };
            let t = t_at(g, u, v, tw, th);
            let c = sample(stops, t, rep);
            let o = (j * pw + i) * 4;
            px[o..o + 4].copy_from_slice(&c.to_le_bytes());
        }
    }
    px
}

/// Rasterize a gradient layer into `Raster` bands covering the clip rect.
fn emit_gradient(
    out: &mut Vec<DisplayItem>,
    g: &Grad,
    l: &Layer,
    clip: (f32, f32, f32, f32),
    pa: (f32, f32, f32, f32),
    budget: &mut usize,
) {
    if clip.2 <= 0.5 || clip.3 <= 0.5 {
        return;
    }
    // tile size: gradients have no intrinsic size — auto/cover/contain fill
    // the positioning-area axis (css-images §3.3.4)
    let (mut tw, mut th) = if l.mode == 0 {
        (
            match l.size.0 {
                Sz::Auto => pa.2,
                Sz::Px(v) => v,
                Sz::Pct(p) => p * pa.2,
            },
            match l.size.1 {
                Sz::Auto => pa.3,
                Sz::Px(v) => v,
                Sz::Pct(p) => p * pa.3,
            },
        )
    } else {
        (pa.2, pa.3)
    };
    if tw < 0.5 || th < 0.5 {
        return;
    }
    if l.rep.0 == 3 {
        tw = pa.2 / (pa.2 / tw).round().max(1.0);
    }
    if l.rep.1 == 3 {
        th = pa.3 / (pa.3 / th).round().max(1.0);
    }
    let ox = pa.0 + l.pos[0].0 * (pa.2 - tw) + l.pos[0].1;
    let oy = pa.1 + l.pos[1].0 * (pa.3 - th) + l.pos[1].1;
    let rg = resolve_grad(g, tw, th);
    // stop positions in px resolve against the gradient axis
    let axis = match &rg {
        RGrad::Linear { len, .. } => *len,
        RGrad::Radial { rx, ry, .. } => (rx + ry) / 2.0,
        RGrad::Conic { .. } => 360.0,
    };
    let stops = resolve_stops(grad_stops(g), axis);
    if stops.is_empty() {
        return;
    }
    const BAND: f32 = 2048.0;
    let (cr, cb) = (clip.0 + clip.2, clip.1 + clip.3);
    let mut yy = clip.1;
    while yy < cb - 0.01 {
        let bh = (cb - yy).min(BAND);
        let mut xx = clip.0;
        while xx < cr - 0.01 {
            let bw = (cr - xx).min(BAND);
            let pw = bw.ceil().max(1.0) as usize;
            let mut ph = bh.ceil().max(1.0) as usize;
            if pw.saturating_mul(ph) > *budget {
                ph = *budget / pw;
                if ph == 0 {
                    return;
                }
            }
            *budget -= pw * ph;
            let pixels = raster_band(&rg, &stops, xx, yy, pw, ph, ox, oy, tw, th, l.rep);
            out.push(DisplayItem::Raster {
                x: xx,
                y: yy,
                w: pw as f32,
                h: ph as f32,
                pixels,
            });
            if (ph as f32) < bh - 0.5 {
                return; // pixel budget exhausted — coverage ends here
            }
            xx += bw;
        }
        yy += bh;
    }
}

fn grad_stops(g: &Grad) -> &Vec<Stop> {
    match g {
        Grad::Linear { stops, .. } | Grad::Radial { stops, .. } | Grad::Conic { stops, .. } => {
            stops
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grad(s: &str) -> Grad {
        parse_gradient(s, 16.0, 768.0, 1024.0, 0).expect("gradient parse")
    }

    fn nstops(g: &Grad) -> usize {
        grad_stops(g).len()
    }

    #[test]
    fn parses_linear_stops() {
        match grad("linear-gradient(90deg, red 0%, blue 100%)") {
            Grad::Linear { dir, .. } => match dir {
                LinDir::Deg(d) => assert_eq!(d, 90.0),
                _ => panic!("corner"),
            },
            _ => panic!("not linear"),
        }
        assert_eq!(nstops(&grad("linear-gradient(red, white, blue)")), 3);
        // double-position syntax expands to two stops
        let g = grad("repeating-linear-gradient(45deg, red 0 10px, blue 10px 20px)");
        assert_eq!(nstops(&g), 4);
        match g {
            Grad::Linear { rep, dir, .. } => {
                assert!(rep);
                match dir {
                    LinDir::Deg(d) => assert_eq!(d, 45.0),
                    _ => panic!(),
                }
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parses_radial_and_conic() {
        let g = grad("radial-gradient(circle at 30% 40%, red, blue)");
        match g {
            Grad::Radial { circle, c, .. } => {
                assert!(circle);
                assert_eq!(c[0], (0.3, 0.0));
                assert_eq!(c[1], (0.4, 0.0));
            }
            _ => panic!("not radial"),
        }
        let g = grad("conic-gradient(from 90deg at center, red, blue)");
        match g {
            Grad::Conic { from, c, .. } => {
                assert_eq!(from, 90.0);
                assert_eq!(c[0], (0.5, 0.0));
            }
            _ => panic!("not conic"),
        }
    }

    #[test]
    fn stop_positions_resolve() {
        let g = grad("linear-gradient(red, blue 60%, green)");
        let stops = resolve_stops(grad_stops(&g), 100.0);
        assert_eq!(stops.len(), 3);
        assert_eq!(stops[0].0, 0.0);
        assert_eq!(stops[1].0, 0.6);
        assert_eq!(stops[2].0, 1.0);
    }

    #[test]
    fn raster_linear_samples() {
        let g = grad("linear-gradient(90deg, #ff0000, #0000ff)");
        let rg = resolve_grad(&g, 100.0, 10.0);
        let stops = resolve_stops(grad_stops(&g), 100.0);
        let px = raster_band(
            &rg,
            &stops,
            0.0,
            0.0,
            100,
            10,
            0.0,
            0.0,
            100.0,
            10.0,
            (1, 1),
        );
        assert_eq!(px.len(), 100 * 10 * 4);
        // left pixel ≈ red, right ≈ blue (BGRA)
        assert!(px[0] < 10 && px[1] < 10 && px[2] > 245 && px[3] == 255);
        let r = 99 * 4;
        assert!(px[r] > 245 && px[r + 1] < 10 && px[r + 2] < 10 && px[r + 3] == 255);
    }

    #[test]
    fn repeating_wraps() {
        let g = grad(
            "repeating-linear-gradient(90deg, #ff0000 0px, #ff0000 50px, #0000ff 50px, #0000ff 100px)",
        );
        let rg = resolve_grad(&g, 200.0, 10.0);
        let stops = resolve_stops(grad_stops(&g), 200.0);
        let px = raster_band(&rg, &stops, 0.0, 0.0, 200, 1, 0.0, 0.0, 200.0, 10.0, (1, 1));
        // pixel 10 → red band, pixel 60 → blue band, pixel 110 → red again
        assert_eq!(px[10 * 4 + 2], 255);
        assert_eq!(px[60 * 4], 255);
        assert_eq!(px[110 * 4 + 2], 255);
    }

    #[test]
    fn position_tokens() {
        assert_eq!(
            parse_pos("right 10px bottom 20px", 16.0, 768.0, 1024.0),
            [(1.0, -10.0), (1.0, -20.0)]
        );
        assert_eq!(
            parse_pos("25% 75%", 16.0, 768.0, 1024.0),
            [(0.25, 0.0), (0.75, 0.0)]
        );
        assert_eq!(
            parse_pos("center", 16.0, 768.0, 1024.0),
            [(0.5, 0.0), (0.5, 0.0)]
        );
        assert_eq!(
            parse_pos("10px", 16.0, 768.0, 1024.0),
            [(0.0, 10.0), (0.5, 0.0)]
        );
    }

    #[test]
    fn size_parsing() {
        assert_eq!(parse_size("cover", 16.0, 768.0, 1024.0).0, 1);
        assert_eq!(parse_size("contain", 16.0, 768.0, 1024.0).0, 2);
        let (m, a, b) = parse_size("50% auto", 16.0, 768.0, 1024.0);
        assert_eq!(m, 0);
        assert!(matches!(a, Sz::Pct(p) if (p - 0.5).abs() < 1e-6));
        assert!(matches!(b, Sz::Auto));
        let (m, a, b) = parse_size("10px 20px", 16.0, 768.0, 1024.0);
        assert_eq!(m, 0);
        assert!(matches!(a, Sz::Px(v) if v == 10.0));
        assert!(matches!(b, Sz::Px(v) if v == 20.0));
    }
}
