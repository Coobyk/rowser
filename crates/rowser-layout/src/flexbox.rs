//! Flexbox: `display:flex` / `display:inline-flex` — full line-based layout.
//!
//! Item model: each in-flow child (element, replaced element, atomic inline,
//! anonymous text run) becomes an [`FItem`] with a hypothetical main size.
//! Items are stably sorted by `order`, packed into lines (`flex-wrap`),
//! sized per line (grow/shrink/min-max clamps/auto margins), then emitted at
//! explicit coordinates: `justify-content` spaces along the main axis,
//! `align-items`/`align-self` position within the line's cross extent, and
//! `align-content` distributes leftover cross space between lines.
//! `row-reverse`/`column-reverse` and `wrap-reverse` are pure coordinate
//! flips over the same forward layout.

use super::*;

/// Cross-axis alignment of one item within its line.
#[derive(Clone, Copy, PartialEq)]
enum FAlign {
    Stretch,
    Start,
    End,
    Center,
    Baseline,
}

/// `align-items`/`align-self` keyword → FAlign; `auto` defers to the
/// container's `align-items` (returns None).
fn falign(v: Option<&String>) -> Option<FAlign> {
    let v = v?.trim();
    if v.contains("baseline") {
        return Some(FAlign::Baseline);
    }
    Some(match v {
        "end" | "flex-end" | "self-end" | "right" | "bottom" => FAlign::End,
        "center" => FAlign::Center,
        "stretch" | "normal" => FAlign::Stretch,
        "auto" => return None,
        // start/flex-start/self-start/left/top + unknown keywords → start edge
        _ => FAlign::Start,
    })
}

/// `flex-basis` value → resolution strategy.
#[derive(Clone, Copy)]
enum FBasis {
    /// `auto` — the item's own `width`/`height` or content size.
    Auto,
    /// definite length/% (already resolved to px).
    Px(f32),
    /// `min-content`.
    MinC,
    /// `max-content` (the `content` keyword approximates to this).
    MaxC,
    /// `fit-content` ≈ min(max-content, available).
    FitC,
}

fn basis_val(v: &str, rel: Option<f32>, em: f32) -> FBasis {
    match v.trim() {
        "auto" => FBasis::Auto,
        "content" | "max-content" => FBasis::MaxC,
        "min-content" => FBasis::MinC,
        "fit-content" => FBasis::FitC,
        v => {
            if let Some(inner) = v
                .strip_prefix("fit-content(")
                .and_then(|s| s.strip_suffix(')'))
            {
                return FBasis::Px(parse_len(inner, rel.unwrap_or(0.0), em).unwrap_or(0.0));
            }
            // a % basis against an indefinite main size falls back to auto
            if v.ends_with('%') && rel.is_none() {
                return FBasis::Auto;
            }
            FBasis::Px(parse_len(v, rel.unwrap_or(0.0), em).unwrap_or(0.0))
        }
    }
}

/// (grow, shrink, basis) for a flex item — longhands override the `flex`
/// shorthand's tokens (both live side by side in the computed map).
fn flex_props(c: &Props, rel: Option<f32>, em: f32) -> (f32, f32, FBasis) {
    let (mut g, mut s, mut b) = (0.0f32, 1.0f32, FBasis::Auto);
    if let Some(v) = c.get("flex") {
        match v.trim() {
            "none" => s = 0.0,
            "initial" => {}
            "auto" => g = 1.0,
            _ => {
                let mut gseen = false;
                let mut bseen = false;
                for t in v.split_whitespace() {
                    if t.parse::<f32>().is_ok() {
                        if !gseen {
                            g = t.parse().unwrap_or(0.0);
                            gseen = true;
                        } else {
                            s = t.parse().unwrap_or(1.0);
                        }
                    } else {
                        b = basis_val(t, rel, em);
                        bseen = true;
                    }
                }
                if gseen && !bseen {
                    b = FBasis::Px(0.0); // `flex:<number>` → `n 1 0%`
                } else if !gseen {
                    g = 1.0; // `flex:<basis>` → `1 1 <basis>`
                }
            }
        }
    }
    if let Some(v) = c.get("flex-grow") {
        g = v.trim().parse().unwrap_or(g);
    }
    if let Some(v) = c.get("flex-shrink") {
        s = v.trim().parse().unwrap_or(s);
    }
    if let Some(v) = c.get("flex-basis") {
        b = basis_val(v, rel, em);
    }
    (g.max(0.0), s.max(0.0), b)
}

/// Per-side `margin-*:auto` flags [t,r,b,l]. Longhands suffice — `each_decl`
/// already expanded the `margin`/`margin-block`/`margin-inline` shorthands.
fn margin_autos(c: &Props) -> [bool; 4] {
    let a = |n: &str| {
        c.get(n)
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("auto"))
    };
    [
        a("margin-top"),
        a("margin-right"),
        a("margin-bottom"),
        a("margin-left"),
    ]
}

/// What a flex child is — decides how it measures and how it paints.
#[derive(Clone, Copy, PartialEq)]
enum FKind {
    /// ordinary element → boxed via `item_box`
    El,
    /// img/svg → fixed replaced payload
    Replaced,
    /// input/select/… → `inline()` captured into a payload
    Capture,
    /// anonymous text item
    Text,
}

/// One flex item through the pipeline: props + measured base → resolved
/// main position/size → laid payload → emitted at (x, y).
struct FItem {
    id: NodeId,
    /// DOM index — stable tie-break under `order`
    dom: usize,
    order: i32,
    kind: FKind,
    /// computed style (element items only)
    cc: Option<Rc<Props>>,
    tag: String,
    /// raw text (text items only)
    text: String,
    cst: Style,
    /// resolved margins [t,r,b,l] (`auto` sides are 0 until distributed)
    m: [f32; 4],
    auto: [bool; 4],
    grow: f32,
    shrink: f32,
    /// hypothetical main size (border-box, min/max-clamped)
    base: f32,
    /// min/max main clamps (border-box)
    floor: f32,
    ceil: f32,
    /// cross size is `auto` — `align-*:stretch` may fill the line
    cross_auto: bool,
    min_cross: f32,
    max_cross: f32,
    align: FAlign,
    /// laid payload in box-local coords + the width it was laid at
    laid_w: f32,
    w: f32,
    h: f32,
    items: Vec<DisplayItem>,
    hits: Vec<Hit>,
    /// leading deco items in `items` — re-painted when the box
    /// height is restretched after layout
    n_deco: usize,
    /// everything `emit_deco` needs to repaint the prefix at a new size
    deco: Option<Deco>,
    /// translate offset baked into the deco rects (re-applied on repaint)
    tr: (f32, f32),
    /// first-baseline offset below the border-box top
    baseline: Option<f32>,
    /// resolved: final border-box main size + margin-box start (forward)
    main: f32,
    pos: f32,
}

