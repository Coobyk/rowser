//! rowser-layout: DOM + CSS → display list.
//!
//! Styles come from the real cascade (rowser-css): a built-in user-agent
//! stylesheet plus any <style> sheets in the document. Supports: display
//! (none/block/inline/inline-block), color, background-color (blocks paint a
//! box rect, inlines a per-word highlight), font-size/-weight/-style/-family
//! (arbitrary family names resolve through FontCache), line-height,
//! text-decoration, text-align, white-space:pre, margin/padding shorthand +
//! sides (incl. margin-?:auto centering with `width`), `border` shorthand,
//! `width`/`height`, float:left|right (lines inset while the float lasts),
//! and minimal auto table layout (table/tr/td/th with per-cell borders).

use rowser_css as css;
pub mod svg;

use rowser_dom::{Document, NodeId, NodeKind};
use rowser_text::FontCache;
use rustc_hash::FxHashMap;
use std::rc::Rc;
use std::sync::OnceLock;

/// Computed-style map for one element (fx-hashed, built by rowser-css).
type Props = css::StrMap<String>;

/// BGRA pixel.
pub type Color = u32;
pub const fn bgra(r: u8, g: u8, b: u8) -> Color {
    u32::from_le_bytes([b, g, r, 255])
}
pub const BLACK: Color = bgra(0x11, 0x11, 0x11);
pub const WHITE: Color = bgra(0xff, 0xff, 0xff);
pub const RULE: Color = bgra(0x99, 0x99, 0x99);

#[derive(Debug, Clone)]
pub enum DisplayItem {
    Text {
        x: f32,
        baseline: f32,
        text: String,
        font: usize,
        size: f32,
        color: Color,
        underline: bool,
        strike: bool,
        href: Option<String>,
    },
    FillRect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        color: Color,
    },
    /// Filled rect with uniform corner radius.
    RoundedRect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        color: Color,
    },
    /// Raw BGRA bitmap painted at (x,y,w,h) — for inline <svg>.
    Raster {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        pixels: Vec<u8>,
    },
    /// Resolved image URL + rect (paint draws the decoded bitmap).
    Image {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        src: String,
    },
}

#[derive(Clone, Copy, PartialEq)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone)]
struct Style {
    size: f32,
    bold: bool,
    italic: bool,
    /// family name handed to FontCache::resolve (fuzzy substring match)
    family: String,
    /// text-transform applied per word
    transform: u8, // 0 none, 1 upper, 2 lower, 3 capitalize
    color: Color,
    underline: bool,
    strike: bool,
    shift: f32,
    pre: bool,
    /// white-space:nowrap (and pre) — lines never break; text overflows
    nowrap: bool,
    /// overflow-wrap:break-word|anywhere, word-break:break-all — overwide
    /// words may split mid-word; otherwise they overflow
    break_word: bool,
    align: Align,
    href: Option<String>,
    /// line-height override: multiplier of font-size or an absolute px length
    line_h: Option<LineH>,
    /// background-color of the nearest inline ancestor; painted per-word.
    /// Cleared when a block/inline-box paints its own background rect.
    inline_bg: Option<Color>,
}

#[derive(Clone, Copy)]
enum LineH {
    Mul(f32),
    Px(f32),
}

const BASE_SIZE: f32 = 16.0;

fn base_style() -> Style {
    Style {
        size: BASE_SIZE,
        bold: false,
        italic: false,
        family: "liberation sans".to_string(),
        transform: 0,
        color: BLACK,
        underline: false,
        strike: false,
        shift: 0.0,
        pre: false,
        nowrap: false,
        break_word: false,
        align: Align::Left,
        href: None,
        line_h: None,
        inline_bg: None,
    }
}

const UA_CSS: &str = r#"
head,title,meta,link,style,script,template,noscript,base{display:none}
html,body,div,p,blockquote,pre,listing,h1,h2,h3,h4,h5,h6,ul,ol,menu,dir,dl,dd,dt,li,
section,article,nav,header,footer,main,aside,form,fieldset,figcaption,figure,
address,table,tr,tbody,thead,tfoot,caption,hr,center{display:block}
center{text-align:center}
table{text-align:left}
p{margin-top:1em;margin-bottom:1em}
h1{font-size:2em;font-weight:bold;margin-top:.67em;margin-bottom:.67em}
h2{font-size:1.5em;font-weight:bold;margin-top:.83em;margin-bottom:.83em}
h3{font-size:1.17em;font-weight:bold;margin-top:1em;margin-bottom:1em}
h4{font-weight:bold;margin-top:1.33em;margin-bottom:1.33em}
h5{font-size:.83em;font-weight:bold;margin-top:1.67em;margin-bottom:1.67em}
h6{font-size:.67em;font-weight:bold;margin-top:2.33em;margin-bottom:2.33em}
ul,ol,menu,dir{margin-top:1em;margin-bottom:1em;padding-left:40px}
dd{padding-left:40px}
blockquote{margin-top:1em;margin-bottom:1em;padding-left:40px;padding-right:40px}
pre,listing{font-family:monospace;white-space:pre;margin-top:1em;margin-bottom:1em}
b,strong{font-weight:bold}
i,em,dfn,var,cite,address{font-style:italic}
code,kbd,samp,tt{font-family:monospace}
a{color:#0000ee;text-decoration:underline}
u,ins{text-decoration:underline}
s,strike,del{text-decoration:line-through}
small{font-size:.83em}
big{font-size:1.17em}
sub{vertical-align:sub;font-size:.8em}
sup{vertical-align:super;font-size:.8em}
hr{margin-top:.5em;margin-bottom:.5em}
td,th{display:inline-block}
th{font-weight:bold;text-align:center}
"#;

fn ua_sheet() -> &'static css::StyleSheet {
    static UA: OnceLock<css::StyleSheet> = OnceLock::new();
    UA.get_or_init(|| css::parse(UA_CSS))
}

// ---------- value parsing ----------

fn named_color(name: &str) -> Option<Color> {
    Some(match name {
        "black" => bgra(0, 0, 0),
        "white" => bgra(255, 255, 255),
        "red" => bgra(255, 0, 0),
        "green" => bgra(0, 128, 0),
        "lime" => bgra(0, 255, 0),
        "blue" => bgra(0, 0, 255),
        "navy" => bgra(0, 0, 128),
        "yellow" => bgra(255, 255, 0),
        "orange" => bgra(255, 165, 0),
        "purple" => bgra(128, 0, 128),
        "fuchsia" | "magenta" => bgra(255, 0, 255),
        "aqua" | "cyan" => bgra(0, 255, 255),
        "teal" => bgra(0, 128, 128),
        "olive" => bgra(128, 128, 0),
        "maroon" => bgra(128, 0, 0),
        "silver" => bgra(192, 192, 192),
        "gray" | "grey" => bgra(128, 128, 128),
        "darkgray" | "darkgrey" => bgra(64, 64, 64),
        "lightgray" | "lightgrey" => bgra(211, 211, 211),
        "pink" => bgra(255, 192, 203),
        "brown" => bgra(165, 42, 42),
        "gold" => bgra(255, 215, 0),
        "indigo" => bgra(75, 0, 130),
        "violet" => bgra(238, 130, 238),
        "coral" => bgra(255, 127, 80),
        "salmon" => bgra(250, 128, 114),
        "khaki" => bgra(240, 230, 140),
        "beige" => bgra(245, 245, 220),
        "ivory" => bgra(255, 255, 240),
        "tan" => bgra(210, 180, 140),
        "skyblue" => bgra(135, 206, 235),
        "steelblue" => bgra(70, 130, 180),
        "royalblue" => bgra(65, 105, 225),
        "dodgerblue" => bgra(30, 144, 255),
        "deepskyblue" => bgra(0, 191, 255),
        "cornflowerblue" => bgra(100, 149, 237),
        "crimson" => bgra(220, 20, 60),
        "firebrick" => bgra(178, 34, 34),
        "darkred" => bgra(139, 0, 0),
        "darkgreen" => bgra(0, 100, 0),
        "darkblue" => bgra(0, 0, 139),
        "darkcyan" => bgra(0, 139, 139),
        "darkmagenta" => bgra(139, 0, 139),
        "darkorange" => bgra(255, 140, 0),
        "darkorchid" => bgra(153, 50, 204),
        "darkslategray" | "darkslategrey" => bgra(47, 79, 79),
        "dimgray" | "dimgrey" => bgra(105, 105, 105),
        "forestgreen" => bgra(34, 139, 34),
        "hotpink" => bgra(255, 105, 180),
        "lightblue" => bgra(173, 216, 230),
        "lightgreen" => bgra(144, 238, 144),
        "mediumpurple" => bgra(147, 112, 219),
        "midnightblue" => bgra(25, 25, 112),
        "orchid" => bgra(218, 112, 214),
        "plum" => bgra(221, 160, 221),
        "rebeccapurple" => bgra(102, 51, 153),
        "seagreen" => bgra(46, 139, 87),
        "sienna" => bgra(160, 82, 45),
        "slategray" | "slategrey" => bgra(112, 128, 144),
        "springgreen" => bgra(0, 255, 127),
        "tomato" => bgra(255, 99, 71),
        "turquoise" => bgra(64, 224, 208),
        "whitesmoke" => bgra(245, 245, 245),
        "yellowgreen" => bgra(154, 205, 50),
        _ => return None,
    })
}

/// BGRA pixel with a real alpha byte. Layout composites any alpha<255
/// color over the enclosing backdrop before emitting (paint is opaque).
const fn bgra_a(r: u8, g: u8, b: u8, a: u8) -> Color {
    u32::from_le_bytes([b, g, r, a])
}
/// channel of a BGRA u32: 0=b, 8=g, 16=r, 24=a
fn ch(c: Color, shift: u32) -> u8 {
    ((c >> shift) & 0xff) as u8
}
fn alpha(c: Color) -> u8 {
    ch(c, 24)
}
/// `c` dimmed toward `dst` by `op` — element-opacity approximation for
/// already-composited item colors.
fn fade(c: Color, dst: Color, op: f32) -> Color {
    let o = (op.clamp(0.0, 1.0) * 255.0) as u32;
    let inv = 255 - o;
    let m = |s: u8, d: u8| ((s as u32 * o + d as u32 * inv) / 255) as u8;
    bgra(
        m(ch(c, 16), ch(dst, 16)),
        m(ch(c, 8), ch(dst, 8)),
        m(ch(c, 0), ch(dst, 0)),
    )
}

/// Composite possibly-translucent `c` over opaque `dst`.
fn over(c: Color, dst: Color) -> Color {
    let a = alpha(c) as u32;
    if a >= 255 {
        return c;
    }
    let m = |s: u8, d: u8| ((s as u32 * a + d as u32 * (255 - a)) / 255) as u8;
    bgra(
        m(ch(c, 16), ch(dst, 16)),
        m(ch(c, 8), ch(dst, 8)),
        m(ch(c, 0), ch(dst, 0)),
    )
}

/// `fn()` argument list: top-level commas when present, else whitespace
/// with `/` as a separator (modern `r g b / a` syntax).
fn fn_args(inner: &str) -> Vec<String> {
    if inner.contains(',') {
        return inner
            .split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
    }
    inner
        .split(|c: char| c.is_whitespace() || c == '/')
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

fn fnum(s: &str) -> f32 {
    if s == "none" {
        0.0
    } else {
        s.trim().parse().unwrap_or(0.0)
    }
}
/// `<pct>` → `n/100*max`; bare numbers pass through.
fn fpct(s: &str, max: f32) -> f32 {
    if let Some(p) = s.trim().strip_suffix('%') {
        fnum(p) / 100.0 * max
    } else {
        fnum(s)
    }
}
/// alpha arg: `<num>` 0-1 (values >1 read as 0-255), `<pct>` → /100.
fn falpha(s: &str) -> f32 {
    let n = fpct(s, 1.0);
    if n > 1.0 {
        n / 255.0
    } else {
        n
    }
}
/// `<hue>`: bare degrees or deg/rad/grad/turn suffixes; `none` → 0.
fn fhue(s: &str) -> f32 {
    let s = s.trim();
    for (suf, k) in [("grad", 0.9f32), ("turn", 360.0), ("deg", 1.0), ("rad", 57.295779513)] {
        if let Some(n) = s.strip_suffix(suf) {
            return fnum(n) * k;
        }
    }
    fnum(s)
}
/// linear-sRGB component (0..1 in) → 8-bit sRGB, gamut-clamped.
fn srgb8(v: f32) -> u8 {
    let v = v.clamp(0.0, 1.0);
    let c = if v <= 0.0031308 {
        12.92 * v
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    };
    (c * 255.0).round().clamp(0.0, 255.0) as u8
}
/// CSS `hsl()` → linear-light-free sRGB (0..1).
fn hsl_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    let h = h.rem_euclid(360.0) / 360.0;
    let a = s * l.min(1.0 - l);
    let f = |n: f32| {
        let k = (n + h * 12.0).rem_euclid(12.0);
        l - a * (k - 3.0).min(9.0 - k).min(1.0).max(-1.0)
    };
    (f(0.0), f(8.0), f(4.0))
}
fn hwb_rgb(h: f32, w: f32, b: f32) -> (f32, f32, f32) {
    if w + b >= 1.0 {
        let g = if w + b > 0.0 { w / (w + b) } else { 0.5 };
        return (g, g, g);
    }
    let (r, g, bl) = hsl_rgb(h, 1.0, 0.5);
    let k = 1.0 - w - b;
    (r * k + w, g * k + w, bl * k + w)
}
/// CIE `lab()` (D50) → linear sRGB (CSS Color 4 matrices).
fn lab_rgb(l: f32, a: f32, b: f32) -> (f32, f32, f32) {
    const K: f32 = 24389.0 / 27.0;
    const E: f32 = 216.0 / 24389.0;
    let fy = (l + 16.0) / 116.0;
    let inv = |t: f32| {
        let t3 = t * t * t;
        if t3 > E {
            t3
        } else {
            (116.0 * t - 16.0) / K
        }
    };
    let x = inv(fy + a / 500.0) * 0.96422;
    let y = if l > K * E { fy * fy * fy } else { l / K };
    let z = inv(fy - b / 200.0) * 0.82521;
    (
        3.1341858371 * x - 1.6172094591 * y - 0.4906945842 * z,
        -0.9787487286 * x + 1.9161301227 * y + 0.0334332507 * z,
        0.0719529066 * x - 0.2289938547 * y + 1.4053853164 * z,
    )
}
/// `oklab()` → linear sRGB (CSS Color 4 matrices).
fn oklab_rgb(l: f32, a: f32, b: f32) -> (f32, f32, f32) {
    let ll = (l + 0.3963377774 * a + 0.2158037573 * b).powi(3);
    let mm = (l - 0.1055613458 * a - 0.0638541729 * b).powi(3);
    let ss = (l - 0.0894841775 * a - 1.2914855480 * b).powi(3);
    (
        4.0767416621 * ll - 3.3077115913 * mm + 0.2309699292 * ss,
        -1.2684380046 * ll + 2.6097574011 * mm - 0.3413193965 * ss,
        -0.0041960863 * ll - 0.7034186147 * mm + 1.7076147010 * ss,
    )
}
/// `(r,g,b)` 0..1 LINEAR floats + alpha → BGRA (lab/oklab conversions).
fn fn_color(r: f32, g: f32, b: f32, a: f32) -> Color {
    bgra_a(
        srgb8(r),
        srgb8(g),
        srgb8(b),
        (a.clamp(0.0, 1.0) * 255.0).round() as u8,
    )
}
/// `(r,g,b)` 0..1 sRGB-gamma floats + alpha → BGRA (hsl/hwb are defined
/// directly over sRGB — no linearization step).
fn fn_color_srgb(r: f32, g: f32, b: f32, a: f32) -> Color {
    let s = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    bgra_a(s(r), s(g), s(b), (a.clamp(0.0, 1.0) * 255.0).round() as u8)
}

pub fn parse_color(v: &str) -> Option<Color> {
    let v = v.trim().to_lowercase();
    if v == "transparent" {
        return None;
    }
    if v.starts_with("color-mix(") {
        let inner = v[v.find('(')? + 1..v.rfind(')')?].trim();
        // `in <space>,` prelude optional
        let inner = match top_comma(inner) {
            Some((pre, rest)) if pre.trim_start().starts_with("in ") => rest.trim(),
            _ => inner,
        };
        let (a, b) = match top_comma(inner) {
            Some((a, b)) => (a, Some(b)),
            None => (inner, None),
        };
        // each arm: color [<pct>]
        let arm = |s: &str| -> Option<(Color, f32)> {
            let s = s.trim();
            if s == "transparent" {
                return Some((bgra_a(0, 0, 0, 0), -1.0));
            }
            if let Some(i) = s.rfind(|c: char| c.is_whitespace()) {
                let (col, w) = (s[..i].trim(), s[i + 1..].trim());
                if let Some(p) = w
                    .strip_suffix('%')
                    .and_then(|p| p.trim().parse::<f32>().ok())
                {
                    let c = if col == "transparent" {
                        bgra_a(0, 0, 0, 0)
                    } else {
                        parse_color(col)?
                    };
                    return Some((c, p / 100.0));
                }
            }
            Some((parse_color(s)?, -1.0))
        };
        let (ca, wa) = arm(a)?;
        let (cb, wb) = match b {
            Some(b) => arm(b)?,
            None => (bgra_a(0, 0, 0, 0), -1.0),
        };
        let (mut wa, mut wb) = (wa, wb);
        match (wa < 0.0, wb < 0.0) {
            (true, true) => {
                wa = 0.5;
                wb = 0.5;
            }
            (true, false) => wa = 1.0 - wb,
            (false, true) => wb = 1.0 - wa,
            _ => {}
        }
        let sum = wa + wb;
        if sum <= 0.0 {
            return Some(bgra_a(0, 0, 0, 0));
        }
        let (na, nb) = if sum > 1.0 { (wa / sum, wb / sum) } else { (wa, wb) };
        // premultiplied mix: each arm contributes color*alpha*weight;
        // missing percentage drains alpha (mixing with nothing)
        let (aa, ab) = (alpha(ca) as f32 / 255.0 * na, alpha(cb) as f32 / 255.0 * nb);
        let oa = aa + ab;
        if oa <= 0.0 {
            return Some(bgra_a(0, 0, 0, 0));
        }
        let mix = |sa: u8, sb: u8| ((sa as f32 * aa + sb as f32 * ab) / oa).round() as u8;
        return Some(bgra_a(
            mix(ch(ca, 16), ch(cb, 16)),
            mix(ch(ca, 8), ch(cb, 8)),
            mix(ch(ca, 0), ch(cb, 0)),
            (oa * 255.0).round() as u8,
        ));
    }
    if let Some(hex) = v.strip_prefix('#') {
        let d = u32::from_str_radix(hex, 16).ok()?;
        return Some(match hex.len() {
            3 => {
                let (r, g, b) = ((d >> 8) & 0xf, (d >> 4) & 0xf, d & 0xf);
                bgra((r * 17) as u8, (g * 17) as u8, (b * 17) as u8)
            }
            4 => {
                let (r, g, b, a) = (
                    (d >> 12) & 0xf,
                    (d >> 8) & 0xf,
                    (d >> 4) & 0xf,
                    d & 0xf,
                );
                bgra_a((r * 17) as u8, (g * 17) as u8, (b * 17) as u8, (a * 17) as u8)
            }
            6 => bgra(((d >> 16) & 0xff) as u8, ((d >> 8) & 0xff) as u8, (d & 0xff) as u8),
            8 => {
                let (r, g, b, a) = (
                    (d >> 24) & 0xff,
                    (d >> 16) & 0xff,
                    (d >> 8) & 0xff,
                    d & 0xff,
                );
                bgra_a(r as u8, g as u8, b as u8, a as u8)
            }
            _ => return None,
        });
    }
    if let Some(open) = v.find('(') {
        if !v.ends_with(')') {
            return named_color(&v);
        }
        let inner = &v[open + 1..v.len() - 1];
        let args = fn_args(inner);
        let arg = |i: usize| args.get(i).map(String::as_str).unwrap_or("");
        // alpha defaults to 1 when the arg is absent (`/a` or 4th arg)
        let fa = |i: usize| {
            args.get(i)
                .map(|s| falpha(s))
                .unwrap_or(1.0)
        };
        let a8 = |i: usize| (fa(i).clamp(0.0, 1.0) * 255.0).round() as u8;
        match &v[..open] {
            "rgb" | "rgba" => {
                if args.len() < 3 {
                    return None;
                }
                let chn = |s: &str| {
                    if let Some(p) = s.strip_suffix('%') {
                        Some(fnum(p) * 2.55)
                    } else {
                        Some(fnum(s))
                    }
                };
                return Some(bgra_a(
                    chn(arg(0))?.clamp(0.0, 255.0).round() as u8,
                    chn(arg(1))?.clamp(0.0, 255.0).round() as u8,
                    chn(arg(2))?.clamp(0.0, 255.0).round() as u8,
                    a8(3),
                ));
            }
            "hsl" | "hsla" => {
                if args.len() < 3 {
                    return None;
                }
                let (r, g, b) = hsl_rgb(
                    fhue(arg(0)),
                    fpct(arg(1), 1.0).clamp(0.0, 1.0),
                    fpct(arg(2), 1.0).clamp(0.0, 1.0),
                );
                return Some(fn_color_srgb(r, g, b, fa(3)));
            }
            "hwb" => {
                if args.len() < 3 {
                    return None;
                }
                let (r, g, b) = hwb_rgb(
                    fhue(arg(0)),
                    fpct(arg(1), 1.0).clamp(0.0, 1.0),
                    fpct(arg(2), 1.0).clamp(0.0, 1.0),
                );
                return Some(fn_color_srgb(r, g, b, fa(3)));
            }
            "lab" | "lch" => {
                if args.len() < 3 {
                    return None;
                }
                let l = fpct(arg(0), 100.0).clamp(0.0, 100.0);
                let (a, b) = if v.starts_with("lch") {
                    let c = fpct(arg(1), 150.0);
                    let h = fhue(arg(2)).to_radians();
                    (c * h.cos(), c * h.sin())
                } else {
                    (fpct(arg(1), 160.0), fpct(arg(2), 160.0))
                };
                let (r, g, b) = lab_rgb(l, a, b);
                return Some(fn_color(r, g, b, fa(3)));
            }
            "oklab" | "oklch" => {
                if args.len() < 3 {
                    return None;
                }
                let l = fpct(arg(0), 1.0);
                let (a, b) = if v.starts_with("oklch") {
                    let c = fpct(arg(1), 0.4);
                    let h = fhue(arg(2)).to_radians();
                    (c * h.cos(), c * h.sin())
                } else {
                    (fpct(arg(1), 0.4), fpct(arg(2), 0.4))
                };
                let (r, g, b) = oklab_rgb(l, a, b);
                return Some(fn_color(r, g, b, fa(3)));
            }
            _ => return named_color(&v),
        }
    }
    named_color(&v)
}

impl Layouter<'_> {
    /// `background-color`/`background` or the legacy `bgcolor` attribute.
    fn bg_of(&self, node: NodeId, c: &Props) -> Option<Color> {
        bg_color(c)
            .or_else(|| self.doc.attr(node, "bgcolor").and_then(|v| parse_color(v)))
    }
}

/// `background-color` or a color inside the `background` shorthand.
fn bg_color(c: &Props) -> Option<Color> {
    if let Some(v) = c.get("background-color") {
        if let Some(col) = parse_color(v) {
            return Some(col);
        }
    }
    let v = c.get("background")?;
    // any *-gradient() → first color stop as flat bg (approximation). A
    // gradient ending in a transparent stop is a glow — dim it rather
    // than painting a solid disc.
    if v.contains("-gradient(") {
        let rest = v[v.find('(')? + 1..].trim_end_matches(')');
        let mut depth = 0i32;
        let stops: Vec<&str> = rest
            .split(|ch| {
                match ch {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                ch == ',' && depth <= 0
            })
            .collect();
        let last = stops.last().map(|s| s.trim()).unwrap_or("");
        let fades_out = last.starts_with("transparent") || last.contains(" 0)");
        for part in &stops {
            let w = part.trim().split_whitespace().next().unwrap_or("");
            if let Some(col) = parse_color(w) {
                if alpha(col) == 0 || !fades_out {
                    return Some(col);
                }
                // first stop is opaque but the ramp ends transparent —
                // average alpha ≈ half across the disc/cone
                return Some(col & !(0xffu32 << 24) | ((alpha(col) as u32 / 2) << 24));
            }
        }
        return None;
    }
    // tokens: color | url() | position/size | repeat | attachment — take first color
    for tok in v.split_whitespace() {
        if let Some(col) = parse_color(tok.trim_end_matches(';')) {
            return Some(col);
        }
    }
    None
}

/// `box-shadow: [inset]? x y [blur] [spread] [color], ...` → first visible
/// shadow as (dx, dy, spread, color). Blur is ignored (solid-offset-blob
/// approximation); a zero-offset positive-spread shadow is a ring and
/// callers draw edges instead of a filled rect. The color is composited
/// over `bd`; `def` stands in for `currentcolor` when none is given.
fn parse_shadow(c: &Props, size: f32, bd: Color, def: Color) -> Option<(f32, f32, f32, Color)> {
    let v = c.get("box-shadow")?;
    if v.trim() == "none" {
        return None;
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
            continue; // fully transparent shadow (e.g. `0 0 #0000`)
        }
        let spread = nums.get(3).copied().unwrap_or(0.0).max(0.0);
        return Some((nums[0], nums[1], spread, over(col, bd)));
    }
    None
}

