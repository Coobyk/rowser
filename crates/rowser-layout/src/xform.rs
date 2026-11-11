//! CSS 2D transforms (css-transforms-1 subset, original implementation).
//!
//! `transform`, `transform-origin` and the individual `translate`/`rotate`/
//! `scale` properties resolve to a single 2x3 affine matrix applied around
//! the element's transform-origin (border box):
//!
//! `M = T(origin) * M_translate * M_rotate * M_scale * M_list * T(-origin)`
//!
//! Individual properties compose BEFORE `transform`'s function list, in the
//! order translate, rotate, scale — i.e. `translate` is outermost.
//!
//! Transforms are visual-only: layout is unaffected. The layouter wraps a
//! transformed element's emitted items in DisplayItem::PushTransform /
//! PopTransform and tags its hit rects with the same matrix — hit-testing
//! inverse-maps the click point, so rotated elements stay exact.
//!
//! Out of scope (parsed but inert): perspective, perspective-origin,
//! transform-style, backface-visibility, will-change — 2D only. rotateX /
//! rotateY / perspective() / rotate3d with a non-z axis are ignored.
//! `position:fixed` inside a transformed ancestor still anchors to the
//! viewport (the spec makes the transform its containing block — kept as
//! an approximation). Likewise, an inline-level descendant's transform
//! wraps only its own word run rather than composing with an ancestor
//! inline's transform (block-level nesting does compose).

use crate::Props;
use std::rc::Rc;

/// Column-vector 2x3 affine `| a c e ; b d f |`: p' = M * p.
/// Field order `[a, b, c, d, e, f]` matches `matrix(a,b,c,d,e,f)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mat(pub [f32; 6]);

