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

pub fn parse_color(v: &str) -> Option<Color> {
    let v = v.trim().to_lowercase();
    if v == "transparent" {
        return None;
    }
    if v.starts_with("color-mix(") {
        let inner = v[v.find('(')? + 1..v.rfind(')')?].trim();
        // `in srgb,` prelude optional
        let inner = inner.split_once(',').map(|(a, b)| {
            if a.contains("srgb") || a.contains("oklab") { b } else { inner }
        }).unwrap_or(inner);
        let mut iter = inner.split(',');
        let a = iter.next()?.trim();
        let b = iter.next().map(str::trim).unwrap_or("transparent");
        // each arm: color [pct]
        let arm = |s: &str| -> Option<(Color, f32)> {
            let mut parts = s.rsplitn(2, ' ');
            let (pct, col) = match parts.next().unwrap_or("").strip_suffix('%') {
                Some(p) => (p.trim().parse::<f32>().ok()? / 100.0, parts.next().unwrap_or("")),
                None => (1.0, s.trim()),
            };
            Some((parse_color(col)?, pct))
        };
        let (ca, pa) = arm(a)?;
        let (cb, pb) = if b == "transparent" {
            (bgra(255, 255, 255), 0.0)
        } else {
            arm(b)?
        };
        let t = if pa + pb > 0.0 { pa / (pa + pb) } else { 0.5 };
        let mix = |x: u8, y: u8| (x as f32 * t + y as f32 * (1.0 - t)) as u8;
        let (_, g1, r1, a1) = ((ca >> 8) & 0xff, (ca >> 16) & 0xff, (ca >> 24) & 0xff, ca & 0xff);
        let _ = (g1, r1, a1);
        let mixc = |shift: u32| mix(((ca >> shift) & 0xff) as u8, ((cb >> shift) & 0xff) as u8);
        return Some(bgra(mixc(16) as u8, mixc(8) as u8, mixc(0) as u8));
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
                // composite semi-transparent colors over white
                let blend = |c: u32| ((c * 17 * a * 17 + 255 * (255 - a * 17)) / 255) as u8;
                bgra(blend(r), blend(g), blend(b))
            }
            6 => bgra(((d >> 16) & 0xff) as u8, ((d >> 8) & 0xff) as u8, (d & 0xff) as u8),
            8 => {
                let (r, g, b, a) = (
                    (d >> 24) & 0xff,
                    (d >> 16) & 0xff,
                    (d >> 8) & 0xff,
                    d & 0xff,
                );
                // composite semi-transparent colors over white
                let blend = |c: u32| ((c * a + 255 * (255 - a)) / 255) as u8;
                bgra(blend(r), blend(g), blend(b))
            }
            _ => return None,
        });
    }
    let rgb_inner = v
        .strip_prefix("rgb(")
        .or_else(|| v.strip_prefix("rgba("))
        .and_then(|s| s.strip_suffix(')'));
    if let Some(inner) = rgb_inner {
        // modern `r g b / a` and legacy `r,g,b[,a]`
        let parts: Vec<f32> = inner
            .split(|c: char| c == ',' || c.is_whitespace() || c == '/')
            .filter(|t| !t.is_empty())
            .filter_map(|p| {
                if let Some(pct) = p.strip_suffix('%') {
                    pct.parse::<f32>().ok().map(|x| x * 2.55)
                } else {
                    p.parse::<f32>().ok()
                }
            })
            .collect();
        if parts.len() >= 3 {
            let a = parts.get(3).copied().unwrap_or(255.0).clamp(0.0, 255.0);
            let blend = |c: f32| (c * a + 255.0 * (255.0 - a)) / 255.0;
            return Some(bgra(blend(parts[0]) as u8, blend(parts[1]) as u8, blend(parts[2]) as u8));
        }
    }
    named_color(&v)
}

/// `background-color` or a color inside the `background` shorthand.
fn bg_color(c: &Props) -> Option<Color> {
    if let Some(v) = c.get("background-color") {
        if let Some(col) = parse_color(v) {
            return Some(col);
        }
    }
    let v = c.get("background")?;
    // tokens: color | url() | position/size | repeat | attachment — take first color
    for tok in v.split_whitespace() {
        if let Some(col) = parse_color(tok.trim_end_matches(';')) {
            return Some(col);
        }
        if let Some(inner) = tok.strip_prefix("linear-gradient(") {
            let _ = inner;
            // gradient → take first color stop as flat bg (approximation)
            let rest = v[v.find('(')? + 1..].trim_end_matches(')');
            for part in rest.split(',') {
                let w = part.trim().split_whitespace().next().unwrap_or("");
                if let Some(col) = parse_color(w) {
                    return Some(col);
                }
            }
            return None;
        }
    }
    None
}