/// `border-radius` → uniform max radius (px).
fn parse_radius(c: &Props, size: f32) -> f32 {
    c.get("border-radius")
        .and_then(|v| {
            v.split_whitespace()
                .filter_map(|t| parse_len(t, size, size))
                .reduce(f32::max)
        })
        .unwrap_or(0.0)
}

/// CSS length → px. `em`/`%` resolve against `rel` (usually parent font size).
/// Tiny `calc()` evaluator: numbers with units, + - * / and parens.
/// `%` resolves against `rel`; unitless numbers pass through.
fn calc_len(expr: &str, rel: f32, em: f32) -> Option<f32> {
    struct P<'a> {
        t: Vec<Tok>,
        i: usize,
        rel: f32,
        em: f32,
        _m: std::marker::PhantomData<&'a ()>,
    }
    #[derive(Clone, Copy, PartialEq)]
    enum Tok {
        Num(f32, Unit),
        Op(char),
        L,
        R,
    }
    #[derive(Clone, Copy, PartialEq)]
    enum Unit {
        None,
        Px,
        Em,
        Rem,
        Pct,
    }
    fn toks(s: &str) -> Option<Vec<Tok>> {
        let b = s.as_bytes();
        let mut i = 0;
        let mut out = Vec::new();
        while i < b.len() {
            let c = b[i] as char;
            if c.is_whitespace() {
                i += 1;
            } else if c.is_ascii_digit() || c == '.' || (c == '-' && i + 1 < b.len() && b[i + 1].is_ascii_digit()) {
                let st = i;
                if c == '-' {
                    i += 1;
                }
                while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                    i += 1;
                }
                let n: f32 = std::str::from_utf8(&b[st..i]).ok()?.parse().ok()?;
                let mut u = Unit::None;
                if b[i..].starts_with(b"px") {
                    u = Unit::Px;
                    i += 2;
                } else if b[i..].starts_with(b"rem") || b[i..].starts_with(b"em") {
                    if b[i..].starts_with(b"rem") {
                        u = Unit::Rem;
                        i += 3;
                    } else {
                        u = Unit::Em;
                        i += 2;
                    }
                } else if b[i..].starts_with(b"pt") {
                    u = Unit::Px;
                    i += 2;
                } else if i < b.len() && b[i] == b'%' {
                    u = Unit::Pct;
                    i += 1;
                }
                out.push(Tok::Num(n, u));
            } else if matches!(c, '+' | '-' | '*' | '/') {
                out.push(Tok::Op(c));
                i += 1;
            } else if c == '(' {
                out.push(Tok::L);
                i += 1;
            } else if c == ')' {
                out.push(Tok::R);
                i += 1;
            } else {
                return None;
            }
        }
        Some(out)
    }
    // `em` = element font-size, `rem` = root size, `rel` = %-base.
    fn px(n: f32, u: Unit, rel: f32, em: f32) -> f32 {
        match u {
            Unit::Px | Unit::None => n,
            Unit::Em => n * em,
            Unit::Rem => n * BASE_SIZE,
            Unit::Pct => n * rel / 100.0,
        }
    }
    impl P<'_> {
        fn expr(&mut self) -> Option<f32> {
            let mut v = self.term()?;
            while let Some(Tok::Op(op @ ('+' | '-'))) = self.t.get(self.i).copied() {
                self.i += 1;
                let r = self.term()?;
                v = if op == '+' { v + r } else { v - r };
            }
            Some(v)
        }
        fn term(&mut self) -> Option<f32> {
            let mut v = self.factor()?;
            while let Some(Tok::Op(op @ ('*' | '/'))) = self.t.get(self.i).copied() {
                self.i += 1;
                let r = self.factor()?;
                v = if op == '*' { v * r } else { v / r };
            }
            Some(v)
        }
        fn factor(&mut self) -> Option<f32> {
            match self.t.get(self.i).copied() {
                Some(Tok::Num(n, u)) => {
                    self.i += 1;
                    Some(px(n, u, self.rel, self.em))
                }
                Some(Tok::L) => {
                    self.i += 1;
                    let v = self.expr();
                    if self.t.get(self.i).copied() == Some(Tok::R) {
                        self.i += 1;
                    }
                    v
                }
                Some(Tok::Op('-')) => {
                    self.i += 1;
                    self.factor().map(|v| -v)
                }
                _ => None,
            }
        }
    }
    let t = toks(&expr.to_lowercase())?;
    let mut p = P { t, i: 0, rel, em, _m: std::marker::PhantomData };
    p.expr()
}

/// Split a function argument list on top-level commas (nested parens don't
/// split — `min(1rem, calc(50vw - 10px))` → two args)
fn top_args(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut last = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            ',' if depth <= 0 => {
                out.push(&s[last..i]);
                last = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[last..]);
    out
}

/// Recursive math eval for calc()/min()/max()/clamp() bodies — supports
/// + - * /, parens, and every unit parse_len_vp knows (incl. vw/vh, which
/// the older calc_len tokenizer lacks).
fn math_eval(s: &str, rel: f32, em: f32, vh: f32, vw: f32) -> Option<f32> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // strip a single enclosing paren group
    if s.starts_with('(') && s.ends_with(')') {
        let mut d = 0i32;
        let mut whole = true;
        for (i, c) in s.char_indices() {
            match c {
                '(' => d += 1,
                ')' => {
                    d -= 1;
                    if d == 0 && i + 1 < s.len() {
                        whole = false;
                        break;
                    }
                }
                _ => {}
            }
        }
        if whole && d == 0 {
            return math_eval(&s[1..s.len() - 1], rel, em, vh, vw);
        }
    }
    let b = s.as_bytes();
    // lowest precedence first: top-level +/- (right-to-left for left assoc)
    let mut depth = 0i32;
    for i in (0..b.len()).rev() {
        match b[i] {
            b')' | b']' => depth += 1,
            b'(' | b'[' => depth -= 1,
            b'+' | b'-' if depth == 0 => {
                if s[..i].trim().is_empty()
                    || s[..i].trim_end().ends_with(['+', '-', '*', '/'])
                {
                    continue; // unary sign
                }
                let a = math_eval(&s[..i], rel, em, vh, vw)?;
                let c = math_eval(&s[i + 1..], rel, em, vh, vw)?;
                return Some(if b[i] == b'+' { a + c } else { a - c });
            }
            _ => {}
        }
    }
    for i in (0..b.len()).rev() {
        match b[i] {
            b')' | b']' => depth += 1,
            b'(' | b'[' => depth -= 1,
            b'*' | b'/' if depth == 0 => {
                let a = math_eval(&s[..i], rel, em, vh, vw)?;
                let c = math_eval(&s[i + 1..], rel, em, vh, vw)?;
                return Some(if b[i] == b'*' { a * c } else { a / c });
            }
            _ => {}
        }
    }
    parse_len_vp(s, rel, em, vh, vw)
}

/// parse_len plus viewport units (vh/svh/dvh/lvh→vh_px, vw→vw_px).
/// `em` resolves against `em` (the element's font-size); `%`/`calc()`
/// percentages against `rel` (containing-block axis size).
fn parse_len_vp(v: &str, rel: f32, em: f32, vh: f32, vw: f32) -> Option<f32> {
    let v = v.trim().to_lowercase();
    for fname in ["calc", "min", "max", "clamp"] {
        if let Some(inner) = v
            .strip_prefix(fname)
            .and_then(|x| x.strip_prefix('('))
            .and_then(|x| x.strip_suffix(')'))
        {
            let args = top_args(inner);
            return match fname {
                "calc" => math_eval(inner, rel, em, vh, vw),
                "min" | "max" => {
                    let mut best: Option<f32> = None;
                    for a in args {
                        if let Some(n) = math_eval(a, rel, em, vh, vw) {
                            best = Some(match (best, fname == "min") {
                                (None, _) => n,
                                (Some(b), true) => b.min(n),
                                (Some(b), false) => b.max(n),
                            });
                        }
                    }
                    best
                }
                _ => {
                    // clamp(min, val, max)
                    let lo = args.first().and_then(|a| math_eval(a, rel, em, vh, vw));
                    let mid = args.get(1).and_then(|a| math_eval(a, rel, em, vh, vw));
                    let hi = args.get(2).and_then(|a| math_eval(a, rel, em, vh, vw));
                    match (lo, mid, hi) {
                        (Some(lo), Some(mid), Some(hi)) => Some(mid.clamp(lo, hi)),
                        _ => None,
                    }
                }
            };
        }
    }
    for suf in ["svh", "dvh", "lvh", "vh"] {
        if let Some(x) = v.strip_suffix(suf) {
            return x.trim().parse::<f32>().ok().map(|n| n * vh / 100.0);
        }
    }
    for suf in ["svw", "dvw", "lvw", "vw"] {
        if let Some(x) = v.strip_suffix(suf) {
            return x.trim().parse::<f32>().ok().map(|n| n * vw / 100.0);
        }
    }
    parse_len(&v, rel, em)
}

fn parse_len(v: &str, rel: f32, em: f32) -> Option<f32> {
    let v = v.trim().to_lowercase();
    if let Some(inner) = v.strip_prefix("calc(").and_then(|x| x.strip_suffix(')')) {
        return calc_len(inner, rel, em);
    }
    let num = |s: &str| s.trim().parse::<f32>().ok();
    if let Some(x) = v.strip_suffix("px") {
        num(x)
    } else if let Some(x) = v.strip_suffix("rem") {
        num(x).map(|n| n * BASE_SIZE)
    } else if let Some(x) = v.strip_suffix("em") {
        num(x).map(|n| n * em)
    } else if let Some(x) = v.strip_suffix('%') {
        num(x).map(|n| n * rel / 100.0)
    } else if let Some(x) = v.strip_suffix("pt") {
        num(x).map(|n| n * 96.0 / 72.0)
    } else {
        num(&v)
    }
}

fn parse_font_size(v: &str, parent: f32) -> f32 {
    match v.trim() {
        "medium" => BASE_SIZE,
        "small" => BASE_SIZE * 0.8,
        "x-small" => BASE_SIZE * 0.6,
        "xx-small" => BASE_SIZE * 0.5,
        "large" => BASE_SIZE * 1.2,
        "x-large" => BASE_SIZE * 1.5,
        "xx-large" => BASE_SIZE * 2.0,
        "larger" => parent * 1.2,
        "smaller" => parent / 1.2,
        other => parse_len(other, parent, parent).unwrap_or(parent).max(4.0),
    }
}

/// Shorthand sides (top, right, bottom, left) from 1–4 values.
fn sides(c: &Props, name: &str, rel: f32, em: f32) -> [Option<f32>; 4] {
    if let Some(v) = c.get(name) {
        let vals: Vec<Option<f32>> = v
            .split_whitespace()
            .map(|p| parse_len(p, rel, em))
            .collect();
        let get = |i: usize| vals.get(i).copied().flatten();
        match vals.len() {
            1 => [get(0); 4],
            2 => [get(0), get(1), get(0), get(1)],
            3 => [get(0), get(1), get(2), get(1)],
            _ => [get(0), get(1), get(2), get(3)],
        }
    } else {
        [None; 4]
    }
}

fn side(c: &Props, name: &str, rel: f32, em: f32) -> Option<f32> {
    c.get(name).and_then(|v| parse_len(v, rel, em))
}

/// `width`/`height` with the `size` shorthand as fallback (first token for
/// width, second-or-first for height).
fn dim(c: &Props, name: &str, rel: f32, em: f32) -> Option<f32> {
    side(c, name, rel, em).or_else(|| {
        c.get("size").and_then(|v| {
            let mut it = v.split_whitespace();
            let a = it.next().and_then(|t| parse_len(t, rel, em));
            let b = it.next().and_then(|t| parse_len(t, rel, em)).or(a);
            if name == "width" {
                a
            } else {
                b
            }
        })
    })
}

/// `dim` for intrinsic sizing: a percentage width has an indefinite basis
/// while measuring intrinsic sizes, so it's treated as `auto` (content
/// sized). Without this `nav{width:100%}` inside a flex item reports the
/// full containing block as its max-content and forces a wrap.
fn dim_intrinsic(c: &Props, name: &str, rel: f32, em: f32) -> Option<f32> {
    if let Some(v) = c.get(name) {
        if v.contains('%') {
            return None;
        }
    }
    dim(c, name, rel, em)
}

/// Tailwind sets `box-sizing:border-box` globally — `width`/`height` then
/// include padding+border rather than sizing the content box.
fn border_box(c: &Props) -> bool {
    c.get("box-sizing").is_some_and(|v| v.trim() == "border-box")
}

/// `translate` property + `transform: translate*(...)` → (dx, dy) px.
/// Percentages resolve against the element's own border box (w, h).
/// scale/rotate/matrix are ignored (report-only).
fn translate_xy(c: &Props, w: f32, h: f32, em: f32) -> (f32, f32) {
    let (mut dx, mut dy) = (0.0f32, 0.0f32);
    if let Some(v) = c.get("translate") {
        if v.trim() != "none" {
            let mut it = v.split_whitespace();
            dx += it.next().and_then(|t| parse_len(t, w, em)).unwrap_or(0.0);
            dy += it.next().and_then(|t| parse_len(t, h, em)).unwrap_or(0.0);
        }
    }
    if let Some(v) = c.get("transform") {
        let v = v.to_lowercase();
        let mut s = v.as_str();
        while let Some(i) = s.find("translate") {
            let rest = &s[i + 9..];
            let (axis, rest) = if let Some(r) = rest.strip_prefix("x(") {
                (0, r)
            } else if let Some(r) = rest.strip_prefix("y(") {
                (1, r)
            } else if let Some(r) = rest.strip_prefix('(') {
                (2, r)
            } else {
                s = rest;
                continue;
            };
            let end = rest.find(')').unwrap_or(rest.len());
            let inner = &rest[..end];
            let mut it = inner.split(',');
            let x = it
                .next()
                .and_then(|t| parse_len(t.trim(), w, em))
                .unwrap_or(0.0);
            let y = it
                .next()
                .and_then(|t| parse_len(t.trim(), h, em))
                .unwrap_or(0.0);
            match axis {
                0 => dx += x,
                1 => dy += x,
                _ => {
                    dx += x;
                    dy += y;
                }
            }
            s = &rest[end..];
        }
    }
    (dx, dy)
}

/// `flex` shorthand → (grow, basis): `flex:<g> <shrink>? <basis>?`, with
/// `flex-grow`/`flex-basis` longhands as fallback/override. Basis `auto`/none
/// → None (caller uses the item's intrinsic size).
fn flex_grow_basis(c: &Props, rel: f32, em: f32) -> (f32, Option<f32>) {
    let (mut g, mut b): (f32, Option<f32>) = (0.0, None);
    if let Some(v) = c.get("flex") {
        let mut it = v.split_whitespace();
        if let Some(t) = it.next() {
            if let Ok(n) = t.parse::<f32>() {
                g = n;
            } else {
                b = parse_len(t, rel, em).or(if t == "auto" { None } else { Some(0.0) });
            }
        }
        let _shrink = it.next();
        if let Some(t) = it.next() {
            b = if t == "auto" { None } else { parse_len(t, rel, em).or(Some(0.0)) };
        }
    }
    if let Some(v) = c.get("flex-grow") {
        g = v.trim().parse().unwrap_or(g);
    }
    if let Some(v) = c.get("flex-basis") {
        let v = v.trim();
        b = if v == "auto" { None } else { parse_len(v, rel, em).or(Some(0.0)) };
    }
    (g, b)
}

/// `flex-shrink`: middle token of the `flex` shorthand, else the longhand;
/// default 1 (every item shrinks unless it opts out).
fn flex_shrink_factor(c: &Props) -> f32 {
    if let Some(v) = c.get("flex-shrink") {
        return v.trim().parse().unwrap_or(1.0f32).max(0.0);
    }
    if let Some(v) = c.get("flex") {
        let mut it = v.split_whitespace();
        let _grow = it.next();
        if let Some(t) = it.next() {
            if let Ok(n) = t.parse::<f32>() {
                return n.max(0.0f32);
            }
        }
    }
    1.0
}

/// Does this element's left margin resolve to `auto`? Checks the longhand
/// and the left slot of `margin`/`margin-inline` shorthands.
fn has_auto_ml(c: &Props) -> bool {
    let auto = |v: &String| v.trim() == "auto";
    if c.get("margin-left").map(&auto).unwrap_or(false) {
        return true;
    }
    if let Some(v) = c.get("margin-inline").or_else(|| c.get("margin-inline-start")) {
        if auto(v) {
            return true;
        }
    }
    if let Some(v) = c.get("margin") {
        let t: Vec<&str> = v.split_whitespace().collect();
        let l = match t.len() {
            1 => Some(t[0]),
            2 | 3 => Some(t[1]),
            4 => Some(t[3]),
            _ => None,
        };
        if l.map(|s| s == "auto").unwrap_or(false) {
            return true;
        }
    }
    false
}

/// margin/padding longhands merged over the shorthand.
/// Returns [top, right, bottom, left] in px (0 when unset).
fn box_sides(c: &Props, name: &str, rel: f32, em: f32) -> [f32; 4] {
    let sh = sides(c, name, rel, em);
    // static longhand names — the old format!() allocated 4 strings per call
    let (t, r, b, l) = match name {
        "margin" => ("margin-top", "margin-right", "margin-bottom", "margin-left"),
        "padding" => (
            "padding-top",
            "padding-right",
            "padding-bottom",
            "padding-left",
        ),
        _ => {
            return [
                side(c, &format!("{name}-top"), rel, em).or(sh[0]).unwrap_or(0.0),
                side(c, &format!("{name}-right"), rel, em).or(sh[1]).unwrap_or(0.0),
                side(c, &format!("{name}-bottom"), rel, em).or(sh[2]).unwrap_or(0.0),
                side(c, &format!("{name}-left"), rel, em).or(sh[3]).unwrap_or(0.0),
            ];
        }
    };
    [
        side(c, t, rel, em).or(sh[0]).unwrap_or(0.0),
        side(c, r, rel, em).or(sh[1]).unwrap_or(0.0),
        side(c, b, rel, em).or(sh[2]).unwrap_or(0.0),
        side(c, l, rel, em).or(sh[3]).unwrap_or(0.0),
    ]
}

/// Is margin-left/right `auto` (longhand or shorthand position)?
fn margin_auto(c: &Props, left: bool) -> bool {
    let long = if left { "margin-left" } else { "margin-right" };
    if c.get(long).is_some_and(|v| v.trim().eq_ignore_ascii_case("auto")) {
        return true;
    }
    if let Some(v) = c.get("margin") {
        let t: Vec<&str> = v.split_whitespace().collect();
        let idx = match t.len() {
            0 => return false,
            1 => 0,
            2 | 3 => 1,
            _ => {
                if left {
                    3
                } else {
                    2
                }
            }
        };
        return t.get(idx).is_some_and(|s| s.eq_ignore_ascii_case("auto"));
    }
    false
}

/// `border: 2px solid red` → (width px, color). The color may contain
/// spaces (`rgb(...)`/`color-mix(...)`) — the rest of the value is tried
/// as one color before the bare token.
fn parse_border(v: &str, em: f32) -> Option<(f32, Color)> {
    let mut width = None;
    let mut color = None;
    let mut rest = v.trim();
    while !rest.is_empty() {
        let part = rest.split_whitespace().next().unwrap();
        if part == "none" || part == "hidden" {
            return None;
        }
        if parse_len(part, 16.0, em).is_some() || matches!(part, "thin" | "medium" | "thick") {
            width = Some(match part {
                "thin" => 1.0,
                "thick" => 6.0,
                "medium" => 3.0,
                p => parse_len(p, 16.0, em).unwrap_or(1.0),
            });
        } else if matches!(
            part,
            "solid" | "dashed" | "dotted" | "double" | "groove" | "ridge" | "inset" | "outset"
        ) {
            // style keyword — all styles paint solid edges
        } else {
            color = parse_color(rest).or_else(|| parse_color(part)).or(color);
            break;
        }
        rest = rest[part.len()..].trim_start();
    }
    width.zip(color.or(Some(BLACK)))
}

/// Per-side border widths + color, merged: `border` shorthand →
/// `border-width`/`border-<side>-width` → `border-(<side>-)?style` →
/// `border-color`. Sides are [top, right, bottom, left]. Longhands apply
/// over the shorthand (preflight `border:0 solid` + `border-t` pattern).
fn border_sides(c: &Props, size: f32) -> Option<([f32; 4], Color)> {
    let mut w = [0.0f32; 4];
    let mut col = None;
    if let Some(v) = c.get("border") {
        // `border:none` or an unparseable shorthand contributes nothing;
        // longhands can still re-enable individual sides
        if let Some((bw, sc)) = parse_border(v, size) {
            w = [bw; 4];
            col = Some(sc);
        }
    }
    let sh = sides(c, "border-width", size, size);
    for (i, s) in sh.iter().copied().enumerate() {
        if let Some(v) = s {
            w[i] = v;
        }
    }
    const SIDES: [(&str, &str, &str); 4] = [
        ("border-top-width", "border-top-style", "border-top-color"),
        ("border-right-width", "border-right-style", "border-right-color"),
        ("border-bottom-width", "border-bottom-style", "border-bottom-color"),
        ("border-left-width", "border-left-style", "border-left-color"),
    ];
    if c.get("border-style")
        .is_some_and(|v| matches!(v.trim(), "none" | "hidden"))
    {
        w = [0.0; 4];
    }
    let mut any = false;
    if let Some(bc) = c.get("border-color").and_then(|v| parse_color(v)) {
        col = Some(bc);
    }
    for (i, (wn, sn, cn)) in SIDES.iter().copied().enumerate() {
        if let Some(v) = c.get(wn).and_then(|v| parse_len(v, size, size)) {
            w[i] = v;
        }
        if c.get(sn).is_some_and(|v| matches!(v.trim(), "none" | "hidden")) {
            w[i] = 0.0;
        }
        if let Some(sc) = c.get(cn).and_then(|v| parse_color(v)) {
            col = Some(sc);
        }
        any |= w[i] > 0.0;
    }
    if !any {
        return None;
    }
    Some((w, col.unwrap_or(BLACK)))
}

// ---------- CSS Grid (autoplacement-capable subset) ----------

/// `aspect-ratio: <a>/<b>` (or a bare number) → w/h ratio.
fn aspect_ratio(c: &Props) -> Option<f32> {
    let v = c.get("aspect-ratio")?.trim().replace("auto", "");
    let v = v.trim();
    if v.is_empty() {
        return None;
    }
    let (a, b) = v
        .split_once('/')
        .map(|(a, b)| (a.trim(), b.trim()))
        .unwrap_or((v, "1"));
    let (a, b) = (a.parse::<f32>().ok()?, b.parse::<f32>().ok()?);
    (a > 0.0 && b > 0.0).then_some(a / b)
}

/// A track size parsed from `grid-template-*`/`grid-auto-*`.
#[derive(Clone, Copy)]
enum GTrack {
    /// Fixed length (px already resolved).
    Fixed(f32),
    /// `Nfr` fraction of leftover space, with a floor (from minmax min).
    Fr(f32, f32),
    /// Content-sized track clamped to [min, max] (auto, minmax, fit-content).
    Auto(f32, f32),
}

/// Whitespace-split `s` into tokens, keeping `fn(...)` groups together.
fn track_tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for ch in s.chars() {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            c if c.is_whitespace() && depth <= 0 => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                continue;
            }
            _ => {}
        }
        cur.push(ch);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Split on the first top-level comma (for repeat()/minmax() args).
