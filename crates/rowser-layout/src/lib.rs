//! rowser-layout: DOM + CSS → display list.
//!
//! Styles come from the real cascade (rowser-css): a built-in user-agent
//! stylesheet plus any <style> sheets in the document. Supports: display
//! (none/block/inline), color, background-color, font-size/-weight/-style/
//! -family, text-decoration, text-align, white-space:pre, margin/padding
//! shorthand + sides, simple `border` shorthand.

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
    family: Family,
    color: Color,
    underline: bool,
    strike: bool,
    shift: f32,
    pre: bool,
    align: Align,
    href: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum Family {
    Sans,
    Serif,
    Mono,
}

impl Family {
    fn name(self) -> &'static str {
        match self {
            Self::Sans => "liberation sans",
            Self::Serif => "liberation serif",
            Self::Mono => "liberation mono",
        }
    }
}

const BASE_SIZE: f32 = 16.0;

fn base_style() -> Style {
    Style {
        size: BASE_SIZE,
        bold: false,
        italic: false,
        family: Family::Sans,
        color: BLACK,
        underline: false,
        strike: false,
        shift: 0.0,
        pre: false,
        align: Align::Left,
        href: None,
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
    if let Some(hex) = v.strip_prefix('#') {
        let d = u32::from_str_radix(hex, 16).ok()?;
        return Some(match hex.len() {
            3 => {
                let (r, g, b) = ((d >> 8) & 0xf, (d >> 4) & 0xf, d & 0xf);
                bgra((r * 17) as u8, (g * 17) as u8, (b * 17) as u8)
            }
            6 => bgra(((d >> 16) & 0xff) as u8, ((d >> 8) & 0xff) as u8, (d & 0xff) as u8),
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

/// CSS length → px. `em`/`%` resolve against `rel` (usually parent font size).
fn parse_len(v: &str, rel: f32) -> Option<f32> {
    let v = v.trim().to_lowercase();
    let num = |s: &str| s.trim().parse::<f32>().ok();
    if let Some(x) = v.strip_suffix("px") {
        num(x)
    } else if let Some(x) = v.strip_suffix("em") {
        num(x).map(|n| n * rel)
    } else if let Some(x) = v.strip_suffix('%') {
        num(x).map(|n| n * rel / 100.0)
    } else if let Some(x) = v.strip_suffix("pt") {
        num(x).map(|n| n * 96.0 / 72.0)
    } else if let Some(x) = v.strip_suffix("rem") {
        num(x).map(|n| n * BASE_SIZE)
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
    width: f32,
    items: Vec<DisplayItem>,
    y: f32,
    margin_x: f32,
    align: Align,
    line: Vec<Word>,
    line_x: f32,
    pending_space: bool,
    list_stack: Vec<(bool, usize)>,
}

impl<'a> Layouter<'a> {
    /// Layout `doc` into a display list `width` px wide. `scroll` shifts the
    /// document upward; returns (items, total document height).
    pub fn layout(
        doc: &'a Document,
        fonts: &'a mut FontCache,
        width: f32,
        scroll: f32,
    ) -> (Vec<DisplayItem>, f32) {
        let author = css::document_sheets(doc);
        let mut sheets: Vec<&css::StyleSheet> = vec![ua_sheet()];
        sheets.extend(author.iter());
        let mut l = Layouter {
            doc,
            sheets,
            fonts,
            width,
            items: Vec::new(),
            y: 8.0 - scroll,
            margin_x: 8.0,
            align: Align::Left,
            line: Vec::new(),
            line_x: 0.0,
            pending_space: false,
            list_stack: Vec::new(),
        };
        l.walk(doc.root, base_style());
        l.flush_line();
        let h = l.y + scroll + 8.0;
        (l.items, h)
    }

    fn content_width(&self) -> f32 {
        (self.width - self.margin_x * 2.0).max(60.0)
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
                let c = css::computed(self.doc, node, &self.sheets);
                let mut st = style.clone();
                self.apply_computed(&c, &mut st);
                match c.get("display").map(String::as_str) {
                    Some("none") => return,
                    Some("block") | Some("list-item") => {
                        self.block(node, tag, &c, st);
                    }
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
            st.family = if first.contains("mono") {
                Family::Mono
            } else if first.contains("serif") && !first.contains("sans") {
                Family::Serif
            } else {
                Family::Sans
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
    }

    fn block(&mut self, node: NodeId, tag: &str, c: &HashMap<String, String>, st: Style) {
        self.flush_line();
        let mt = side(c, "margin-top", st.size)
            .or_else(|| sides(c, "margin", st.size)[0])
            .unwrap_or(0.0);
        let mb = side(c, "margin-bottom", st.size)
            .or_else(|| sides(c, "margin", st.size)[2])
            .unwrap_or(0.0);
        let ml = side(c, "margin-left", st.size)
            .or_else(|| sides(c, "margin", st.size)[3])
            .unwrap_or(0.0);
        let pl = side(c, "padding-left", st.size)
            .or_else(|| sides(c, "padding", st.size)[3])
            .unwrap_or(0.0);
        let bg = c.get("background-color").and_then(|v| parse_color(v));
        let border = c.get("border").and_then(|v| parse_border(v, st.size));

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
        let saved_align = self.align;
        self.margin_x += ml.max(0.0) + pl.max(0.0);
        self.align = st.align;

        // background/border need the final rect — reserve placeholders.
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
                x: self.margin_x - pl.max(0.0),
                y: self.y,
                w: self.width - (self.margin_x - pl.max(0.0)) * 2.0,
                bg,
                border,
            })
        } else {
            None
        };

        if tag == "hr" {
            self.items.push(DisplayItem::FillRect {
                x: self.margin_x,
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
            for &ch in self.doc.children(node) {
                self.walk(ch, st.clone());
            }
            self.flush_line();
        }

        if let Some(p) = patch {
            self.finish_patch(&p);
        }
        self.margin_x = saved_mx;
        self.align = saved_align;
        self.y += mb.max(0.0);
        if tag == "ul" || tag == "ol" || tag == "menu" || tag == "dir" {
            self.list_stack.pop();
        }
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

    fn inline(&mut self, node: NodeId, tag: &str, _c: &HashMap<String, String>, mut st: Style) {
        if tag == "a" {
            st.href = self.doc.attr(node, "href").map(String::from);
        }
        match tag {
            "br" => self.flush_line(),
            "img" | "input" | "textarea" | "select" | "button" => {
                let alt = self
                    .doc
                    .attr(node, "alt")
                    .map(|a| format!("[{a}]"))
                    .unwrap_or_else(|| "[image]".into());
                self.word(&alt, st.clone());
            }
            _ => {
                for &ch in self.doc.children(node) {
                    self.walk(ch, st.clone());
                }
            }
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
            .resolve(st.family.name(), st.bold, st.italic)
            .or_else(|| self.fonts.resolve("liberation sans", false, false))
    }

    fn word_with_space(&mut self, word: &str, st: Style) {
        let Some(font) = self.resolve_font(&st) else { return };
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
                f.line_height(st.size),
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
        let mut x = self.margin_x
            + match self.align {
                Align::Left => 0.0,
                Align::Center => ((self.content_width() - used) / 2.0).max(0.0),
                Align::Right => (self.content_width() - used).max(0.0),
            };
        for w in self.line.drain(..) {
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

    fn lay(html: &str) -> (Vec<DisplayItem>, f32) {
        let d = doc(html);
        let mut fonts = FontCache::new();
        Layouter::layout(&d, &mut fonts, 800.0, 0.0)
    }

    fn texts(items: &[DisplayItem]) -> Vec<&DisplayItem> {
        items
            .iter()
            .filter(|i| matches!(i, DisplayItem::Text { .. }))
            .collect()
    }

    #[test]
    fn css_color_applies() {
        let (items, _) = lay("<style>p{color:red}</style><p>hi</p>");
        let t = texts(&items);
        if let DisplayItem::Text { color, .. } = t[0] {
            assert_eq!(*color, bgra(255, 0, 0));
        }
    }

    #[test]
    fn class_selector_styles() {
        let (items, _) = lay(
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
        let (items, _) = lay("<style>.hide{display:none}</style><p class=hide>gone</p><p>here</p>");
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
        let (items, _) = lay("<style>div{background-color:#ff0000}</style><div>inside</div>");
        assert!(items
            .iter()
            .any(|i| matches!(i, DisplayItem::FillRect { color, .. } if *color == bgra(255,0,0))));
    }

    #[test]
    fn text_align_center() {
        let (items, _) =
            lay("<style>p{text-align:center}</style><p>mid</p>");
        let t = texts(&items);
        if let DisplayItem::Text { x, .. } = t[0] {
            assert!(*x > 200.0, "centered text should be right of center, x={x}");
        }
    }

    #[test]
    fn headings_bigger() {
        let (items, _) = lay("<h1>big</h1><p>small</p>");
        let t = texts(&items);
        if let DisplayItem::Text { size, .. } = t[0] {
            assert!(*size > 30.0);
        }
    }
}
