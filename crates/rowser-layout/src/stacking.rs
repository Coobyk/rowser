//! Paint-order (stacking) helpers + physical/logical inset resolution.
//!
//! The display list is emitted in DOM order, but CSS paint order for a
//! sibling group is (CSS 2.1 Appendix E, approximated):
//!   in-flow block-level → floats → inline-level → positioned (z-index
//!   sorted; negatives first, below in-flow siblings).
//! `reorder` re-slices the items/hits a parent just emitted for a run of
//! siblings into that order — stable, so equal keys keep tree order.

use crate::{parse_len, side, sides, DisplayItem, Props};

/// Sibling paint-layer keys — lower paints earlier.
/// Negative-z positioned siblings paint below in-flow content (real CSS
/// puts them below block boxes but above the parent's background — the
/// parent's own bg/border emit before its children, so that falls out).
pub const L_NEG: i64 = -1_000_000;
pub const L_BLOCK: i64 = 0;
pub const L_FLOAT: i64 = 1_000_000;
pub const L_INLINE: i64 = 2_000_000;
pub const L_POS: i64 = 3_000_000;

/// Does the child emit its own display-item run (vs. merging words into
/// the open line)? Mirrors `walk`'s routing: positioned oof, floats, and
/// block-level displays go through the block path.
pub fn blockish(c: &Props) -> bool {
    if matches!(
        c.get("position").map(String::as_str),
        Some("absolute") | Some("fixed")
    ) {
        return true;
    }
    if matches!(
        c.get("float").map(String::as_str),
        Some("left") | Some("right")
    ) {
        return true;
    }
    matches!(
        c.get("display").map(String::as_str),
        Some("block") | Some("list-item") | Some("flex") | Some("grid")
    )
}

/// Parsed z-index: integers only; `auto`/unparseable → None.
fn z_index(c: &Props) -> Option<i64> {
    c.get("z-index").and_then(|v| v.trim().parse::<i64>().ok())
}

/// Paint-layer key for a sibling that owns its emitted items (block-level
/// child, float, positioned box, grid/flex item).
pub fn item_layer(c: &Props) -> i64 {
    let positioned = matches!(
        c.get("position").map(String::as_str),
        Some("absolute") | Some("fixed") | Some("sticky") | Some("relative")
    );
    if positioned {
        // z-index:auto behaves as 0 within the positioned layer; stacking
        // contexts are approximated by sorting only within each sibling run
        let z = z_index(c).unwrap_or(0);
        if z < 0 {
            L_NEG + z
        } else {
            L_POS + z
        }
    } else if matches!(
        c.get("float").map(String::as_str),
        Some("left") | Some("right")
    ) {
        L_FLOAT
    } else {
        L_BLOCK
    }
}

/// `item_layer` for normal-flow children: None when the child merges into
/// the surrounding inline/block run (no segment to reorder).
pub fn child_layer(c: &Props) -> Option<i64> {
    if blockish(c) {
        Some(item_layer(c))
    } else {
        None
    }
}

/// Re-slice `items`/`hits` tail segments into paint order. `segs` are
/// (items_start, hits_start, key) markers — each segment ends where the
/// next begins (the last runs to the vec ends). Stable sort: equal keys
/// keep DOM order.
pub fn reorder(
    items: &mut Vec<DisplayItem>,
    hits: &mut Vec<crate::Hit>,
    segs: &[(usize, usize, i64)],
) {
    let n = segs.len();
    if n <= 1 {
        return;
    }
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&i| segs[i].2);
    if order.iter().copied().eq(0..n) {
        return; // already in paint order
    }
    let seg_range = |i: usize| -> (usize, usize, usize, usize) {
        let i0 = segs[i].0;
        let i1 = if i + 1 < n {
            segs[i + 1].0
        } else {
            items.len()
        };
        let h0 = segs[i].1;
        let h1 = if i + 1 < n { segs[i + 1].1 } else { hits.len() };
        (i0, i1, h0, h1)
    };
    let mut items_out: Vec<DisplayItem> = Vec::new();
    let mut hits_out: Vec<crate::Hit> = Vec::new();
    for &i in &order {
        let (i0, i1, h0, h1) = seg_range(i);
        items_out.extend_from_slice(&items[i0..i1]);
        hits_out.extend_from_slice(&hits[h0..h1]);
    }
    let (i0, _, h0, _) = seg_range(0);
    items.splice(i0.., items_out);
    hits.splice(h0.., hits_out);
}

/// `inset` shorthand + `top|right|bottom|left` + logical aliases
/// (`inset-block[-start|-end]`, `inset-inline[-start|-end]` — ltr, so
/// block→top/bottom and inline→left/right) → [t, r, b, l].
/// `w`/`h` are the containing-block dims for % resolution.
pub fn inset_sides(c: &Props, em: f32, w: f32, h: f32) -> [Option<f32>; 4] {
    let sh_w = sides(c, "inset", w, em);
    let sh_h = sides(c, "inset", h, em);
    // axis shorthands: `inset-block: <s> <e>?` / `inset-inline: <s> <e>?`
    let axis = |name: &str, rel: f32| -> [Option<f32>; 2] {
        match c.get(name) {
            Some(v) => {
                let vals: Vec<Option<f32>> = v
                    .split_whitespace()
                    .map(|t| parse_len(t, rel, em))
                    .collect();
                let s = vals.first().copied().flatten();
                [s, vals.get(1).copied().flatten().or(s)]
            }
            None => [None; 2],
        }
    };
    let ib = axis("inset-block", h);
    let ii = axis("inset-inline", w);
    let pick = |names: &[&str], i: usize, rel: f32, fb: Option<f32>| -> Option<f32> {
        names
            .iter()
            .find_map(|n| side(c, n, rel, em))
            .or(fb)
            .or(if i % 2 == 0 { sh_h[i] } else { sh_w[i] })
    };
    [
        pick(&["top", "inset-block-start"], 0, h, ib[0]),
        pick(&["right", "inset-inline-end"], 1, w, ii[1]),
        pick(&["bottom", "inset-block-end"], 2, h, ib[1]),
        pick(&["left", "inset-inline-start"], 3, w, ii[0]),
    ]
}