impl FItem {
    fn new(id: NodeId, dom: usize, kind: FKind, cst: Style) -> FItem {
        FItem {
            id,
            dom,
            order: 0,
            kind,
            cc: None,
            tag: String::new(),
            text: String::new(),
            cst,
            m: [0.0; 4],
            auto: [false; 4],
            grow: 0.0,
            shrink: 1.0,
            base: 0.0,
            floor: 0.0,
            ceil: f32::MAX,
            cross_auto: false,
            min_cross: 0.0,
            max_cross: f32::MAX,
            align: FAlign::Stretch,
            laid_w: 0.0,
            w: 0.0,
            h: 0.0,
            items: Vec::new(),
            hits: Vec::new(),
            n_deco: 0,
            deco: None,
            tr: (0.0, 0.0),
            baseline: None,
            main: 0.0,
            pos: 0.0,
        }
    }
}

/// A flex line: an index range into the sorted item vec plus the resolved
/// cross extent and baseline metrics.
struct FLine {
    s: usize,
    e: usize,
    /// cross size of the line (bands stack along the cross axis)
    cross: f32,
    /// shared baseline: max distance from margin-box top to first baseline
    bmax: f32,
    /// downward shift so no baseline-aligned item pokes above the band
    bshift: f32,
    /// natural main extent (items + gaps) — the mirror span when the
    /// container's main size is indefinite (`column-reverse`)
    span: f32,
}

/// Everything `borderfx::emit_deco` needs to repaint a box's leading
/// decoration prefix at a new size — captured once at layout, replayed
/// by `restretch` (`align-items:stretch`, column grow, flexed main).
#[derive(Clone)]
pub(crate) struct Deco {
    bg: Option<Color>,
    border: Option<borderfx::Borders>,
    radius: borderfx::RadiusSpec,
    shadows: Vec<borderfx::BoxShadow>,
    clip: bool,
    outline: Option<borderfx::Outline>,
    bgl: Option<bgimg::BoxBgs>,
    /// font size for %-radius resolution
    em: f32,
}

/// Laid box payload for `item_box`: deco + children in box-local coords
/// with enough decoration info to repaint at a new height.
pub(crate) struct FBox {
    pub(crate) w: f32,
    pub(crate) h: f32,
    pub(crate) items: Vec<DisplayItem>,
    pub(crate) hits: Vec<Hit>,
    pub(crate) n_deco: usize,
    pub(crate) deco: Deco,
    /// translate offset baked into the deco rects (re-applied on repaint)
    pub(crate) tr: (f32, f32),
    /// the box's 2D transform spec — a caller may tag/wrap its span
    pub(crate) xf: Option<Rc<xform::Spec>>,
    pub(crate) baseline: Option<f32>,
}

impl<'a> Layouter<'a> {
    /// Repaint the leading deco items of a laid payload at `new_h` —
    /// content items are untouched.
    fn restretch(
        &self,
        items: &mut Vec<DisplayItem>,
        n_deco: usize,
        deco: &Deco,
        tr: (f32, f32),
        w: f32,
        new_h: f32,
    ) {
        let mut out: Vec<DisplayItem> = Vec::new();
        let radii = deco.radius.resolve(w, new_h, deco.em);
        let layers = deco
            .bgl
            .as_ref()
            .map(|b| bgimg::items_for(b, 0.0, 0.0, w, new_h, (self.viewport_w, self.viewport_h)))
            .unwrap_or_default();
        borderfx::emit_deco(
            &mut out,
            0.0,
            0.0,
            w,
            new_h,
            deco.bg,
            deco.border.as_ref(),
            &radii,
            &deco.shadows,
            deco.clip,
            layers,
        );
        if tr.0 != 0.0 || tr.1 != 0.0 {
            out = out
                .into_iter()
                .map(|it| offset_item(it, tr.0, tr.1))
                .collect();
        }
        items.splice(..n_deco, out);
    }
}

/// First text baseline inside a laid payload (px below the box top).
fn payload_baseline(items: &[DisplayItem], skip: usize) -> Option<f32> {
    items.iter().skip(skip).find_map(|it| match it {
        DisplayItem::Text { baseline, .. } => Some(*baseline),
        _ => None,
    })
}

