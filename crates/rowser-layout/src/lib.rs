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
use rowser_dom::{Document, NodeId, NodeKind};
use rowser_text::FontCache;
use std::collections::HashMap;
use std::sync::OnceLock;

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
            3 | 4 => {
                let (r, g, b, a) = (
                    (d >> 12) & 0xf,
                    (d >> 8) & 0xf,
                    (d >> 4) & 0xf,
                    if hex.len() == 4 { d & 0xf } else { 0xf },
                );
                bgra((r * 17) as u8, (g * 17) as u8, (b * 17) as u8)
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
    if let Some(inner) = v.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')')) {
        let parts: Vec<f32> = inner
            .split(',')
            .filter_map(|p| {
                let p = p.trim();
                if let Some(pct) = p.strip_suffix('%') {
                    pct.parse::<f32>().ok().map(|x| x * 2.55)
                } else {
                    p.parse::<f32>().ok()
                }
            })
            .collect();
        if parts.len() >= 3 {
            return Some(bgra(parts[0] as u8, parts[1] as u8, parts[2] as u8));
        }
    }
    named_color(&v)
}

/// `background-color` or a color inside the `background` shorthand.
fn bg_color(c: &HashMap<String, String>) -> Option<Color> {
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

/// `border-width`/`border-style`/`border-color` split properties.
fn split_border(c: &HashMap<String, String>, size: f32) -> Option<(f32, Color)> {
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
fn sides(c: &HashMap<String, String>, name: &str, rel: f32) -> [Option<f32>; 4] {
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

fn side(c: &HashMap<String, String>, name: &str, rel: f32) -> Option<f32> {
    c.get(name).and_then(|v| parse_len(v, rel))
}

/// margin/padding longhands merged over the shorthand.
/// Returns [top, right, bottom, left] in px (0 when unset).
fn box_sides(c: &HashMap<String, String>, name: &str, rel: f32) -> [f32; 4] {
    let sh = sides(c, name, rel);
    [
        side(c, &format!("{name}-top"), rel).or(sh[0]).unwrap_or(0.0),
        side(c, &format!("{name}-right"), rel).or(sh[1]).unwrap_or(0.0),
        side(c, &format!("{name}-bottom"), rel).or(sh[2]).unwrap_or(0.0),
        side(c, &format!("{name}-left"), rel).or(sh[3]).unwrap_or(0.0),
    ]
}

/// Is margin-left/right `auto` (longhand or shorthand position)?
fn margin_auto(c: &HashMap<String, String>, left: bool) -> bool {
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
}

pub struct Layouter<'a> {
    doc: &'a Document,
    sheets: Vec<&'a css::StyleSheet>,
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
    ) -> (Vec<DisplayItem>, f32, Vec<(String, f32)>) {
        let author = css::document_sheets(doc);
        let mut sheets: Vec<&css::StyleSheet> = vec![ua_sheet()];
        sheets.extend(external.iter());
        sheets.extend(author.iter());
        let mut l = Layouter {
            doc,
            sheets,
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
        };
        l.walk(doc.root, base_style());
        l.flush_line();
        // floats don't consume vertical space at their site; make sure the
        // document is at least as tall as the deepest float
        for f in &l.floats {
            l.y = l.y.max(f.bottom);
        }
        let h = l.y + scroll + 8.0;
        (l.items, h, l.anchors)
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

    fn walk(&mut self, node: NodeId, style: Style) {
        match self.doc.kind(node) {
            NodeKind::Document => {
                for &c in self.doc.children(node) {
                    self.walk(c, style.clone());
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
                let c = css::computed(self.doc, node, &self.sheets);
                let mut st = style.clone();
                self.apply_computed(&c, &mut st);
                match c.get("display").map(String::as_str) {
                    Some("none") => return,
                    Some("block") | Some("list-item") | Some("flex") => {
                        self.block(node, tag, &c, st);
                    }
                    Some("inline-block") => self.inline_block(node, &c, st),
                    _ => self.inline(node, tag, &c, st),
                }
            }
        }
    }

    fn apply_computed(&mut self, c: &HashMap<String, String>, st: &mut Style) {
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

    fn block(&mut self, node: NodeId, tag: &str, c: &HashMap<String, String>, mut st: Style) {
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
        let flow_y = if positioned {
            let fy = self.y;
            let fmx = self.margin_x;
            let pos = c.get("position").map(String::as_str);
            if let Some(t) = c.get("top").and_then(|v| parse_len(v, st.size)) {
                // y is viewport-mapped (scroll already baked in): fixed pins
                // to `top`, sticky clamps at `top` once scrolled past
                self.y = match pos {
                    Some("absolute") => fy + t,
                    Some("sticky") => self.y.max(t),
                    _ => t,
                };
            }
            if let Some(l) = c.get("left").and_then(|v| parse_len(v, self.avail_w)) {
                self.margin_x = l.max(0.0);
            }
            Some((fy, fmx))
        } else {
            None
        };
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
        let w_prop = side(c, "width", self.avail_w);
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
                x: self.margin_x - pl - bw,
                y: self.y,
                w: pl + inner_w + pr + 2.0 * bw,
                bg,
                border,
            })
        } else {
            None
        };

        // padding-top + border push content down inside the painted box
        self.y += pt + bw;
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
                self.word(&m, st.clone());
            }
            if matches!(c.get("display").map(String::as_str), Some("flex")) {
                self.flex_children(node, c, st);
            } else {
                for &ch in self.doc.children(node) {
                    self.walk(ch, st.clone());
                }
            }
            self.flush_line();
        }
        // padding-bottom + border extend the painted box below content
        self.y += pb + bw;

        if let Some(p) = patch {
            self.finish_patch(&p);
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
        c: &HashMap<String, String>,
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
            })
        } else {
            None
        };

        self.y += pt + bw;
        for &ch in self.doc.children(node) {
            self.walk(ch, st.clone());
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
    fn inline_block(&mut self, node: NodeId, c: &HashMap<String, String>, mut st: Style) {
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

        let (items, y_end) = self.nest(node, &st, pl + bw, pt + bw, inner, c);
        let box_h = side(c, "height", self.avail_w).unwrap_or(y_end + pb + bw).max(1.0);

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
        self.push_box(box_w, box_h, out, mb.max(0.0));
        if mr > 0.0 {
            self.spacer(mr);
        }
    }

    /// Minimal auto table layout: column width = max over rows of each cell's
    /// min-content width (longest word + padding + border), scaled down to fit.
    /// Each row is laid out cell-by-cell via `nest` at the column's x.
    /// Approximations: no colspan/rowspan, borders always drawn per-cell
    /// (border-collapse ≈ same visual since cells abut), rows don't wrap.
    fn table(&mut self, node: NodeId, c: &HashMap<String, String>, mut st: Style) {
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
                let cs = css::computed(self.doc, cell, &self.sheets);
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
            )> = Vec::new();
            let mut x = self.margin_x;
            let mut row_h = 0.0f32;
            for (i, &cell) in row.iter().enumerate() {
                let cw = col_w[i];
                let cs = css::computed(self.doc, cell, &self.sheets);
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
                let (items, y_end) = self.nest(cell, &cst, pl + bw, pt + bw, inner, &cs);
                let min_h = self.eff_line_h(&cst);
                let cell_h = (y_end + pb + bw).max(min_h);
                boxes.push((x, cw, cell_h, cbg, cborder, items));
                x += cw;
                row_h = row_h.max(cell_h);
            }
            // second pass: stretch cell bg/borders to row height, place content
            for (cx, cw, _ch, cbg, cborder, items) in boxes {
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
            self.walk(e, st.clone());
        }
    }

    /// Lay out `node`'s children off-screen into a private item list.
    /// Content box: origin (x0, y0), `avail` wide. Returns (items, bottom y).
    /// `display:flex` — element children laid as inline-block items on a
    /// row with `gap` spacing.
    fn flex_children(&mut self, node: NodeId, c: &HashMap<String, String>, st: Style) {
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
                self.walk(ch, st.clone());
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
                    let cc = css::computed(self.doc, ch, &self.sheets);
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
                NodeKind::Text(t) => self.text(t, st.clone()),
                _ => {}
            }
        }
    }

    fn nest(
        &mut self,
        node: NodeId,
        st: &Style,
        x0: f32,
        y0: f32,
        avail: f32,
        c: &HashMap<String, String>,
    ) -> (Vec<DisplayItem>, f32) {
        let saved_items = std::mem::take(&mut self.items);
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
        if matches!(c.get("display").map(String::as_str), Some("flex")) {
            self.flex_children(node, c, st.clone());
        } else {
            for &ch in self.doc.children(node) {
                self.walk(ch, st.clone());
            }
        }
        self.flush_line();
        let bottom = self.y;
        let items = std::mem::replace(&mut self.items, saved_items);
        self.y = saved.0;
        self.margin_x = saved.1;
        self.avail_w = saved.2;
        self.align = saved.3;
        self.line = saved_line;
        self.line_x = saved_lx;
        self.pending_space = saved_ps;
        self.floats = saved_floats;
        (items, bottom)
    }

    /// min-content width approximation: the longest word inside `node`
    /// measured in the current font (or a default for textless content).
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
        if w <= 0.0 {
            w = st.size * 4.0; // default for empty/image-ish content
        }
        w
    }

    /// Intrinsic width of a flex container: children's widths + gaps + their
    /// own padding/margins — a max-content approximation.
    fn flex_shrink_width(&mut self, node: NodeId, c: &HashMap<String, String>, st: &Style) -> f32 {
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
            let cc = css::computed(self.doc, ch, &self.sheets);
            if cc.get("display").map(String::as_str) == Some("none") {
                continue;
            }
            let mut cst = st.clone();
            self.apply_computed(&cc, &mut cst);
            let [_t, pr, _b, pl] = box_sides(&cc, "padding", cst.size);
            let [_t, mr, _b, ml] = box_sides(&cc, "margin", cst.size);
            w += self.shrink_width(ch, &cst) + pl.max(0.0) + pr.max(0.0) + ml + mr;
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
            box_h: 0.0,
        });
        self.line_x += w;
    }

    /// Push an inline-block box (pre-rendered items) onto the line like a word.
    fn push_box(&mut self, w: f32, h: f32, items: Vec<DisplayItem>, mb: f32) {
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
            box_h: h,
        });
        self.line_x += w;
    }

    fn finish_patch(&mut self, p: &BlockPatch) {
        let h = self.y - p.y;
        let mut put = |i: usize, it: DisplayItem| self.items[p.idx + i] = it;
        if let Some(bg) = p.bg {
            put(0, DisplayItem::FillRect { x: p.x, y: p.y, w: p.w, h, color: bg });
        }
        if let Some((bw, col)) = p.border {
            put(1, DisplayItem::FillRect { x: p.x, y: p.y, w: p.w, h: bw, color: col });
            put(2, DisplayItem::FillRect { x: p.x, y: p.y + h - bw, w: p.w, h: bw, color: col });
            put(3, DisplayItem::FillRect { x: p.x, y: p.y, w: bw, h, color: col });
            put(4, DisplayItem::FillRect { x: p.x + p.w - bw, y: p.y, w: bw, h, color: col });
        }
    }

    fn inline(&mut self, node: NodeId, tag: &str, c: &HashMap<String, String>, mut st: Style) {
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
            "img" => {
                self.flush_line();
                let w = self.doc.attr(node, "width")
                    .and_then(|v| v.trim_end_matches("px").parse().ok())
                    .unwrap_or(64.0f32);
                let h = self.doc.attr(node, "height")
                    .and_then(|v| v.trim_end_matches("px").parse().ok())
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
            "input" | "textarea" | "select" | "button" => {
                self.word("[control]", st.clone());
            }
            _ => {
                for &ch in self.doc.children(node) {
                    self.walk(ch, st.clone());
                }
            }
        }
        if mr > 0.0 {
            self.spacer(mr);
        }
    }

    fn text(&mut self, text: &str, st: Style) {
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
                    self.word_with_space(word, st.clone());
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
            self.word_with_space(word, st.clone());
            first = false;
        }
        self.pending_space = text.chars().last().is_some_and(|c| c.is_whitespace());
    }

    fn resolve_font(&mut self, st: &Style) -> Option<usize> {
        self.fonts
            .resolve(&st.family, st.bold, st.italic)
            .or_else(|| self.fonts.resolve("liberation sans", false, false))
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

    fn word_with_space(&mut self, word: &str, st: Style) {
        let Some(font) = self.resolve_font_covering(word, &st) else { return };
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
        self.push_word(word, st.clone(), font);
    }

    fn word(&mut self, word: &str, st: Style) {
        if let Some(font) = self.resolve_font(&st) {
            self.push_word(word, st.clone(), font);
        }
    }

    fn push_word(&mut self, word: &str, st: Style, font: usize) {
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
        Layouter::layout(&d, &mut fonts, 800.0, 0.0, &[])
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
        let (items, _, _) = Layouter::layout(&d, &mut fonts, 800.0, 0.0, &[sh2]);
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
