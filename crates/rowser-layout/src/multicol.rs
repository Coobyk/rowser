//! Multi-column layout (CSS Multi-column Level 1 subset).
//!
//! A block with `columns`/`column-count`/`column-width` flows its children
//! into `n` balanced columns of equal width separated by `column-gap` (with
//! an optional `column-rule` between them). `column-fill:balance` (default)
//! picks the smallest column content height that fits the content in `n`
//! columns; `column-fill:auto` fills each column up to the container's
//! definite height before starting the next.
//!
//! Implementation: the segment's children are laid out once off-screen at
//! column width (`nest_units`), tracking block-level unit boundaries. Each
//! emitted item is then assigned to a column *band* — a window `[s_k,
//! s_k + H)` of the natural (single-column) y-axis — and re-emitted offset
//! into that column's x position. Rects straddling a band boundary are
//! split; text/images land whole in the band holding their top edge. Units
//! with `break-inside:avoid` stay atomic (shifted to the next band when they
//! don't fit); `break-before/after:column` (and the `page-break-*` aliases)
//! start/end bands early. `column-span:all` children split the flow into
//! segments laid full width between column sets.

use super::*;

/// Does this element establish a multi-column layout?
pub(crate) fn multicol_on(c: &Props) -> bool {
    if let Some(v) = c.get("columns") {
        let v = v.trim();
        if !v.is_empty() && v != "auto" {
            return true;
        }
    }
    if c.get("column-count")
        .is_some_and(|v| v.trim().parse::<f32>().is_ok_and(|n| n >= 1.0))
    {
        return true;
    }
    c.get("column-width")
        .is_some_and(|v| !v.trim().is_empty() && v.trim() != "auto")
}

/// A laid block-level unit in the natural single-column flow.
struct ColUnit {
    i0: usize,
    i1: usize,
    h0: usize,
    h1: usize,
    y0: f32,
    y1: f32,
    /// break-before:column (or page-break-before) on the unit's element
    force_break: bool,
    /// break-inside:avoid — keep the unit in one column
    avoid: bool,
}

/// Block-level display values: children with these become their own units;
/// everything else merges into shared inline runs whose lines may split.
fn col_blockish(d: Option<&str>) -> bool {
    matches!(
        d,
        Some("block")
            | Some("list-item")
            | Some("flow-root")
            | Some("flex")
            | Some("grid")
            | Some("table")
            | Some("table-row")
            | Some("table-cell")
            | Some("table-caption")
            | Some("table-row-group")
            | Some("table-header-group")
            | Some("table-footer-group")
    )
}

/// break-before/after value → does it force a column break?
fn col_breaks(v: Option<&String>) -> bool {
    matches!(
        v.map(|s| s.trim()),
        Some("column") | Some("always") | Some("page") | Some("left") | Some("right")
    )
}

/// break-inside value → avoid splitting?
fn col_avoid(v: Option<&String>) -> bool {
    matches!(
        v.map(|s| s.trim()),
        Some("avoid") | Some("avoid-column") | Some("avoid-page")
    )
}

/// Top edge of an item's band footprint (text: approx line top).
fn item_top(it: &DisplayItem) -> f32 {
    match it {
        DisplayItem::Text { baseline, size, .. } => baseline - 0.8 * size,
        DisplayItem::FillRect { y, .. }
        | DisplayItem::RoundedRect { y, .. }
        | DisplayItem::Raster { y, .. }
        | DisplayItem::Image { y, .. } => *y,
    }
}

/// Bottom edge of an item's band footprint.
fn item_bottom(it: &DisplayItem) -> f32 {
    match it {
        DisplayItem::Text { baseline, size, .. } => baseline + 0.3 * size,
        DisplayItem::FillRect { y, h, .. }
        | DisplayItem::RoundedRect { y, h, .. }
        | DisplayItem::Raster { y, h, .. }
        | DisplayItem::Image { y, h, .. } => y + h,
    }
}