impl<'a> Layouter<'a> {
    /// Laid box for a flex item / inline-block: `hint` pins the border-box
    /// width (flexed main size / stretch), `forced_h` pins the border-box
    /// height when the item's own `height` is auto. Same geometry as the
    /// inline-block path — bg, border, padding, border-box sizing, nested
    /// content via `nest`, translate — plus decoration info so a flex
    /// stretch/grow can repaint the box without a second layout.
    pub(crate) fn item_box(
        &mut self,
        node: NodeId,
        c: &Props,
        mut st: Style,
        hint: Option<f32>,
        forced_h: Option<f32>,
    ) -> FBox {
        let size = st.size;
        let [pt, pr, pb, pl] = box_sides(c, "padding", self.avail_w, size).map(|v| v.max(0.0));
        let bd = self.backdrop;
        let bb = border_box(c);
        let bg = self.bg_of(node, c, Some(st.color)).map(|col| over(col, bd));
        let border = borderfx::borders(c, size, st.color).map(|mut b| {
            for col in b.color.iter_mut() {
                *col = over(*col, bg.unwrap_or(bd));
            }
            b
        });
        st.inline_bg = None; // the box paints its own bg
        if let Some(b) = bg {
            if let Some(col) = c
                .get("color")
                .and_then(|v| parse_color_ctx(v, Some(st.color)))
            {
                st.color = over(col, b);
            }
        }
        let [bw_t, bw_r, bw_b, bw_l] = border.map(|b| b.w).unwrap_or([0.0; 4]);
        let bw_h = bw_l + bw_r;
        let bw_v = bw_t + bw_b;

        // width property else shrink-wrap (min-content approx; flex sums children)
        let inner = if let Some(w) = hint {
            // resolved flex size — an outer (border-box) target
            (w - pl - pr - bw_h).max(1.0)
        } else if let Some(w) = dim(c, "width", self.avail_w, size) {
            if bb { w - pl - pr - bw_h } else { w }.max(1.0)
        } else if matches!(
            c.get("display").map(String::as_str),
            Some("flex") | Some("inline-flex") | Some("inline-grid")
        ) {
            self.flex_maxc_width(node, c, &st)
        } else {
            // shrink-to-fit ≈ max-content — capped by the room in `box_w`
            self.max_content_width(node, &st)
        };
        let room = self.content_width();
        let box_w = (inner + pl + pr + bw_h).min(room);
        let inner = (box_w - pl - pr - bw_h).max(1.0);

        // `height`/aspect-ratio/min|max-height: under border-box these are
        // border-box lengths; content-box adds padding+border. `forced_h`
        // (stretch/grow) applies when no explicit height exists.
        let hrel = self.pct_h.unwrap_or(self.viewport_h);
        let outer_h = |v: f32| if bb { v } else { v + pt + pb + bw_v };
        let mut h_prop = dim(c, "height", hrel, size)
            .or(forced_h)
            .or_else(|| aspect_ratio(c).map(|r| box_w / r));
        if let Some(mn) = c
            .get("min-height")
            .and_then(|v| parse_len_vp(v, hrel, size, self.viewport_h, self.avail_w))
        {
            h_prop = Some(h_prop.map_or(mn, |h| h.max(mn)));
        }
        if let Some(mx) = c
            .get("max-height")
            .and_then(|v| parse_len_vp(v, hrel, size, self.viewport_h, self.avail_w))
        {
            h_prop = h_prop.map(|h| h.min(mx));
        }
        let saved_bd = self.backdrop;
        if let Some(b) = bg {
            self.backdrop = b;
        }
        // %-height children resolve against this box's definite content height
        let saved_ph = self.pct_h;
        self.pct_h = h_prop
            .map(|h| if bb { h - pt - pb - bw_v } else { h })
            .or(saved_ph);
        let (items, y_end, nhits) = self.nest(node, &st, pl + bw_l, pt + bw_t, inner, c);
        self.backdrop = saved_bd;
        self.pct_h = saved_ph;
        // explicit height/aspect-ratio/forced/max-height may pin the painted
        // box; min-height alone only floors it
        let natural = y_end + pb + bw_b;
        let pinned = dim(c, "height", hrel, size).is_some()
            || forced_h.is_some()
            || aspect_ratio(c).is_some()
            || c.get("max-height").is_some();
        let box_h = match (h_prop, pinned) {
            (Some(h), true) => outer_h(h).max(1.0),
            (Some(h), false) => outer_h(h).max(natural),
            _ => natural.max(1.0),
        };

        // decoration first (shadows → bg → layers → borders → clip) so a
        // height restretch can rebuild just this prefix
        let deco = Deco {
            bg,
            border: border.clone(),
            radius: borderfx::radius_spec(c),
            shadows: borderfx::parse_shadows(c, size, bd, st.color),
            clip: borderfx::clips_overflow(c),
            outline: borderfx::outline(c, size, bd, st.color),
            bgl: if self.canvas_bg_node == Some(node) {
                None
            } else {
                bgimg::parse_box(
                    c,
                    [pt, pr, pb, pl],
                    [bw_t, bw_r, bw_b, bw_l],
                    size,
                    self.viewport_h,
                    self.viewport_w,
                    st.color,
                )
            },
            em: size,
        };
        let radii = deco.radius.resolve(box_w, box_h, size);
        let layers = deco
            .bgl
            .as_ref()
            .map(|b| {
                bgimg::items_for(
                    b,
                    0.0,
                    0.0,
                    box_w,
                    box_h,
                    (self.viewport_w, self.viewport_h),
                )
            })
            .unwrap_or_default();
        let mut out: Vec<DisplayItem> = Vec::new();
        borderfx::emit_deco(
            &mut out,
            0.0,
            0.0,
            box_w,
            box_h,
            deco.bg,
            deco.border.as_ref(),
            &radii,
            &deco.shadows,
            deco.clip,
            layers,
        );
        let n_deco = out.len();
        out.extend(items);
        borderfx::emit_tail(
            &mut out,
            0.0,
            0.0,
            box_w,
            box_h,
            deco.clip,
            deco.outline.as_ref(),
        );
        // clipped children can't paint hits outside the padding box
        let nhits: Vec<_> = if deco.clip {
            let bw = border.map(|b| b.w).unwrap_or([0.0; 4]);
            let (cx, cy, cw, chh) = borderfx::padding_box(0.0, 0.0, box_w, box_h, bw);
            let mut v = nhits;
            clip_hits(&mut v, 0, cx, cy, cw, chh);
            v
        } else {
            nhits
        };
        // filter/clip-path/blend/backdrop group the whole box as one
        // paint-time effect — wraps deco AND content
        let fx = colorfx::element_fx(c, size, st.color, Some([0.0, 0.0, box_w, box_h]));
        // a pushed effect group shifts the deco prefix right by one marker
        let fx_wrap = !fx.is_empty() && !out.is_empty();
        if fx_wrap {
            colorfx::wrap_vec(&mut out, 0, fx);
        }
        // translate/transform: a spec (translate included) tags the box's
        // line word or wraps its span — never applied here (double-shift)
        let xf = xform::spec(c, size);
        let (tx, ty) = if xf.is_some() {
            (0.0, 0.0)
        } else {
            translate_xy(c, box_w, box_h, size)
        };
        let nhits: Vec<_> = if tx != 0.0 || ty != 0.0 {
            out = out.into_iter().map(|it| offset_item(it, tx, ty)).collect();
            nhits
                .into_iter()
                .map(|h| xform::off_hit(h, tx, ty))
                .collect()
        } else {
            nhits
        };
        let baseline = payload_baseline(&out, n_deco);
        FBox {
            w: box_w,
            h: box_h,
            items: out,
            hits: nhits,
            n_deco: n_deco + usize::from(fx_wrap),
            deco,
            tr: (tx, ty),
            xf,
            baseline,
        }
    }