fn top_comma(s: &str) -> Option<(&str, &str)> {
    let mut depth = 0i32;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth <= 0 => return Some((&s[..i], &s[i + 1..])),
            _ => {}
        }
    }
    None
}

fn track_floor(t: &GTrack) -> f32 {
    match *t {
        GTrack::Fixed(x) => x,
        GTrack::Fr(_, m) => m,
        GTrack::Auto(m, _) => m,
    }
}

/// One non-repeat track token → GTrack. `rel` is the %/em base (axis size).
fn track_spec(t: &str, rel: f32, em: f32, vh: f32, vw: f32) -> GTrack {
    let t = t.trim().to_lowercase();
    if let Some(f) = t.strip_suffix("fr") {
        return GTrack::Fr(f.trim().parse::<f32>().unwrap_or(1.0).max(0.0), 0.0);
    }
    if t == "auto" || t == "min-content" || t == "max-content" {
        return GTrack::Auto(0.0, f32::MAX);
    }
    if let Some(inner) = t
        .strip_prefix("minmax(")
        .and_then(|x| x.strip_suffix(')'))
    {
        let (a, b) = top_comma(inner).unwrap_or((inner, "auto"));
        let min = if a.trim() == "auto" {
            0.0
        } else {
            parse_len_vp(a.trim(), rel, em, vh, vw).unwrap_or(0.0)
        };
        let b = b.trim();
        if let Some(f) = b.strip_suffix("fr") {
            return GTrack::Fr(f.trim().parse::<f32>().unwrap_or(1.0).max(0.0), min);
        }
        if b == "auto" || b == "min-content" || b == "max-content" {
            return GTrack::Auto(min, f32::MAX);
        }
        return GTrack::Auto(min, parse_len_vp(b, rel, em, vh, vw).unwrap_or(f32::MAX));
    }
    if let Some(inner) = t
        .strip_prefix("fit-content(")
        .and_then(|x| x.strip_suffix(')'))
    {
        return GTrack::Auto(0.0, parse_len_vp(inner.trim(), rel, em, vh, vw).unwrap_or(f32::MAX));
    }
    GTrack::Fixed(parse_len_vp(&t, rel, em, vh, vw).unwrap_or(0.0))
}

/// `grid-template-*`/`grid-auto-*` value → expanded track list plus named
/// lines: `names[i]` holds the names of the line *before* track i (len =
/// tracks+1). `none`/empty → ([], []). repeat() expands; auto-fill/auto-fit
/// size against `avail` using the repeat unit's floor width.
fn grid_track_list(
    v: Option<&String>,
    rel: f32,
    em: f32,
    vh: f32,
    vw: f32,
    avail: f32,
    gap: f32,
) -> (Vec<GTrack>, Vec<Vec<String>>) {
    let Some(v) = v else { return (Vec::new(), Vec::new()) };
    let v = v.trim();
    if v.is_empty() || v == "none" {
        return (Vec::new(), Vec::new());
    }
    let mut out: Vec<GTrack> = Vec::new();
    let mut names: Vec<Vec<String>> = vec![Vec::new()];
    for tok in track_tokens(v) {
        let t = tok.trim().to_lowercase();
        if t.starts_with('[') {
            // [name ...] — names for the line at the current track boundary
            let inner = t.trim_start_matches('[').trim_end_matches(']');
            names[out.len()].extend(inner.split_whitespace().map(String::from));
            continue;
        }
        if let Some(inner) = t
            .strip_prefix("repeat(")
            .and_then(|x| x.strip_suffix(')'))
        {
            let (cnt, rest) = top_comma(inner).unwrap_or((inner, ""));
            let sub: Vec<GTrack> = track_tokens(rest)
                .iter()
                .filter(|s| !s.trim().starts_with('['))
                .map(|s| track_spec(s, rel, em, vh, vw))
                .collect();
            if sub.is_empty() {
                continue;
            }
            let cnt = cnt.trim();
            let n = if cnt == "auto-fill" || cnt == "auto-fit" {
                let unit: f32 =
                    sub.iter().map(track_floor).sum::<f32>() + gap * (sub.len() - 1) as f32;
                (((avail + gap) / (unit + gap).max(1.0)).floor() as usize).max(1)
            } else {
                cnt.parse::<usize>().unwrap_or(1)
            };
            for _ in 0..n.min(1000) {
                out.extend_from_slice(&sub);
            }
        } else {
            out.push(track_spec(&t, rel, em, vh, vw));
        }
        while names.len() <= out.len() {
            names.push(Vec::new());
        }
    }
    (out, names)
}

/// A grid line placement term: `auto`, an integer line, `span N`, or a
/// named line (`content`/`content-start`/`extended-full-end`).
#[derive(Clone, PartialEq)]
enum GLine {
    Auto,
    Line(i32),
    Span(i32),
    Named(String),
}

fn grid_line_val(t: &str) -> GLine {
    let t = t.trim().to_lowercase();
    if let Some(rest) = t.strip_prefix("span") {
        return GLine::Span(rest.trim().parse::<i32>().unwrap_or(1));
    }
    match t.parse::<i32>() {
        Ok(0) | Err(_) => {
            if t.is_empty() || t == "auto" {
                GLine::Auto
            } else {
                GLine::Named(t.to_string())
            }
        }
        Ok(n) => GLine::Line(n),
    }
}

/// `grid-column`/`grid-row` + `-start`/`-end`/`grid-area` → (start, end)
/// line terms. `grid-area: rs/cs/re/ce` sets all four at once.
fn grid_lines(c: &Props, axis: &str) -> (GLine, GLine) {
    let (mut s, mut e) = (GLine::Auto, GLine::Auto);
    if let Some(v) = c.get("grid-area") {
        let parts: Vec<&str> = v.split('/').map(str::trim).collect();
        if !parts.is_empty() {
            let (si, ei) = if axis == "row" { (0, 2) } else { (1, 3) };
            s = grid_line_val(parts.get(si).copied().unwrap_or("auto"));
            e = grid_line_val(parts.get(ei).copied().unwrap_or("auto"));
        }
    }
    if let Some(v) = c.get(&format!("grid-{axis}")) {
        let mut it = v.split('/');
        s = grid_line_val(it.next().unwrap_or(""));
        if let Some(t) = it.next() {
            e = grid_line_val(t);
        }
    }
    if let Some(v) = c.get(&format!("grid-{axis}-start")) {
        s = grid_line_val(v);
    }
    if let Some(v) = c.get(&format!("grid-{axis}-end")) {
        e = grid_line_val(v);
    }
    (s, e)
}

/// Resolve (start, end) line terms → (explicit 0-based track index, span).
/// Start is None when the item needs auto-placement on this axis.
/// Negative lines resolve against the explicit track count (-1 = last line).
/// `names[i]` are the names of the line before track i; a bare `x` start
/// term looks for `x-start` then `x`, an end term `x-end` then `x`.
fn resolve_axis(
    (s, e): (GLine, GLine),
    ntracks: usize,
    names: &[Vec<String>],
) -> (Option<usize>, usize) {
    let named = |g: &GLine, is_start: bool| -> GLine {
        let GLine::Named(n) = g else { return g.clone() };
        let suffix = if is_start { "-start" } else { "-end" };
        for w in [format!("{n}{suffix}"), n.clone()] {
            if let Some(i) = names.iter().position(|ns| ns.iter().any(|x| x == &w)) {
                return GLine::Line(i as i32 + 1);
            }
        }
        GLine::Auto
    };
    let (s, e) = (named(&s, true), named(&e, false));
    // line k>0 → track index k-1; line k<0 → ntracks+1+k (so -1 → ntracks)
    let idx = |l: i32| -> i64 {
        if l > 0 {
            l as i64 - 1
        } else {
            ntracks as i64 + 1 + l as i64
        }
    };
    match (s, e) {
        (GLine::Line(a), GLine::Line(b)) => {
            let (a, b) = (idx(a), idx(b));
            let (lo, hi) = (a.min(b), a.max(b));
            (Some(lo.max(0) as usize), (hi - lo).max(1) as usize)
        }
        (GLine::Line(a), GLine::Span(n)) => (Some(idx(a).max(0) as usize), n.max(1) as usize),
        (GLine::Line(a), GLine::Auto) => (Some(idx(a).max(0) as usize), 1),
        (GLine::Auto, GLine::Line(b)) => (Some((idx(b) - 1).max(0) as usize), 1),
        (GLine::Span(n), GLine::Line(b)) => {
            let end = idx(b);
            (Some((end - n as i64).max(0) as usize), n.max(1) as usize)
        }
        (GLine::Span(n), _) | (GLine::Auto, GLine::Span(n)) => (None, n.max(1) as usize),
        (GLine::Auto, GLine::Auto) => (None, 1),
        _ => (None, 1),
    }
}

/// Item/box alignment along one axis (justify-items/self, align-items/self).
#[derive(Clone, Copy, PartialEq)]
enum GAlign {
    Stretch,
    Start,
    End,
    Center,
}

fn galign(v: Option<&String>) -> Option<GAlign> {
    Some(match v?.trim() {
        "start" | "flex-start" | "left" | "top" | "self-start" | "baseline" => GAlign::Start,
        "end" | "flex-end" | "right" | "bottom" | "self-end" => GAlign::End,
        "center" => GAlign::Center,
        "stretch" | "normal" | "auto" => GAlign::Stretch,
        _ => return None,
    })
}

/// *-content distribution: leftover space → (extra per gap, start offset).
fn content_offset(v: Option<&String>, extra: f32, n: usize) -> (f32, f32) {
    let kw = v.map(|s| s.as_str()).unwrap_or("start");
    let kw = kw.split_whitespace().last().unwrap_or("start");
    match kw {
        "end" | "right" | "flex-end" | "bottom" => (0.0, extra),
        "center" => (0.0, extra / 2.0),
        "space-between" if n > 1 => (extra / (n - 1) as f32, 0.0),
        "space-around" => (extra / n.max(1) as f32, extra / n.max(1) as f32 / 2.0),
        "space-evenly" => (extra / (n + 1) as f32, extra / (n + 1) as f32),
        _ => (0.0, 0.0),
    }
}

/// Is the r×c rect of `rs`×`cs` cells free in the occupancy grid?
fn grid_free(occ: &[Vec<bool>], ncols: usize, r: usize, c: usize, rs: usize, cs: usize) -> bool {
    if c + cs > ncols {
        return false;
    }
    for row in occ.iter().skip(r).take(rs) {
        if row[c..c + cs].iter().any(|&x| x) {
            return false;
        }
    }
    true
}

// ---------- the layouter ----------

struct Word {
    text: String,
    font: usize,
    size: f32,
    color: Color,
    underline: bool,
    strike: bool,
    shift: f32,
    width: f32,
    ascent: f32,
    line_h: f32,
    href: Option<String>,
    /// inline background — FillRect emitted behind the text
    bg: Option<Color>,
    /// inline-block payload: display items in box-local coords (origin top-left)
    items: Option<Vec<DisplayItem>>,
    /// element boxes inside the payload — same coord space as `items`
    hits: Vec<(f32, f32, f32, f32, NodeId)>,
    /// inline-block height (baseline alignment: box bottom sits on baseline)
    box_h: f32,
}

/// A floated box occupying [x .. x+w] horizontally until `bottom`.
/// While `y < bottom`, lines inset by `w` on the float's side.
struct Float {
    right: bool,
    w: f32,
    bottom: f32,
}

/// Translate a display item by (dx, dy).
fn offset_item(it: DisplayItem, dx: f32, dy: f32) -> DisplayItem {
    match it {
        DisplayItem::Raster { x, y, w, h, pixels } => DisplayItem::Raster {
            x: x + dx,
            y: y + dy,
            w,
            h,
            pixels,
        },
        DisplayItem::RoundedRect { x, y, w, h, radius, color } => DisplayItem::RoundedRect {
            x: x + dx,
            y: y + dy,
            w,
            h,
            radius,
            color,
        },
        DisplayItem::Text {
            x,
            baseline,
            text,
            font,
            size,
            color,
            underline,
            strike,
            href,
        } => DisplayItem::Text {
            x: x + dx,
            baseline: baseline + dy,
            text,
            font,
            size,
            color,
            underline,
            strike,
            href,
        },
        DisplayItem::FillRect { x, y, w, h, color } => DisplayItem::FillRect {
            x: x + dx,
            y: y + dy,
            w,
            h,
            color,
        },
        DisplayItem::Image { x, y, w, h, src } => DisplayItem::Image {
            x: x + dx,
            y: y + dy,
            w,
            h,
            src,
        },
    }
}

/// Push the 4 edge FillRects of a `w`×`h` box border.
fn edge_rects(out: &mut Vec<DisplayItem>, x: f32, y: f32, w: f32, h: f32, bw: f32, color: Color) {
    out.push(DisplayItem::FillRect { x, y, w, h: bw, color });
    out.push(DisplayItem::FillRect { x, y: y + h - bw, w, h: bw, color });
    out.push(DisplayItem::FillRect { x, y, w: bw, h, color });
    out.push(DisplayItem::FillRect { x: x + w - bw, y, w: bw, h, color });
}

/// Push a FillRect per non-zero side of a per-side border.
fn side_rects(out: &mut Vec<DisplayItem>, x: f32, y: f32, w: f32, h: f32, bw: [f32; 4], color: Color) {
    if bw[0] > 0.0 {
        out.push(DisplayItem::FillRect { x, y, w, h: bw[0], color });
    }
    if bw[1] > 0.0 {
        out.push(DisplayItem::FillRect { x: x + w - bw[1], y, w: bw[1], h, color });
    }
    if bw[2] > 0.0 {
        out.push(DisplayItem::FillRect { x, y: y + h - bw[2], w, h: bw[2], color });
    }
    if bw[3] > 0.0 {
        out.push(DisplayItem::FillRect { x, y, w: bw[3], h, color });
    }
}

struct BlockPatch {
    /// index of first of up to 9 placeholder rects
    /// (bg + 4 border edges + 4 shadow/ring edges)
    idx: usize,
    x: f32,
    y: f32,
    w: f32,
    bg: Option<Color>,
    border: Option<([f32; 4], Color)>,
    radius: f32,
    shadow: Option<(f32, f32, f32, Color)>,
}

pub struct Layouter<'a> {
    doc: &'a Document,
    /// sheet set (UA + external + <style>); shared with worker StyleCtxs
    /// during the parallel pre-resolve
    sheets: Vec<&'a css::StyleSheet>,
    /// style resolution context (candidate buffer + cached :root customs)
    ctx: css::StyleCtx<'a>,
    /// computed-style memo — table cells, flex children and shrink-width
    /// probes all re-resolve the same nodes; the cascade is deterministic
    /// per node so the result is shared (Rc) instead of recomputed.
    style_cache: FxHashMap<NodeId, Rc<Props>>,
    /// (family, bold, italic) → font index memo — FontCache::resolve scans
    /// every installed face per call, so resolving once per style is a must.
    font_memo: FxHashMap<String, [u32; 4]>,
    fonts: &'a mut FontCache,
    items: Vec<DisplayItem>,
    y: f32,
    /// left edge of the current content box
    margin_x: f32,
    /// width available to the current content box (before float insets)
    avail_w: f32,
    align: Align,
    line: Vec<Word>,
    line_x: f32,
    pending_space: bool,
    list_stack: Vec<(bool, bool, usize)>,
    /// active floats; inset following lines until `y` passes `bottom`
    floats: Vec<Float>,
    /// current scroll offset — `position:fixed` anchors to the viewport
    scroll: f32,
    /// element `id` → document-y, for #fragment navigation
    anchors: Vec<(String, f32)>,
    /// viewport height — vh/svh units and flex justify-center
    viewport_h: f32,
    /// definite height of the current containing block — the nearest
    /// positioned ancestor with an explicit height; drives `top`+`bottom`
    /// stretching on `position:absolute` descendants
    cb_h: Option<f32>,
    /// `top`+`bottom` stretch of the block currently laying its children —
    /// feeds `grid_children`'s definite container height
    inset_h: Option<f32>,
    /// element border boxes (x, y, w, h, node) in paint order — hit-testing
    /// for click dispatch; later entries paint above earlier ones
    hits: Vec<(f32, f32, f32, f32, NodeId)>,
    /// color translucent paints composite over — the nearest ancestor
    /// background (body's dark page bg makes `bg-white/5` a dim veil)
    backdrop: Color,
    /// definite content height of the box currently laying its children —
    /// `%`/`h-full` heights on descendants (e.g. chess-piece imgs in fixed
    /// cells) resolve against it; propagated to auto-height children so a
    /// `h-full` chain still finds the nearest definite ancestor
    pct_h: Option<f32>,
}

impl<'a> Layouter<'a> {
    /// Layout `doc` into a display list `width` px wide. `scroll` shifts the
    /// document upward; returns (items, total document height).
    pub fn layout(
        doc: &'a Document,
        fonts: &'a mut FontCache,
        width: f32,
        scroll: f32,
        external: &[css::StyleSheet],
    ) -> (Vec<DisplayItem>, f32, Vec<(String, f32)>, Vec<(f32, f32, f32, f32, NodeId)>) {
        Self::layout_vh(doc, fonts, width, scroll, 768.0, external)
    }

    /// layout with an explicit viewport height (vh units, flex centering).
    pub fn layout_vh(
        doc: &'a Document,
        fonts: &'a mut FontCache,
        width: f32,
        scroll: f32,
        viewport_h: f32,
        external: &[css::StyleSheet],
    ) -> (Vec<DisplayItem>, f32, Vec<(String, f32)>, Vec<(f32, f32, f32, f32, NodeId)>) {
        let author = css::document_sheets_vw(doc, width);
        let mut sheets: Vec<&css::StyleSheet> = vec![ua_sheet()];
        sheets.extend(external.iter());
        sheets.extend(author.iter());
        let mut l = Layouter {
            doc,
            ctx: css::StyleCtx::new(doc, &sheets),
            sheets,
            style_cache: FxHashMap::default(),
            font_memo: FxHashMap::default(),
            fonts,
            items: Vec::new(),
            y: 8.0 - scroll,
            margin_x: 8.0,
            avail_w: width - 16.0,
            align: Align::Left,
            line: Vec::new(),
            line_x: 0.0,
            pending_space: false,
            list_stack: Vec::new(),
            floats: Vec::new(),
            scroll,
            anchors: Vec::new(),
            viewport_h,
            cb_h: None,
            inset_h: None,
            hits: Vec::new(),
            backdrop: WHITE,
            pct_h: None,
        };
        l.preresolve_styles();
        l.walk(doc.root, &base_style());
        l.flush_line();
        // floats don't consume vertical space at their site; make sure the
        // document is at least as tall as the deepest float
        for f in &l.floats {
            l.y = l.y.max(f.bottom);
        }
        let h = l.y + scroll + 8.0;
        (l.items, h, l.anchors, l.hits)
    }

    /// Horizontal space taken by floats on (left, right) at the current y.
    fn float_insets(&self) -> (f32, f32) {
        let (mut l, mut r) = (0.0f32, 0.0f32);
        for f in &self.floats {
            if self.y < f.bottom {
                if f.right {
                    r += f.w;
                } else {
                    l += f.w;
                }
            }
        }
        (l, r)
    }

    fn content_width(&self) -> f32 {
        let (l, r) = self.float_insets();
        (self.avail_w - l - r).max(1.0)
    }

    /// x where a new line starts (left float insets included).
    fn line_origin(&self) -> f32 {
        self.margin_x + self.float_insets().0
    }

    /// Resolve every element's computed style up front, fanning out across
    /// cores for large documents — the cascade is deterministic per node, so
    /// pre-filling `style_cache` doesn't change results; it just moves the
    /// ~90%-of-layout style work off the serial walk.
    fn preresolve_styles(&mut self) {
        let elems: Vec<NodeId> = self
            .doc
            .descendants(self.doc.root)
            .filter(|&id| self.doc.tag(id).is_some())
            .collect();
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(8)
            .min(elems.len() / 256);
        if workers <= 1 {
            for id in elems {
                self.style(id);
            }
            return;
        }
        let chunk = elems.len().div_ceil(workers);
        let mut out: Vec<Option<Props>> =
            std::iter::repeat_with(|| None).take(elems.len()).collect();
        let doc = self.doc;
        let sheets = &self.sheets[..];
        std::thread::scope(|s| {
            for (work, oc) in elems.chunks(chunk).zip(out.chunks_mut(chunk)) {
                s.spawn(move || {
                    let mut ctx = css::StyleCtx::new(doc, sheets);
                    for (i, &id) in work.iter().enumerate() {
                        oc[i] = Some(ctx.computed(id));
                    }
                });
            }
        });
        for (id, p) in elems.into_iter().zip(out) {
            if let Some(p) = p {
                self.style_cache.insert(id, Rc::new(p));
            }
        }
    }

    /// Computed style for `node`, memoized per node.
    fn style(&mut self, node: NodeId) -> Rc<Props> {
        if let Some(c) = self.style_cache.get(&node) {
            return Rc::clone(c);
        }
        let c = Rc::new(self.ctx.computed(node));
        self.style_cache.insert(node, Rc::clone(&c));
        c
    }

    fn walk(&mut self, node: NodeId, style: &Style) {
        match self.doc.kind(node) {
            NodeKind::Document => {
                for &c in self.doc.children(node) {
                    self.walk(c, style);
                }
            }
            NodeKind::Text(t) => self.text(t, style),
            NodeKind::Comment(_) => {}
            NodeKind::Element { tag, .. } => {
                if let Some(id) = self.doc.attr(node, "id") {
                    if !id.is_empty() {
                        self.anchors.push((id.to_string(), self.y + self.scroll));
                    }
                }
                let c = self.style(node);
                let mut st = style.clone();
                self.apply_computed(&c, &mut st);
                // the `.sr-only` a11y pattern — a 1x1 overflow:hidden clip.
                // Laying it out would wrap its text into a visible strip.
                let visually_hidden = c.get("overflow").map(String::as_str) == Some("hidden")
                    && dim(c.as_ref(), "width", self.avail_w, st.size).is_some_and(|w| w <= 2.0)
                    && dim(c.as_ref(), "height", self.avail_w, st.size).is_some_and(|h| h <= 2.0);
                // replaced/void elements emit their payload via inline()
                // whatever `display` says (Tailwind blocks imgs with
                // `display:block` — that must not swallow the image)
                let replaced = matches!(
                    tag.as_str(),
                    "img" | "svg" | "input" | "select" | "textarea" | "video" | "canvas"
                        | "iframe" | "object" | "embed" | "br"
                );
                // `opacity` folds into every emitted item: the painter
                // stores flattened opaque colors, so each item emitted for
                // this subtree is faded toward the backdrop beneath it.
                let op = c
                    .get("opacity")
                    .and_then(|v| v.trim().parse::<f32>().ok())
                    .unwrap_or(1.0)
                    .clamp(0.0, 1.0);
                let (i0, h0, bd) = (self.items.len(), self.hits.len(), self.backdrop);
                // visibility:hidden keeps its box but paints nothing — the
                // same truncation as opacity:0 (a `visibility:visible`
                // descendant that should re-show is rare and unsupported)
                let hidden = matches!(
                    c.get("visibility").map(String::as_str),
                    Some("hidden") | Some("collapse")
                );
                match c.get("display").map(String::as_str) {
                    _ if visually_hidden => return,
                    Some("none") => return,
                    // CSS 2.1 §9.7: absolute/fixed blockify AND leave normal
                    // flow — an <input> overlay (Vector's dropdown checkbox)
                    // or an <a> must not contribute line box size either
                    _ if matches!(
                        c.get("position").map(String::as_str),
                        Some("absolute") | Some("fixed")
                    ) =>
                    {
                        self.block(node, tag, &c, st);
                    }
                    _ if replaced => self.inline(node, tag, &c, st),
                    Some("block") | Some("list-item") | Some("flex") | Some("grid") => {
                        self.block(node, tag, &c, st);
                    }
                    Some("inline-block") | Some("inline-flex") | Some("inline-grid") => {
                        self.inline_block(node, &c, st)
                    }
                    _ => self.inline(node, tag, &c, st),
                }
                if op <= 0.0 || hidden {
                    // fully transparent/hidden: nothing painted, nothing clickable
                    self.items.truncate(i0);
                    self.hits.truncate(h0);
                } else if op < 1.0 {
                    for it in &mut self.items[i0..] {
                        match it {
                            DisplayItem::Text { color, .. }
                            | DisplayItem::FillRect { color, .. }
                            | DisplayItem::RoundedRect { color, .. } => {
                                *color = fade(*color, bd, op);
                            }
                            DisplayItem::Raster { pixels, .. } => {
                                let o = (op * 255.0) as u32;
                                for p in pixels.chunks_exact_mut(4) {
                                    for ch in p.iter_mut() {
                                        *ch = (*ch as u32 * o / 255) as u8;
                                    }
                                }
                            }
                            DisplayItem::Image { .. } => {}
                        }
                    }
                }
            }
        }
    }