/// `box-shadow: x y [blur] [spread] [color]` → (dx, dy, color); blur/spread
/// ignored (flat shadow approximation).
fn parse_shadow(c: &Props, size: f32) -> Option<(f32, f32, Color)> {
    let v = c.get("box-shadow")?;
    if v.trim() == "none" {
        return None;
    }
    // split on top-level commas (not inside parens) — take first shadow
    let mut depth = 0i32;
    let first = v
        .split(|ch| {
            match ch {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            ch == ',' && depth <= 0
        })
        .next()?;
    let mut nums = Vec::new();
    let mut color = None;
    for tok in first.split_whitespace() {
        if let Some(n) = parse_len(tok, size) {
            nums.push(n);
        } else if let Some(col) = parse_color(tok.trim_end_matches(')')) {
            color = Some(col);
        } else if tok.starts_with("rgb") || tok.starts_with("#") {
            color = parse_color(first[first.find(tok).unwrap_or(0)..].trim_end_matches(')'));
            break;
        }
    }
    if nums.len() >= 2 {
        Some((nums[0], nums[1], color.unwrap_or(0x33000000)))
    } else {
        None
    }
}

/// `border-radius` → uniform max radius (px).
fn parse_radius(c: &Props, size: f32) -> f32 {
    c.get("border-radius")
        .and_then(|v| {
            v.split_whitespace()
                .filter_map(|t| parse_len(t, size))
                .reduce(f32::max)
        })
        .unwrap_or(0.0)
}

/// `border-width`/`border-style`/`border-color` split properties.
fn split_border(c: &Props, size: f32) -> Option<(f32, Color)> {
    let w = c.get("border-width").and_then(|v| parse_len(v, size))?;
    if w <= 0.0 || c.get("border-style").map(String::as_str) == Some("none") {
        return None;
    }
    let col = c.get("border-color").and_then(|v| parse_color(v))?;
    Some((w, col))
}

/// CSS length → px. `em`/`%` resolve against `rel` (usually parent font size).
/// Tiny `calc()` evaluator: numbers with units, + - * / and parens.
/// `%` resolves against `rel`; unitless numbers pass through.
fn calc_len(expr: &str, rel: f32) -> Option<f32> {
    struct P<'a> {
        t: Vec<Tok>,
        i: usize,
        rel: f32,
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
                    let w = if b[i..].starts_with(b"rem") { 3 } else { 2 };
                    u = Unit::Em;
                    i += w;
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
    // em/rem always mean root font size here; `rel` is the %-base.
    fn px(n: f32, u: Unit, rel: f32) -> f32 {
        match u {
            Unit::Px | Unit::None => n,
            Unit::Em => n * 16.0,
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
                    Some(px(n, u, self.rel))
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
    let mut p = P { t, i: 0, rel, _m: std::marker::PhantomData };
    p.expr()
}

/// parse_len plus viewport units (vh/svh/dvh/lvh→vh_px, vw→vw_px).
fn parse_len_vp(v: &str, rel: f32, vh: f32, vw: f32) -> Option<f32> {
    let v = v.trim().to_lowercase();
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
    parse_len(&v, rel)
}

fn parse_len(v: &str, rel: f32) -> Option<f32> {
    let v = v.trim().to_lowercase();
    if let Some(inner) = v.strip_prefix("calc(").and_then(|x| x.strip_suffix(')')) {
        return calc_len(inner, rel);
    }
    let num = |s: &str| s.trim().parse::<f32>().ok();
    if let Some(x) = v.strip_suffix("px") {
        num(x)
    } else if let Some(x) = v.strip_suffix("rem") {
        num(x).map(|n| n * BASE_SIZE)
    } else if let Some(x) = v.strip_suffix("em") {
        num(x).map(|n| n * rel)
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
        other => parse_len(other, parent).unwrap_or(parent).max(4.0),
    }
}

/// Shorthand sides (top, right, bottom, left) from 1–4 values.
fn sides(c: &Props, name: &str, rel: f32) -> [Option<f32>; 4] {
    if let Some(v) = c.get(name) {
        let vals: Vec<Option<f32>> = v
            .split_whitespace()
            .map(|p| parse_len(p, rel))
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

fn side(c: &Props, name: &str, rel: f32) -> Option<f32> {
    c.get(name).and_then(|v| parse_len(v, rel))
}

/// margin/padding longhands merged over the shorthand.
/// Returns [top, right, bottom, left] in px (0 when unset).
fn box_sides(c: &Props, name: &str, rel: f32) -> [f32; 4] {
    let sh = sides(c, name, rel);
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
                side(c, &format!("{name}-top"), rel).or(sh[0]).unwrap_or(0.0),
                side(c, &format!("{name}-right"), rel).or(sh[1]).unwrap_or(0.0),
                side(c, &format!("{name}-bottom"), rel).or(sh[2]).unwrap_or(0.0),
                side(c, &format!("{name}-left"), rel).or(sh[3]).unwrap_or(0.0),
            ];
        }
    };
    [
        side(c, t, rel).or(sh[0]).unwrap_or(0.0),
        side(c, r, rel).or(sh[1]).unwrap_or(0.0),
        side(c, b, rel).or(sh[2]).unwrap_or(0.0),
        side(c, l, rel).or(sh[3]).unwrap_or(0.0),
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

/// `border: 2px solid red` → (width px, color).
fn parse_border(v: &str, _rel: f32) -> Option<(f32, Color)> {
    let mut width = None;
    let mut color = None;
    for part in v.split_whitespace() {
        if part == "none" || part == "hidden" {
            return None;
        }
        if parse_len(part, 16.0).is_some() || matches!(part, "thin" | "medium" | "thick") {
            width = Some(match part {
                "thin" => 1.0,
                "thick" => 6.0,
                "medium" => 3.0,
                p => parse_len(p, 16.0).unwrap_or(1.0),
            });
        } else if let Some(c) = parse_color(part) {
            color = Some(c);
        }
    }
    width.zip(color.or(Some(BLACK)))
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
            '(' => depth += 1,
            ')' => depth -= 1,
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
fn track_spec(t: &str, rel: f32, vh: f32, vw: f32) -> GTrack {
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
            parse_len_vp(a.trim(), rel, vh, vw).unwrap_or(0.0)
        };
        let b = b.trim();
        if let Some(f) = b.strip_suffix("fr") {
            return GTrack::Fr(f.trim().parse::<f32>().unwrap_or(1.0).max(0.0), min);
        }
        if b == "auto" || b == "min-content" || b == "max-content" {
            return GTrack::Auto(min, f32::MAX);
        }
        return GTrack::Auto(min, parse_len_vp(b, rel, vh, vw).unwrap_or(f32::MAX));
    }
    if let Some(inner) = t
        .strip_prefix("fit-content(")
        .and_then(|x| x.strip_suffix(')'))
    {
        return GTrack::Auto(0.0, parse_len_vp(inner.trim(), rel, vh, vw).unwrap_or(f32::MAX));
    }
    GTrack::Fixed(parse_len_vp(&t, rel, vh, vw).unwrap_or(0.0))
}

/// `grid-template-*`/`grid-auto-*` value → expanded track list.
/// `none`/empty → []. repeat() expands; auto-fill/auto-fit size against
/// `avail` using the repeat unit's floor width.
fn grid_track_list(v: Option<&String>, rel: f32, vh: f32, vw: f32, avail: f32, gap: f32) -> Vec<GTrack> {
    let Some(v) = v else { return Vec::new() };
    let v = v.trim();
    if v.is_empty() || v == "none" {
        return Vec::new();
    }
    let mut out = Vec::new();
    for tok in track_tokens(v) {
        let t = tok.trim().to_lowercase();
        if let Some(inner) = t
            .strip_prefix("repeat(")
            .and_then(|x| x.strip_suffix(')'))
        {
            let (cnt, rest) = top_comma(inner).unwrap_or((inner, ""));
            let sub: Vec<GTrack> = track_tokens(rest)
                .iter()
                .map(|s| track_spec(s, rel, vh, vw))
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
            out.push(track_spec(&t, rel, vh, vw));
        }
    }
    out
}

/// A grid line placement term: `auto`, an integer line, or `span N`.
#[derive(Clone, Copy, PartialEq)]
enum GLine {
    Auto,
    Line(i32),
    Span(i32),
}

fn grid_line_val(t: &str) -> GLine {
    let t = t.trim().to_lowercase();
    if let Some(rest) = t.strip_prefix("span") {
        return GLine::Span(rest.trim().parse::<i32>().unwrap_or(1));
    }
    match t.parse::<i32>() {
        Ok(0) | Err(_) => GLine::Auto,
        Ok(n) => GLine::Line(n),
    }
}

/// `grid-column`/`grid-row` + `-start`/`-end` → (start, end) line terms.
fn grid_lines(c: &Props, axis: &str) -> (GLine, GLine) {
    let (mut s, mut e) = (GLine::Auto, GLine::Auto);
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
fn resolve_axis((s, e): (GLine, GLine), ntracks: usize) -> (Option<usize>, usize) {
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

struct BlockPatch {
    /// index of first of up to 5 placeholder rects (bg + 4 borders)
    idx: usize,
    x: f32,
    y: f32,
    w: f32,
    bg: Option<Color>,
    border: Option<(f32, Color)>,
    radius: f32,
    shadow: Option<(f32, f32, Color)>,
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
    list_stack: Vec<(bool, usize)>,
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
        let author = css::document_sheets(doc);
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
                    && side(c.as_ref(), "width", self.avail_w).is_some_and(|w| w <= 2.0)
                    && side(c.as_ref(), "height", self.avail_w).is_some_and(|h| h <= 2.0);
                match c.get("display").map(String::as_str) {
                    _ if visually_hidden => return,
                    Some("none") => return,
                    Some("block") | Some("list-item") | Some("flex") | Some("grid") => {
                        self.block(node, tag, &c, st);
                    }
                    Some("inline-block") | Some("inline-flex") | Some("inline-grid") => {
                        self.inline_block(node, &c, st)
                    }
                    _ => self.inline(node, tag, &c, st),
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
                st.color = col;
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
            st.pre = v.as_str() == "pre";
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
                parse_len(v, st.size).map(LineH::Px)
            };
        }
        if let Some(col) = bg_color(c) {
            st.inline_bg = Some(col);
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
        // descendants and a floor for this box's own extent
        let h_prop = side(c, "height", self.viewport_h);
        // `inset` shorthand + longhands merged into [top, right, bottom, left]
        let ish = sides(c, "inset", self.avail_w);
        let inset_l = |name: &str, i: usize, rel: f32| {
            c.get(name).and_then(|v| parse_len(v, rel)).or(ish[i])
        };
        let ins_t = inset_l("top", 0, st.size);
        let ins_r = inset_l("right", 1, self.avail_w);
        let ins_b = inset_l("bottom", 2, st.size);
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
        let [mt, mr, mb, ml] = box_sides(c, "margin", size);
        let [pt, pr, pb, pl] = box_sides(c, "padding", size).map(|v| v.max(0.0));
        let bg = bg_color(c);
        let border = c
            .get("border")
            .and_then(|v| parse_border(v, size))
            .or_else(|| split_border(c, size));
        // the block's own background paints behind the whole box, not per word
        st.inline_bg = None;
        let bw = border.map(|b| b.0).unwrap_or(0.0);

        if matches!(c.get("float").map(String::as_str), Some("left") | Some("right")) {
            return self.float_block(node, c, st, mt, mb);
        }

        // `width` caps the content box; `margin-?:auto` splits leftover space
        let mut ml = ml;
        let mut mr = mr;
        let mut w_prop = side(c, "width", self.avail_w).or(inset_w);
        if let Some(mxw) = c.get("max-width").and_then(|v| parse_len_vp(v, self.avail_w, self.viewport_h, self.avail_w)) {
            w_prop = Some(w_prop.unwrap_or(self.avail_w).min(mxw));
        }
        if let Some(mnw) = c.get("min-width").and_then(|v| parse_len_vp(v, self.avail_w, self.viewport_h, self.avail_w)) {
            w_prop = Some(w_prop.unwrap_or(0.0).max(mnw));
        }
        let w_prop = w_prop;
        if let Some(w) = w_prop {
            let extra = (self.avail_w - ml - mr - pl - pr - 2.0 * bw - w).max(0.0);
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

        match tag {
            "ul" | "menu" | "dir" => self.list_stack.push((false, 0)),
            "ol" => self.list_stack.push((true, 0)),
            _ => {}
        }
        // <ol>/<ul> counter only ticks on <li>
        let marker = if tag == "li" {
            let (ordered, num) = match self.list_stack.last_mut() {
                Some((o, n)) => {
                    *n += 1;
                    (*o, *n)
                }
                None => (false, 1),
            };
            Some(if ordered { format!("{num}. ") } else { "•  ".to_string() })
        } else {
            None
        };

        self.y += mt.max(0.0);
        let saved_mx = self.margin_x;
        let saved_avail = self.avail_w;
        let saved_align = self.align;
        let inner_w = if let Some(w) = w_prop {
            w.max(0.0)
        } else {
            (self.avail_w - ml - mr - pl - pr - 2.0 * bw).max(1.0)
        };
        self.margin_x += ml.max(0.0) + pl + bw;
        self.avail_w = inner_w;
        self.align = st.align;

        // background/border need the final rect — reserve placeholders.
        // The patch covers the border box (padding included).
        let patch = if bg.is_some() || border.is_some() {
            let shadow = parse_shadow(c, size);
            let idx = self.items.len();
            for _ in 0..6 {
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
                x: self.margin_x - pl - bw,
                y: self.y,
                w: pl + inner_w + pr + 2.0 * bw,
                bg,
                border,
                radius: parse_radius(c, size),
                shadow,
            })
        } else {
            None
        };

        // padding-top + border push content down inside the painted box
        self.y += pt + bw;
        let content_top = self.y;
        let children_items_start = self.items.len();
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
            // min-height/height: grow the box; centered flex columns shift children
            if let Some(mh) = c
                .get("min-height")
                .and_then(|v| parse_len_vp(v, self.avail_w, self.viewport_h, self.avail_w))
                .or(h_prop)
            {
                let extra = mh - (self.y - content_top);
                if extra > 0.0 {
                    let centered = c.get("display").map(String::as_str) == Some("flex")
                        && matches!(
                            c.get("justify-content").map(String::as_str),
                            Some("center") | Some("space-evenly") | Some("space-around")
                        );
                    if centered {
                        let dy = extra / 2.0;
                        for i in children_items_start..self.items.len() {
                            let it = std::mem::replace(
                                &mut self.items[i],
                                DisplayItem::FillRect { x: 0.0, y: 0.0, w: 0.0, h: 0.0, color: 0 },
                            );
                            self.items[i] = offset_item(it, 0.0, dy);
                        }
                    }
                    self.y += extra;
                }
            }
        }
        // positioned box with `top`+`bottom` (e.g. `inset:0`): stretch to the
        // containing height even when content is shorter
        if let Some(ih) = inset_h {
            let extra = ih - (self.y - content_top);
            if extra > 0.0 {
                self.y += extra;
            }
        }
        // padding-bottom + border extend the painted box below content
        self.y += pb + bw;

        if let Some(p) = patch {
            self.finish_patch(&p);
        }
        // element border box → click hit-test map
        self.hits.push((
            self.margin_x - pl - bw,
            content_top - pt - bw,
            inner_w + pl + pr + 2.0 * bw,
            self.y - content_top + pt + bw,
            node,
        ));
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
        let [pt, pr, pb, pl] = box_sides(c, "padding", size).map(|v| v.max(0.0));
        let [_t, mr, _b, ml] = box_sides(c, "margin", size);
        let bg = bg_color(c);
        let border = c
            .get("border")
            .and_then(|v| parse_border(v, size))
            .or_else(|| split_border(c, size));
        st.inline_bg = None;
        let bw = border.map(|b| b.0).unwrap_or(0.0);
        let right = c.get("float").is_some_and(|v| v.trim() == "right");

        let inner = if let Some(w) = side(c, "width", self.avail_w) {
            w.max(1.0)
        } else {
            self.shrink_width(node, &st)
        };
        let (li, ri) = self.float_insets();
        let room = (self.avail_w - li - ri).max(1.0);
        let box_w = (inner + pl + pr + 2.0 * bw).min(room);
        let inner = (box_w - pl - pr - 2.0 * bw).max(1.0);

        self.y += mt.max(0.0);
        let top = self.y;
        let saved = (self.margin_x, self.avail_w, self.align);
        let outer_x = if right {
            self.margin_x + self.avail_w - ri - box_w - mr.max(0.0)
        } else {
            self.margin_x + li + ml.max(0.0)
        };
        self.margin_x = outer_x + pl + bw;
        self.avail_w = inner;
        self.align = st.align;

        let patch = if bg.is_some() || border.is_some() {
            let idx = self.items.len();
            for _ in 0..5 {
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
                shadow: None,
            })
        } else {
            None
        };

        self.y += pt + bw;
        for &ch in self.doc.children(node) {
            self.walk(ch, &st);
        }
        self.flush_line();
        self.y += pb + bw;
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
    fn inline_block(&mut self, node: NodeId, c: &Props, mut st: Style) {
        let size = st.size;
        let [pt, pr, pb, pl] = box_sides(c, "padding", size).map(|v| v.max(0.0));
        let [_t, mr, mb, ml] = box_sides(c, "margin", size);
        let bg = bg_color(c);
        let border = c
            .get("border")
            .and_then(|v| parse_border(v, size))
            .or_else(|| split_border(c, size));
        st.inline_bg = None; // the box paints its own bg
        let bw = border.map(|b| b.0).unwrap_or(0.0);

        // width property else shrink-wrap (min-content approx; flex sums children)
        let inner = if let Some(w) = side(c, "width", self.avail_w) {
            w.max(1.0)
        } else if matches!(
            c.get("display").map(String::as_str),
            Some("flex") | Some("inline-flex") | Some("inline-grid")
        ) {
            self.flex_shrink_width(node, c, &st)
        } else {
            self.shrink_width(node, &st)
        };
        let room = self.content_width();
        let box_w = (inner + pl + pr + 2.0 * bw).min(room);
        let inner = (box_w - pl - pr - 2.0 * bw).max(1.0);

        let (items, y_end, nhits) = self.nest(node, &st, pl + bw, pt + bw, inner, c);
        let box_h = side(c, "height", self.avail_w)
            .or_else(|| aspect_ratio(c).map(|r| box_w / r))
            .unwrap_or(y_end + pb + bw)
            .max(1.0);

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
            edge_rects(&mut out, 0.0, 0.0, box_w, box_h, bw2, col);
        }
        out.extend(items);

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
        let [mt, _mr, mb, ml] = box_sides(c, "margin", size);
        let bg = bg_color(c);
        let border = c
            .get("border")
            .and_then(|v| parse_border(v, size))
            .or_else(|| split_border(c, size));
        st.inline_bg = None;
        // <table border=N> fallback cell borders (border="0" = none)
        let attr_border = self
            .doc
            .attr(node, "border")
            .and_then(|v| v.trim().parse::<f32>().ok())
            .filter(|w| *w > 0.0)
            .map(|w| (w, BLACK));

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

        // column widths: max min-content width per column
        let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        let mut col_w = vec![0.0f32; ncols];
        for row in &rows {
            for (i, &cell) in row.iter().enumerate() {
                let cs = self.style(cell);
                let mut cst = st.clone();
                self.apply_computed(&cs, &mut cst);
                let [_t, pr, _b, pl] = box_sides(&cs, "padding", cst.size).map(|v| v.max(0.0));
                let bw = cs
                    .get("border")
                    .and_then(|v| parse_border(v, cst.size))
                    .or(attr_border)
                    .map(|b| b.0)
                    .unwrap_or(0.0);
                let mut w = self.shrink_width(cell, &cst) + pl + pr + 2.0 * bw;
                if let Some(wp) = side(&cs, "width", cst.size) {
                    w = w.max(wp + pl + pr + 2.0 * bw);
                }
                col_w[i] = col_w[i].max(w);
            }
        }

        self.y += mt.max(0.0);
        let saved = (self.margin_x, self.avail_w, self.align);
        self.margin_x += ml.max(0.0);
        self.avail_w -= ml.max(0.0);
        self.align = st.align;

        // scale columns down to fit the available width
        let avail = self.avail_w.max(1.0);
        let total: f32 = col_w.iter().sum();
        if total > avail && total > 0.0 {
            for w in col_w.iter_mut() {
                *w *= avail / total;
            }
        }
        let table_w: f32 = col_w.iter().sum();

        let patch = if bg.is_some() || border.is_some() {
            let idx = self.items.len();
            for _ in 0..5 {
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
                shadow: None,
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
                Option<(f32, Color)>,
                Vec<DisplayItem>,
                Vec<(f32, f32, f32, f32, NodeId)>,
            )> = Vec::new();
            let mut x = self.margin_x;
            let mut row_h = 0.0f32;
            for (i, &cell) in row.iter().enumerate() {
                let cw = col_w[i];
                let cs = self.style(cell);
                let mut cst = st.clone();
                self.apply_computed(&cs, &mut cst);
                cst.inline_bg = None; // cell bg paints the whole cell
                let [pt, pr, pb, pl] = box_sides(&cs, "padding", cst.size).map(|v| v.max(0.0));
                let cborder = cs
                    .get("border")
                    .and_then(|v| parse_border(v, cst.size))
                    .or(attr_border);
                let bw = cborder.map(|b| b.0).unwrap_or(0.0);
                let cbg = cs.get("background-color").and_then(|v| parse_color(v));
                let inner = (cw - pl - pr - 2.0 * bw).max(1.0);
                let (items, y_end, nhits) = self.nest(cell, &cst, pl + bw, pt + bw, inner, &cs);
                let min_h = self.eff_line_h(&cst);
                let cell_h = (y_end + pb + bw).max(min_h);
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
                    edge_rects(&mut self.items, cx, self.y, cw, row_h, bw2, col);
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
        let gap = c
            .get("gap")
            .and_then(|v| parse_len(v, st.size))
            .unwrap_or(0.0);
        // column direction: children stack as blocks, vertical gap
        if c.get("flex-direction").map(String::as_str) == Some("column") {
            let mut st = st;
            if c.get("align-items").map(String::as_str) == Some("center") {
                st.align = Align::Center;
            }
            let mut first = true;
            for &ch in self.doc.children(node) {
                if !matches!(self.doc.kind(ch), NodeKind::Element { .. } | NodeKind::Text(_)) {
                    continue;
                }
                if !first && gap > 0.0 {
                    self.y += gap;
                }
                self.walk(ch, &st);
                first = false;
            }
            return;
        }
        // justify-content on the main axis: pre-measure total child width
        let total = self.flex_shrink_width(node, c, &st);
        let extra = (self.content_width() - total).max(0.0);
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
                    if !first && effective_gap > 0.0 {
                        self.spacer(effective_gap);
                    }
                    let mut cst = st.clone();
                    self.apply_computed(&cc, &mut cst);
                    // replaced elements (img etc.) emit their own item via inline()
                    match self.doc.tag(ch) {
                        Some(t @ ("img" | "input" | "select" | "textarea" | "video" | "canvas" | "iframe" | "object" | "embed" | "svg" | "br" | "hr")) => {
                            self.inline(ch, t, &cc, cst)
                        }
                        _ => self.inline_block(ch, &cc, cst),
                    }
                    first = false;
                }
                NodeKind::Text(t) => self.text(t, &st),
                _ => {}
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
            bw: f32,
            bg: Option<Color>,
            border: Option<(f32, Color)>,
            radius: f32,
            shadow: Option<(f32, f32, Color)>,
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
        let cont_h = c
            .get("height")
            .and_then(|v| parse_len_vp(v, self.viewport_h, self.viewport_h, avail))
            .or(self.inset_h)
            .filter(|h| *h > 0.0);

        // gaps: `gap: <row> <col>?`, then longhands override
        let (mut row_gap, mut col_gap) = (0.0f32, 0.0f32);
        if let Some(v) = c.get("gap") {
            let mut it = v.split_whitespace();
            row_gap = it
                .next()
                .and_then(|t| parse_len_vp(t, cont_h.unwrap_or(avail), self.viewport_h, avail))
                .unwrap_or(0.0);
            col_gap = it
                .next()
                .and_then(|t| parse_len_vp(t, avail, self.viewport_h, avail))
                .unwrap_or(row_gap);
        }
        if let Some(v) = c.get("row-gap") {
            row_gap = parse_len_vp(v, cont_h.unwrap_or(avail), self.viewport_h, avail)
                .unwrap_or(row_gap);
        }
        if let Some(v) = c.get("column-gap") {
            col_gap = parse_len_vp(v, avail, self.viewport_h, avail).unwrap_or(col_gap);
        }

        let row_rel = cont_h.unwrap_or(avail);
        let col_specs =
            grid_track_list(c.get("grid-template-columns"), avail, self.viewport_h, avail, avail, col_gap);
        let row_specs =
            grid_track_list(c.get("grid-template-rows"), row_rel, self.viewport_h, avail, row_rel, row_gap);
        let auto_cols =
            grid_track_list(c.get("grid-auto-columns"), avail, self.viewport_h, avail, avail, col_gap);
        let auto_rows =
            grid_track_list(c.get("grid-auto-rows"), row_rel, self.viewport_h, avail, row_rel, row_gap);
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
            let (ecol, cs) = resolve_axis(grid_lines(&ic, "column"), exp_cols);
            let (erow, rs) = resolve_axis(grid_lines(&ic, "row"), exp_rows);
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
            let m = box_sides(&cc, "margin", cst.size);
            let p = box_sides(&cc, "padding", cst.size).map(|v| v.max(0.0));
            let bg = bg_color(&cc);
            let border = cc
                .get("border")
                .and_then(|v| parse_border(v, cst.size))
                .or_else(|| split_border(&cc, cst.size));
            let bw = border.map(|b| b.0).unwrap_or(0.0);
            let radius = parse_radius(&cc, cst.size);
            let shadow = parse_shadow(&cc, cst.size);
            let w_prop = side(&cc, "width", avail).map(|w| {
                let mut w = w;
                if let Some(mx) = cc.get("max-width").and_then(|v| parse_len_vp(v, avail, self.viewport_h, avail)) {
                    w = w.min(mx);
                }
                if let Some(mn) = cc.get("min-width").and_then(|v| parse_len_vp(v, avail, self.viewport_h, avail)) {
                    w = w.max(mn);
                }
                w
            });
            let intrinsic_w = w_prop
                .unwrap_or_else(|| self.flex_shrink_width(ch, &cc, &cst).min(avail))
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
            let h_prop = side(&cc, "height", cont_h.unwrap_or(avail));
            let min_h = cc
                .get("min-height")
                .and_then(|v| parse_len_vp(v, avail, self.viewport_h, avail));
            let max_h = cc
                .get("max-height")
                .and_then(|v| parse_len_vp(v, avail, self.viewport_h, avail));
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
            let need = it.intrinsic_w + it.p[1] + it.p[3] + 2.0 * it.bw + it.m[1] + it.m[3];
            if it.cs == 1 {
                if let GTrack::Auto(_, mx) = col_spec[it.c] {
                    cw[it.c] = cw[it.c].max(need.min(mx));
                }
            }
        }
        for it in &items {
            if it.cs > 1 {
                let need = it.intrinsic_w + it.p[1] + it.p[3] + 2.0 * it.bw + it.m[1] + it.m[3];
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
            let inner_w = (cell_w - it.m[1] - it.m[3] - it.p[1] - it.p[3] - 2.0 * it.bw).max(1.0);
            let content_w = it
                .w_prop
                .map(|w| w.min(inner_w))
                .unwrap_or(match it.just {
                    GAlign::Stretch => inner_w,
                    _ => it.intrinsic_w.min(inner_w),
                })
                .max(1.0);
            let (laid, bottom, nhits) = self.nest(
                it.id,
                &it.st,
                it.p[3] + it.bw,
                it.p[0] + it.bw,
                content_w,
                &it.cc,
            );
            it.laid = laid;
            it.hits = nhits;
            let content_h = bottom + it.p[2] + it.bw; // border-box height
            let box_w = content_w + it.p[1] + it.p[3] + 2.0 * it.bw;
            let mut box_h = it
                .h_prop
                .map(|h| h + it.p[0] + it.p[2] + 2.0 * it.bw)
                .or_else(|| it.ratio.map(|r| box_w / r))
                .unwrap_or(content_h);
            if let Some(mn) = it.min_h {
                box_h = box_h.max(mn + it.p[0] + it.p[2] + 2.0 * it.bw);
            }
            if let Some(mx) = it.max_h {
                box_h = box_h.min(mx + it.p[0] + it.p[2] + 2.0 * it.bw);
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
            let content_w = it
                .w_prop
                .map(|w| w.min(cell_w - it.m[1] - it.m[3] - it.p[1] - it.p[3] - 2.0 * it.bw).max(1.0))
                .unwrap_or(match it.just {
                    GAlign::Stretch => {
                        (cell_w - it.m[1] - it.m[3] - it.p[1] - it.p[3] - 2.0 * it.bw).max(1.0)
                    }
                    _ => it.intrinsic_w
                        .min(cell_w - it.m[1] - it.m[3] - it.p[1] - it.p[3] - 2.0 * it.bw)
                        .max(1.0),
                });
            let box_w = content_w + it.p[1] + it.p[3] + 2.0 * it.bw;
            let mut box_h = if let Some(h) = it.h_prop {
                h + it.p[0] + it.p[2] + 2.0 * it.bw
            } else if let Some(r) = it.ratio {
                box_w / r
            } else if it.al == GAlign::Stretch {
                (cell_h - it.m[0] - it.m[2]).max(1.0)
            } else {
                it.box_h_intr
            };
            if let Some(mn) = it.min_h {
                box_h = box_h.max(mn + it.p[0] + it.p[2] + 2.0 * it.bw);
            }
            if let Some(mx) = it.max_h {
                box_h = box_h.min(mx + it.p[0] + it.p[2] + 2.0 * it.bw);
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
            if let Some((ox, oy, col)) = it.shadow {
                self.items.push(DisplayItem::RoundedRect {
                    x: ix + ox,
                    y: iy + oy,
                    w: box_w,
                    h: box_h,
                    radius: it.radius,
                    color: col,
                });
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
                edge_rects(&mut self.items, ix, iy, box_w, box_h, bw2, col);
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
            if let Some(h) = side(c, "height", self.viewport_h) {
                self.cb_h = Some(h);
            }
        }
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

    /// min-content width approximation: the longest word inside `node`
    /// measured in the current font (or a default for textless content).
    /// Explicit `width`, nested flex rows, and fixed-track grids contribute
    /// their own geometry rather than text width.
    fn shrink_width(&mut self, node: NodeId, st: &Style) -> f32 {
        let text = self.doc.text_content(node);
        let mut w = 0.0f32;
        if let Some(fi) = self.resolve_font(st) {
            if let Some(f) = self.fonts.font(fi) {
                for word in text.split_whitespace() {
                    w = w.max(f.measure(word, st.size));
                }
            }
        }
        // layout-aware: nested containers out-measure their text
        let c = self.style(node);
        if let Some(ew) = side(&c, "width", self.avail_w) {
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
                                    side(&cc, "width", self.avail_w)
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
                    let tracks = grid_track_list(
                        c.get("grid-template-columns"),
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

    /// Intrinsic width of a flex container: children's widths + gaps + their
    /// own padding/margins — a max-content approximation.
    fn flex_shrink_width(&mut self, node: NodeId, c: &Props, st: &Style) -> f32 {
        let gap = c
            .get("gap")
            .and_then(|v| parse_len(v, st.size))
            .unwrap_or(0.0);
        let mut w = 0.0;
        let mut n = 0usize;
        for &ch in self.doc.children(node) {
            if let NodeKind::Text(t) = self.doc.kind(ch) {
                if let Some(fi) = self.resolve_font(st) {
                    if let Some(f) = self.fonts.font(fi) {
                        w += f.measure(&t.split_whitespace().collect::<Vec<_>>().join(" "), st.size);
                    }
                }
                continue;
            }
            if !matches!(self.doc.kind(ch), NodeKind::Element { .. }) {
                continue;
            }
            let cc = self.style(ch);
            if cc.get("display").map(String::as_str) == Some("none") {
                continue;
            }
            let mut cst = st.clone();
            self.apply_computed(&cc, &mut cst);
            let [_t, pr, _b, pl] = box_sides(&cc, "padding", cst.size);
            let [_t, mr, _b, ml] = box_sides(&cc, "margin", cst.size);
            // an explicit `width` contributes itself, not the content's
            // min-content estimate (a 512px board inside a flex row is 512
            // no matter how small its text is)
            let cw = side(&cc, "width", self.avail_w)
                .unwrap_or_else(|| self.shrink_width(ch, &cst));
            w += cw + pl.max(0.0) + pr.max(0.0) + ml + mr;
            n += 1;
        }
        (w + gap * n.saturating_sub(1) as f32).max(st.size)
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
        if self.line_x + w > self.content_width() && !self.line.is_empty() {
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
        if let Some((ox, oy, col)) = p.shadow {
            put(5, DisplayItem::RoundedRect {
                x: p.x + ox,
                y: p.y + oy,
                w: p.w,
                h,
                radius: p.radius,
                color: col,
            });
        }
        if let Some(bg) = p.bg {
            if p.radius > 0.5 {
                put(0, DisplayItem::RoundedRect { x: p.x, y: p.y, w: p.w, h, radius: p.radius, color: bg });
            } else {
                put(0, DisplayItem::FillRect { x: p.x, y: p.y, w: p.w, h, color: bg });
            }
        }
        if let Some((bw, col)) = p.border {
            put(1, DisplayItem::FillRect { x: p.x, y: p.y, w: p.w, h: bw, color: col });
            put(2, DisplayItem::FillRect { x: p.x, y: p.y + h - bw, w: p.w, h: bw, color: col });
            put(3, DisplayItem::FillRect { x: p.x, y: p.y, w: bw, h, color: col });
            put(4, DisplayItem::FillRect { x: p.x + p.w - bw, y: p.y, w: bw, h, color: col });
        }
    }

    fn inline(&mut self, node: NodeId, tag: &str, c: &Props, mut st: Style) {
        if tag == "a" {
            st.href = self.doc.attr(node, "href").map(String::from);
        }
        // inline margins become spacing around the element's runs
        let [_t, mr, _b, ml] = box_sides(c, "margin", st.size);
        if ml > 0.0 {
            self.spacer(ml);
        }
        match tag {
            "br" => self.flush_line(),
            "svg" => {
                self.flush_line();
                let size = side(c, "width", self.avail_w)
                    .or_else(|| side(c, "height", self.avail_w))
                    .unwrap_or(24.0);
                let lh = self.eff_line_h(&st);
                let color = st.color;
                self.items.push(DisplayItem::Raster {
                    x: self.line_origin(),
                    y: self.y,
                    w: size,
                    h: size,
                    pixels: svg::render(self.doc, node, size as u32, size as u32, color),
                });
                self.y += size.max(lh);
            }
            "img" => {
                self.flush_line();
                // CSS width/height beat the attributes (inline style and
                // utility classes size almost every modern <img>)
                let w = side(c, "width", self.avail_w)
                    .or_else(|| self.doc.attr(node, "width")
                        .and_then(|v| v.trim_end_matches("px").parse().ok()))
                    .unwrap_or(64.0f32);
                let h = side(c, "height", self.avail_w)
                    .or_else(|| self.doc.attr(node, "height")
                        .and_then(|v| v.trim_end_matches("px").parse().ok()))
                    .unwrap_or(48.0f32);
                let line_h = self.eff_line_h(&st);
                if let Some(src) = self.doc.attr(node, "src") {
                    self.items.push(DisplayItem::Image {
                        x: self.line_origin(),
                        y: self.y,
                        w,
                        h,
                        src: src.to_string(),
                    });
                }
                self.y += h.max(line_h);
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
        if self.line_x + w > self.content_width() && !self.line.is_empty() {
            self.flush_line();
        }
        if w > self.content_width() && self.line.is_empty() {
            // word longer than the line — split mid-word
            let (mut chunk, mut cw) = (String::new(), 0.0f32);
            for c in word.chars() {
                let fw = self
                    .fonts
                    .font(font)
                    .map(|f| {
                        f.advance_units(f.glyph_id(c)) as f32 * st.size / f.units_per_em() as f32
                    })
                    .unwrap_or(st.size * 0.5);
                if cw + fw > self.content_width() && !chunk.is_empty() {
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

#[cfg(test)]
mod dbg3 {
    use super::*;
    #[test]
    fn dbg_nav() {
        let css = std::fs::read_to_string("/tmp/coobyk.css").unwrap();
        let sh = rowser_css::parse(&css);
        let d = rowser_html::parse(&std::fs::read_to_string("/tmp/coobyk.html").unwrap());
        let mut tags = std::collections::HashMap::<String, usize>::new();
        for id in d.descendants(d.root) {
            if let Some(t) = d.tag(id) {
                *tags.entry(t.to_string()).or_insert(0) += 1;
            }
        }
        eprintln!("TAGS: {:?}", tags);
        let sh2 = rowser_css::parse(&css);
        let mut fonts = FontCache::new();
        let (items, _, _, _) = Layouter::layout(&d, &mut fonts, 800.0, 0.0, &[sh2]);
        for it in &items {
            match it {
                DisplayItem::Image { x, y, w, h, src } => eprintln!("IMG {x:.0},{y:.0} {w:.0}x{h:.0} {src}"),
                DisplayItem::Text { x, baseline, text, size, .. } if text.contains("Coobyk") || text.contains("I do") => eprintln!("T {x:.0},{baseline:.0} size={size:.0} {text:?}"),
                _ => {}
            }
        }
        for id in d.descendants(d.root) {
            if d.tag(id) == Some("h1") {
                eprintln!("H1 computed: {:?}", rowser_css::computed(&d, id, &[&sh]).iter().filter(|(k,_)| k.starts_with("font")||k.starts_with("padding")).collect::<Vec<_>>());
            }
        }
    }
}