    /// Lay inline content off-screen into a private item list rooted at
    /// (0,0), `avail` px wide. Returns (items, height, hits, used width) —
    /// like `nest` but driven by a closure, so text runs and `inline()`
    /// elements work, not just element children.
    fn capture_in(
        &mut self,
        avail: f32,
        align: Align,
        f: impl FnOnce(&mut Self),
    ) -> (Vec<DisplayItem>, f32, Vec<Hit>, f32) {
        let saved_items = std::mem::take(&mut self.items);
        let saved_hits = std::mem::take(&mut self.hits);
        let saved_line = std::mem::take(&mut self.line);
        let saved_floats = std::mem::take(&mut self.floats);
        let saved = (
            self.y,
            self.margin_x,
            self.avail_w,
            self.align,
            self.line_x,
            self.pending_space,
        );
        self.y = 0.0;
        self.margin_x = 0.0;
        self.avail_w = avail.max(1.0);
        self.align = align;
        self.line_x = 0.0;
        self.pending_space = false;
        f(self);
        self.flush_line();
        let bottom = self.y;
        let items = std::mem::replace(&mut self.items, saved_items);
        let hits = std::mem::replace(&mut self.hits, saved_hits);
        self.y = saved.0;
        self.margin_x = saved.1;
        self.avail_w = saved.2;
        self.align = saved.3;
        self.line_x = saved.4;
        self.pending_space = saved.5;
        self.line = saved_line;
        self.floats = saved_floats;
        let used_w = items
            .iter()
            .map(|it| match it {
                DisplayItem::Text {
                    x,
                    text,
                    font,
                    size,
                    ..
                } => {
                    *x + self
                        .fonts
                        .font(*font)
                        .map(|f| f.measure(text, *size))
                        .unwrap_or(0.0)
                }
                DisplayItem::FillRect { x, w, .. }
                | DisplayItem::RoundedRect { x, w, .. }
                | DisplayItem::Raster { x, w, .. }
                | DisplayItem::Image { x, w, .. } => *x + *w,
                _ => 0.0,
            })
            .fold(0.0f32, f32::max);
        (items, bottom, hits, used_w)
    }

    /// (max-content, min-content) of a text run in `st`'s font.
    fn text_measure(&mut self, t: &str, st: &Style) -> (f32, f32) {
        let joined: String = t.split_whitespace().collect::<Vec<_>>().join(" ");
        if let Some(fi) = self.resolve_font(st) {
            if let Some(f) = self.fonts.font(fi) {
                let maxc = f.measure(&joined, st.size);
                let minc = t
                    .split_whitespace()
                    .map(|w| f.measure(w, st.size))
                    .fold(0.0f32, f32::max);
                return (maxc.max(1.0), minc.max(1.0));
            }
        }
        (st.size * 4.0, st.size)
    }

    /// Lay a text flex item at `w` px: words wrap inside the item box.
    fn lay_flex_text(&mut self, it: &mut FItem, w: f32) {
        let txt = it.text.clone();
        let cst = it.cst.clone();
        let (pitems, ph, phits, _uw) = self.capture_in(w, cst.align, |s| s.text(&txt, &cst));
        it.baseline = payload_baseline(&pitems, 0);
        it.items = pitems;
        it.hits = phits;
        it.w = w;
        it.h = ph.max(1.0);
        it.laid_w = w;
    }

    /// Re-lay a captured inline element (`input`/`select`/…) at `w` px.
    fn lay_flex_capture(&mut self, it: &mut FItem, w: f32) {
        let id = it.id;
        let tag = it.tag.clone();
        let cc = it.cc.clone().unwrap_or_default();
        let cst = it.cst.clone();
        let (pitems, ph, phits, uw) =
            self.capture_in(w, cst.align, |s| s.inline(id, &tag, &cc, cst));
        it.baseline = payload_baseline(&pitems, 0);
        it.items = pitems;
        it.hits = phits;
        it.w = uw.max(1.0);
        it.h = ph.max(1.0);
        it.laid_w = w;
    }