    fn apply_computed(&mut self, c: &Props, st: &mut Style) {
        if let Some(v) = c.get("font-size") {
            st.size = parse_font_size(v, st.size);
        }
        if let Some(v) = c.get("font-weight") {
            st.bold = matches!(v.as_str(), "bold" | "bolder")
                || v.parse::<u32>().is_ok_and(|n| n >= 600);
        }
        if let Some(v) = c.get("font-style") {
            st.italic = matches!(v.as_str(), "italic" | "oblique");
        }
        if let Some(v) = c.get("text-transform") {
            st.transform = match v.trim() {
                "uppercase" => 1,
                "lowercase" => 2,
                "capitalize" => 3,
                _ => 0,
            };
        }
        if let Some(v) = c.get("font-family") {
            let first = v.split(',').next().unwrap_or("").trim().trim_matches('"').trim_matches('\'').to_lowercase();
            // generic families map to our bundled Liberation set; anything
            // else is passed through so FontCache::resolve can fuzzy-match
            // real installed family names (e.g. "DejaVu Sans")
            st.family = match first.as_str() {
                "monospace" | "mono" | "fixed" | "fixed-width" => {
                    "liberation mono".to_string()
                }
                "serif" => "liberation serif".to_string(),
                "" | "sans" | "sans-serif" | "system-ui" => "liberation sans".to_string(),
                other => other.to_string(),
            };
        }
        if let Some(v) = c.get("color") {
            if let Some(col) = parse_color(v) {
                st.color = over(col, self.backdrop);
            }
        }
        if let Some(v) = c.get("text-decoration") {
            st.underline = v.contains("underline");
            st.strike = v.contains("line-through");
        }
        if let Some(v) = c.get("text-align") {
            st.align = match v.as_str() {
                "center" => Align::Center,
                "right" => Align::Right,
                _ => Align::Left,
            };
        }
        if let Some(v) = c.get("white-space") {
            let v = v.trim();
            st.pre = matches!(v, "pre" | "pre-wrap");
            st.nowrap = matches!(v, "pre" | "pre-wrap" | "nowrap");
        }
        if let Some(v) = c.get("overflow-wrap").or_else(|| c.get("word-wrap")) {
            st.break_word |= matches!(v.trim(), "break-word" | "anywhere");
        }
        if let Some(v) = c.get("word-break") {
            st.break_word |= matches!(v.trim(), "break-all" | "break-word");
        }
        if let Some(v) = c.get("vertical-align") {
            st.shift = match v.as_str() {
                "sub" => -0.3,
                "super" => 0.35,
                _ => 0.0,
            };
        }
        if let Some(v) = c.get("line-height") {
            let v = v.trim();
            st.line_h = if v == "normal" {
                None
            } else if let Ok(n) = v.parse::<f32>() {
                Some(LineH::Mul(n))
            } else {
                parse_len(v, st.size, st.size).map(LineH::Px)
            };
        }
        if let Some(col) = bg_color(c) {
            st.inline_bg = Some(over(col, self.backdrop));
        }
    }

    fn block(&mut self, node: NodeId, tag: &str, c: &Props, mut st: Style) {
        self.flush_line();
        if tag == "table" {
            return self.table(node, c, st);
        }
        // positioned boxes: fixed/sticky anchor to the viewport top, absolute
        // and fixed take `left` against the containing width (approximation —
        // flow space is still reserved, no z-order yet)
        // positioned boxes leave no flow space: remember the flow y and
        // restore it after the children are laid (block-end restore below)
        let positioned = matches!(
            c.get("position").map(String::as_str),
            Some("fixed") | Some("sticky") | Some("absolute")
        );
        // explicit `height` — definite containing-block height for positioned
        // descendants and a floor for this box's own extent. % resolves
        // against the nearest definite ancestor height, else the viewport.
        let h_prop = dim(c, "height", self.pct_h.unwrap_or(self.viewport_h), st.size);
        // `inset` shorthand + longhands merged into [top, right, bottom, left];
        // % on the block axis resolves against the containing height
        let hrel = self.cb_h.or(self.pct_h).unwrap_or(self.viewport_h);
        let ish = sides(c, "inset", self.avail_w, st.size);
        let ish_h = sides(c, "inset", hrel, st.size);
        let inset_l = |name: &str, i: usize, rel: f32| {
            c.get(name).and_then(|v| parse_len(v, rel, st.size)).or(if i % 2 == 0 { ish_h[i] } else { ish[i] })
        };
        let ins_t = inset_l("top", 0, hrel);
        let ins_r = inset_l("right", 1, self.avail_w);
        let ins_b = inset_l("bottom", 2, hrel);
        let ins_l = inset_l("left", 3, self.avail_w);
        let flow_y = if positioned {
            let fy = self.y;
            let fmx = self.margin_x;
            let pos = c.get("position").map(String::as_str);
            if let Some(t) = ins_t {
                // y is viewport-mapped (scroll already baked in): fixed pins
                // to `top`, sticky clamps at `top` once scrolled past
                self.y = match pos {
                    Some("absolute") => fy + t,
                    Some("sticky") => self.y.max(t),
                    _ => t,
                };
            }
            if let Some(l) = ins_l {
                self.margin_x = fmx + l.max(0.0);
            }
            Some((fy, fmx))
        } else {
            None
        };
        // `left`+`right`/`top`+`bottom` on a positioned box stretch it to the
        // containing block (approx: current available width / viewport height)
        let inset_w = ins_l
            .zip(ins_r)
            .map(|(l, r)| (self.avail_w - l - r).max(0.0));
        let inset_h = ins_t
            .zip(ins_b)
            .map(|(t, b)| (self.cb_h.unwrap_or(self.viewport_h) - t - b).max(0.0));
        let size = st.size;
        let [mt, mr, mb, ml] = box_sides(c, "margin", self.avail_w, size);
        let [pt, pr, pb, pl] = box_sides(c, "padding", self.avail_w, size).map(|v| v.max(0.0));
        let bd = self.backdrop;
        let bb = border_box(c);
        let bg = self.bg_of(node, c).map(|col| over(col, bd));
        let border = border_sides(c, size)
            .map(|(w, col)| (w, over(col, bg.unwrap_or(bd))));
        // the block's own background paints behind the whole box, not per word
        st.inline_bg = None;
        // this element's text sits on its own bg — re-composite the color
        if let Some(b) = bg {
            if let Some(col) = c.get("color").and_then(|v| parse_color(v)) {
                st.color = over(col, b);
            }
        }
        let [bw_t, bw_r, bw_b, bw_l] = border.map(|b| b.0).unwrap_or([0.0; 4]);
        let bw_h = bw_l + bw_r; // horizontal border total
        let bw_v = bw_t + bw_b; // vertical border total

        if matches!(c.get("float").map(String::as_str), Some("left") | Some("right")) {
            return self.float_block(node, c, st, mt, mb);
        }

        // `width` caps the content box; `margin-?:auto` splits leftover space
        let mut ml = ml;
        let mut mr = mr;
        // inset-stretched boxes are sized by left+right, a border-box length
        let w_explicit = dim(c, "width", self.avail_w, size);
        let mut w_prop = w_explicit.or(inset_w);
        if let Some(mxw) = c.get("max-width").and_then(|v| parse_len_vp(v, self.avail_w, size, self.viewport_h, self.avail_w)) {
            w_prop = Some(w_prop.unwrap_or(self.avail_w).min(mxw));
        }
        if let Some(mnw) = c.get("min-width").and_then(|v| parse_len_vp(v, self.avail_w, size, self.viewport_h, self.avail_w)) {
            // min-width constrains the box — it must not *create* a width:
            // `min-width:0` on a width:auto block must stay full-width
            if mnw > 0.0 {
                w_prop = Some(w_prop.unwrap_or(self.avail_w).max(mnw));
            }
        }
        let w_prop = w_prop;
        // does `w_prop` already include padding+border? (border-box, or an
        // inset stretch which is always a border-box length)
        let w_is_outer = bb || w_explicit.is_none();
        if let Some(w) = w_prop {
            let outer = if w_is_outer { w } else { w + pl + pr + bw_h };
            let extra = (self.avail_w - ml - mr - outer).max(0.0);
            match (margin_auto(c, true), margin_auto(c, false)) {
                (true, true) => {
                    ml += extra / 2.0;
                    mr += extra / 2.0;
                }
                (true, false) => ml += extra,
                (false, true) => mr += extra,
                (false, false) => {}
            }
        }

        // `list-style[-type]:none` on the list or the item hides markers
        let hide_marker = c.get("list-style-type").is_some_and(|v| v.trim() == "none")
            || c.get("list-style").is_some_and(|v| v.contains("none"));
        match tag {
            "ul" | "menu" | "dir" => self.list_stack.push((hide_marker, false, 0)),
            "ol" => self.list_stack.push((hide_marker, true, 0)),
            _ => {}
        }
        // <ol>/<ul> counter only ticks on <li>
        let marker = if tag == "li" && !hide_marker {
            let (hide, ordered, num) = match self.list_stack.last_mut() {
                Some((h, o, n)) => {
                    *n += 1;
                    (*h, *o, *n)
                }
                None => (false, false, 1),
            };
            if hide {
                None
            } else {
                Some(if ordered { format!("{num}. ") } else { "•  ".to_string() })
            }
        } else {
            None
        };

        self.y += mt.max(0.0);
        let saved_mx = self.margin_x;
        let saved_avail = self.avail_w;
        let saved_align = self.align;
        let inner_w = if let Some(w) = w_prop {
            if w_is_outer {
                (w - pl - pr - bw_h).max(0.0)
            } else {
                w.max(0.0)
            }
        } else {
            (self.avail_w - ml - mr - pl - pr - bw_h).max(1.0)
        };
        // a definite-width block in a text-align:center context (<center>)
        // or with margin-inline:auto gets centered
        if w_prop.is_some() {
            let outer = inner_w + pl + pr + bw_h;
            let centered = saved_align == Align::Center
                || (margin_auto(c, true) && margin_auto(c, false));
            if centered && outer < self.avail_w {
                self.margin_x += ((self.avail_w - outer) / 2.0).max(0.0);
            }
        }
        self.margin_x += ml.max(0.0) + pl + bw_l;
        self.avail_w = inner_w;
        self.align = st.align;

        // background/border need the final rect — reserve placeholders.
        // The patch covers the border box (padding included).
        // 9 slots: 4 shadow/ring edges (or 1 blob) + 1 bg + 4 border edges
        let shadow = parse_shadow(c, size, bd, st.color);
        let patch = if bg.is_some() || border.is_some() || shadow.is_some() {
            let idx = self.items.len();
            for _ in 0..9 {
                self.items.push(DisplayItem::FillRect {
                    x: 0.0,
                    y: 0.0,
                    w: 0.0,
                    h: 0.0,
                    color: 0,
                });
            }
            Some(BlockPatch {
                idx,
                x: self.margin_x - pl - bw_l,
                y: self.y,
                w: pl + inner_w + pr + bw_h,
                bg,
                border,
                radius: parse_radius(c, size),
                shadow,
            })
        } else {
            None
        };

        // padding-top + border push content down inside the painted box
        self.y += pt + bw_t;
        let content_top = self.y;
        let children_items_start = self.items.len();
        let children_hits_start = self.hits.len();
        // descendants composite translucent paints over this box's bg
        let saved_bd = self.backdrop;
        if let Some(b) = bg {
            self.backdrop = b;
        }
        // content-height target: min-height/height/max-height/aspect-ratio.
        // Under border-box these are border-box lengths → strip padding+border.
        let ht = |v: f32| if bb { v - pt - pb - bw_v } else { v };
        let hrel = self.pct_h.unwrap_or(self.viewport_h);
        let mut mh_prop = h_prop;
        if let Some(mn) = c
            .get("min-height")
            .and_then(|v| parse_len_vp(v, hrel, size, self.viewport_h, self.avail_w))
        {
            mh_prop = Some(mh_prop.map_or(mn, |h| h.max(mn)));
        }
        if let Some(mx) = c
            .get("max-height")
            .and_then(|v| parse_len_vp(v, hrel, size, self.viewport_h, self.avail_w))
        {
            mh_prop = mh_prop.map(|h| h.min(mx));
        }
        let mh_prop = mh_prop.map(ht).or_else(|| {
            // aspect-ratio applies to the box-size the element uses
            aspect_ratio(c).map(|r| {
                if bb {
                    (inner_w + pl + pr + bw_h) / r - pt - pb - bw_v
                } else {
                    inner_w / r
                }
            })
        });
        // %-height children resolve against a definite content height;
        // auto-height boxes pass the nearest definite ancestor through
        let saved_ph = self.pct_h;
        self.pct_h = mh_prop
            .or_else(|| inset_h.map(|ih| ih - pt - pb - bw_v))
            .or(saved_ph);
        if tag == "hr" {
            self.items.push(DisplayItem::FillRect {
                x: self.line_origin(),
                y: self.y + 0.3 * st.size,
                w: self.content_width(),
                h: 1.5,
                color: RULE,
            });
            self.y += st.size * 0.4 + 6.0;
        } else {
            if let Some(m) = marker {
                self.word(&m, &st);
            }
            // containing-block height for positioned descendants: a box with
            // position + a definite height anchors `inset`-stretched children
            let saved_cb_h = self.cb_h;
            let saved_inset = self.inset_h;
            if positioned || c.get("position").map(String::as_str) == Some("relative") {
                if let Some(h) = h_prop {
                    self.cb_h = Some(h);
                }
            }
            self.inset_h = inset_h;
            match c.get("display").map(String::as_str) {
                Some("flex") | Some("inline-flex") => self.flex_children(node, c, st),
                Some("grid") | Some("inline-grid") => self.grid_children(node, c, &st),
                _ => {
                    for &ch in self.doc.children(node) {
                        self.walk(ch, &st);
                    }
                }
            }
            self.cb_h = saved_cb_h;
            self.inset_h = saved_inset;
            self.flush_line();
            // grow the box to the content-height target computed above;
            // centered flex content shifts down by half the surplus
            if let Some(mh) = mh_prop {
                let extra = mh - (self.y - content_top);
                if extra > 0.0 {
                    // flex row `items-center` / any `justify-content:center`:
                    // children shift down by half the surplus
                    let is_col =
                        c.get("flex-direction").map(String::as_str) == Some("column");
                    let centered = c.get("display").map(String::as_str) == Some("flex")
                        && (matches!(
                            c.get("justify-content").map(String::as_str),
                            Some("center") | Some("space-evenly") | Some("space-around")
                        ) || (!is_col
                            && (c.get("align-items").map(String::as_str) == Some("center")
                                || c.get("place-items").map(String::as_str) == Some("center"))));
                    if centered {
                        let dy = extra / 2.0;
                        for i in children_items_start..self.items.len() {
                            let it = std::mem::replace(
                                &mut self.items[i],
                                DisplayItem::FillRect { x: 0.0, y: 0.0, w: 0.0, h: 0.0, color: 0 },
                            );
                            self.items[i] = offset_item(it, 0.0, dy);
                        }
                        for h in self.hits.iter_mut().skip(children_hits_start) {
                            h.1 += dy;
                        }
                    }
                    self.y += extra;
                }
            }
        }
        self.backdrop = saved_bd;
        self.pct_h = saved_ph;
        // positioned box with `top`+`bottom` (e.g. `inset:0`): stretch to the
        // containing height even when content is shorter. The inset extent is
        // a border-box length.
        if let Some(ih) = inset_h {
            let extra = ih - pt - pb - bw_v - (self.y - content_top);
            if extra > 0.0 {
                self.y += extra;
            }
        }
        // padding-bottom + border extend the painted box below content
        self.y += pb + bw_b;

        if let Some(p) = patch.as_ref() {
            self.finish_patch(p);
        }
        // element border box → click hit-test map
        self.hits.push((
            self.margin_x - pl - bw_l,
            content_top - pt - bw_t,
            inner_w + pl + pr + bw_h,
            self.y - content_top + pt + bw_t,
            node,
        ));
        // `translate`/`transform:translate*()` + `position:relative` offsets —
        // post-layout visual offset; flow position is unaffected.
        let (mut tx, mut ty) = translate_xy(
            c,
            inner_w + pl + pr + bw_h,
            self.y - (content_top - pt - bw_t),
            size,
        );
        if c.get("position").map(String::as_str) == Some("relative") {
            tx += ins_l.unwrap_or(0.0) - ins_r.unwrap_or(0.0);
            ty += ins_t.unwrap_or(0.0) - ins_b.unwrap_or(0.0);
        }
        if tx != 0.0 || ty != 0.0 {
            for i in children_items_start..self.items.len() {
                let it = std::mem::replace(
                    &mut self.items[i],
                    DisplayItem::FillRect { x: 0.0, y: 0.0, w: 0.0, h: 0.0, color: 0 },
                );
                self.items[i] = offset_item(it, tx, ty);
            }
            if let Some(p) = &patch {
                for i in p.idx..p.idx + 9usize.min(self.items.len() - p.idx) {
                    let it = std::mem::replace(
                        &mut self.items[i],
                        DisplayItem::FillRect { x: 0.0, y: 0.0, w: 0.0, h: 0.0, color: 0 },
                    );
                    self.items[i] = offset_item(it, tx, ty);
                }
            }
            for h in self.hits.iter_mut().skip(children_hits_start) {
                h.0 += tx;
                h.1 += ty;
            }
        }
        self.margin_x = saved_mx;
        self.avail_w = saved_avail;
        self.align = saved_align;
        // positioned elements reserve no flow space
        if let Some((fy, fmx)) = flow_y {
            self.y = fy;
            self.margin_x = fmx;
        }
        self.y += mb.max(0.0);
        if tag == "ul" || tag == "ol" || tag == "menu" || tag == "dir" {
            self.list_stack.pop();
        }
    }

    /// `float:left|right` block: shrink-wrapped (or `width`-sized), placed at
    /// the margin edge; following lines inset via `self.floats` until `bottom`.
    /// Approximations: shrink-wrap = longest word (min-content); no stacking
    /// when floats don't fit; floats don't consume vertical space in flow.
    fn float_block(
        &mut self,
        node: NodeId,
        c: &Props,
        mut st: Style,
        mt: f32,
        mb: f32,
    ) {
        let size = st.size;
        let [pt, pr, pb, pl] = box_sides(c, "padding", self.avail_w, size).map(|v| v.max(0.0));
        let [_t, mr, _b, ml] = box_sides(c, "margin", self.avail_w, size);
        let bd = self.backdrop;
        let bg = self.bg_of(node, c).map(|col| over(col, bd));
        let border = border_sides(c, size)
            .map(|(w, col)| (w, over(col, bg.unwrap_or(bd))));
        st.inline_bg = None;
        let [bw_t, bw_r, bw_b, bw_l] = border.map(|b| b.0).unwrap_or([0.0; 4]);
        let bw_h = bw_l + bw_r;
        let right = c.get("float").is_some_and(|v| v.trim() == "right");

        let inner = if let Some(w) = dim(c, "width", self.avail_w, size) {
            let w = if border_box(c) { w - pl - pr - bw_h } else { w };
            w.max(1.0)
        } else {
            self.shrink_width(node, &st)
        };
        let (li, ri) = self.float_insets();
        let room = (self.avail_w - li - ri).max(1.0);
        let box_w = (inner + pl + pr + bw_h).min(room);
        let inner = (box_w - pl - pr - bw_h).max(1.0);

        self.y += mt.max(0.0);
        let top = self.y;
        let saved = (self.margin_x, self.avail_w, self.align);
        let outer_x = if right {
            self.margin_x + self.avail_w - ri - box_w - mr.max(0.0)
        } else {
            self.margin_x + li + ml.max(0.0)
        };
        self.margin_x = outer_x + pl + bw_l;
        self.avail_w = inner;
        self.align = st.align;

        let shadow = parse_shadow(c, size, bd, st.color);
        let patch = if bg.is_some() || border.is_some() || shadow.is_some() {
            let idx = self.items.len();
            for _ in 0..9 {
                self.items.push(DisplayItem::FillRect {
                    x: 0.0,
                    y: 0.0,
                    w: 0.0,
                    h: 0.0,
                    color: 0,
                });
            }
            Some(BlockPatch {
                idx,
                x: outer_x,
                y: self.y,
                w: box_w,
                bg,
                border,
                radius: parse_radius(c, size),
                shadow,
            })
        } else {
            None
        };

        self.y += pt + bw_t;
        let saved_bd = self.backdrop;
        if let Some(b) = bg {
            self.backdrop = b;
        }
        for &ch in self.doc.children(node) {
            self.walk(ch, &st);
        }
        self.flush_line();
        self.backdrop = saved_bd;
        self.y += pb + bw_b;
        if let Some(p) = patch {
            self.finish_patch(&p);
        }
        let bottom = self.y + mb.max(0.0);

        self.margin_x = saved.0;
        self.avail_w = saved.1;
        self.align = saved.2;
        // floats take no vertical space in normal flow
        self.y = top;
        self.floats.push(Float {
            right,
            w: box_w + ml.max(0.0) + mr.max(0.0),
            bottom,
        });
    }

    /// `display:inline-block`: a box sitting on the text line like a word.
    /// Content is laid out off-screen via `nest`, then emitted offset to the
    /// box's position on the line. Bottom of the box sits on the baseline.
    /// `hint` overrides the shrink-wrap width (flex items lay at their
    /// resolved main size — basis+grow share, or max-content base).
    fn inline_block_w(&mut self, node: NodeId, c: &Props, st: Style, hint: Option<f32>) {
        self.inline_block_inner(node, c, st, hint)
    }

    fn inline_block(&mut self, node: NodeId, c: &Props, st: Style) {
        self.inline_block_inner(node, c, st, None)
    }