impl<'a> Layouter<'a> {
    /// Lay `node`'s children into `n` balanced columns inside the current
    /// block context (self.y/margin_x/avail_w already account for the
    /// element's own padding/border).
    pub(crate) fn multicol_children(&mut self, node: NodeId, c: &Props, st: Style) {
        let em = st.size;
        let avail = self.content_width().max(1.0);
        let vh = self.viewport_h;
        // column-gap: normal ⇒ 1em
        let gap = match c.get("column-gap") {
            Some(v) if v.trim() != "normal" => {
                parse_len_vp(v, avail, em, vh, avail).unwrap_or(em).max(0.0)
            }
            _ => em,
        };
        // columns shorthand + longhands
        let (mut cnt, mut cw) = (None::<usize>, None::<f32>);
        if let Some(v) = c.get("columns") {
            for t in v.split_whitespace() {
                if t == "auto" {
                    continue;
                }
                if let Ok(k) = t.parse::<f32>() {
                    cnt = Some(k.max(1.0) as usize);
                } else if let Some(l) = parse_len_vp(t, avail, em, vh, avail) {
                    cw = Some(l);
                }
            }
        }
        if let Some(v) = c.get("column-count") {
            if let Ok(k) = v.trim().parse::<f32>() {
                cnt = Some(k.max(1.0) as usize);
            }
        }
        if let Some(v) = c.get("column-width") {
            if v.trim() != "auto" {
                cw = parse_len_vp(v, avail, em, vh, avail).or(cw);
            }
        }
        let n = match (cnt, cw) {
            (Some(n), Some(w)) => {
                n.min(((avail + gap) / (w + gap)).floor().max(1.0) as usize)
            }
            (Some(n), None) => n,
            (None, Some(w)) => ((avail + gap) / (w + gap)).floor().max(1.0) as usize,
            (None, None) => 1,
        }
        .clamp(1, 64);
        if n <= 1 {
            for &ch in self.doc.children(node) {
                self.walk(ch, &st);
            }
            return;
        }
        let col_w = ((avail - gap * (n - 1) as f32) / n as f32).max(1.0);

        // column-rule: painted only when a style keyword is present
        let bd = self.backdrop;
        let (mut rw, mut rstyle, mut rcol) = (0.0f32, "none", st.color);
        if let Some(v) = c.get("column-rule") {
            for t in v.split_whitespace() {
                if matches!(
                    t,
                    "none" | "hidden" | "solid" | "dashed" | "dotted" | "double"
                        | "groove" | "ridge" | "inset" | "outset"
                ) {
                    rstyle = match t {
                        "solid" => "solid",
                        _ => t,
                    };
                } else if let Some(w) = parse_len(t, avail, em) {
                    rw = w;
                } else if let Some(col) = parse_color(t) {
                    rcol = over(col, bd);
                }
            }
        }
        if let Some(v) = c.get("column-rule-width") {
            rw = parse_len(v, avail, em).unwrap_or(rw);
        }
        if let Some(v) = c.get("column-rule-style") {
            rstyle = match v.trim() {
                "none" | "hidden" => "none",
                _ => "solid",
            };
        }
        if let Some(v) = c.get("column-rule-color") {
            if let Some(col) = parse_color(v) {
                rcol = over(col, bd);
            }
        }
        let rule = if !matches!(rstyle, "none" | "hidden") && rw > 0.0 {
            Some((rw, rcol))
        } else {
            None
        };
        // column-fill:auto fills each column to a definite height; balance
        // (or an indefinite height) equalizes column heights instead
        let fill_h = if c.get("column-fill").map(String::as_str) == Some("auto") {
            dim(c, "height", self.pct_h.unwrap_or(vh), em).or(self.inset_h)
        } else {
            None
        };

        let x0 = self.line_origin();
        // split children into segments at column-span:all elements
        let kids: &[NodeId] = self.doc.children(node);
        let mut seg_start = 0usize;
        let mut i = 0usize;
        while i <= kids.len() {
            let spanner = if i < kids.len() && self.is_col_spanner(kids[i]) {
                Some(kids[i])
            } else {
                None
            };
            if spanner.is_some() || i == kids.len() {
                if seg_start < i {
                    self.multicol_segment(&kids[seg_start..i], &st, x0, col_w, n, gap, rule, fill_h);
                }
                if let Some(sp) = spanner {
                    self.flush_line();
                    self.walk(sp, &st);
                }
                seg_start = i + 1;
            }
            i += 1;
        }
    }