    /// `display:flex` — order-aware, wrapping-capable line layout.
    /// Emits items directly (no shared text line), so both axes and paint
    /// order follow `order` and wrap lines.
    pub(crate) fn flex_layout(&mut self, node: NodeId, c: &Props, st: Style) {
        self.flush_line();
        let dir = c.get("flex-direction").map(String::as_str).unwrap_or("row");
        let (col, rev) = match dir {
            "column" => (true, false),
            "column-reverse" => (true, true),
            "row-reverse" => (false, true),
            _ => (false, false),
        };
        let wrap_v = c.get("flex-wrap").map(String::as_str);
        let wrap = matches!(wrap_v, Some("wrap") | Some("wrap-reverse"));
        let wrap_rev = wrap_v == Some("wrap-reverse");
        // `gap` shorthand then axis longhands: row-gap runs along the block
        // axis (% vs the container's definite height), column-gap along the
        // inline axis (% vs the width).
        let hrel = self.pct_h.unwrap_or(self.viewport_h);
        let mut row_gap = 0.0f32;
        let mut col_gap = 0.0f32;
        if let Some(v) = c.get("gap") {
            let mut it = v.split_whitespace();
            if let Some(t) = it.next() {
                row_gap = parse_len(t, hrel, st.size).unwrap_or(0.0);
                col_gap = it
                    .next()
                    .and_then(|t| parse_len(t, self.avail_w, st.size))
                    .unwrap_or(row_gap);
            }
        }
        if let Some(v) = c.get("row-gap") {
            row_gap = parse_len(v, hrel, st.size).unwrap_or(row_gap);
        }
        if let Some(v) = c.get("column-gap") {
            col_gap = parse_len(v, self.avail_w, st.size).unwrap_or(col_gap);
        }
        let (main_gap, cross_gap) = if col {
            (row_gap, col_gap)
        } else {
            (col_gap, row_gap)
        };
        // container content-box extents: row main = the width (always
        // definite here); column main = height (definite only via pct_h).
        // Cross is the other axis: always definite for column, definite for
        // row only when the container has a resolvable height.
        let cont_main = if col {
            self.pct_h
        } else {
            Some(self.content_width())
        };
        let cont_cross = if col {
            Some(self.content_width())
        } else {
            self.pct_h
        };
        let (x0, y0) = (self.margin_x, self.y);
        // margin side indices: (main-lead, main-trail, cross-lead, cross-trail)
        let (ml_i, mt_i, kc_i, ke_i) = if col { (0, 2, 3, 1) } else { (3, 1, 0, 2) };
        let box_main = |it: &FItem| if col { it.h } else { it.w };
        let box_cross = |it: &FItem| if col { it.w } else { it.h };
        let cont_align = falign(c.get("align-items"))
            .or_else(|| {
                falign(
                    c.get("place-items")
                        .and_then(|v| v.split_whitespace().next().map(String::from))
                        .as_ref(),
                )
            })
            .unwrap_or(FAlign::Stretch);
        // physical left/right don't apply to a column's block main axis
        let jc = c.get("justify-content").cloned().or_else(|| {
            c.get("place-content")
                .and_then(|v| v.split_whitespace().nth(1).map(String::from))
        });
        let jc_norm = if col
            && matches!(
                jc.as_deref()
                    .map(|s| s.split_whitespace().last().unwrap_or("")),
                Some("left") | Some("right")
            ) {
            None
        } else {
            jc
        };
        let ac = c.get("align-content").cloned().or_else(|| {
            c.get("place-content")
                .and_then(|v| v.split_whitespace().next().map(String::from))
        });

        // ---- collect items --------------------------------------------
        let mut items: Vec<FItem> = Vec::new();
        let mut oof: Vec<NodeId> = Vec::new();
        for (dom, &ch) in self.doc.children(node).iter().enumerate() {
            match self.doc.kind(ch) {
                NodeKind::Text(t) => {
                    // whitespace between items collapses in real flexbox
                    if t.trim().is_empty() {
                        continue;
                    }
                    let (maxc, minc) = self.text_measure(t, &st);
                    let mut it = FItem::new(ch, dom, FKind::Text, st.clone());
                    it.text = t.clone();
                    it.align = cont_align;
                    if col {
                        // main = height: lay now at the resolved cross width
                        let w = if it.align == FAlign::Stretch {
                            cont_cross.unwrap_or(maxc)
                        } else {
                            maxc.min(cont_cross.unwrap_or(maxc))
                        }
                        .max(1.0);
                        self.lay_flex_text(&mut it, w);
                        it.base = it.h;
                    } else {
                        it.base = maxc;
                        it.floor = minc.max(1.0);
                        it.cross_auto = true;
                    }
                    items.push(it);
                }
                NodeKind::Element { .. } => {
                    let cc = self.style(ch);
                    if cc.get("display").map(String::as_str) == Some("none") {
                        continue;
                    }
                    // out-of-flow children aren't flex items — they still
                    // paint, laid out after the lines (below)
                    if matches!(
                        cc.get("position").map(String::as_str),
                        Some("absolute") | Some("fixed")
                    ) {
                        oof.push(ch);
                        continue;
                    }
                    let mut cst = st.clone();
                    self.apply_computed(&cc, &mut cst);
                    let size = cst.size;
                    let m = box_sides(&cc, "margin", self.avail_w, size);
                    let auto = margin_autos(&cc);
                    let tag = self.doc.tag(ch).unwrap_or("").to_string();
                    let kind = match tag.as_str() {
                        "img" | "svg" => FKind::Replaced,
                        "input" | "select" | "textarea" | "video" | "canvas" | "iframe"
                        | "object" | "embed" | "br" | "hr" => FKind::Capture,
                        _ => FKind::El,
                    };
                    let main_rel = if col { self.pct_h } else { Some(self.avail_w) };
                    let (g, s, basis) = flex_props(&cc, main_rel, size);
                    let order = cc
                        .get("order")
                        .and_then(|v| v.trim().parse::<i32>().ok())
                        .unwrap_or(0);
                    let align = falign(cc.get("align-self")).unwrap_or(cont_align);
                    let [pt, pr, pb, pl] =
                        box_sides(&cc, "padding", self.avail_w, size).map(|v| v.max(0.0));
                    let bw = borderfx::borders(&cc, size, cst.color)
                        .map(|b| b.w)
                        .unwrap_or([0.0; 4]);
                    let bb = border_box(&cc);
                    // box extras on each axis, for border-box conversion
                    let main_pad = if col {
                        pt + pb + bw[0] + bw[2]
                    } else {
                        pl + pr + bw[1] + bw[3]
                    };
                    let cross_pad = if col {
                        pl + pr + bw[1] + bw[3]
                    } else {
                        pt + pb + bw[0] + bw[2]
                    };
                    let (cp, mnp, mxp) = if col {
                        ("width", "min-width", "max-width")
                    } else {
                        ("height", "min-height", "max-height")
                    };
                    let cross_rel = if col { self.avail_w } else { hrel };
                    let to_bb = |v: f32| if bb { v } else { v + cross_pad };
                    let fixed_cross = dim(&cc, cp, cross_rel, size).map(to_bb);
                    let min_cross = dim(&cc, mnp, cross_rel, size).map(to_bb).unwrap_or(0.0);
                    let max_cross = dim(&cc, mxp, cross_rel, size)
                        .map(to_bb)
                        .unwrap_or(f32::MAX);
                    let mut it = FItem::new(ch, dom, kind, cst.clone());
                    it.cc = Some(Rc::clone(&cc));
                    it.tag = tag;
                    it.m = m;
                    it.auto = auto;
                    it.grow = g;
                    it.shrink = s;
                    it.align = align;
                    it.cross_auto = fixed_cross.is_none() && (col || aspect_ratio(&cc).is_none());
                    it.min_cross = min_cross;
                    it.max_cross = max_cross;
                    it.order = order;
                    match kind {
                        FKind::Replaced => {
                            if let Some((w, h, pitems, phits)) =
                                self.replaced_payload(ch, &it.tag, &cc, &cst)
                            {
                                let (tx, ty) = translate_xy(&cc, w, h, size);
                                let (pitems, phits) = if tx != 0.0 || ty != 0.0 {
                                    (
                                        pitems
                                            .into_iter()
                                            .map(|i2| offset_item(i2, tx, ty))
                                            .collect(),
                                        phits
                                            .into_iter()
                                            .map(|h2| xform::off_hit(h2, tx, ty))
                                            .collect(),
                                    )
                                } else {
                                    (pitems, phits)
                                };
                                it.w = w;
                                it.h = h;
                                it.items = pitems;
                                it.hits = phits;
                                it.baseline = Some(h + m[2].max(0.0));
                                it.base = match basis {
                                    FBasis::Px(v) => {
                                        if bb {
                                            v
                                        } else {
                                            v + main_pad
                                        }
                                    }
                                    _ => {
                                        if col {
                                            h
                                        } else {
                                            w
                                        }
                                    }
                                };
                                // atomic — can't shrink below its own size
                                it.floor = it.base.max(1.0);
                                it.cross_auto = false;
                            }
                        }
                        FKind::Capture => {
                            let (pitems, ph, phits, uw) = self.capture_in(
                                cont_main.unwrap_or_else(|| self.content_width()),
                                cst.align,
                                |s| s.inline(ch, &it.tag, &cc, cst.clone()),
                            );
                            it.baseline = payload_baseline(&pitems, 0);
                            it.items = pitems;
                            it.hits = phits;
                            it.laid_w = uw.max(1.0);
                            it.w = it.laid_w;
                            it.h = ph.max(1.0);
                            it.base = match basis {
                                FBasis::Px(v) => {
                                    if bb {
                                        v
                                    } else {
                                        v + main_pad
                                    }
                                }
                                _ => {
                                    if col {
                                        it.h
                                    } else {
                                        it.w
                                    }
                                }
                            };
                            it.floor = size;
                        }
                        _ => {
                            if col {
                                // main = height: measure-lay at the resolved
                                // cross width (stretch → fill, width prop →
                                // item_box, auto → shrink-to-fit)
                                let hint = if it.cross_auto && it.align == FAlign::Stretch {
                                    Some(
                                        (cont_cross.unwrap_or_else(|| self.content_width())
                                            - m[3]
                                            - m[1])
                                            .max(1.0),
                                    )
                                } else {
                                    None
                                };
                                let fb = self.item_box(ch, &cc, cst.clone(), hint, None);
                                it.w = fb.w;
                                it.h = fb.h;
                                it.items = fb.items;
                                it.hits = fb.hits;
                                it.n_deco = fb.n_deco;
                                it.deco = Some(fb.deco);
                                it.tr = fb.tr;
                                it.baseline = fb.baseline;
                                it.laid_w = fb.w;
                                it.base = match basis {
                                    FBasis::Px(v) => {
                                        if bb {
                                            v
                                        } else {
                                            v + main_pad
                                        }
                                    }
                                    _ => fb.h,
                                };
                                it.floor = dim(&cc, "min-height", hrel, size)
                                    .map(|v| if bb { v } else { v + main_pad })
                                    .unwrap_or(0.0);
                                it.ceil = dim(&cc, "max-height", hrel, size)
                                    .map(|v| if bb { v } else { v + main_pad })
                                    .unwrap_or(f32::MAX);
                                it.base = it.base.clamp(it.floor, it.ceil);
                            } else {
                                it.base = match basis {
                                    FBasis::Px(v) => {
                                        if bb {
                                            v
                                        } else {
                                            v + main_pad
                                        }
                                    }
                                    FBasis::Auto => dim(&cc, "width", self.avail_w, size)
                                        .map(|v| if bb { v } else { v + main_pad })
                                        .unwrap_or_else(|| {
                                            self.max_content_width(ch, &cst) + main_pad
                                        }),
                                    FBasis::MinC => self.shrink_width(ch, &cst) + main_pad,
                                    FBasis::MaxC => self.max_content_width(ch, &cst) + main_pad,
                                    FBasis::FitC => {
                                        self.max_content_width(ch, &cst).min(self.content_width())
                                            + main_pad
                                    }
                                };
                                // min-width else min-content floor, like real
                                // flexbox's automatic minimum
                                let minc = self.shrink_width(ch, &cst) + main_pad;
                                it.floor = dim(&cc, "min-width", self.avail_w, size)
                                    .map(|v| if bb { v } else { v + main_pad })
                                    .unwrap_or(minc)
                                    .max(1.0);
                                it.ceil = dim(&cc, "max-width", self.avail_w, size)
                                    .map(|v| if bb { v } else { v + main_pad })
                                    .unwrap_or(f32::MAX);
                                it.base = it.base.clamp(it.floor, it.ceil);
                            }
                        }
                    }
                    items.push(it);
                }
                _ => {}
            }
        }
        if items.is_empty() {
            self.flex_emit_oof(oof, &st);
            return;
        }

        // `order` reorders items before line breaking; DOM order is the
        // stable tie-break — painting follows the same sequence.
        items.sort_by(|a, b| a.order.cmp(&b.order).then(a.dom.cmp(&b.dom)));

        // ---- pack items into lines -------------------------------------
        let mut lines: Vec<FLine> = Vec::new();
        if !wrap || cont_main.is_none() || items.len() <= 1 {
            let span = items
                .iter()
                .map(|it| it.base + it.m[ml_i] + it.m[mt_i])
                .sum::<f32>()
                + main_gap * items.len().saturating_sub(1) as f32;
            lines.push(FLine {
                s: 0,
                e: items.len(),
                cross: 0.0,
                bmax: 0.0,
                bshift: 0.0,
                span,
            });
        } else {
            let cm = cont_main.unwrap();
            let mut s = 0usize;
            let mut span = 0.0f32;
            for i in 0..items.len() {
                let outer = items[i].base + items[i].m[ml_i] + items[i].m[mt_i];
                let step = if i > s { outer + main_gap } else { outer };
                if i > s && span + step > cm + 0.5 {
                    lines.push(FLine {
                        s,
                        e: i,
                        cross: 0.0,
                        bmax: 0.0,
                        bshift: 0.0,
                        span,
                    });
                    s = i;
                    span = outer;
                } else {
                    span += step;
                }
            }
            lines.push(FLine {
                s,
                e: items.len(),
                cross: 0.0,
                bmax: 0.0,
                bshift: 0.0,
                span,
            });
        }

        // ---- per-line main sizing: shrink, grow, auto margins, justify --
        for l in &mut lines {
            let (s, e) = (l.s, l.e);
            let n = e - s;
            let outer_of =
                |items: &[FItem], i: usize| items[i].base + items[i].m[ml_i] + items[i].m[mt_i];
            let mut total: f32 =
                (s..e).map(|i| outer_of(&items, i)).sum::<f32>() + main_gap * (n - 1) as f32;
            let mut free = cont_main.map(|cm| cm - total);
            // overflow → shrink ∝ shrink×base, floored at min-width /
            // min-content (one pass, like the old row path)
            if let Some(f) = free {
                if f < -0.01 {
                    let sw: f32 = items[s..e]
                        .iter()
                        .map(|it| {
                            if it.shrink > 0.0 {
                                it.shrink * it.base
                            } else {
                                0.0
                            }
                        })
                        .sum();
                    if sw > 0.0 {
                        for it in &mut items[s..e] {
                            if it.shrink > 0.0 {
                                let fl = it.floor.min(it.base);
                                it.base = (it.base + f * it.shrink * it.base / sw).max(fl);
                            }
                        }
                        total = (s..e).map(|i| outer_of(&items, i)).sum::<f32>()
                            + main_gap * (n - 1) as f32;
                        free = cont_main.map(|cm| cm - total);
                    }
                }
            }
            // leftover space → grow items ∝ grow, capped at max-size
            let sg: f32 = items[s..e].iter().map(|it| it.grow).sum();
            if let Some(f) = free {
                if f > 0.01 && sg > 0.0 {
                    for it in &mut items[s..e] {
                        if it.grow > 0.0 {
                            it.base = (it.base + f * it.grow / sg).min(it.ceil);
                        }
                    }
                    total = (s..e).map(|i| outer_of(&items, i)).sum::<f32>()
                        + main_gap * (n - 1) as f32;
                    free = cont_main.map(|cm| cm - total);
                }
            }
            // auto main margins absorb what's left (spec: before justify —
            // any auto margin or grow disables justification)
            let n_auto: usize = items[s..e]
                .iter()
                .map(|it| it.auto[ml_i] as usize + it.auto[mt_i] as usize)
                .sum();
            let auto_share = if sg > 0.0 || n_auto == 0 {
                0.0
            } else {
                free.unwrap_or(0.0).max(0.0) / n_auto as f32
            };
            if auto_share > 0.0 {
                for it in &mut items[s..e] {
                    if it.auto[ml_i] {
                        it.m[ml_i] += auto_share;
                    }
                    if it.auto[mt_i] {
                        it.m[mt_i] += auto_share;
                    }
                }
                total =
                    (s..e).map(|i| outer_of(&items, i)).sum::<f32>() + main_gap * (n - 1) as f32;
                free = cont_main.map(|cm| cm - total);
            }
            let extra = if sg > 0.0 || n_auto > 0 {
                0.0
            } else {
                free.unwrap_or(0.0).max(0.0)
            };
            let (gap_add, start_off) = if extra > 0.01 {
                content_offset(jc_norm.as_ref(), extra, n)
            } else {
                (0.0, 0.0)
            };
            let mut cur = start_off;
            for (j, it) in items[s..e].iter_mut().enumerate() {
                it.main = it.base;
                it.pos = cur;
                cur += it.base + it.m[ml_i] + it.m[mt_i];
                if j + 1 < n {
                    cur += main_gap + gap_add;
                }
            }
            let _ = total;
        }

        // ---- lay each item at its resolved size; line cross sizes ------
        for l in &mut lines {
            let (s, e) = (l.s, l.e);
            for i in s..e {
                let it = &mut items[i];
                match it.kind {
                    FKind::El => {
                        if col {
                            // flexed main size → repaint box decoration
                            if (it.main - it.h).abs() > 0.01 {
                                let d = it.deco.clone().unwrap();
                                self.restretch(&mut it.items, it.n_deco, &d, it.tr, it.w, it.main);
                                it.h = it.main;
                            }
                        } else {
                            let cc = it.cc.clone().unwrap();
                            let cst = it.cst.clone();
                            let main = it.main;
                            let fb = self.item_box(it.id, &cc, cst, Some(main), None);
                            it.w = fb.w;
                            it.h = fb.h;
                            it.items = fb.items;
                            it.hits = fb.hits;
                            it.n_deco = fb.n_deco;
                            it.deco = Some(fb.deco);
                            it.tr = fb.tr;
                            it.baseline = fb.baseline;
                            it.laid_w = fb.w;
                        }
                    }
                    FKind::Replaced => {
                        // stretch the replaced payload to the flexed main size
                        if col {
                            if (it.main - it.h).abs() > 0.01 {
                                for d in &mut it.items {
                                    match d {
                                        DisplayItem::Image { h, .. }
                                        | DisplayItem::Raster { h, .. }
                                        | DisplayItem::FillRect { h, .. }
                                        | DisplayItem::RoundedRect { h, .. } => *h = it.main,
                                        _ => {}
                                    }
                                }
                                for hd in &mut it.hits {
                                    hd.3 = it.main;
                                }
                                it.h = it.main;
                            }
                        } else if (it.main - it.w).abs() > 0.01 {
                            for d in &mut it.items {
                                match d {
                                    DisplayItem::Image { w, .. }
                                    | DisplayItem::Raster { w, .. }
                                    | DisplayItem::FillRect { w, .. }
                                    | DisplayItem::RoundedRect { w, .. } => *w = it.main,
                                    _ => {}
                                }
                            }
                            for hd in &mut it.hits {
                                hd.2 = it.main;
                            }
                            it.w = it.main;
                        }
                    }
                    FKind::Text => {
                        if !col && (it.laid_w - it.main).abs() > 0.5 {
                            self.lay_flex_text(it, it.main);
                        }
                    }
                    FKind::Capture => {
                        if !col && (it.laid_w - it.main).abs() > 0.5 {
                            self.lay_flex_capture(it, it.main);
                        }
                    }
                }
            }
            // line cross = largest margin-box; nowrap's single line takes
            // the container's definite cross instead (align-*:stretch etc.)
            let mut cross = if !wrap {
                cont_cross.unwrap_or(0.0)
            } else {
                0.0
            };
            for it in &items[s..e] {
                cross = cross.max(box_cross(it) + it.m[kc_i] + it.m[ke_i]);
            }
            // baseline alignment (row direction): shared baseline = the max
            // margin-top + first-baseline of baseline-aligned items
            let mut bmax = 0.0f32;
            let mut bmin = 0.0f32;
            if !col {
                for it in &items[s..e] {
                    if it.align == FAlign::Baseline {
                        let b = it.baseline.unwrap_or(it.h);
                        bmax = bmax.max(it.m[0] + b);
                    }
                }
                if bmax > 0.0 {
                    for it in &items[s..e] {
                        if it.align == FAlign::Baseline {
                            let b = it.baseline.unwrap_or(it.h);
                            let top = bmax - b - it.m[0];
                            bmin = bmin.min(top);
                            cross = cross.max(top + it.h + it.m[0] + it.m[2]);
                        }
                    }
                }
            }
            l.bshift = (-bmin).max(0.0);
            l.bmax = bmax + l.bshift;
            l.cross = cross + l.bshift;
        }

        // ---- line bands along the cross axis (align-content) -----------
        let nl = lines.len();
        if wrap && nl > 0 {
            if let Some(ccw) = cont_cross {
                let total_c = lines.iter().map(|l| l.cross).sum::<f32>()
                    + cross_gap * nl.saturating_sub(1) as f32;
                let extra = ccw - total_c;
                if extra > 0.01 {
                    let kw = ac
                        .as_deref()
                        .and_then(|s| s.split_whitespace().next())
                        .unwrap_or("stretch");
                    if kw == "stretch" {
                        // leftover cross space grows every line equally
                        for l in &mut lines {
                            l.cross += extra / nl as f32;
                        }
                    }
                }
            }
        }
        let total_c =
            lines.iter().map(|l| l.cross).sum::<f32>() + cross_gap * nl.saturating_sub(1) as f32;
        let (mut ac_gap, mut ac_start) = (0.0f32, 0.0f32);
        if wrap {
            if let Some(ccw) = cont_cross {
                let extra = (ccw - total_c).max(0.0);
                if extra > 0.01 {
                    let kw = ac
                        .as_deref()
                        .and_then(|s| s.split_whitespace().next())
                        .unwrap_or("stretch");
                    if kw != "stretch" {
                        let (g, st2) = content_offset(ac.as_ref(), extra, nl);
                        ac_gap = g;
                        ac_start = st2;
                    }
                }
            }
        }
        let mut bands = vec![0.0f32; nl];
        let mut cur = ac_start;
        for (i, l) in lines.iter().enumerate() {
            bands[i] = cur;
            cur += l.cross + cross_gap + ac_gap;
        }
        let band_end = if nl > 0 {
            bands[nl - 1] + lines[nl - 1].cross
        } else {
            0.0
        };
        // wrap-reverse mirrors the band sequence inside the cross span
        let span_c = cont_cross.unwrap_or(band_end).max(band_end);

        // ---- emit ------------------------------------------------------
        let mut max_main_end = 0.0f32;
        for (li, l) in lines.iter().enumerate() {
            let band = if wrap_rev {
                span_c - bands[li] - l.cross
            } else {
                bands[li]
            };
            let main_span = cont_main.unwrap_or(l.span);
            for i in l.s..l.e {
                let it = &mut items[i];
                // auto cross margins absorb leftover cross space first
                let mut mkc = it.m[kc_i];
                let mut mke = it.m[ke_i];
                let nac = it.auto[kc_i] as usize + it.auto[ke_i] as usize;
                if nac > 0 {
                    let share = ((l.cross - box_cross(it) - mkc - mke) / nac as f32).max(0.0);
                    if it.auto[kc_i] {
                        mkc += share;
                    }
                    if it.auto[ke_i] {
                        mke += share;
                    }
                }
                // stretch fills the line's cross (auto cross size only)
                if it.align == FAlign::Stretch && it.cross_auto {
                    let target = (l.cross - mkc - mke).clamp(it.min_cross, it.max_cross);
                    match it.kind {
                        FKind::El => {
                            if col {
                                // cross = width → reflow at the new width
                                if (it.w - target).abs() > 0.5 {
                                    let cc = it.cc.clone().unwrap();
                                    let cst = it.cst.clone();
                                    let fb = self.item_box(it.id, &cc, cst, Some(target), None);
                                    it.w = fb.w;
                                    it.h = fb.h;
                                    it.items = fb.items;
                                    it.hits = fb.hits;
                                    it.n_deco = fb.n_deco;
                                    it.deco = Some(fb.deco);
                                    it.tr = fb.tr;
                                    it.baseline = fb.baseline;
                                    // the box still paints at its flexed main
                                    if (it.main - it.h).abs() > 0.01 {
                                        let d = it.deco.clone().unwrap();
                                        self.restretch(
                                            &mut it.items,
                                            it.n_deco,
                                            &d,
                                            it.tr,
                                            it.w,
                                            it.main,
                                        );
                                        it.h = it.main;
                                    }
                                }
                            } else if (it.h - target).abs() > 0.5 {
                                let d = it.deco.clone().unwrap();
                                self.restretch(&mut it.items, it.n_deco, &d, it.tr, it.w, target);
                                it.h = target;
                            }
                        }
                        FKind::Replaced => {}
                        _ => {
                            // text/capture items have no box to stretch —
                            // track the target for alignment only
                            if !col && (it.h - target).abs() > 0.5 {
                                it.h = target;
                            }
                        }
                    }
                }
                // margin-box offset within the band
                let outer_k = box_cross(it) + mkc + mke;
                let off = if nac > 0 {
                    l.bshift
                } else {
                    match it.align {
                        FAlign::Start | FAlign::Stretch => l.bshift,
                        FAlign::End => l.bshift + (l.cross - outer_k).max(0.0),
                        FAlign::Center => l.bshift + ((l.cross - outer_k) / 2.0).max(0.0),
                        FAlign::Baseline => {
                            if col {
                                l.bshift
                            } else {
                                let b = it.baseline.unwrap_or(it.h);
                                l.bmax - b - mkc
                            }
                        }
                    }
                };
                let bcross = band + off + mkc;
                let outer_m = box_main(it) + it.m[ml_i] + it.m[mt_i];
                let bmain = if rev {
                    main_span - it.pos - outer_m + it.m[mt_i]
                } else {
                    it.pos + it.m[ml_i]
                };
                let (dx, dy) = if col {
                    (x0 + bcross, y0 + bmain)
                } else {
                    (x0 + bmain, y0 + bcross)
                };
                let (i0, h0) = (self.items.len(), self.hits.len());
                for d in it.items.drain(..) {
                    self.items.push(offset_item(d, dx, dy));
                }
                for hd in it.hits.iter() {
                    self.hits.push(xform::off_hit(*hd, dx, dy));
                }
                // the item's own border box joins the hit map
                if it.kind != FKind::Text && it.kind != FKind::Replaced {
                    self.hits
                        .push((dx, dy, it.w, it.h, it.id, xform::Mat::IDENTITY));
                }
                // a transformed item wraps its emitted span (hit matrices
                // compose inside wrap_span)
                if let Some(sp) = it.cc.as_ref().and_then(|cc| xform::spec(cc, it.cst.size)) {
                    self.wrap_span(&sp, it.id, i0, h0);
                }
                max_main_end = max_main_end.max(bmain + box_main(it) + it.m[mt_i]);
            }
        }

        if col {
            self.y = y0 + max_main_end.max(0.0);
        } else {
            self.y = y0 + band_end.max(0.0);
        }

        // out-of-flow children paint last, as plain blocks (position
        // offsets resolve against the nearest positioned ancestor's cb)
        self.flex_emit_oof(oof, &st);
    }

    /// Absolutely-positioned flex children: painted after the items, laid
    /// as ordinary blocks.
    fn flex_emit_oof(&mut self, oof: Vec<NodeId>, st: &Style) {
        if oof.is_empty() {
            return;
        }
        self.flush_line();
        for ch in oof {
            let cc = self.style(ch);
            let mut cst = st.clone();
            self.apply_computed(&cc, &mut cst);
            self.block(ch, self.doc.tag(ch).unwrap_or("div"), &cc, cst);
        }
    }
}