    fn inline_block_inner(&mut self, node: NodeId, c: &Props, mut st: Style, hint: Option<f32>) {
        let size = st.size;
        let [pt, pr, pb, pl] = box_sides(c, "padding", self.avail_w, size).map(|v| v.max(0.0));
        let [_t, mr, mb, ml] = box_sides(c, "margin", self.avail_w, size);
        let bd = self.backdrop;
        let bb = border_box(c);
        let bg = self.bg_of(node, c).map(|col| over(col, bd));
        let border = border_sides(c, size)
            .map(|(w, col)| (w, over(col, bg.unwrap_or(bd))));
        st.inline_bg = None; // the box paints its own bg
        if let Some(b) = bg {
            if let Some(col) = c.get("color").and_then(|v| parse_color(v)) {
                st.color = over(col, b);
            }
        }
        let [bw_t, bw_r, bw_b, bw_l] = border.map(|b| b.0).unwrap_or([0.0; 4]);
        let bw_h = bw_l + bw_r;
        let bw_v = bw_t + bw_b;

        // width property else shrink-wrap (min-content approx; flex sums children)
        let inner = if let Some(w) = hint {
            // resolved flex main size — an outer (border-box) target
            (w - pl - pr - bw_h).max(1.0)
        } else if let Some(w) = dim(c, "width", self.avail_w, size) {
            if bb {
                w - pl - pr - bw_h
            } else {
                w
            }
            .max(1.0)
        } else if matches!(
            c.get("display").map(String::as_str),
            Some("flex") | Some("inline-flex") | Some("inline-grid")
        ) {
            self.flex_maxc_width(node, c, &st)
        } else {
            // shrink-to-fit ≈ max-content — capped by the remaining line room
            // in `box_w` below (spec: min(max-content, available))
            self.max_content_width(node, &st)
        };
        let room = self.content_width();
        let box_w = (inner + pl + pr + bw_h).min(room);
        let inner = (box_w - pl - pr - bw_h).max(1.0);

        // `height`/aspect-ratio/min|max-height: under border-box these are
        // border-box lengths; content-box adds padding+border.
        let hrel = self.pct_h.unwrap_or(self.viewport_h);
        let outer_h = |v: f32| if bb { v } else { v + pt + pb + bw_v };
        let mut h_prop =
            dim(c, "height", hrel, size).or_else(|| aspect_ratio(c).map(|r| box_w / r));
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
        let (items, y_end, nhits) =
            self.nest(node, &st, pl + bw_l, pt + bw_t, inner, c);
        self.backdrop = saved_bd;
        self.pct_h = saved_ph;
        // explicit height/aspect-ratio/max-height may pin the painted box;
        // min-height alone only floors it
        let natural = y_end + pb + bw_b;
        let pinned = dim(c, "height", hrel, size).is_some()
            || aspect_ratio(c).is_some()
            || c.get("max-height").is_some();
        let box_h = match (h_prop, pinned) {
            (Some(h), true) => outer_h(h).max(1.0),
            (Some(h), false) => outer_h(h).max(natural),
            _ => natural.max(1.0),
        };

        let mut out: Vec<DisplayItem> = Vec::new();
        if let Some(col) = bg {
            out.push(DisplayItem::FillRect {
                x: 0.0,
                y: 0.0,
                w: box_w,
                h: box_h,
                color: col,
            });
        }
        if let Some((bw2, col)) = border {
            side_rects(&mut out, 0.0, 0.0, box_w, box_h, bw2, col);
        }
        out.extend(items);
        // translate/transform: visual offset of the whole box
        let (tx, ty) = translate_xy(c, box_w, box_h, size);
        let nhits: Vec<_> = if tx != 0.0 || ty != 0.0 {
            out = out.into_iter().map(|it| offset_item(it, tx, ty)).collect();
            nhits.into_iter().map(|h| (h.0 + tx, h.1 + ty, h.2, h.3, h.4)).collect()
        } else {
            nhits
        };

        // a pending inter-word space becomes a gap before the box
        if self.pending_space && !self.line.is_empty() {
            let space_w = self
                .resolve_font(&st)
                .and_then(|f| {
                    self.fonts.font(f).map(|f| {
                        f.advance_units(f.glyph_id(' ')) as f32 * st.size
                            / f.units_per_em() as f32
                    })
                })
                .unwrap_or(st.size * 0.3);
            self.line_x += space_w;
            if let Some(last) = self.line.last_mut() {
                last.width += space_w;
            }
        }
        self.pending_space = false;
        if ml > 0.0 {
            self.spacer(ml);
        }
        self.push_box(box_w, box_h, out, nhits, mb.max(0.0));
        if mr > 0.0 {
            self.spacer(mr);
        }
    }

    /// Minimal auto table layout: column width = max over rows of each cell's
    /// min-content width (longest word + padding + border), scaled down to fit.
    /// Each row is laid out cell-by-cell via `nest` at the column's x.
    /// Approximations: no colspan/rowspan, borders always drawn per-cell
    /// (border-collapse ≈ same visual since cells abut), rows don't wrap.
    fn table(&mut self, node: NodeId, c: &Props, mut st: Style) {
        let size = st.size;
        let [mt, _mr, mb, ml] = box_sides(c, "margin", self.avail_w, size);
        let bd = self.backdrop;
        let bg = self.bg_of(node, c).map(|col| over(col, bd));
        let border = border_sides(c, size)
            .map(|(w, col)| (w, over(col, bg.unwrap_or(bd))));
        st.inline_bg = None;
        if let Some(b) = bg {
            if let Some(col) = c.get("color").and_then(|v| parse_color(v)) {
                st.color = over(col, b);
            }
        }
        // <table border=N> fallback cell borders (border="0" = none)
        let attr_border = self
            .doc
            .attr(node, "border")
            .and_then(|v| v.trim().parse::<f32>().ok())
            .filter(|w| *w > 0.0)
            .map(|w| ([w; 4], BLACK));

        // gather rows: direct <tr> children plus tr inside row groups
        let cells_of = |doc: &Document, tr: NodeId| -> Vec<NodeId> {
            doc.children(tr)
                .iter()
                .copied()
                .filter(|&ch| matches!(doc.tag(ch), Some("td") | Some("th")))
                .collect()
        };
        let mut rows: Vec<Vec<NodeId>> = Vec::new();
        let mut extra: Vec<NodeId> = Vec::new();
        for &ch in self.doc.children(node) {
            match self.doc.tag(ch) {
                Some("tr") => rows.push(cells_of(self.doc, ch)),
                Some("tbody") | Some("thead") | Some("tfoot") => {
                    for &g in self.doc.children(ch) {
                        if self.doc.tag(g) == Some("tr") {
                            rows.push(cells_of(self.doc, g));
                        } else {
                            extra.push(g);
                        }
                    }
                }
                _ => extra.push(ch),
            }
        }

        // column widths: per-column min-content (col_w) and max-content
        // (col_pref); colspan>1 cells share their width across the spanned
        // columns (deficit split evenly)
        let colspan = |cell| {
            self.doc
                .attr(cell, "colspan")
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(1)
                .max(1)
        };
        let ncols = rows
            .iter()
            .map(|r| r.iter().map(|&c| colspan(c)).sum())
            .max()
            .unwrap_or(0);
        let mut col_w = vec![0.0f32; ncols];
        let mut col_pref = vec![0.0f32; ncols];
        for row in &rows {
            let mut i = 0usize;
            for &cell in row {
                let span = colspan(cell).min(ncols - i.min(ncols));
                let cs = self.style(cell);
                let mut cst = st.clone();
                self.apply_computed(&cs, &mut cst);
                let [_t, pr, _b, pl] = box_sides(&cs, "padding", self.avail_w, cst.size).map(|v| v.max(0.0));
                let [_, cbw_r, _, cbw_l] = border_sides(&cs, cst.size)
                    .or(attr_border)
                    .map(|b| b.0)
                    .unwrap_or([0.0; 4]);
                let cbw_h = cbw_l + cbw_r;
                let mut w = self.shrink_width(cell, &cst) + pl + pr + cbw_h;
                let mut pw = self.max_content_width(cell, &cst) + pl + pr + cbw_h;
                if let Some(wp) = dim(&cs, "width", self.avail_w, cst.size) {
                    let wp = if border_box(&cs) { wp } else { wp + pl + pr + cbw_h };
                    w = w.max(wp);
                    pw = pw.max(wp);
                }
                if span <= 1 {
                    col_w[i] = col_w[i].max(w);
                    col_pref[i] = col_pref[i].max(pw);
                } else {
                    // min-content only: a colspan cell's intrinsic width must
                    // still fit somewhere, but its max-content shouldn't
                    // inflate every spanned column's growth need
                    let cur: f32 = col_w[i..i + span].iter().sum();
                    if w > cur {
                        let add = (w - cur) / span as f32;
                        for cw in col_w[i..i + span].iter_mut() {
                            *cw += add;
                        }
                    }
                }
                i += span;
            }
        }

        self.y += mt.max(0.0);
        let saved = (self.margin_x, self.avail_w, self.align);
        self.margin_x += ml.max(0.0);
        self.avail_w -= ml.max(0.0);
        self.align = st.align;

        // scale columns to fit the available width; `width` (attribute px/%
        // or CSS) sizes the table box itself — columns scale up to fill it
        let avail = self.avail_w.max(1.0);
        let spec_w = self
            .doc
            .attr(node, "width")
            .and_then(|v| {
                let v = v.trim();
                if let Some(p) = v.strip_suffix('%') {
                    p.parse::<f32>().ok().map(|n| avail * n / 100.0)
                } else {
                    v.parse::<f32>().ok()
                }
            })
            .or_else(|| dim(c, "width", self.avail_w, st.size));
        // auto layout: width = min(max-content, available) unless specified;
        // grow columns toward their max-content need, then any leftover
        // proportionally; shrink proportionally when over budget
        let total: f32 = col_w.iter().sum();
        let pref: f32 = col_pref.iter().sum();
        let auto = pref.max(total).min(avail);
        let target = spec_w.unwrap_or(auto).min(avail).max(total.min(avail));
        let deficit = target - total;
        if deficit > 0.01 && total > 0.0 {
            // grow toward each column's max-content share, floored at its
            // min-content; renormalize any leftover proportionally
            let pref_sum: f32 = col_pref.iter().sum::<f32>().max(total);
            for (w, &p) in col_w.iter_mut().zip(&col_pref) {
                *w = w.max(target * p / pref_sum);
            }
            let cur: f32 = col_w.iter().sum();
            if target - cur > 0.01 && cur > 0.0 {
                for w in col_w.iter_mut() {
                    *w *= target / cur;
                }
            }
        } else if deficit < -0.01 && total > 0.0 {
            for w in col_w.iter_mut() {
                *w *= target / total;
            }
        }
        let table_w: f32 = col_w.iter().sum();
        // align="center"|"right" attribute, margin:auto, or a parent
        // text-align:center (<center> / -webkit-center behavior) centers it
        let center = self.doc.attr(node, "align") == Some("center")
            || (margin_auto(c, true) && margin_auto(c, false))
            || saved.2 == Align::Center;
        if center && table_w < avail {
            self.margin_x += (avail - table_w) / 2.0;
        } else if self.doc.attr(node, "align") == Some("right") && table_w < avail {
            self.margin_x += avail - table_w;
        }

        let shadow = parse_shadow(c, size, bd, st.color);
        let patch = if bg.is_some() || border.is_some() || shadow.is_some() {
            let idx = self.items.len();
            for _ in 0..9 {
                self.items.push(DisplayItem::FillRect {
                    x: 0.0,
                    y: 0.0,
                    w: 0.0,
                    h: 0.0,
                    color: 0,
                });
            }
            Some(BlockPatch {
                idx,
                x: self.margin_x,
                y: self.y,
                w: table_w,
                bg,
                border,
                radius: parse_radius(c, st.size),
                shadow,
            })
        } else {
            None
        };

        for row in &rows {
            // (x, cell w, cell h, bg, border, items in cell-local coords)
            let mut boxes: Vec<(
                f32,
                f32,
                f32,
                Option<Color>,
                Option<([f32; 4], Color)>,
                Vec<DisplayItem>,
                Vec<(f32, f32, f32, f32, NodeId)>,
            )> = Vec::new();
            let mut x = self.margin_x;
            let mut row_h = 0.0f32;
            let mut ci = 0usize;
            for &cell in row {
                let span = colspan(cell).min(ncols - ci.min(ncols));
                let cw: f32 = col_w[ci..(ci + span).min(ncols)].iter().sum();
                ci += span;
                let cs = self.style(cell);
                let mut cst = st.clone();
                self.apply_computed(&cs, &mut cst);
                cst.inline_bg = None; // cell bg paints the whole cell
                match self.doc.attr(cell, "align") {
                    Some("right") => cst.align = Align::Right,
                    Some("center") => cst.align = Align::Center,
                    _ => {}
                }
                let [pt, pr, pb, pl] = box_sides(&cs, "padding", self.avail_w, cst.size).map(|v| v.max(0.0));
                let cborder = border_sides(&cs, cst.size)
                    .or(attr_border)
                    .map(|(w, col)| (w, over(col, bg.unwrap_or(bd))));
                let [bw_t, bw_r, bw_b, bw_l] =
                    cborder.map(|b| b.0).unwrap_or([0.0; 4]);
                let cbg = cs
                    .get("background-color")
                    .and_then(|v| parse_color(v))
                    .or_else(|| {
                        self.doc.attr(cell, "bgcolor").and_then(|v| parse_color(v))
                    })
                    .map(|col| over(col, bg.unwrap_or(bd)));
                let inner = (cw - pl - pr - bw_l - bw_r).max(1.0);
                let (items, y_end, nhits) =
                    self.nest(cell, &cst, pl + bw_l, pt + bw_t, inner, &cs);
                let min_h = self.eff_line_h(&cst);
                let cell_h = (y_end + pb + bw_b).max(min_h);
                boxes.push((x, cw, cell_h, cbg, cborder, items, nhits));
                x += cw;
                row_h = row_h.max(cell_h);
            }
            // second pass: stretch cell bg/borders to row height, place content
            for (cx, cw, _ch, cbg, cborder, items, nhits) in boxes {
                if let Some(col) = cbg {
                    self.items.push(DisplayItem::FillRect {
                        x: cx,
                        y: self.y,
                        w: cw,
                        h: row_h,
                        color: col,
                    });
                }
                if let Some((bw2, col)) = cborder {
                    side_rects(&mut self.items, cx, self.y, cw, row_h, bw2, col);
                }
                for it in items {
                    self.items.push(offset_item(it, cx, self.y));
                }
                for h in nhits {
                    self.hits.push((h.0 + cx, h.1 + self.y, h.2, h.3, h.4));
                }
            }
            self.y += row_h;
        }

        if let Some(p) = patch {
            self.finish_patch(&p);
        }
        self.margin_x = saved.0;
        self.avail_w = saved.1;
        self.align = saved.2;
        self.y += mb.max(0.0);
        // non-row children (captions, stray content) laid out after the grid
        for &e in &extra {
            self.walk(e, &st);
        }
    }

    /// Lay out `node`'s children off-screen into a private item list.
    /// Content box: origin (x0, y0), `avail` wide. Returns (items, bottom y).
    /// `display:flex` — element children laid as inline-block items on a
    /// row with `gap` spacing.
    fn flex_children(&mut self, node: NodeId, c: &Props, st: Style) {
        // `gap` shorthand then axis longhands (Tailwind emits gap-x/gap-y)
        let base_gap = c
            .get("gap")
            .and_then(|v| parse_len(v, self.avail_w, st.size))
            .unwrap_or(0.0);
        let col_dir = c.get("flex-direction").map(String::as_str) == Some("column");
        let gap = c
            .get(if col_dir { "row-gap" } else { "column-gap" })
            .and_then(|v| parse_len(v, self.avail_w, st.size))
            .unwrap_or(base_gap);
        // column direction: children stack as blocks, vertical gap.
        // `flex-1`/`grow-*` children share the container's definite height.
        if c.get("flex-direction").map(String::as_str) == Some("column") {
            let mut st = st;
            if c.get("align-items").map(String::as_str) == Some("center") {
                st.align = Align::Center;
            }
            // container's own definite content height → grow target. pct_h is
            // this box's target when its own props set a height, else an
            // ancestor's (must not stretch to that).
            let hrel = self.pct_h.unwrap_or(self.viewport_h);
            let own_def = dim(c, "height", hrel, st.size).is_some()
                || c.get("min-height").is_some()
                || self.inset_h.is_some();
            let target = if own_def { self.pct_h } else { None };
            let start_y = self.y;
            // per-child item/hit ranges so grown slots can shift content down
            let mut segs: Vec<(usize, usize, usize, usize, f32)> = Vec::new();
            let mut first = true;
            for &ch in self.doc.children(node) {
                if !matches!(self.doc.kind(ch), NodeKind::Element { .. } | NodeKind::Text(_)) {
                    continue;
                }
                if !first && gap > 0.0 {
                    self.y += gap;
                }
                let (i0, h0) = (self.items.len(), self.hits.len());
                let grow = if matches!(self.doc.kind(ch), NodeKind::Element { .. }) {
                    let cc = self.style(ch);
                    cc.get("flex")
                        .and_then(|v| v.split_whitespace().next().and_then(|t| t.parse().ok()))
                        .or_else(|| {
                            cc.get("flex-grow").and_then(|v| v.trim().parse().ok())
                        })
                        .unwrap_or(0.0f32)
                } else {
                    0.0
                };
                self.walk(ch, &st);
                segs.push((i0, self.items.len(), h0, self.hits.len(), grow));
                first = false;
            }
            if let Some(t) = target {
                let free = t - (self.y - start_y);
                let n_grow: f32 = segs.iter().map(|s| s.4).sum();
                if free > 0.0 && n_grow > 0.0 {
                    // each grow child's slot gains `share`; its content sits
                    // centered in the slot (flex items-center labels)
                    let share = free / n_grow;
                    let mut consumed = 0.0;
                    for (i0, i1, h0, h1, g) in segs {
                        let shift = if g > 0.0 {
                            let s = consumed + share * g / 2.0;
                            consumed += share * g;
                            s
                        } else {
                            consumed
                        };
                        if shift != 0.0 {
                            for i in i0..i1 {
                                let it = std::mem::replace(
                                    &mut self.items[i],
                                    DisplayItem::FillRect {
                                        x: 0.0, y: 0.0, w: 0.0, h: 0.0, color: 0,
                                    },
                                );
                                self.items[i] = offset_item(it, 0.0, shift);
                            }
                            for h in self.hits.iter_mut().skip(h0).take(h1 - h0) {
                                h.1 += shift;
                            }
                        }
                    }
                }
                self.y = start_y + t.max(self.y - start_y);
            }
            return;
        }
        // justify-content on the main axis: pre-measure total child width.
        // Each element child's base size is its `width`, flex-basis (grow
        // items), or max-content — never min-content, which would wrap
        // `flex:1` labels to one word per line. `margin-left:auto` items share
        // the leftover space before it (pushed to the row's far edge).
        let mut base_w: FxHashMap<NodeId, f32> = FxHashMap::default();
        let mut grow_meta: FxHashMap<NodeId, (f32, f32)> = FxHashMap::default();
        let mut min_w: FxHashMap<NodeId, f32> = FxHashMap::default();
        let mut shrink_f: FxHashMap<NodeId, f32> = FxHashMap::default();
        let mut oof: Vec<NodeId> = Vec::new();
        let mut sum_grow = 0.0f32;
        let mut shrink_w = 0.0f32;
        let mut n_auto_ml = 0usize;
        let mut total = 0.0f32; // flex-item bases only
        let mut text_w = 0.0f32;
        let mut margins = 0.0f32; // positive outer margins (excluded from shrink)
        let mut n_vis = 0usize;
        for &ch in self.doc.children(node) {
            match self.doc.kind(ch) {
                NodeKind::Text(t) => {
                    if let Some(fi) = self.resolve_font(&st) {
                        if let Some(f) = self.fonts.font(fi) {
                            let tw = f.measure(
                                &t.split_whitespace().collect::<Vec<_>>().join(" "),
                                st.size,
                            );
                            text_w += tw;
                            total += tw;
                        }
                    }
                }
                NodeKind::Element { .. } => {
                    let cc = self.style(ch);
                    if cc.get("display").map(String::as_str) == Some("none") {
                        continue;
                    }
                    // out-of-flow children aren't flex items — they still
                    // paint, laid out after the row (below)
                    if matches!(
                        cc.get("position").map(String::as_str),
                        Some("absolute") | Some("fixed")
                    ) {
                        oof.push(ch);
                        continue;
                    }
                    let mut cst = st.clone();
                    self.apply_computed(&cc, &mut cst);
                    let [_t, pr, _b, pl] =
                        box_sides(&cc, "padding", self.avail_w, cst.size).map(|v| v.max(0.0));
                    let [_t, mr, _b, ml] = box_sides(&cc, "margin", self.avail_w, cst.size);
                    let (g, basis) = flex_grow_basis(&cc, self.avail_w, cst.size);
                    let w = if g > 0.0 {
                        grow_meta.insert(ch, (g, basis.unwrap_or(0.0)));
                        sum_grow += g;
                        basis.unwrap_or(0.0)
                    } else {
                        dim(&cc, "width", self.avail_w, cst.size)
                            .map(|w| {
                                if border_box(&cc) {
                                    w - pl - pr
                                } else {
                                    w
                                }
                            })
                            .unwrap_or_else(|| self.max_content_width(ch, &cst))
                    };
                    base_w.insert(ch, w + pl + pr);
                    total += w + pl + pr;
                    margins += ml.max(0.0) + mr.max(0.0);
                    // flex-shrink + min-content floor: an overflowing row
                    // shrinks items ∝ shrink×base, floored at min-width
                    // (auto → min-content, like real flexbox)
                    let floor = dim(&cc, "min-width", self.avail_w, cst.size)
                        .unwrap_or_else(|| self.shrink_width(ch, &cst) + pl + pr);
                    min_w.insert(ch, floor.max(1.0));
                    let sh = flex_shrink_factor(&cc);
                    if sh > 0.0 {
                        shrink_w += sh * (w + pl + pr);
                        shrink_f.insert(ch, sh);
                    }
                    if has_auto_ml(&cc) {
                        n_auto_ml += 1;
                    }
                    n_vis += 1;
                }
                _ => {}
            }
        }
        let gaps = gap * n_vis.saturating_sub(1) as f32;
        let cw = self.content_width();
        let overflow = total + margins + gaps - cw;
        if overflow > 0.0 && shrink_w > 0.0 {
            // negative free space → shrink bases ∝ shrink×base, floored at
            // min-width (one pass; floors can leave residual overflow)
            for (ch, sh) in &shrink_f {
                if let Some(b) = base_w.get_mut(ch) {
                    // floor never exceeds the pre-shrink base: a min-content
                    // estimate larger than the base would grow, not shrink
                    let floor = min_w.get(ch).copied().unwrap_or(1.0).min(*b);
                    *b = (*b - overflow * sh * *b / shrink_w).max(floor);
                }
            }
            total = text_w + base_w.values().sum::<f32>();
        }
        let free = (cw - total - margins - gaps).max(0.0);
        // grow items absorb all leftover space; auto margins only split what
        // grow leaves (spec: auto margins are 0 when any item grows)
        let auto_share = if sum_grow > 0.0 || n_auto_ml == 0 {
            0.0
        } else {
            free / n_auto_ml as f32
        };
        let extra = if sum_grow > 0.0 || n_auto_ml > 0 {
            0.0
        } else {
            free
        };
        let n_children = self
            .doc
            .children(node)
            .iter()
            .filter(|&&ch| matches!(self.doc.kind(ch), NodeKind::Element { .. }))
            .count();
        let mut effective_gap = gap;
        match c.get("justify-content").map(String::as_str) {
            Some("center") => self.spacer(extra / 2.0),
            Some("flex-end") | Some("right") | Some("end") => self.spacer(extra),
            Some("space-between") if n_children > 1 => {
                effective_gap = gap + extra / (n_children - 1) as f32;
            }
            _ => {}
        }
        let mut first = true;
        for &ch in self.doc.children(node) {
            match self.doc.kind(ch) {
                NodeKind::Element { .. } => {
                    let cc = self.style(ch);
                    if cc.get("display").map(String::as_str) == Some("none") {
                        continue;
                    }
                    if matches!(
                        cc.get("position").map(String::as_str),
                        Some("absolute") | Some("fixed")
                    ) {
                        continue; // in `oof` — painted after the row
                    }
                    if !first && effective_gap > 0.0 {
                        self.spacer(effective_gap);
                    }
                    if has_auto_ml(&cc) && auto_share > 0.0 {
                        self.spacer(auto_share);
                    }
                    let mut cst = st.clone();
                    self.apply_computed(&cc, &mut cst);
                    // img/svg become a word on the flex line — they share in
                    // justify-content spacing and align/justify centering
                    match self.doc.tag(ch) {
                        Some(t @ ("img" | "svg")) => {
                            if let Some((w, h, items, hits)) =
                                self.replaced_payload(ch, t, &cc, &cst)
                            {
                                let (tx, ty) = translate_xy(&cc, w, h, cst.size);
                                let items = if tx != 0.0 || ty != 0.0 {
                                    items
                                        .into_iter()
                                        .map(|it| offset_item(it, tx, ty))
                                        .collect()
                                } else {
                                    items
                                };
                                let hits = if tx != 0.0 || ty != 0.0 {
                                    hits.into_iter()
                                        .map(|h| (h.0 + tx, h.1 + ty, h.2, h.3, h.4))
                                        .collect()
                                } else {
                                    hits
                                };
                                let mb =
                                    box_sides(&cc, "margin", self.avail_w, cst.size)[2].max(0.0);
                                self.push_box(w, h, items, hits, mb);
                            }
                        }
                        Some(t @ ("input" | "select" | "textarea" | "video" | "canvas" | "iframe" | "object" | "embed" | "br" | "hr")) => {
                            self.inline(ch, t, &cc, cst)
                        }
                        _ => {
                            // flex item main size: grow → basis + share of the
                            // leftover; otherwise its measured base (explicit
                            // `width` still wins via the `dim` branch inside).
                            // `min-width:auto` floors at min-content.
                            let mut hint = if let Some(&(g, bpx)) = grow_meta.get(&ch) {
                                Some(bpx + free * g / sum_grow)
                            } else if dim(&cc, "width", self.avail_w, cst.size).is_none() {
                                base_w.get(&ch).copied()
                            } else {
                                None
                            };
                            // min-content floor — capped at the item's own base
                            // size: a floor above base inflates the item past
                            // what the free-space math budgeted → phantom wrap
                            if let (Some(h), Some(&fl)) = (hint, min_w.get(&ch)) {
                                let cap = base_w.get(&ch).copied().unwrap_or(h);
                                hint = Some(h.max(fl.min(cap)));
                            }
                            self.inline_block_w(ch, &cc, cst, hint)
                        }
                    }
                    first = false;
                }
                NodeKind::Text(t) => {
                    // whitespace between flex items collapses in real flexbox;
                    // a pending_space here would add a phantom gap that can
                    // wrap an edge-aligned item past the row's end
                    if !t.trim().is_empty() {
                        self.text(t, &st)
                    }
                }
                _ => {}
            }
        }
        // out-of-flow children paint last, as plain blocks (position
        // offsets resolve against the nearest positioned ancestor's cb)
        if !oof.is_empty() {
            self.flush_line();
            for ch in oof {
                let cc = self.style(ch);
                let mut cst = st.clone();
                self.apply_computed(&cc, &mut cst);
                self.block(ch, self.doc.tag(ch).unwrap_or("div"), &cc, cst);
            }
        }
    }