impl Mat {
    pub const IDENTITY: Mat = Mat([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    pub fn translate(x: f32, y: f32) -> Mat {
        Mat([1.0, 0.0, 0.0, 1.0, x, y])
    }

    pub fn scale(x: f32, y: f32) -> Mat {
        Mat([x, 0.0, 0.0, y, 0.0, 0.0])
    }

    /// CSS rotate(): positive angles turn clockwise in y-down screen space.
    pub fn rotate(deg: f32) -> Mat {
        let (s, c) = (deg * std::f32::consts::PI / 180.0).sin_cos();
        Mat([c, s, -s, c, 0.0, 0.0])
    }

    /// skew(ax, ay): x' = x + tan(ax)*y ; y' = tan(ay)*x + y
    pub fn skew(ax_deg: f32, ay_deg: f32) -> Mat {
        let t = std::f32::consts::PI / 180.0;
        Mat([1.0, (ay_deg * t).tan(), (ax_deg * t).tan(), 1.0, 0.0, 0.0])
    }

    /// self * o — `o` applies first (post-multiply, column-vector).
    pub fn mul(&self, o: &Mat) -> Mat {
        let [a1, b1, c1, d1, e1, f1] = self.0;
        let [a2, b2, c2, d2, e2, f2] = o.0;
        Mat([
            a1 * a2 + c1 * b2,
            b1 * a2 + d1 * b2,
            a1 * c2 + c1 * d2,
            b1 * c2 + d1 * d2,
            a1 * e2 + c1 * f2 + e1,
            b1 * e2 + d1 * f2 + f1,
        ])
    }

    pub fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        let [a, b, c, d, e, f] = self.0;
        (a * x + c * y + e, b * x + d * y + f)
    }

    pub fn inverse(&self) -> Option<Mat> {
        let [a, b, c, d, e, f] = self.0;
        let det = a * d - b * c;
        if det.abs() < 1e-12 {
            return None;
        }
        Some(Mat([
            d / det,
            -b / det,
            -c / det,
            a / det,
            (c * f - d * e) / det,
            (b * e - a * f) / det,
        ]))
    }

    pub fn is_identity(&self) -> bool {
        const E: f32 = 1e-6;
        let [a, b, c, d, e, f] = self.0;
        (a - 1.0).abs() < E
            && b.abs() < E
            && c.abs() < E
            && (d - 1.0).abs() < E
            && e.abs() < E
            && f.abs() < E
    }

    /// Pure translation — the painter can offset items directly.
    pub fn is_translate(&self) -> bool {
        const E: f32 = 1e-6;
        let [a, b, c, d, _, _] = self.0;
        (a - 1.0).abs() < E && b.abs() < E && c.abs() < E && (d - 1.0).abs() < E
    }

    /// No rotation/skew — an axis-aligned rect maps to an axis-aligned rect.
    pub fn is_axis_aligned(&self) -> bool {
        const E: f32 = 1e-6;
        self.0[1].abs() < E && self.0[2].abs() < E
    }

    /// T(d) * self * T(-d): applying the transform to points that were
    /// shifted by `d` yields the original result shifted by `d` — used when
    /// laid-out items inside a transform get post-hoc offsets (inline-block
    /// placement, scroll shifts).
    pub fn conjugated(&self, dx: f32, dy: f32) -> Mat {
        let [a, b, c, d, e, f] = self.0;
        Mat([
            a,
            b,
            c,
            d,
            e + dx - (a * dx + c * dy),
            f + dy - (b * dx + d * dy),
        ])
    }
}

/// Parsed transform ingredients for one element. Values stay raw — the
/// matrix is resolved per emission site because `transform-origin` and
/// percentage lengths need the element's border box (an inline span's box
/// is its flushed word-run extent).
#[derive(Debug)]
pub struct Spec {
    /// element font-size — resolves em-ish lengths
    pub em: f32,
    /// true when the spec contains more than translations — only those
    /// elements get a PushTransform wrap; pure-translate specs ride the
    /// existing direct-offset path (translate_xy), which needs no painter
    /// support and keeps hit rects exact.
    pub wrap: bool,
    transform: Option<String>,
    translate: Option<String>,
    rotate: Option<String>,
    scale: Option<String>,
    origin: Option<String>,
}

fn nonempty(v: Option<&String>) -> Option<String> {
    let v = v?.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("none") {
        None
    } else {
        Some(v.to_string())
    }
}

/// Build the element's transform spec, or None when it has no
/// transform/translate/rotate/scale declarations.
pub fn spec(c: &Props, em: f32) -> Option<Rc<Spec>> {
    let transform = nonempty(c.get("transform"));
    let translate = nonempty(c.get("translate"));
    let rotate = nonempty(c.get("rotate"));
    let scale = nonempty(c.get("scale"));
    if transform.is_none() && translate.is_none() && rotate.is_none() && scale.is_none() {
        return None;
    }
    let wrap = transform.as_deref().is_some_and(list_needs_wrap)
        || rotate
            .as_deref()
            .is_some_and(|v| rotate_prop_angle(v).is_some_and(|a| a.abs() > 1e-6))
        || scale.as_deref().is_some_and(|v| {
            let m = scale_prop_mat(v);
            (m.0[0] - 1.0).abs() > 1e-6 || (m.0[3] - 1.0).abs() > 1e-6
        });
    Some(Rc::new(Spec {
        em,
        wrap,
        transform,
        translate,
        rotate,
        scale,
        origin: c.get("transform-origin").cloned(),
    }))
}

/// Does this element's transform need a matrix wrap (anything beyond the
/// translate functions the direct-offset path already handles)?
pub fn has_matrix(c: &Props) -> bool {
    spec(c, 0.0).is_some_and(|s| s.wrap)
}

impl Spec {
    /// Resolve the full transform matrix around `bbox` (the element's
    /// border box in current emission coordinates).
    pub fn matrix(&self, bbox: (f32, f32, f32, f32)) -> Mat {
        let (bx, by, bw, bh) = bbox;
        let mut m = Mat::IDENTITY;
        if let Some(v) = &self.translate {
            m = m.mul(&translate_prop_mat(v, bw, bh, self.em));
        }
        if let Some(v) = &self.rotate {
            if let Some(a) = rotate_prop_angle(v) {
                m = m.mul(&Mat::rotate(a));
            }
        }
        if let Some(v) = &self.scale {
            m = m.mul(&scale_prop_mat(v));
        }
        if let Some(v) = &self.transform {
            m = m.mul(&list_matrix(v, bw, bh, self.em));
        }
        let (ox, oy) = origin_xy(self.origin.as_deref(), bw, bh, self.em);
        let (ax, ay) = (bx + ox, by + oy);
        Mat::translate(ax, ay)
            .mul(&m)
            .mul(&Mat::translate(-ax, -ay))
    }
}

// ---------- value parsing ----------

/// `<angle>` → degrees (deg/grad/rad/turn; a bare number reads as deg).
fn angle_deg(s: &str) -> Option<f32> {
    let s = s.trim().to_lowercase();
    for (suf, k) in [
        ("grad", 0.9f32),
        ("deg", 1.0),
        ("rad", 180.0 / std::f32::consts::PI),
        ("turn", 360.0),
    ] {
        if let Some(n) = s.strip_suffix(suf) {
            return n.trim().parse::<f32>().ok().map(|n| n * k);
        }
    }
    s.parse::<f32>().ok()
}

fn num(s: &str) -> Option<f32> {
    s.trim().parse::<f32>().ok()
}

/// Function args: comma-separated, else whitespace-separated.
fn fn_args(s: &str) -> Vec<&str> {
    if s.contains(',') {
        s.split(',').map(str::trim).collect()
    } else {
        s.split_whitespace().collect()
    }
}

fn len(v: &str, rel: f32, em: f32) -> f32 {
    crate::parse_len(v, rel, em).unwrap_or(0.0)
}

/// `translate: x y [z]` — %s resolve against the element's own box.
fn translate_prop_mat(v: &str, w: f32, h: f32, em: f32) -> Mat {
    let mut it = v.split_whitespace();
    let x = it.next().map(|t| len(t, w, em)).unwrap_or(0.0);
    let y = it.next().map(|t| len(t, h, em)).unwrap_or(0.0);
    Mat::translate(x, y)
}

/// `rotate: <angle> | [x|y|z|x y z] <angle>` — 2D keeps the z-axis case:
/// a bare angle, `z <angle>` or `0 0 1 <angle>`; other axes are ignored.
fn rotate_prop_angle(v: &str) -> Option<f32> {
    let toks: Vec<&str> = v.split_whitespace().collect();
    let (axis, a) = match toks.as_slice() {
        [a] => ("z", *a),
        [axis, a] => (*axis, *a),
        [x, y, z, a] => {
            let (x, y, z) = (
                num(x).unwrap_or(0.0),
                num(y).unwrap_or(0.0),
                num(z).unwrap_or(1.0),
            );
            if x.abs() < 1e-6 && y.abs() < 1e-6 && z.abs() > 1e-6 {
                ("z", *a)
            } else {
                ("?", *a)
            }
        }
        _ => ("?", ""),
    };
    if axis == "z" {
        angle_deg(a)
    } else {
        None
    }
}

/// `scale: s | x y [z]` — z ignored.
fn scale_prop_mat(v: &str) -> Mat {
    let toks: Vec<f32> = v.split_whitespace().filter_map(num).collect();
    match toks.as_slice() {
        [s] => Mat::scale(*s, *s),
        [x, y, ..] => Mat::scale(*x, *y),
        _ => Mat::IDENTITY,
    }
}

/// Walk `name(args)` functions left to right — post-multiplying each, so
/// the leftmost function is outermost.
fn list_matrix(v: &str, w: f32, h: f32, em: f32) -> Mat {
    let v = v.to_lowercase();
    let b = v.as_bytes();
    let mut m = Mat::IDENTITY;
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_alphabetic() {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && b[i].is_ascii_alphanumeric() {
            i += 1;
        }
        let name = &v[start..i];
        if b.get(i) != Some(&b'(') {
            continue;
        }
        let arg_start = i + 1;
        let mut depth = 0i32;
        while i < b.len() {
            match b[i] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        m = m.mul(&fn_matrix(name, &v[arg_start..i.min(b.len())], w, h, em));
        i += 1;
    }
    m
}

/// A transform list "needs a wrap" when it does more than move pixels —
/// plain translate()/translateX()/translateY() stay on the direct-offset
/// path; translate3d/translateZ route through the matrix so their 2D part
/// still applies.
fn list_needs_wrap(v: &str) -> bool {
    let v = v.to_lowercase();
    let b = v.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_alphabetic() {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && b[i].is_ascii_alphanumeric() {
            i += 1;
        }
        if b.get(i) == Some(&b'(') {
            if !matches!(&v[start..i], "translate" | "translatex" | "translatey") {
                return true;
            }
            while i < b.len() && b[i] != b')' {
                i += 1;
            }
        }
        i += 1;
    }
    false
}

fn fn_matrix(name: &str, args: &str, w: f32, h: f32, em: f32) -> Mat {
    let a = fn_args(args);
    match name {
        "translate" | "translatex" | "translatey" | "translatez" | "translate3d" => {
            let (mut x, mut y) = (0.0f32, 0.0f32);
            match name {
                "translatex" => x = len(a.first().unwrap_or(&"0"), w, em),
                "translatey" => y = len(a.first().unwrap_or(&"0"), h, em),
                "translatez" => {}
                _ => {
                    x = len(a.first().unwrap_or(&"0"), w, em);
                    y = len(a.get(1).unwrap_or(&"0"), h, em);
                }
            }
            Mat::translate(x, y)
        }
        "scale" | "scalex" | "scaley" | "scalez" | "scale3d" => {
            let (mut sx, mut sy) = (1.0f32, 1.0f32);
            match name {
                "scalex" => sx = num(a.first().unwrap_or(&"1")).unwrap_or(1.0),
                "scaley" => sy = num(a.first().unwrap_or(&"1")).unwrap_or(1.0),
                "scalez" => {}
                _ => {
                    sx = num(a.first().unwrap_or(&"1")).unwrap_or(1.0);
                    sy = a.get(1).and_then(|s| num(s)).unwrap_or(sx);
                }
            }
            Mat::scale(sx, sy)
        }
        "rotate" | "rotatez" => angle_deg(a.first().unwrap_or(&"0"))
            .map(Mat::rotate)
            .unwrap_or(Mat::IDENTITY),
        // rotateX/rotateY/perspective are 3D — out of scope.
        "rotatex" | "rotatey" | "perspective" => Mat::IDENTITY,
        "rotate3d" => {
            let n = |i: usize, d: f32| a.get(i).and_then(|s| num(s)).unwrap_or(d);
            if n(0, 0.0).abs() < 1e-6 && n(1, 0.0).abs() < 1e-6 && n(2, 1.0).abs() > 1e-6 {
                angle_deg(a.get(3).unwrap_or(&"0"))
                    .map(Mat::rotate)
                    .unwrap_or(Mat::IDENTITY)
            } else {
                Mat::IDENTITY
            }
        }
        "skew" | "skewx" | "skewy" => {
            let (mut ax, mut ay) = (0.0f32, 0.0f32);
            match name {
                "skewx" => ax = angle_deg(a.first().unwrap_or(&"0")).unwrap_or(0.0),
                "skewy" => ay = angle_deg(a.first().unwrap_or(&"0")).unwrap_or(0.0),
                _ => {
                    ax = angle_deg(a.first().unwrap_or(&"0")).unwrap_or(0.0);
                    ay = a.get(1).and_then(|s| angle_deg(s)).unwrap_or(0.0);
                }
            }
            Mat::skew(ax, ay)
        }
        "matrix" => {
            let n = |i: usize| a.get(i).and_then(|s| num(s));
            match (n(0), n(1), n(2), n(3), n(4), n(5)) {
                (Some(a), Some(b), Some(c), Some(d), Some(e), Some(f)) => {
                    Mat([a, b, c, d, e, f])
                }
                _ => Mat::IDENTITY,
            }
        }
        // matrix3d(m11..m44), column-major: keep the 2D part (z discarded).
        "matrix3d" => {
            let n = |i: usize| a.get(i).and_then(|s| num(s)).unwrap_or(0.0);
            Mat([n(0), n(1), n(4), n(5), n(12), n(13)])
        }
        _ => Mat::IDENTITY,
    }
}

/// `transform-origin: <x> <y> [z]` — px/%/keywords against the border box;
/// returns the origin OFFSET from the box's top-left (default centered).
fn origin_xy(v: Option<&str>, w: f32, h: f32, em: f32) -> (f32, f32) {
    let mut xkey: Option<f32> = None;
    let mut ykey: Option<f32> = None;
    let mut lens: Vec<f32> = Vec::new();
    if let Some(v) = v {
        for t in v.to_lowercase().split_whitespace().take(2) {
            match t {
                "left" => xkey = Some(0.0),
                "right" => xkey = Some(w),
                "top" => ykey = Some(0.0),
                "bottom" => ykey = Some(h),
                "center" => {}
                _ => {
                    // a length lands on the first axis not taken by a
                    // keyword — x (rel w) then y (rel h)
                    let rel = if xkey.is_none() && lens.is_empty() { w } else { h };
                    if let Some(l) = crate::parse_len(t, rel, em) {
                        lens.push(l);
                    }
                }
            }
        }
    }
    let x = xkey.or_else(|| lens.first().copied()).unwrap_or(w * 0.5);
    let y = ykey
        .or_else(|| {
            if xkey.is_some() {
                lens.first().copied()
            } else {
                lens.get(1).copied()
            }
        })
        .unwrap_or(h * 0.5);
    (x, y)
}

// ---------- emission helpers ----------

/// Shift a hit record's rect by `d`, conjugating its transform so the
/// painted region moves by `d` as well: T(d)·M·T(-d) applied to R+d maps
/// to M(R)+d. Used for post-layout offsets (scroll, flex grow, inline
/// placement) exactly like offset_item conjugates PushTransform.
pub fn off_hit(h: crate::Hit, dx: f32, dy: f32) -> crate::Hit {
    (h.0 + dx, h.1 + dy, h.2, h.3, h.4, h.5.conjugated(dx, dy))
}

/// Fallback border box for elements that push no own hit (tables, floats):
/// the union of their emitted items' bounds. Text height is approximate.
pub fn items_bbox(items: &[crate::DisplayItem]) -> Option<(f32, f32, f32, f32)> {
    use crate::DisplayItem as D;
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    let mut any = false;
    for it in items {
        let (x, y, w, h) = match it {
            D::FillRect { x, y, w, h, .. }
            | D::RoundedRect { x, y, w, h, .. }
            | D::Raster { x, y, w, h, .. }
            | D::Image { x, y, w, h, .. } => (*x, *y, *w, *h),
            D::Text {
                x,
                baseline,
                text,
                size,
                ..
            } => {
                let w = text.chars().count() as f32 * size * 0.6;
                (*x, baseline - size, w, size * 1.3)
            }
            D::PushTransform { .. } | D::PopTransform => continue,
        };
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x + w);
        y1 = y1.max(y + h);
        any = true;
    }
    any.then_some((x0, y0, x1 - x0, y1 - y0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rowser_css::StrMap;

    fn props(transform: &str, origin: Option<&str>) -> Props {
        let mut p: Props = StrMap::default();
        p.insert("transform".to_string(), transform.to_string());
        if let Some(o) = origin {
            p.insert("transform-origin".to_string(), o.to_string());
        }
        p
    }

    #[test]
    fn compose_order_list() {
        // translate then rotate in the list → T·R (translate outermost):
        // point (10,0) → rotate90 → (0,10) → translate → (10,10)
        let sp = spec(&props("translate(10px,0px) rotate(90deg)", Some("0 0")), 16.0).unwrap();
        let m = sp.matrix((0.0, 0.0, 100.0, 50.0));
        let (x, y) = m.apply(10.0, 0.0);
        assert!((x - 10.0).abs() < 0.01 && (y - 10.0).abs() < 0.01, "({x},{y})");
    }

    #[test]
    fn origin_centers_rotation() {
        // default origin = box center → rotating about it fixes the center
        let sp = spec(&props("rotate(30deg)", None), 16.0).unwrap();
        let m = sp.matrix((10.0, 20.0, 100.0, 50.0));
        let (x, y) = m.apply(60.0, 45.0);
        assert!((x - 60.0).abs() < 0.01 && (y - 45.0).abs() < 0.01, "({x},{y})");
    }

    #[test]
    fn translate_only_spec_no_wrap() {
        assert!(!spec(&props("translate(10px,20px)", None), 16.0)
            .unwrap()
            .wrap);
        assert!(spec(&props("rotate(10deg)", None), 16.0).unwrap().wrap);
        assert!(spec(&props("translate3d(1px,2px,3px)", None), 16.0)
            .unwrap()
            .wrap);
    }

    #[test]
    fn matrix_fn() {
        let sp = spec(&props("matrix(1,0,0,1,10,20)", Some("0 0")), 16.0).unwrap();
        let (x, y) = sp.matrix((0.0, 0.0, 10.0, 10.0)).apply(0.0, 0.0);
        assert!((x - 10.0).abs() < 0.01 && (y - 20.0).abs() < 0.01);
    }

    #[test]
    fn individual_props_compose_before_transform() {
        // translate prop outermost: scale(2) about origin (0,0) then +10x
        let mut p: Props = StrMap::default();
        p.insert("translate".to_string(), "10px 0px".to_string());
        p.insert("transform".to_string(), "scale(2)".to_string());
        p.insert("transform-origin".to_string(), "0 0".to_string());
        let m = spec(&p, 16.0).unwrap().matrix((0.0, 0.0, 10.0, 10.0));
        let (x, y) = m.apply(5.0, 5.0);
        assert!((x - 20.0).abs() < 0.01 && (y - 10.0).abs() < 0.01, "({x},{y})");
    }
}