    /// `column-span:all` child?
    fn is_col_spanner(&mut self, ch: NodeId) -> bool {
        if !matches!(self.doc.kind(ch), NodeKind::Element { .. }) {
            return false;
        }
        self.style(ch).get("column-span").map(String::as_str) == Some("all")
    }

    /// One segment of children (between column-span:all elements): lay once
    /// at column width, then slice items into `n` column bands.
    #[allow(clippy::too_many_arguments)]
    fn multicol_segment(
        &mut self,
        kids: &[NodeId],
        st: &Style,
        x0: f32,
        col_w: f32,
        n: usize,
        gap: f32,
        rule: Option<(f32, Color)>,
        fill_h: Option<f32>,
    ) {
        let top = self.y;
        let (items, hits, bottom, units) = self.nest_units(kids, st, col_w);
        if items.is_empty() || bottom <= 0.0 {
            return;
        }
        // column content height: fill:auto ⇒ definite height; balance ⇒
        // smallest H fitting the content in n bands (each avoid-unit must
        // fit in a single band as well)
        let mut h = match fill_h {
            Some(h) => h.max(1.0),
            None => bottom / n as f32,
        };
        for u in &units {
            if u.avoid {
                h = h.max(u.y1 - u.y0);
            }
        }
        // assign every item/hit a band index; band k covers natural range
        // [s_k, s_k + H). Forced/avoid units start a band at their own top.
        let mut band_of_item: Vec<usize> = vec![0; items.len()];
        let mut band_of_hit: Vec<usize> = vec![0; hits.len()];
        let mut starts: Vec<f32> = vec![0.0];
        // For balance, too many bands ⇒ grow H until the content fits.
        for _try in 0..24 {
            band_of_item.fill(0);
            band_of_hit.fill(0);
            starts.clear();
            starts.push(0.0);
            let (mut k, mut s) = (0usize, 0.0f32);
            for u in &units {
                if u.force_break {
                    k += 1;
                    s = u.y0;
                    if starts.len() <= k {
                        starts.push(s);
                    } else {
                        starts[k] = s;
                    }
                }
                if u.avoid {
                    // move the whole unit to a fresh band when it doesn't
                    // fit in the rest of the current one
                    if u.y1 - s > h + 0.5 && u.y0 - s > 0.5 {
                        k += 1;
                        s = u.y0;
                        if starts.len() <= k {
                            starts.push(s);
                        } else {
                            starts[k] = s;
                        }
                    }
                    for it in band_of_item.iter_mut().take(u.i1).skip(u.i0) {
                        *it = k;
                    }
                    for bt in band_of_hit.iter_mut().take(u.h1).skip(u.h0) {
                        *bt = k;
                    }
                    continue;
                }
                for (idx, it) in items.iter().enumerate().take(u.i1).skip(u.i0) {
                    let t = item_top(it);
                    while t >= s + h - 0.01 && k + 1 < 4096 {
                        k += 1;
                        s += h;
                        if starts.len() <= k {
                            starts.push(s);
                        }
                    }
                    band_of_item[idx] = k;
                }
                // hits ride along with their unit's band
                for bt in band_of_hit.iter_mut().take(u.h1).skip(u.h0) {
                    *bt = k;
                }
            }
            let used = band_of_item.iter().copied().max().unwrap_or(0) + 1;
            if fill_h.is_some() || used <= n || h >= bottom {
                break;
            }
            h *= 1.08;
        }
        // emit: item in band k shifts by (k * (col_w+gap), top - s_k)
        let mut seg_bottom = 0.0f32;
        for (i, it) in items.into_iter().enumerate() {
            let k = band_of_item[i].min(starts.len().saturating_sub(1));
            let s = starts[k];
            let dx = x0 + k as f32 * (col_w + gap);
            let dy = top - s;
            seg_bottom = seg_bottom.max(item_bottom(&it) - s);
            self.items.push(offset_item(it, dx, dy));
        }
        for (i, hd) in hits.into_iter().enumerate() {
            let k = band_of_hit[i].min(starts.len().saturating_sub(1));
            let s = starts[k];
            let dx = x0 + k as f32 * (col_w + gap);
            self.hits.push((hd.0 + dx, hd.1 + top - s, hd.2, hd.3, hd.4));
        }
        // column-rule in the middle of each gap spanning the column set
        if let Some((rw, col)) = rule {
            let bands = band_of_item.iter().copied().max().unwrap_or(0) + 1;
            let hgt = seg_bottom.max(h);
            for k in 1..bands.min(n) {
                self.items.push(DisplayItem::FillRect {
                    x: x0 + k as f32 * (col_w + gap) - gap / 2.0 - rw / 2.0,
                    y: top,
                    w: rw,
                    h: hgt,
                    color: col,
                });
            }
        }
        // the column set is at least one full band tall
        self.y = top + seg_bottom.max(h);
    }