    /// `display:grid|inline-grid` children. Resolve the track templates,
    /// place items (explicit line numbers or row/column auto-flow), size
    /// tracks (fixed lengths, `fr` shares of leftover space, content-sized
    /// auto/minmax), then lay each item into its cell rect via `nest` and
    /// emit its box at the aligned offset.
    /// Subset: no named lines/areas/subgrid; non-element children skipped.
    fn grid_children(&mut self, node: NodeId, c: &Props, st: &Style) {
        struct GItem {
            id: NodeId,
            r: usize,
            c: usize,
            rs: usize,
            cs: usize,
            st: Style,
            cc: Rc<Props>,
            m: [f32; 4],
            p: [f32; 4],
            bw: [f32; 4],
            bg: Option<Color>,
            border: Option<([f32; 4], Color)>,
            radius: f32,
            shadow: Option<(f32, f32, f32, Color)>,
            bb: bool,
            w_prop: Option<f32>,
            h_prop: Option<f32>,
            min_h: Option<f32>,
            max_h: Option<f32>,
            intrinsic_w: f32,
            ratio: Option<f32>,
            just: GAlign,
            al: GAlign,
            laid: Vec<DisplayItem>,
            /// child element boxes laid relative to the item's content box —
            /// offset to (ix, iy+dy) at emit for click hit-testing
            hits: Vec<(f32, f32, f32, f32, NodeId)>,
            /// border-box height of the laid content (pre-stretch)
            box_h_intr: f32,
        }
        let avail = self.avail_w.max(1.0);
        let x0 = self.margin_x;
        let y0 = self.y;
        // definite container height (`height` prop, or `top`+`bottom` stretch
        // on a positioned grid) — drives fr rows and align-content; % resolves
        // against the viewport as approximation
        let cont_h = dim(c, "height", self.viewport_h, st.size)
            .or(self.inset_h)
            .filter(|h| *h > 0.0);

        // gaps: `gap: <row> <col>?`, then longhands override
        let (mut row_gap, mut col_gap) = (0.0f32, 0.0f32);
        if let Some(v) = c.get("gap") {
            let mut it = v.split_whitespace();
            row_gap = it
                .next()
                .and_then(|t| parse_len_vp(t, cont_h.unwrap_or(avail), st.size, self.viewport_h, avail))
                .unwrap_or(0.0);
            col_gap = it
                .next()
                .and_then(|t| parse_len_vp(t, avail, st.size, self.viewport_h, avail))
                .unwrap_or(row_gap);
        }
        if let Some(v) = c.get("row-gap") {
            row_gap = parse_len_vp(v, cont_h.unwrap_or(avail), st.size, self.viewport_h, avail)
                .unwrap_or(row_gap);
        }
        if let Some(v) = c.get("column-gap") {
            col_gap = parse_len_vp(v, avail, st.size, self.viewport_h, avail).unwrap_or(col_gap);
        }

        let row_rel = cont_h.unwrap_or(avail);
        // `grid-template` shorthand: `rows / cols`, rows may carry inline
        // area names ('a a' 1fr / 1fr) — strip quoted segments for tracks,
        // collect them as area rows when `grid-template-areas` is absent
        let (t_rows, t_cols, t_areas) = c
            .get("grid-template")
            .map(|v| {
                let (rows, cols) = v.split_once('/').unwrap_or((v.as_str(), ""));
                let mut tracks = String::new();
                let mut names: Vec<String> = Vec::new();
                let mut seg = String::new();
                let mut in_q = false;
                for ch in rows.chars() {
                    match ch {
                        '\'' | '"' => {
                            if in_q {
                                names.push(seg.trim().to_string());
                                seg.clear();
                            }
                            in_q = !in_q;
                        }
                        _ if in_q => seg.push(ch),
                        _ => tracks.push(ch),
                    }
                }
                (
                    Some(tracks.trim().to_string()).filter(|s| !s.is_empty()),
                    Some(cols.trim().to_string()).filter(|s| !s.is_empty()),
                    names,
                )
            })
            .unwrap_or_default();
        let col_src = c
            .get("grid-template-columns")
            .map(String::as_str)
            .or(t_cols.as_deref())
            .map(String::from);
        let row_src = c
            .get("grid-template-rows")
            .map(String::as_str)
            .or(t_rows.as_deref())
            .map(String::from);
        let (col_specs, col_names) =
            grid_track_list(col_src.as_ref(), avail, st.size, self.viewport_h, avail, avail, col_gap);
        let (row_specs, row_names) =
            grid_track_list(row_src.as_ref(), row_rel, st.size, self.viewport_h, avail, row_rel, row_gap);
        // `grid-template-areas: 'a b' 'c b'` → name → (row, col, row+1, col+1)
        // bounding box; `grid-area: name` items resolve to those cells.
        let mut areas: FxHashMap<String, (usize, usize, usize, usize)> = FxHashMap::default();
        let area_rows: Vec<String> = c
            .get("grid-template-areas")
            .map(|v| {
                v.split('\'')
                    .enumerate()
                    .filter(|(i, _)| i % 2 == 1)
                    .map(|(_, s)| s.to_string())
                    .collect()
            })
            .unwrap_or(t_areas);
        for (r, seg) in area_rows.iter().enumerate() {
            for (ci, name) in seg.split_whitespace().enumerate() {
                if name == "." {
                    continue;
                }
                let e = areas.entry(name.to_string()).or_insert((r, ci, r, ci));
                e.0 = e.0.min(r);
                e.1 = e.1.min(ci);
                e.2 = e.2.max(r);
                e.3 = e.3.max(ci);
            }
        }
        let (auto_cols, _) = grid_track_list(
            c.get("grid-auto-columns"),
            avail,
            st.size,
            self.viewport_h,
            avail,
            avail,
            col_gap,
        );
        let (auto_rows, _) = grid_track_list(
            c.get("grid-auto-rows"),
            row_rel,
            st.size,
            self.viewport_h,
            avail,
            row_rel,
            row_gap,
        );
        let spec_at = |specs: &[GTrack], auto: &[GTrack], i: usize| -> GTrack {
            if let Some(t) = specs.get(i) {
                return *t;
            }
            if auto.is_empty() {
                GTrack::Auto(0.0, f32::MAX)
            } else {
                auto[(i - specs.len()) % auto.len()]
            }
        };

        // container-level item alignment defaults
        let (pi_a, pi_j) = c
            .get("place-items")
            .map(|v| {
                let mut it = v.split_whitespace();
                let a = it.next().unwrap_or("normal").to_string();
                let j = it.next().map(String::from).unwrap_or_else(|| a.clone());
                (a, j)
            })
            .unwrap_or_default();
        let def_align = galign(c.get("align-items"))
            .or_else(|| galign(Some(&pi_a)))
            .unwrap_or(GAlign::Stretch);
        let def_just = galign(c.get("justify-items"))
            .or_else(|| galign(Some(&pi_j)))
            .unwrap_or(GAlign::Stretch);

        // element children only — grid blockifies them; display:none skipped
        let kids: Vec<NodeId> = self
            .doc
            .children(node)
            .iter()
            .copied()
            .filter(|&ch| matches!(self.doc.kind(ch), NodeKind::Element { .. }))
            .collect();
        let mut kids2: Vec<NodeId> = Vec::with_capacity(kids.len());
        for ch in kids {
            if self.style(ch).get("display").map(String::as_str) != Some("none") {
                kids2.push(ch);
            }
        }

        // ---- placement ----
        let exp_cols = col_specs.len();
        let exp_rows = row_specs.len();
        let mut ncols = exp_cols.max(1);
        let mut req: Vec<(NodeId, Option<usize>, usize, Option<usize>, usize)> =
            Vec::with_capacity(kids2.len());
        for &ch in &kids2 {
            let ic = self.style(ch);
            // `grid-area: <name>` → the named area's cell box; falls back to
            // line terms when the name isn't declared
            let named = ic
                .get("grid-area")
                .map(|v| v.trim())
                .filter(|v| !v.is_empty() && !v.contains('/'))
                .and_then(|v| areas.get(v));
            let (ecol, cs, erow, rs) = if let Some(&(r0, c0, r1, c1)) = named {
                (
                    Some(c0),
                    c1 - c0 + 1,
                    Some(r0),
                    r1 - r0 + 1,
                )
            } else {
                let (ecol, cs) = resolve_axis(grid_lines(&ic, "column"), exp_cols, &col_names);
                let (erow, rs) = resolve_axis(grid_lines(&ic, "row"), exp_rows, &row_names);
                (ecol, cs, erow, rs)
            };
            ncols = ncols.max(ecol.map(|cix| cix + cs).unwrap_or(cs));
            req.push((ch, ecol, cs, erow, rs));
        }

        let col_flow = c
            .get("grid-auto-flow")
            .is_some_and(|v| v.contains("column"));
        let mut occ: Vec<Vec<bool>> = Vec::new();
        let mut placed: Vec<(NodeId, usize, usize, usize, usize)> = Vec::with_capacity(req.len());
        let (mut cur_r, mut cur_c) = (0usize, 0usize);
        let row_bound = exp_rows.max(1); // wrap bound for column flow
        for &(ch, ecol, cs, erow, rs) in &req {
            let (r, cix) = match (erow, ecol) {
                (Some(r), Some(cix)) => (r, cix),
                (Some(r), None) => {
                    let mut cix = 0usize;
                    while cix + cs <= ncols && !grid_free(&occ, ncols, r, cix, rs, cs) {
                        cix += 1;
                    }
                    if cix + cs > ncols {
                        cix = 0; // no fit — overlap at the row start
                    }
                    (r, cix)
                }
                (None, Some(cix)) => {
                    let mut r = 0usize;
                    while !grid_free(&occ, ncols, r, cix, rs, cs) {
                        r += 1;
                    }
                    (r, cix)
                }
                (None, None) => {
                    if col_flow {
                        loop {
                            if cur_r + rs > row_bound {
                                cur_r = 0;
                                cur_c += 1;
                                continue;
                            }
                            if cur_c + cs > ncols {
                                ncols = cur_c + cs; // implicit columns
                                for row in occ.iter_mut() {
                                    row.resize(ncols, false);
                                }
                            }
                            if grid_free(&occ, ncols, cur_r, cur_c, rs, cs) {
                                break;
                            }
                            cur_r += 1;
                        }
                        let p = (cur_r, cur_c);
                        cur_r += rs;
                        p
                    } else {
                        loop {
                            if cur_c + cs > ncols {
                                cur_c = 0;
                                cur_r += 1;
                                continue;
                            }
                            if grid_free(&occ, ncols, cur_r, cur_c, rs, cs) {
                                break;
                            }
                            cur_c += 1;
                        }
                        let p = (cur_r, cur_c);
                        cur_c += cs;
                        p
                    }
                }
            };
            while occ.len() < r + rs {
                occ.push(vec![false; ncols]);
            }
            for row in occ.iter_mut().skip(r).take(rs) {
                for cell in row.iter_mut().skip(cix).take(cs) {
                    *cell = true;
                }
            }
            placed.push((ch, r, cix, rs, cs));
        }
        let nrows = occ.len().max(exp_rows).max(1);

        // ---- column track sizes ----
        let col_spec: Vec<GTrack> = (0..ncols).map(|i| spec_at(&col_specs, &auto_cols, i)).collect();
        let mut cw: Vec<f32> = col_spec.iter().map(track_floor).collect();
        // pre-measure each item's box metrics + intrinsic width
        let mut items: Vec<GItem> = Vec::with_capacity(placed.len());
        for &(ch, r, cix, rs, cs) in &placed {
            let cc = self.style(ch);
            let mut cst = st.clone();
            self.apply_computed(&cc, &mut cst);
            cst.inline_bg = None;
            let m = box_sides(&cc, "margin", avail, cst.size);
            let p = box_sides(&cc, "padding", avail, cst.size).map(|v| v.max(0.0));
            let bd = self.backdrop;
            let bb = border_box(&cc);
            let bg = self.bg_of(ch, &cc).map(|col| over(col, bd));
            let border = border_sides(&cc, cst.size)
                .map(|(w, col)| (w, over(col, bg.unwrap_or(bd))));
            let bw = border.map(|b| b.0).unwrap_or([0.0; 4]);
            let radius = parse_radius(&cc, cst.size);
            let shadow = parse_shadow(&cc, cst.size, bd, cst.color);
            let w_prop = dim(&cc, "width", avail, cst.size).map(|w| {
                let mut w = w;
                if let Some(mx) = cc.get("max-width").and_then(|v| parse_len_vp(v, avail, cst.size, self.viewport_h, avail)) {
                    w = w.min(mx);
                }
                if let Some(mn) = cc.get("min-width").and_then(|v| parse_len_vp(v, avail, cst.size, self.viewport_h, avail)) {
                    w = w.max(mn);
                }
                // border-box `width` → strip padding+border for content size
                if bb { w - p[1] - p[3] - bw[1] - bw[3] } else { w }
            });
            // auto tracks size to the item's max-content contribution
            // (minmax(0,X) clamps at X) — min-content would wrap every word
            let intrinsic_w = w_prop
                .unwrap_or_else(|| self.max_content_width(ch, &cst).min(avail))
                .max(0.0);
            let ps = cc.get("place-self").map(|v| {
                let mut it = v.split_whitespace();
                let a = it.next().unwrap_or("auto").to_string();
                let j = it.next().map(String::from).unwrap_or_else(|| a.clone());
                (a, j)
            });
            let al = galign(cc.get("align-self"))
                .or_else(|| ps.as_ref().and_then(|(a, _)| galign(Some(a))))
                .unwrap_or(def_align);
            let just = galign(cc.get("justify-self"))
                .or_else(|| ps.as_ref().and_then(|(_, j)| galign(Some(j))))
                .unwrap_or(def_just);
            // heights stored content-relative: border-box subtracts pad+border
            let hconv =
                |v: f32| if bb { v - p[0] - p[2] - bw[0] - bw[2] } else { v };
            let ihrel = cont_h.or(self.pct_h).unwrap_or(avail);
            let h_prop = dim(&cc, "height", ihrel, cst.size).map(hconv);
            let min_h = cc
                .get("min-height")
                .and_then(|v| parse_len_vp(v, ihrel, cst.size, self.viewport_h, avail))
                .map(hconv);
            let max_h = cc
                .get("max-height")
                .and_then(|v| parse_len_vp(v, ihrel, cst.size, self.viewport_h, avail))
                .map(hconv);
            let ratio = aspect_ratio(&cc);
            items.push(GItem {
                id: ch,
                r,
                c: cix,
                rs,
                cs,
                st: cst,
                cc,
                m,
                p,
                bw,
                bg,
                border,
                radius,
                shadow,
                bb,
                w_prop,
                h_prop,
                min_h,
                max_h,
                intrinsic_w,
                ratio,
                just,
                al,
                laid: Vec::new(),
                hits: Vec::new(),
                box_h_intr: 0.0,
            });
        }

        // auto columns take the widest single-span item's margin-box width
        for it in &items {
            let need =
                it.intrinsic_w + it.p[1] + it.p[3] + it.bw[1] + it.bw[3] + it.m[1] + it.m[3];
            if it.cs == 1 {
                if let GTrack::Auto(_, mx) = col_spec[it.c] {
                    cw[it.c] = cw[it.c].max(need.min(mx));
                }
            }
        }
        for it in &items {
            if it.cs > 1 {
                let need = it.intrinsic_w
                    + it.p[1]
                    + it.p[3]
                    + it.bw[1]
                    + it.bw[3]
                    + it.m[1]
                    + it.m[3];
                let have: f32 = cw[it.c..it.c + it.cs].iter().sum::<f32>()
                    + col_gap * (it.cs - 1) as f32;
                if need > have {
                    let add = (need - have) / it.cs as f32;
                    for w in cw.iter_mut().skip(it.c).take(it.cs) {
                        *w += add;
                    }
                }
            }
        }
        // fr columns share the leftover space
        let total_fr: f32 = col_spec
            .iter()
            .map(|t| if let GTrack::Fr(f, _) = t { *f } else { 0.0 })
            .sum();
        if total_fr > 0.0 {
            let non_fr: f32 = cw
                .iter()
                .zip(&col_spec)
                .map(|(w, t)| if matches!(t, GTrack::Fr(..)) { 0.0 } else { *w })
                .sum();
            let leftover = (avail - col_gap * ncols.saturating_sub(1) as f32 - non_fr).max(0.0);
            for (i, t) in col_spec.iter().enumerate() {
                if let GTrack::Fr(f, m) = t {
                    cw[i] = (*m).max(leftover * *f / total_fr);
                }
            }
        }
        // ---- measure items at their final content width ----
        for it in items.iter_mut() {
            let cell_w: f32 =
                cw[it.c..it.c + it.cs].iter().sum::<f32>() + col_gap * (it.cs - 1) as f32;
            let inner_w = (cell_w
                - it.m[1]
                - it.m[3]
                - it.p[1]
                - it.p[3]
                - it.bw[1]
                - it.bw[3])
                .max(1.0);
            let content_w = it
                .w_prop
                .map(|w| w.min(inner_w))
                .unwrap_or(match it.just {
                    GAlign::Stretch => inner_w,
                    _ => it.intrinsic_w.min(inner_w),
                })
                .max(1.0);
            let box_w = content_w + it.p[1] + it.p[3] + it.bw[1] + it.bw[3];
            let saved_bd = self.backdrop;
            if let Some(b) = it.bg {
                self.backdrop = b;
            }
            // %-height descendants resolve against the item's definite
            // content height (`height`/`size`, or aspect-ratio from width)
            let saved_ph = self.pct_h;
            self.pct_h = it
                .h_prop
                .or_else(|| {
                    it.ratio.map(|r| {
                        if it.bb {
                            box_w / r - it.p[0] - it.p[2] - it.bw[0] - it.bw[2]
                        } else {
                            content_w / r
                        }
                    })
                })
                .or(saved_ph);
            let (laid, bottom, nhits) = self.nest(
                it.id,
                &it.st,
                it.p[3] + it.bw[3],
                it.p[0] + it.bw[0],
                content_w,
                &it.cc,
            );
            self.backdrop = saved_bd;
            self.pct_h = saved_ph;
            it.laid = laid;
            it.hits = nhits;
            let content_h = bottom + it.p[2] + it.bw[2]; // border-box height
            let mut box_h = it
                .h_prop
                .map(|h| h + it.p[0] + it.p[2] + it.bw[0] + it.bw[2])
                .or_else(|| {
                    it.ratio.map(|r| {
                        if it.bb {
                            box_w / r
                        } else {
                            content_w / r + it.p[0] + it.p[2] + it.bw[0] + it.bw[2]
                        }
                    })
                })
                .unwrap_or(content_h);
            if let Some(mn) = it.min_h {
                box_h = box_h.max(mn + it.p[0] + it.p[2] + it.bw[0] + it.bw[2]);
            }
            if let Some(mx) = it.max_h {
                box_h = box_h.min(mx + it.p[0] + it.p[2] + it.bw[0] + it.bw[2]);
            }
            it.box_h_intr = box_h;
        }

        // ---- row track sizes ----
        let row_spec: Vec<GTrack> = (0..nrows).map(|i| spec_at(&row_specs, &auto_rows, i)).collect();
        // fr rows with an indefinite container height behave as auto
        let mut rh: Vec<f32> = row_spec
            .iter()
            .map(|t| match *t {
                GTrack::Fixed(x) => x,
                GTrack::Fr(_, m) if cont_h.is_some() => m,
                _ => track_floor(&GTrack::Auto(track_floor(t), f32::MAX)),
            })
            .collect();
        for it in &items {
            let need = it.box_h_intr + it.m[0] + it.m[2];
            if it.rs == 1 {
                let autoish = match row_spec[it.r] {
                    GTrack::Auto(..) => true,
                    GTrack::Fr(..) => cont_h.is_none(),
                    GTrack::Fixed(_) => false,
                };
                if autoish {
                    if let GTrack::Auto(_, mx) | GTrack::Fr(_, mx) = row_spec[it.r] {
                        let cap = if cont_h.is_none() { f32::MAX } else { mx };
                        rh[it.r] = rh[it.r].max(need.min(cap));
                    }
                }
            }
        }
        for it in &items {
            if it.rs > 1 {
                let need = it.box_h_intr + it.m[0] + it.m[2];
                let have: f32 = rh[it.r..it.r + it.rs].iter().sum::<f32>()
                    + row_gap * (it.rs - 1) as f32;
                if need > have {
                    let add = (need - have) / it.rs as f32;
                    for h in rh.iter_mut().skip(it.r).take(it.rs) {
                        *h += add;
                    }
                }
            }
        }
        if let Some(ch) = cont_h {
            let total_fr: f32 = row_spec
                .iter()
                .map(|t| if let GTrack::Fr(f, _) = t { *f } else { 0.0 })
                .sum();
            if total_fr > 0.0 {
                let non_fr: f32 = rh
                    .iter()
                    .zip(&row_spec)
                    .map(|(h, t)| if matches!(t, GTrack::Fr(..)) { 0.0 } else { *h })
                    .sum();
                let leftover =
                    (ch - row_gap * nrows.saturating_sub(1) as f32 - non_fr).max(0.0);
                for (i, t) in row_spec.iter().enumerate() {
                    if let GTrack::Fr(f, m) = t {
                        rh[i] = (*m).max(leftover * *f / total_fr);
                    }
                }
            }
        }

        // ---- emit ----
        let tracks_w = cw.iter().sum::<f32>() + col_gap * ncols.saturating_sub(1) as f32;
        let tracks_h = rh.iter().sum::<f32>() + row_gap * nrows.saturating_sub(1) as f32;
        let (pc_a, pc_j) = c
            .get("place-content")
            .map(|v| {
                let mut it = v.split_whitespace();
                let a = it.next().unwrap_or("start").to_string();
                let j = it.next().map(String::from).unwrap_or_else(|| a.clone());
                (a, j)
            })
            .unwrap_or_default();
        let jc = c.get("justify-content").or(if pc_j.is_empty() { None } else { Some(&pc_j) });
        let ac = c.get("align-content").or(if pc_a.is_empty() { None } else { Some(&pc_a) });
        let (egap_x, off_x) = content_offset(jc, (avail - tracks_w).max(0.0), ncols);
        let (egap_y, off_y) = content_offset(
            ac,
            (cont_h.unwrap_or(tracks_h) - tracks_h).max(0.0),
            nrows,
        );
        let egx = col_gap + egap_x;
        let egy = row_gap + egap_y;
        let mut col_x = vec![0.0f32; ncols + 1];
        for i in 0..ncols {
            col_x[i + 1] = col_x[i] + cw[i] + egx;
        }
        let mut row_y = vec![0.0f32; nrows + 1];
        for i in 0..nrows {
            row_y[i + 1] = row_y[i] + rh[i] + egy;
        }

        for it in items.iter_mut() {
            let cell_x = x0 + off_x + col_x[it.c];
            let cell_w = cw[it.c..it.c + it.cs].iter().sum::<f32>() + egx * (it.cs - 1) as f32;
            let cell_y = y0 + off_y + row_y[it.r];
            let cell_h = rh[it.r..it.r + it.rs].iter().sum::<f32>() + egy * (it.rs - 1) as f32;
            let inner_w = (cell_w
                - it.m[1]
                - it.m[3]
                - it.p[1]
                - it.p[3]
                - it.bw[1]
                - it.bw[3])
                .max(1.0);
            let content_w = it
                .w_prop
                .map(|w| w.min(inner_w).max(1.0))
                .unwrap_or(match it.just {
                    GAlign::Stretch => inner_w,
                    _ => it.intrinsic_w.min(inner_w).max(1.0),
                });
            let box_w = content_w + it.p[1] + it.p[3] + it.bw[1] + it.bw[3];
            let mut box_h = if let Some(h) = it.h_prop {
                h + it.p[0] + it.p[2] + it.bw[0] + it.bw[2]
            } else if let Some(r) = it.ratio {
                if it.bb {
                    box_w / r
                } else {
                    content_w / r + it.p[0] + it.p[2] + it.bw[0] + it.bw[2]
                }
            } else if it.al == GAlign::Stretch {
                (cell_h - it.m[0] - it.m[2]).max(1.0)
            } else {
                it.box_h_intr
            };
            if let Some(mn) = it.min_h {
                box_h = box_h.max(mn + it.p[0] + it.p[2] + it.bw[0] + it.bw[2]);
            }
            if let Some(mx) = it.max_h {
                box_h = box_h.min(mx + it.p[0] + it.p[2] + it.bw[0] + it.bw[2]);
            }
            let room_w = (cell_w - it.m[1] - it.m[3]).max(0.0);
            let ix = cell_x
                + it.m[3]
                + match it.just {
                    GAlign::End => (room_w - box_w).max(0.0),
                    GAlign::Center => ((room_w - box_w) / 2.0).max(0.0),
                    _ => 0.0,
                };
            let room_h = (cell_h - it.m[0] - it.m[2]).max(0.0);
            let iy = cell_y
                + it.m[0]
                + match it.al {
                    GAlign::End => (room_h - box_h).max(0.0),
                    GAlign::Center => ((room_h - box_h) / 2.0).max(0.0),
                    _ => 0.0,
                };
            // translate/transform shifts the painted box + its hit rects
            let (tx, ty) = translate_xy(&it.cc, box_w, box_h, it.st.size);
            let (ix, iy) = (ix + tx, iy + ty);
            if let Some((ox, oy, sp, col)) = it.shadow {
                if sp > 0.0 && ox == 0.0 && oy == 0.0 {
                    edge_rects(
                        &mut self.items,
                        ix - sp,
                        iy - sp,
                        box_w + 2.0 * sp,
                        box_h + 2.0 * sp,
                        sp,
                        col,
                    );
                } else {
                    self.items.push(DisplayItem::RoundedRect {
                        x: ix + ox,
                        y: iy + oy,
                        w: box_w,
                        h: box_h,
                        radius: it.radius,
                        color: col,
                    });
                }
            }
            if let Some(col) = it.bg {
                if it.radius > 0.5 {
                    self.items.push(DisplayItem::RoundedRect {
                        x: ix,
                        y: iy,
                        w: box_w,
                        h: box_h,
                        radius: it.radius,
                        color: col,
                    });
                } else {
                    self.items.push(DisplayItem::FillRect {
                        x: ix,
                        y: iy,
                        w: box_w,
                        h: box_h,
                        color: col,
                    });
                }
            }
            if let Some((bw2, col)) = it.border {
                side_rects(&mut self.items, ix, iy, box_w, box_h, bw2, col);
            }
            // `align-items:center` on the item centers its content inside the
            // (possibly stretched) box
            let dy = if matches!(
                it.cc.get("align-items").or_else(|| it.cc.get("place-items")).map(String::as_str),
                Some("center")
            ) {
                ((box_h - it.box_h_intr) / 2.0).max(0.0)
            } else {
                0.0
            };
            for di in it.laid.drain(..) {
                self.items.push(offset_item(di, ix, iy + dy));
            }
            self.hits.push((ix, iy, box_w, box_h, it.id));
            for h in it.hits.drain(..) {
                self.hits.push((h.0 + ix, h.1 + iy + dy, h.2, h.3, h.4));
            }
        }

        // the container consumes max(content, declared height) in flow
        self.y = y0 + tracks_h.max(cont_h.unwrap_or(0.0));
    }

    fn nest(
        &mut self,
        node: NodeId,
        st: &Style,
        x0: f32,
        y0: f32,
        avail: f32,
        c: &Props,
    ) -> (Vec<DisplayItem>, f32, Vec<(f32, f32, f32, f32, NodeId)>) {
        let saved_items = std::mem::take(&mut self.items);
        let saved_hits = std::mem::take(&mut self.hits);
        let saved_line = std::mem::take(&mut self.line);
        let saved_floats = std::mem::take(&mut self.floats);
        let saved = (self.y, self.margin_x, self.avail_w, self.align);
        let saved_lx = self.line_x;
        let saved_ps = self.pending_space;
        self.y = y0;
        self.margin_x = x0;
        self.avail_w = avail.max(1.0);
        self.align = st.align;
        self.line_x = 0.0;
        self.pending_space = false;
        // containing-block height for positioned descendants
        let saved_cb_h = self.cb_h;
        if matches!(
            c.get("position").map(String::as_str),
            Some("relative") | Some("absolute") | Some("fixed") | Some("sticky")
        ) {
            if let Some(h) = dim(c, "height", self.viewport_h, st.size) {
                self.cb_h = Some(h);
            }
        }
        // the enclosing block's inset stretch must not leak into nested
        // containers — a positioned descendant sets its own
        let saved_ih = self.inset_h;
        self.inset_h = None;
        match c.get("display").map(String::as_str) {
            Some("flex") | Some("inline-flex") => self.flex_children(node, c, st.clone()),
            Some("grid") | Some("inline-grid") => self.grid_children(node, c, st),
            _ => {
                for &ch in self.doc.children(node) {
                    self.walk(ch, st);
                }
            }
        }
        self.cb_h = saved_cb_h;
        self.inset_h = saved_ih;
        self.flush_line();
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
        (items, bottom, hits)
    }

    /// Invoke `f` on each descendant text node's raw text (node boundaries
    /// matter — `</li><li>` with no whitespace still breaks between items
    /// once a generated separator or list box splits them)
    fn each_text(doc: &Document, node: NodeId, f: &mut impl FnMut(&str)) {
        if let Some(t) = doc.text(node) {
            f(t);
        }
        for &c in doc.children(node) {
            if matches!(
                doc.tag(c),
                Some("style") | Some("script") | Some("noscript") | Some("template")
            ) {
                continue;
            }
            Self::each_text(doc, c, f);
        }
    }

    /// min-content width approximation: the longest word inside `node`
    /// measured in the current font (or a default for textless content).
    /// Explicit `width`, nested flex rows, and fixed-track grids contribute
    /// their own geometry rather than text width.
    fn shrink_width(&mut self, node: NodeId, st: &Style) -> f32 {
        let mut w = 0.0f32;
        let doc = self.doc;
        if let Some(fi) = self.resolve_font(st) {
            if let Some(f) = self.fonts.font(fi) {
                Self::each_text(doc, node, &mut |t| {
                    for word in t.split_whitespace() {
                        w = w.max(f.measure(word, st.size));
                    }
                });
            }
        }
        // layout-aware: nested containers out-measure their text
        let c = self.style(node);
        if let Some(ew) = dim_intrinsic(&c, "width", self.avail_w, st.size) {
            // border-box `width` already includes the box's own padding
            let ew = if border_box(&c) {
                let [_t, pr, _b, pl] = box_sides(&c, "padding", self.avail_w, st.size).map(|v| v.max(0.0));
                ew - pl - pr
            } else {
                ew
            };
            w = w.max(ew);
        } else {
            match c.get("display").map(String::as_str) {
                Some("flex") | Some("inline-flex") => {
                    if c.get("flex-direction").map(String::as_str) == Some("column") {
                        // column: widest child's contribution
                        for &ch in self.doc.children(node) {
                            if matches!(self.doc.kind(ch), NodeKind::Element { .. }) {
                                let mut cst = st.clone();
                                let cc = self.style(ch);
                                self.apply_computed(&cc, &mut cst);
                                w = w.max(
                                    dim_intrinsic(&cc, "width", self.avail_w, cst.size)
                                        .unwrap_or_else(|| self.shrink_width(ch, &cst)),
                                );
                            }
                        }
                    } else {
                        w = w.max(self.flex_shrink_width(node, &c, st));
                    }
                }
                Some("grid") | Some("inline-grid") => {
                    // fixed track sum beats text (repeat(8,64px) → 512)
                    let (tracks, _) = grid_track_list(
                        c.get("grid-template-columns"),
                        self.avail_w,
                        st.size,
                        self.viewport_h,
                        self.avail_w,
                        self.avail_w,
                        0.0,
                    );
                    let fixed: f32 = tracks
                        .iter()
                        .map(|t| match t {
                            GTrack::Fixed(p) => *p,
                            GTrack::Fr(_, floor) | GTrack::Auto(_, floor) => *floor,
                        })
                        .sum();
                    if fixed > 0.0 {
                        w = w.max(fixed);
                    }
                }
                _ => {}
            }
        }
        if w <= 0.0 {
            w = st.size * 4.0; // default for empty/image-ish content
        }
        w
    }

    /// max-content width approximation: the widest unbroken inline line in
    /// the subtree. Block-level children interrupt the line and contribute
    /// only their own max-content as a lower bound — concatenating all
    /// descendant text onto one line would grossly oversize menus. Explicit
    /// `width` and nested flex rows contribute their own geometry. Flex
    /// items base their size on this — shrink-to-fit min-content would wrap
    /// labels to one word per line.
    fn max_content_width(&mut self, node: NodeId, st: &Style) -> f32 {
        let c = self.style(node);
        if let Some(ew) = dim_intrinsic(&c, "width", self.avail_w, st.size) {
            return if border_box(&c) {
                let [_t, pr, _b, pl] =
                    box_sides(&c, "padding", self.avail_w, st.size).map(|v| v.max(0.0));
                (ew - pl - pr).max(1.0)
            } else {
                ew
            };
        }
        if matches!(
            c.get("display").map(String::as_str),
            Some("flex") | Some("inline-flex")
        ) {
            return self.flex_maxc_width(node, &c, st);
        }
        let (mut line, mut best) = (0.0f32, 0.0f32);
        self.maxc_inline(node, st, &mut line, &mut best);
        best.max(line).max(1.0)
    }

    /// max-content of a flex container: each child's own max-content plus
    /// gaps/padding/margins — `flex_shrink_width` is the *min*-content sum
    /// and would under-measure `a > span + icon` links.
    fn flex_maxc_width(&mut self, node: NodeId, c: &Props, st: &Style) -> f32 {
        let gap = c
            .get("column-gap")
            .and_then(|v| parse_len(v, self.avail_w, st.size))
            .or_else(|| c.get("gap").and_then(|v| parse_len(v, self.avail_w, st.size)))
            .unwrap_or(0.0);
        let mut w = 0.0;
        let mut n = 0usize;
        for &ch in self.doc.children(node) {
            if let NodeKind::Text(t) = self.doc.kind(ch) {
                if let Some(fi) = self.resolve_font(st) {
                    if let Some(f) = self.fonts.font(fi) {
                        w += f.measure(
                            &t.split_whitespace().collect::<Vec<_>>().join(" "),
                            st.size,
                        );
                    }
                }
                continue;
            }
            if !matches!(self.doc.kind(ch), NodeKind::Element { .. }) {
                continue;
            }
            let cc = self.style(ch);
            if cc.get("display").map(String::as_str) == Some("none")
                || matches!(
                    cc.get("position").map(String::as_str),
                    Some("absolute") | Some("fixed")
                )
            {
                continue; // hidden or out-of-flow: no intrinsic size
            }
            let mut cst = st.clone();
            self.apply_computed(&cc, &mut cst);
            let [_t, pr, _b, pl] = box_sides(&cc, "padding", self.avail_w, cst.size);
            let [_t, mr, _b, ml] = box_sides(&cc, "margin", self.avail_w, cst.size);
            let cw = dim_intrinsic(&cc, "width", self.avail_w, cst.size)
                .map(|w| {
                    if border_box(&cc) {
                        w - pl.max(0.0) - pr.max(0.0)
                    } else {
                        w
                    }
                })
                .unwrap_or_else(|| self.max_content_width(ch, &cst));
            w += cw + pl.max(0.0) + pr.max(0.0) + ml + mr;
            n += 1;
        }
        (w + gap * n.saturating_sub(1) as f32).max(st.size)
    }

    /// Inline-order max-content walk: text words accumulate on `line`;
    /// atomic inline boxes (replaced, inline-block) add their own width;
    /// block-level children flush the line and contribute a lower bound.
    fn maxc_inline(&mut self, node: NodeId, st: &Style, line: &mut f32, best: &mut f32) {
        for &ch in self.doc.children(node) {
            match self.doc.kind(ch) {
                NodeKind::Text(t) => {
                    if let Some(fi) = self.resolve_font(st) {
                        if let Some(f) = self.fonts.font(fi) {
                            for word in t.split_whitespace() {
                                let sp = if *line > 0.0 {
                                    f.measure(" ", st.size)
                                } else {
                                    0.0
                                };
                                *line += sp + f.measure(word, st.size);
                            }
                        }
                    }
                }
                NodeKind::Element { .. } => {
                    let cc = self.style(ch);
                    let disp = cc.get("display").map(String::as_str);
                    if disp == Some("none")
                        || matches!(
                            cc.get("position").map(String::as_str),
                            Some("absolute") | Some("fixed")
                        )
                    {
                        continue;
                    }
                    let mut cst = st.clone();
                    self.apply_computed(&cc, &mut cst);
                    let tag = self.doc.tag(ch).unwrap_or("");
                    let atomic = matches!(
                        tag,
                        "img" | "svg" | "input" | "select" | "textarea" | "video"
                            | "canvas" | "iframe" | "object" | "embed"
                    ) || matches!(
                        disp,
                        Some("inline-block") | Some("inline-flex") | Some("inline-grid")
                    );
                    if atomic {
                        *line += dim_intrinsic(&cc, "width", self.avail_w, cst.size)
                            .unwrap_or_else(|| self.max_content_width(ch, &cst));
                    } else if tag == "br" || matches!(
                        disp,
                        Some("block") | Some("list-item") | Some("flex") | Some("grid")
                            | Some("table") | Some("flow-root")
                    ) {
                        *best = (*best).max(*line);
                        *line = 0.0;
                        if tag != "br" {
                            *best = (*best).max(self.max_content_width(ch, &cst));
                        }
                    } else {
                        let [_t, mr, _b, ml] = box_sides(&cc, "margin", self.avail_w, cst.size);
                        *line += ml.max(0.0);
                        self.maxc_inline(ch, &cst, line, best);
                        *line += mr.max(0.0);
                    }
                }
                _ => {}
            }
        }
    }

    /// Intrinsic width of a flex container: children's widths + gaps + their
    /// own padding/margins — a max-content approximation.
    fn flex_shrink_width(&mut self, node: NodeId, c: &Props, st: &Style) -> f32 {
        let gap = c
            .get("column-gap")
            .and_then(|v| parse_len(v, self.avail_w, st.size))
            .or_else(|| c.get("gap").and_then(|v| parse_len(v, self.avail_w, st.size)))
            .unwrap_or(0.0);
        // flex-wrap:wrap rows can break between items — the min-content
        // contribution is the widest single item, not the whole line
        let wrap = matches!(
            c.get("flex-wrap").map(String::as_str),
            Some("wrap") | Some("wrap-reverse")
        );
        let mut w = 0.0f32;
        let mut n = 0usize;
        for &ch in self.doc.children(node) {
            if let NodeKind::Text(t) = self.doc.kind(ch) {
                if let Some(fi) = self.resolve_font(st) {
                    if let Some(f) = self.fonts.font(fi) {
                        let tw = f.measure(
                            &t.split_whitespace().collect::<Vec<_>>().join(" "),
                            st.size,
                        );
                        if wrap {
                            w = w.max(tw);
                        } else {
                            w += tw;
                        }
                    }
                }
                continue;
            }
            if !matches!(self.doc.kind(ch), NodeKind::Element { .. }) {
                continue;
            }
            let cc = self.style(ch);
            if cc.get("display").map(String::as_str) == Some("none")
                || matches!(
                    cc.get("position").map(String::as_str),
                    Some("absolute") | Some("fixed")
                )
            {
                continue;
            }
            let mut cst = st.clone();
            self.apply_computed(&cc, &mut cst);
            let [_t, pr, _b, pl] = box_sides(&cc, "padding", self.avail_w, cst.size);
            let [_t, mr, _b, ml] = box_sides(&cc, "margin", self.avail_w, cst.size);
            // an explicit `width` contributes itself, not the content's
            // min-content estimate (a 512px board inside a flex row is 512
            // no matter how small its text is). border-box widths already
            // include the padding.
            let cw = dim_intrinsic(&cc, "width", self.avail_w, cst.size)
                .map(|w| {
                    if border_box(&cc) {
                        w - pl.max(0.0) - pr.max(0.0)
                    } else {
                        w
                    }
                })
                .unwrap_or_else(|| self.shrink_width(ch, &cst));
            let item = cw + pl.max(0.0) + pr.max(0.0) + ml + mr;
            if wrap {
                w = w.max(item);
            } else {
                w += item;
            }
            n += 1;
        }
        let gaps = if wrap { 0.0 } else { gap * n.saturating_sub(1) as f32 };
        (w + gaps).max(st.size)
    }

    /// Effective line height for a style: line-height override else font metric.
    fn eff_line_h(&mut self, st: &Style) -> f32 {
        match st.line_h {
            Some(LineH::Mul(m)) => st.size * m,
            Some(LineH::Px(p)) => p,
            None => self
                .resolve_font(st)
                .and_then(|fi| self.fonts.font(fi).map(|f| f.line_height(st.size)))
                .unwrap_or(st.size * 1.2),
        }
    }

    /// Empty word used for inline margins/spacing — contributes width only.
    fn spacer(&mut self, w: f32) {
        if w <= 0.0 {
            return;
        }
        self.line.push(Word {
            text: String::new(),
            font: usize::MAX,
            size: 0.0,
            color: 0,
            underline: false,
            strike: false,
            shift: 0.0,
            width: w,
            ascent: 0.0,
            line_h: 0.0,
            href: None,
            bg: None,
            items: None,
            hits: Vec::new(),
            box_h: 0.0,
        });
        self.line_x += w;
    }

    /// Push an inline-block box (pre-rendered items) onto the line like a word.
    fn push_box(
        &mut self,
        w: f32,
        h: f32,
        items: Vec<DisplayItem>,
        hits: Vec<(f32, f32, f32, f32, NodeId)>,
        mb: f32,
    ) {
        if self.line_x + w > self.content_width() + 0.5 && !self.line.is_empty() {
            self.flush_line();
        }
        self.line.push(Word {
            text: String::new(),
            font: usize::MAX,
            size: 0.0,
            color: 0,
            underline: false,
            strike: false,
            shift: 0.0,
            width: w,
            ascent: h, // box bottom sits on the baseline
            line_h: h + mb,
            href: None,
            bg: None,
            items: Some(items),
            hits,
            box_h: h,
        });
        self.line_x += w;
    }

    fn finish_patch(&mut self, p: &BlockPatch) {
        let h = self.y - p.y;
        let mut put = |i: usize, it: DisplayItem| self.items[p.idx + i] = it;
        // paint order = slot order: shadow (0-3) behind bg (4) behind
        // border edges (5-8)
        if let Some((ox, oy, sp, col)) = p.shadow {
            if sp > 0.0 && ox == 0.0 && oy == 0.0 {
                // ring (Tailwind `ring-*`): outline just outside the box
                let mut tmp = Vec::with_capacity(4);
                edge_rects(&mut tmp, p.x - sp, p.y - sp, p.w + 2.0 * sp, h + 2.0 * sp, sp, col);
                for (i, it) in tmp.into_iter().enumerate() {
                    put(i, it);
                }
            } else {
                put(0, DisplayItem::RoundedRect {
                    x: p.x + ox,
                    y: p.y + oy,
                    w: p.w,
                    h,
                    radius: p.radius,
                    color: col,
                });
            }
        }
        if let Some(bg) = p.bg {
            if p.radius > 0.5 {
                put(4, DisplayItem::RoundedRect { x: p.x, y: p.y, w: p.w, h, radius: p.radius, color: bg });
            } else {
                put(4, DisplayItem::FillRect { x: p.x, y: p.y, w: p.w, h, color: bg });
            }
        }
        if let Some((bw, col)) = p.border {
            let mut tmp = Vec::with_capacity(4);
            side_rects(&mut tmp, p.x, p.y, p.w, h, bw, col);
            for (i, it) in tmp.into_iter().enumerate() {
                put(5 + i, it);
            }
        }
    }

    /// Replaced-element payload: box size + items in box-local coords +
    /// hit rects. Shared by the block-flow path (own row) and the flex-item
    /// path (a word on the line, so justify/align still apply).
    fn replaced_payload(
        &mut self,
        node: NodeId,
        tag: &str,
        c: &Props,
        st: &Style,
    ) -> Option<(f32, f32, Vec<DisplayItem>, Vec<(f32, f32, f32, f32, NodeId)>)> {
        // % heights resolve against the definite containing height
        let hrel = self.pct_h.unwrap_or(self.avail_w);
        match tag {
            "svg" => {
                let w = dim(c, "width", self.avail_w, st.size)
                    .or_else(|| dim(c, "height", hrel, st.size))
                    .unwrap_or(24.0);
                let h = dim(c, "height", hrel, st.size).unwrap_or(w);
                let color = st.color;
                let pixels =
                    svg::render(self.doc, node, w.max(1.0) as u32, h.max(1.0) as u32, color);
                Some((
                    w,
                    h,
                    vec![DisplayItem::Raster { x: 0.0, y: 0.0, w, h, pixels }],
                    vec![(0.0, 0.0, w, h, node)],
                ))
            }
            "img" => {
                // CSS width/height beat the attributes (inline style and
                // utility classes size almost every modern <img>)
                let w = dim(c, "width", self.avail_w, st.size)
                    .or_else(|| self.doc.attr(node, "width")
                        .and_then(|v| v.trim_end_matches("px").parse().ok()))
                    .or_else(|| {
                        // only height given + aspect-ratio/intrinsic guess
                        dim(c, "height", hrel, st.size).filter(|_| aspect_ratio(c).is_some())
                            .map(|h| h * aspect_ratio(c).unwrap())
                    })
                    .unwrap_or(64.0f32);
                let h = dim(c, "height", hrel, st.size)
                    .or_else(|| self.doc.attr(node, "height")
                        .and_then(|v| v.trim_end_matches("px").parse().ok()))
                    .or_else(|| aspect_ratio(c).map(|r| w / r))
                    .unwrap_or(w.min(48.0).max(16.0));
                let items = self
                    .doc
                    .attr(node, "src")
                    .map(|src| {
                        vec![DisplayItem::Image {
                            x: 0.0,
                            y: 0.0,
                            w,
                            h,
                            src: src.to_string(),
                        }]
                    })
                    .unwrap_or_default();
                Some((w, h, items, vec![(0.0, 0.0, w, h, node)]))
            }
            _ => None,
        }
    }

    fn inline(&mut self, node: NodeId, tag: &str, c: &Props, mut st: Style) {
        if tag == "a" {
            st.href = self.doc.attr(node, "href").map(String::from);
        }
        // inline margins become spacing around the element's runs
        let [_t, mr, _b, ml] = box_sides(c, "margin", self.avail_w, st.size);
        if ml > 0.0 {
            self.spacer(ml);
        }
        match tag {
            "br" => self.flush_line(),
            "svg" | "img" => {
                self.flush_line();
                if let Some((w, h, items, hits)) = self.replaced_payload(node, tag, c, &st) {
                    let (tx, ty) = translate_xy(c, w, h, st.size);
                    let x = self.line_origin();
                    let y = self.y;
                    for it in items {
                        self.items.push(offset_item(it, x + tx, y + ty));
                    }
                    for hd in hits {
                        self.hits
                            .push((hd.0 + x + tx, hd.1 + y + ty, hd.2, hd.3, hd.4));
                    }
                    let lh = self.eff_line_h(&st);
                    self.y += h.max(lh);
                }
            }
            "input" | "textarea" | "select" => {
                // Childless controls: show their value/placeholder, else a
                // generic control label.
                let label = self
                    .doc
                    .attr(node, "value")
                    .or_else(|| self.doc.attr(node, "placeholder"))
                    .unwrap_or("[control]");
                self.word(label, &st);
            }
            _ => {
                for &ch in self.doc.children(node) {
                    self.walk(ch, &st);
                }
            }
        }
        if mr > 0.0 {
            self.spacer(mr);
        }
    }