    /// Lay `kids` off-screen at `avail` px width in one shared inline
    /// context, recording block-level unit boundaries (each block child is
    /// a unit; maximal inline runs share one unit so their lines merge).
    /// Returns (items, hits, bottom, units) with items in box-local coords.
    fn nest_units(
        &mut self,
        kids: &[NodeId],
        st: &Style,
        avail: f32,
    ) -> (
        Vec<DisplayItem>,
        Vec<(f32, f32, f32, f32, NodeId)>,
        f32,
        Vec<ColUnit>,
    ) {
        let saved_items = std::mem::take(&mut self.items);
        let saved_hits = std::mem::take(&mut self.hits);
        let saved_line = std::mem::take(&mut self.line);
        let saved_floats = std::mem::take(&mut self.floats);
        let saved = (self.y, self.margin_x, self.avail_w, self.align);
        let saved_lx = self.line_x;
        let saved_ps = self.pending_space;
        let saved_ih = self.inset_h;
        self.inset_h = None;
        self.y = 0.0;
        self.margin_x = 0.0;
        self.avail_w = avail.max(1.0);
        self.align = st.align;
        self.line_x = 0.0;
        self.pending_space = false;
        let mut units: Vec<ColUnit> = Vec::new();
        let mut pending_break = false;
        let mut i = 0usize;
        while i < kids.len() {
            let ch = kids[i];
            let (blockish, brk_before, brk_after, avoid, skip) =
                match self.doc.kind(ch) {
                    NodeKind::Element { .. } => {
                        let cc = self.style(ch);
                        let d = cc.get("display").map(String::as_str);
                        let before = cc
                            .get("break-before")
                            .or_else(|| cc.get("page-break-before"));
                        let after = cc
                            .get("break-after")
                            .or_else(|| cc.get("page-break-after"));
                        let inside = cc
                            .get("break-inside")
                            .or_else(|| cc.get("page-break-inside"));
                        (
                            col_blockish(d),
                            col_breaks(before),
                            col_breaks(after),
                            col_avoid(inside),
                            d == Some("none"),
                        )
                    }
                    _ => (false, false, false, false, false),
                };
            if skip {
                i += 1;
                continue;
            }
            let u_y0 = self.y;
            let i0 = self.items.len();
            let h0 = self.hits.len();
            if blockish {
                self.walk(ch, st);
                self.flush_line();
                i += 1;
            } else {
                // inline run: merge consecutive inline-level children so
                // words on one line stay together when sliced into columns
                while i < kids.len() {
                    let k2 = kids[i];
                    if let NodeKind::Element { .. } = self.doc.kind(k2) {
                        let cc = self.style(k2);
                        let d = cc.get("display").map(String::as_str);
                        if d == Some("none") {
                            i += 1;
                            continue;
                        }
                        if col_blockish(d) {
                            break;
                        }
                    }
                    self.walk(k2, st);
                    i += 1;
                }
                self.flush_line();
            }
            units.push(ColUnit {
                i0,
                i1: self.items.len(),
                h0,
                h1: self.hits.len(),
                y0: u_y0,
                y1: self.y,
                force_break: pending_break | brk_before,
                avoid,
            });
            pending_break = brk_after;
        }
        let bottom = self.y;
        let items = std::mem::replace(&mut self.items, saved_items);
        let hits = std::mem::replace(&mut self.hits, saved_hits);
        self.y = saved.0;
        self.margin_x = saved.1;
        self.avail_w = saved.2;
        self.align = saved.3;
        self.line = saved_line;
        self.line_x = saved_lx;
        self.pending_space = saved_ps;
        self.floats = saved_floats;
        self.inset_h = saved_ih;
        (items, hits, bottom, units)
    }
}