    fn text(&mut self, text: &str, st: &Style) {
        if st.pre {
            for (i, seg) in text.split('\n').enumerate() {
                if i > 0 {
                    self.flush_line();
                }
                for word in seg.split(' ') {
                    if word.is_empty() {
                        self.pending_space = true;
                        continue;
                    }
                    self.word_with_space(word, st);
                    self.pending_space = true;
                }
                self.pending_space = false;
            }
            return;
        }
        if text.chars().next().is_some_and(|c| c.is_whitespace()) {
            self.pending_space = true;
        }
        let mut first = true;
        for word in text.split_whitespace() {
            if !first {
                self.pending_space = true;
            }
            self.word_with_space(word, st);
            first = false;
        }
        self.pending_space = text.chars().last().is_some_and(|c| c.is_whitespace());
    }

    fn resolve_font(&mut self, st: &Style) -> Option<usize> {
        // FontCache::resolve scans every installed face; memoize per
        // (family, bold, italic). Slots encode: u32::MAX = not computed,
        // u32::MAX-1 = resolved-to-none, else font index.
        let slot = (st.bold as usize) * 2 + st.italic as usize;
        if let Some(slots) = self.font_memo.get(st.family.as_str()) {
            let v = slots[slot];
            if v != u32::MAX {
                return if v == u32::MAX - 1 { None } else { Some(v as usize) };
            }
        }
        let r = self
            .fonts
            .resolve(&st.family, st.bold, st.italic)
            .or_else(|| self.fonts.resolve("liberation sans", false, false));
        self.font_memo
            .entry(st.family.clone())
            .or_insert([u32::MAX; 4])[slot] = r.map(|i| i as u32).unwrap_or(u32::MAX - 1);
        r
    }

    /// Like resolve_font, but for `word` containing chars the primary font
    /// lacks — fall back through symbol-capable families.
    fn resolve_font_covering(&mut self, word: &str, st: &Style) -> Option<usize> {
        let covers = |fonts: &mut FontCache, idx: usize, w: &str| {
            fonts
                .font(idx)
                .is_some_and(|f| w.chars().all(|c| f.glyph_id(c) != 0))
        };
        let primary = self.resolve_font(st)?;
        if covers(&mut self.fonts, primary, word) {
            return Some(primary);
        }
        const ALT: &[&str] = &[
            "noto sans symbols2", "noto sans symbols", "noto sans math",
            "symbola", "dejavu sans", "noto sans",
        ];
        for fam in ALT {
            if let Some(idx) = self.fonts.resolve(fam, st.bold, st.italic) {
                if covers(&mut self.fonts, idx, word) {
                    return Some(idx);
                }
            }
        }
        Some(primary)
    }

    fn word_with_space(&mut self, word: &str, st: &Style) {
        let Some(font) = self.resolve_font_covering(word, st) else { return };
        let space_w = self
            .fonts
            .font(font)
            .map(|f| f.advance_units(f.glyph_id(' ')) as f32 * st.size / f.units_per_em() as f32)
            .unwrap_or(st.size * 0.3);
        if self.pending_space && !self.line.is_empty() {
            self.line_x += space_w;
            if let Some(last) = self.line.last_mut() {
                last.width += space_w;
            }
        }
        self.pending_space = false;
        self.push_word(word, st, font);
    }

    fn word(&mut self, word: &str, st: &Style) {
        if let Some(font) = self.resolve_font(st) {
            self.push_word(word, st, font);
        }
    }

    fn push_word(&mut self, word: &str, st: &Style, font: usize) {
        let owned: String;
        let word = match st.transform {
            1 => { owned = word.to_uppercase(); &owned }
            2 => { owned = word.to_lowercase(); &owned }
            3 => { owned = word.chars().enumerate().map(|(i, c)| if i == 0 { c.to_uppercase().next().unwrap_or(c) } else { c }).collect(); &owned }
            _ => word,
        };
        let (w, ascent, line_h) = {
            let Some(f) = self.fonts.font(font) else { return };
            let upem = f.units_per_em() as f32;
            (
                f.measure(word, st.size),
                f.ascender as f32 * st.size / upem,
                match st.line_h {
                    Some(LineH::Mul(m)) => st.size * m,
                    Some(LineH::Px(p)) => p,
                    None => f.line_height(st.size),
                },
            )
        };
        if !st.nowrap
            && self.line_x + w > self.content_width() + 0.5
            && !self.line.is_empty()
        {
            self.flush_line();
        }
        if st.break_word
            && !st.nowrap
            && w > self.content_width() + 0.5
            && self.line.is_empty()
        {
            // word longer than the line — split mid-word (overflow-wrap etc.)
            let (mut chunk, mut cw) = (String::new(), 0.0f32);
            for c in word.chars() {
                let fw = self
                    .fonts
                    .font(font)
                    .map(|f| {
                        f.advance_units(f.glyph_id(c)) as f32 * st.size / f.units_per_em() as f32
                    })
                    .unwrap_or(st.size * 0.5);
                if cw + fw > self.content_width() + 0.5 && !chunk.is_empty() {
                    self.line.push(Word {
                        text: chunk.clone(),
                        font,
                        size: st.size,
                        color: st.color,
                        underline: st.underline,
                        strike: st.strike,
                        shift: st.shift,
                        href: st.href.clone(),
                        width: cw,
                        ascent,
                        line_h,
                        bg: st.inline_bg,
                        items: None,
            hits: Vec::new(),
                        box_h: 0.0,
                    });
                    self.flush_line();
                    chunk.clear();
                    cw = 0.0;
                }
                chunk.push(c);
                cw += fw;
            }
            if !chunk.is_empty() {
                self.line.push(Word {
                    text: chunk,
                    font,
                    size: st.size,
                    color: st.color,
                    underline: st.underline,
                    strike: st.strike,
                    shift: st.shift,
                    href: st.href.clone(),
                    width: cw,
                    ascent,
                    line_h,
                    bg: st.inline_bg,
                    items: None,
            hits: Vec::new(),
                    box_h: 0.0,
                });
                self.line_x += cw;
            }
            return;
        }
        self.line.push(Word {
            text: word.to_string(),
            font,
            size: st.size,
            color: st.color,
            underline: st.underline,
            strike: st.strike,
            shift: st.shift,
            href: st.href.clone(),
            width: w,
            ascent,
            line_h,
            bg: st.inline_bg,
            items: None,
            hits: Vec::new(),
            box_h: 0.0,
        });
        self.line_x += w;
    }

    fn flush_line(&mut self) {
        if self.line.is_empty() {
            self.pending_space = false;
            return;
        }
        let line_h = self
            .line
            .iter()
            .map(|w| w.line_h)
            .fold(0.0f32, f32::max)
            .max(self
                .line
                .iter()
                .map(|w| w.ascent + w.size * 0.25)
                .fold(0.0, f32::max));
        let max_ascent = self.line.iter().map(|w| w.ascent).fold(0.0f32, f32::max);
        let baseline = self.y + max_ascent;
        let used = self.line.iter().map(|w| w.width).sum::<f32>();
        let mut x = self.line_origin()
            + match self.align {
                Align::Left => 0.0,
                Align::Center => ((self.content_width() - used) / 2.0).max(0.0),
                Align::Right => (self.content_width() - used).max(0.0),
            };
        for w in self.line.drain(..) {
            if let Some(items) = w.items {
                // inline-block: box bottom sits on the baseline
                let dy = baseline - w.box_h;
                for it in items {
                    self.items.push(offset_item(it, x, dy));
                }
                for h in w.hits {
                    self.hits.push((h.0 + x, h.1 + dy, h.2, h.3, h.4));
                }
            } else if !w.text.is_empty() {
                if let Some(bg) = w.bg {
                    // inline background: rect behind the word's extent
                    self.items.push(DisplayItem::FillRect {
                        x,
                        y: baseline - w.ascent,
                        w: w.width,
                        h: w.ascent + w.size * 0.3,
                        color: bg,
                    });
                }
                self.items.push(DisplayItem::Text {
                    x,
                    baseline: baseline - w.shift * w.size,
                    text: w.text,
                    font: w.font,
                    size: w.size,
                    color: w.color,
                    underline: w.underline,
                    strike: w.strike,
                    href: w.href,
                });
            }
            // empty words (inline-margin spacers) just advance x
            x += w.width;
        }
        self.y += line_h;
        self.line_x = 0.0;
        self.pending_space = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(html: &str) -> Document {
        rowser_html::parse(html)
    }

    fn lay(html: &str) -> (Vec<DisplayItem>, f32, Vec<(String, f32)>) {
        let d = doc(html);
        let mut fonts = FontCache::new();
        let (items, h, anchors, _) = Layouter::layout(&d, &mut fonts, 800.0, 0.0, &[]);
        (items, h, anchors)
    }

    fn texts(items: &[DisplayItem]) -> Vec<&DisplayItem> {
        items
            .iter()
            .filter(|i| matches!(i, DisplayItem::Text { .. }))
            .collect()
    }

    #[test]
    fn css_color_applies() {
        let (items, _, _) = lay("<style>p{color:red}</style><p>hi</p>");
        let t = texts(&items);
        if let DisplayItem::Text { color, .. } = t[0] {
            assert_eq!(*color, bgra(255, 0, 0));
        }
    }

    #[test]
    fn class_selector_styles() {
        let (items, _, _) = lay(
            "<style>.big{font-size:40px}</style><p class=big>huge</p><p>small</p>",
        );
        let t = texts(&items);
        if let DisplayItem::Text { size, .. } = t[0] {
            assert!(*size > 30.0);
        }
        if let DisplayItem::Text { size, .. } = t[1] {
            assert!(*size < 20.0);
        }
    }

    #[test]
    fn display_none_hides() {
        let (items, _, _) = lay("<style>.hide{display:none}</style><p class=hide>gone</p><p>here</p>");
        let all: String = texts(&items)
            .iter()
            .filter_map(|i| match i {
                DisplayItem::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(!all.contains("gone"));
        assert!(all.contains("here"));
    }

    #[test]
    fn background_rect_emitted() {
        let (items, _, _) = lay("<style>div{background-color:#ff0000}</style><div>inside</div>");
        assert!(items
            .iter()
            .any(|i| matches!(i, DisplayItem::FillRect { color, .. } if *color == bgra(255,0,0))));
    }

    #[test]
    fn text_align_center() {
        let (items, _, _) =
            lay("<style>p{text-align:center}</style><p>mid</p>");
        let t = texts(&items);
        if let DisplayItem::Text { x, .. } = t[0] {
            assert!(*x > 200.0, "centered text should be right of center, x={x}");
        }
    }

    #[test]
    fn headings_bigger() {
        let (items, _, _) = lay("<h1>big</h1><p>small</p>");
        let t = texts(&items);
        if let DisplayItem::Text { size, .. } = t[0] {
            assert!(*size > 30.0);
        }
    }

    fn text_pos(items: &[DisplayItem], needle: &str) -> (f32, f32) {
        for i in items {
            if let DisplayItem::Text { x, baseline, text, .. } = i {
                if text.as_str() == needle {
                    return (*x, *baseline);
                }
            }
        }
        panic!("text {needle:?} not found");
    }

    #[test]
    fn inline_block_advances_line() {
        let (items, _, _) = lay(
            "<style>.ib{display:inline-block;width:100px;height:12px}</style>\
             <p>a<span class=ib></span>b</p>",
        );
        let (xa, _) = text_pos(&items, "a");
        let (xb, _) = text_pos(&items, "b");
        assert!(xb - xa > 100.0, "b should sit past the 100px box: {xa} -> {xb}");
    }

    #[test]
    fn inline_block_bg_and_border() {
        let (items, _, _) = lay(
            "<style>.ib{display:inline-block;width:50px;height:20px;\
             background-color:#ff0000;border:2px solid blue}</style>\
             <p><span class=ib></span></p>",
        );
        assert!(items
            .iter()
            .any(|i| matches!(i, DisplayItem::FillRect { w, h, color, .. }
                if *color == bgra(255, 0, 0) && *w >= 50.0 && *h >= 20.0)));
        assert!(items
            .iter()
            .any(|i| matches!(i, DisplayItem::FillRect { color, .. }
                if *color == bgra(0, 0, 255))));
    }

    #[test]
    fn table_places_cells_in_columns() {
        let (items, _, _) = lay(
            "<table><tr><td>a1</td><td>bbb1</td></tr>\
             <tr><td>a2</td><td>b2</td></tr></table>",
        );
        let (xa1, ya1) = text_pos(&items, "a1");
        let (xb1, yb1) = text_pos(&items, "bbb1");
        let (xa2, ya2) = text_pos(&items, "a2");
        let (xb2, _yb2) = text_pos(&items, "b2");
        // same row → same baseline; second row below; columns aligned
        assert!((ya1 - yb1).abs() < 0.01, "same-row cells share baseline");
        assert!(ya2 > ya1, "second row below first");
        assert!(xb1 > xa1, "second column right of first");
        assert!((xa2 - xa1).abs() < 0.01, "column 0 aligned across rows");
        assert!((xb2 - xb1).abs() < 0.01, "column 1 aligned across rows");
    }

    #[test]
    fn td_border_draws() {
        let (items, _, _) = lay(
            "<style>td{border:2px solid #ff0000}</style>\
             <table><tr><td>x</td></tr></table>",
        );
        let reds = items
            .iter()
            .filter(|i| matches!(i, DisplayItem::FillRect { color, .. } if *color == bgra(255, 0, 0)))
            .count();
        assert_eq!(reds, 4, "four border edges around the cell");
    }

    #[test]
    fn float_narrows_following_lines() {
        let (items, _, _) = lay(
            "<style>.fl{float:left;width:200px}</style>\
             <div class=fl>nav</div><p>hello world</p>",
        );
        let (xnav, _) = text_pos(&items, "nav");
        let (xhello, _) = text_pos(&items, "hello");
        assert!(xnav < 30.0, "float content at left edge, x={xnav}");
        assert!(xhello >= xnav + 190.0, "text flows beside the float: {xhello}");
    }

    #[test]
    fn padding_expands_background_rect() {
        let (items, _, _) = lay(
            "<style>div{background-color:#ff0000}.p{padding:20px}</style>\
             <div>a</div><div class=p>b</div>",
        );
        let hs: Vec<f32> = items
            .iter()
            .filter_map(|i| match i {
                DisplayItem::FillRect { h, color, .. } if *color == bgra(255, 0, 0) => Some(*h),
                _ => None,
            })
            .collect();
        assert_eq!(hs.len(), 2);
        assert!(hs[1] - hs[0] >= 30.0, "padding grows bg: {:?} vs {:?}", hs[0], hs[1]);
    }

    #[test]
    fn inline_margin_and_background() {
        let (items, _, _) = lay(
            "<style>span{margin-left:50px;background-color:#00ff00}</style>\
             <p>a<span>b</span></p>",
        );
        let (xa, _) = text_pos(&items, "a");
        let (xb, _) = text_pos(&items, "b");
        assert!(xb - xa >= 50.0, "margin-left shifts the run: {xa} -> {xb}");
        // a word-sized green rect sits behind "b"
        assert!(items.iter().any(|i| matches!(
            i,
            DisplayItem::FillRect { x, w, color, .. }
                if *color == bgra(0, 255, 0) && *w < 100.0 && (*x - xb).abs() < 1.0
        )));
    }

    #[test]
    fn line_height_grows_line_box() {
        let (items, _, _) =
            lay("<style>.lh{line-height:3}</style><p class=lh>a</p><p>b</p>");
        let (_, ya) = text_pos(&items, "a");
        let (_, yb) = text_pos(&items, "b");
        assert!(yb - ya > 40.0, "line-height 3x16 separates baselines: {yb} - {ya}");
    }

    // ---------- grid ----------

    #[test]
    fn grid_fixed_columns() {
        let (items, _, _) = lay(
            "<style>.g{display:grid;grid-template-columns:100px 100px}</style>\
             <div class=g><div>a1</div><div>b1</div><div>a2</div><div>b2</div></div>",
        );
        let (xa1, ya1) = text_pos(&items, "a1");
        let (xb1, _) = text_pos(&items, "b1");
        let (xa2, ya2) = text_pos(&items, "a2");
        let (xb2, _) = text_pos(&items, "b2");
        assert!((xb1 - xa1 - 100.0).abs() < 1.0, "col step 100: {xa1} -> {xb1}");
        assert!(ya2 > ya1, "second row below first");
        assert!((xa2 - xa1).abs() < 1.0, "col 0 aligned across rows");
        assert!((xb2 - xb1).abs() < 1.0, "col 1 aligned across rows");
    }

    #[test]
    fn grid_repeat_fr_distribution() {
        // avail 784 (800-16) minus 20 gap → two equal fr columns
        let (items, _, _) = lay(
            "<style>.g{display:grid;grid-template-columns:repeat(2,1fr);gap:20px}</style>\
             <div class=g><div>a</div><div>b</div></div>",
        );
        let (xa, _) = text_pos(&items, "a");
        let (xb, _) = text_pos(&items, "b");
        let fr = (784.0 - 20.0) / 2.0;
        assert!((xb - xa - fr - 20.0).abs() < 2.0, "fr column step: {xa} -> {xb}");
    }

    #[test]
    fn grid_minmax() {
        // minmax(60px,1fr) + fixed 100px → fr column eats the rest (684)
        let (items, _, _) = lay(
            "<style>.g{display:grid;grid-template-columns:minmax(60px,1fr) 100px}</style>\
             <div class=g><div>a</div><div>b</div></div>",
        );
        let (_xa, _) = text_pos(&items, "a");
        let (xb, _) = text_pos(&items, "b");
        assert!((xb - 8.0 - 684.0).abs() < 2.0, "minmax fr grows: b at {xb}");
    }

    #[test]
    fn grid_gaps_and_spans() {
        let (items, _, _) = lay(
            "<style>.g{display:grid;grid-template-columns:100px 100px;gap:10px}\
             .wide{grid-column:span 2}</style>\
             <div class=g><div class=wide>a</div><div>b</div><div>c</div></div>",
        );
        let (xa, ya) = text_pos(&items, "a");
        let (xb, yb) = text_pos(&items, "b");
        let (xc, yc) = text_pos(&items, "c");
        // wide spans row0; b and c land on row1 in cols 0 and 1
        assert!((xb - xa).abs() < 1.0, "b under a's start");
        assert!((xc - xb - 110.0).abs() < 1.0, "col step incl gap: {xb} -> {xc}");
        assert!((yb - ya).abs() > 10.0, "row below");
        assert!((yc - yb).abs() < 1.0, "same row");
    }

    #[test]
    fn grid_implicit_rows_auto() {
        // no template rows → implicit rows size to content
        let (items, _, _) = lay(
            "<style>.g{display:grid;grid-template-columns:50px 50px}</style>\
             <div class=g><div>a</div><div>b</div><div>c</div></div>",
        );
        let (xa, ya) = text_pos(&items, "a");
        let (xc, yc) = text_pos(&items, "c");
        assert!(yc > ya, "implicit second row below");
        assert!((xc - xa).abs() < 1.0, "wraps to column 0");
    }

    #[test]
    fn grid_aspect_ratio_cell() {
        // aspect-ratio:1 cells in 64px tracks → square boxes
        let (items, _, _) = lay(
            "<style>.g{display:grid;grid-template-columns:repeat(2,64px)}\
             .sq{aspect-ratio:1;background:#ff0000}</style>\
             <div class=g><div class=sq></div><div class=sq></div></div>",
        );
        let sqs: Vec<(f32, f32, f32, f32)> = items
            .iter()
            .filter_map(|i| match i {
                DisplayItem::FillRect { x, y, w, h, color } if *color == bgra(255, 0, 0) => {
                    Some((*x, *y, *w, *h))
                }
                _ => None,
            })
            .collect();
        assert_eq!(sqs.len(), 2);
        for (_, _, w, h) in &sqs {
            assert!((w - 64.0).abs() < 1.0 && (h - 64.0).abs() < 1.0, "square cell {w}x{h}");
        }
    }

    #[test]
    fn grid_item_alignment() {
        // place-items:center centers a fixed-size item in its cell
        let (items, _, _) = lay(
            "<style>.g{display:grid;grid-template-columns:200px;grid-template-rows:100px;\
             place-items:center}.i{width:50px;height:20px;background:#ff0000}</style>\
             <div class=g><div class=i></div></div>",
        );
        let r = items
            .iter()
            .find(|i| matches!(i, DisplayItem::FillRect { color, .. } if *color == bgra(255, 0, 0)));
        let Some(DisplayItem::FillRect { x, y, w, h, .. }) = r else {
            panic!("cell bg not found")
        };
        assert!((*x - 8.0 - 75.0).abs() < 1.0, "centered x {x}");
        assert!((*y - 8.0 - 40.0).abs() < 1.0, "centered y {y}");
        assert!((*w - 50.0).abs() < 1.0 && (*h - 20.0).abs() < 1.0);
    }

    #[test]
    fn grid_explicit_placement() {
        let (items, _, _) = lay(
            "<style>.g{display:grid;grid-template-columns:100px 100px}\
             .x{grid-column:2;grid-row:1}</style>\
             <div class=g><div>a</div><div class=x>b</div><div>c</div></div>",
        );
        let (xa, ya) = text_pos(&items, "a");
        let (xb, yb) = text_pos(&items, "b");
        let (_xc, yc) = text_pos(&items, "c");
        assert!((xb - xa - 100.0).abs() < 1.0, "b pinned to col 2: {xa} -> {xb}");
        assert!((yb - ya).abs() < 1.0, "b on row 1");
        assert!(yc > ya, "c auto-places on the next row");
    }

    #[test]
    fn grid_nested_grid() {
        let (items, _, _) = lay(
            "<style>.g{display:grid;grid-template-columns:200px}.in{display:grid;\
             grid-template-columns:50px 50px}</style>\
             <div class=g><div class=in><div>a</div><div>b</div></div></div>",
        );
        let (xa, _) = text_pos(&items, "a");
        let (xb, _) = text_pos(&items, "b");
        assert!((xb - xa - 50.0).abs() < 1.0, "nested grid columns: {xa} -> {xb}");
    }

    #[test]
    fn grid_board_8x8_inline_style() {
        // the goonchess case: inline-style grid, 64 square cells
        let mut cells = String::new();
        for _ in 0..64 {
            cells.push_str("<button style=\"aspect-ratio:1;background:#123456\"></button>");
        }
        let html = format!(
            "<div class=board-grid-shadow style=\"display:grid;\
             grid-template-columns:repeat(8,64px);grid-template-rows:repeat(8,64px)\">{cells}</div>"
        );
        let (items, _, _) = lay(&html);
        let mut rects: Vec<(f32, f32, f32, f32)> = items
            .iter()
            .filter_map(|i| match i {
                DisplayItem::FillRect { x, y, w, h, color } if *color == bgra(0x12, 0x34, 0x56) => {
                    Some((*x, *y, *w, *h))
                }
                _ => None,
            })
            .collect();
        assert_eq!(rects.len(), 64, "64 board cells");
        rects.sort_by(|a, b| (a.1, a.0).partial_cmp(&(b.1, b.0)).unwrap());
        for (i, r) in rects.iter().enumerate() {
            let (ex, ey) = (8.0 + (i % 8) as f32 * 64.0, 8.0 + (i / 8) as f32 * 64.0);
            assert!(
                (r.0 - ex).abs() < 1.0 && (r.1 - ey).abs() < 1.0,
                "cell {i} at ({},{}) want ({ex},{ey})",
                r.0,
                r.1
            );
            assert!((r.2 - 64.0).abs() < 1.0 && (r.3 - 64.0).abs() < 1.0, "square cell");
        }
    }

    #[test]
    fn width_caps_block_content() {
        let (items, _, _) = lay(
            "<style>.w{width:150px}</style>\
             <div class=w>aa bb cc dd ee ff gg hh ii jj kk ll</div>",
        );
        let t = texts(&items);
        assert!(t.len() >= 4, "words wrap inside the narrow block");
        let mut baselines: Vec<f32> = t
            .iter()
            .filter_map(|i| match i {
                DisplayItem::Text { baseline, .. } => Some(*baseline),
                _ => None,
            })
            .collect();
        baselines.sort_by(|a, b| a.partial_cmp(b).unwrap());
        baselines.dedup_by(|a, b| (*a - *b).abs() < 0.01);
        assert!(baselines.len() >= 2, "capped width forces multiple lines");
        for i in &t {
            if let DisplayItem::Text { x, .. } = i {
                assert!(*x < 8.0 + 150.0 + 1.0, "word starts inside 150px box, x={x}");
            }
        }
    }
}

