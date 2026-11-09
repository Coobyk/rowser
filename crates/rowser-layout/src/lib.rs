//! rowser-layout: DOM → display list.
//!
//! M1 uses a hardcoded UA stylesheet (the real CSS engine lands in M2):
//! block/inline display, font sizes/weights per tag, margins, list markers,
//! `<pre>` whitespace preservation, `<br>`, `<hr>`, underline/strike-through.
//!
//! Inline layout: text is split into words; a word that overflows the
//! content width starts a new line. Lines take the max ascent of their
//! runs. Margin collapsing is approximated (margins simply add).

use rowser_dom::{Document, NodeId, NodeKind};
use rowser_text::FontCache;

/// BGRA pixel.
pub type Color = u32;
pub const fn bgra(r: u8, g: u8, b: u8) -> Color {
    u32::from_le_bytes([b, g, r, 255])
}

pub const BLACK: Color = bgra(0x11, 0x11, 0x11);
pub const WHITE: Color = bgra(0xff, 0xff, 0xff);
pub const LINK: Color = bgra(0x1a, 0x5f, 0xd4);
pub const RULE: Color = bgra(0x99, 0x99, 0x99);

#[derive(Debug, Clone)]
pub enum DisplayItem {
    /// A text run: draw `text` at (x, baseline) with font `font` at `size`.
    Text {
        x: f32,
        baseline: f32,
        text: String,
        font: usize,
        size: f32,
        color: Color,
        underline: bool,
        strike: bool,
    },
    FillRect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        color: Color,
    },
}

#[derive(Clone, Copy)]
struct Style {
    size: f32,
    bold: bool,
    italic: bool,
    family: Family,
    color: Color,
    underline: bool,
    strike: bool,
    /// baseline shift in ems (sub/sup)
    shift: f32,
    pre: bool,
}

#[derive(Clone, Copy, PartialEq)]
#[allow(dead_code)] // Serif is unused until CSS font-family lands
enum Family {
    Sans,
    Serif, // used once CSS font-family lands (M2)
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
    }
}

/// display + margin info for a block-level element
struct BlockSpec {
    display_none: bool,
    margin_top: f32,
    margin_bottom: f32,
    indent: f32,
}

/// Per-tag UA style: (block?, margin top/bottom em, indent px, style tweaks).
fn tag_spec(tag: &str) -> (bool, BlockSpec) {
    let none = BlockSpec {
        display_none: true,
        margin_top: 0.0,
        margin_bottom: 0.0,
        indent: 0.0,
    };
    let block = |mt: f32, mb: f32, indent: f32| {
        (
            true,
            BlockSpec {
                display_none: false,
                margin_top: mt,
                margin_bottom: mb,
                indent,
            },
        )
    };
    match tag {
        "head" | "title" | "meta" | "link" | "style" | "script" | "template" | "noscript"
        | "base" => (true, none),
        "html" | "body" => block(0.0, 0.0, 0.0),
        "p" => block(1.0, 1.0, 0.0),
        "div" | "section" | "article" | "nav" | "header" | "footer" | "main" | "aside"
        | "form" | "fieldset" | "figcaption" | "figure" | "address" => block(0.0, 0.0, 0.0),
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => block(0.67, 0.67, 0.0),
        "ul" | "ol" | "menu" | "dir" => block(1.0, 1.0, 40.0),
        "dl" => block(1.0, 1.0, 0.0),
        "dd" => block(0.0, 0.0, 40.0),
        "dt" => block(0.0, 0.0, 0.0),
        "li" => block(0.0, 0.0, 0.0),
        "blockquote" => block(1.0, 1.0, 40.0),
        "pre" | "listing" => block(1.0, 1.0, 0.0),
        "table" | "tr" | "tbody" | "thead" | "tfoot" | "caption" => block(0.0, 0.0, 0.0),
        "td" | "th" => (false, BlockSpec { display_none: false, ..none }),
        "hr" => block(0.5, 0.5, 0.0),
        "center" => block(0.0, 0.0, 0.0),
        _ => (false, BlockSpec { display_none: false, ..none }),
    }
}

fn apply_inline(tag: &str, st: &mut Style, list_marker: &mut Option<String>, ordered: bool, li_num: usize) {
    match tag {
        "h1" => {
            st.size = BASE_SIZE * 2.0;
            st.bold = true;
        }
        "h2" => {
            st.size = BASE_SIZE * 1.5;
            st.bold = true;
        }
        "h3" => {
            st.size = BASE_SIZE * 1.17;
            st.bold = true;
        }
        "h4" => {
            st.size = BASE_SIZE;
            st.bold = true;
        }
        "h5" => {
            st.size = BASE_SIZE * 0.83;
            st.bold = true;
        }
        "h6" => {
            st.size = BASE_SIZE * 0.67;
            st.bold = true;
        }
        "b" | "strong" | "th" => st.bold = true,
        "i" | "em" | "dfn" | "var" | "cite" | "address" => st.italic = true,
        "code" | "kbd" | "samp" | "tt" | "pre" | "listing" => st.family = Family::Mono,
        "a" => {
            st.color = LINK;
            st.underline = true;
        }
        "u" | "ins" => st.underline = true,
        "s" | "strike" | "del" => st.strike = true,
        "small" => st.size *= 0.83,
        "big" => st.size *= 1.17,
        "sub" => {
            st.size *= 0.8;
            st.shift = -0.25;
        }
        "sup" => {
            st.size *= 0.8;
            st.shift = 0.35;
        }
        "li" => {
            *list_marker = Some(if ordered {
                format!("{li_num}. ")
            } else {
                "•  ".to_string()
            });
        }
        _ => {}
    }
    if tag == "pre" || tag == "listing" {
        st.pre = true;
    }
    if tag == "code" || tag == "kbd" || tag == "samp" {
        // subtle affordance: inline code gets a gray tone
        st.color = bgra(0x33, 0x33, 0x66);
    }
}

/// One word waiting to be placed on the current line.
struct Word {
    text: String,
    font: usize,
    size: f32,
    color: Color,
    underline: bool,
    strike: bool,
    shift: f32,
    width: f32,
    /// ascent/line-height at this word's size
    ascent: f32,
    line_h: f32,
}

pub struct Layouter<'a> {
    doc: &'a Document,
    fonts: &'a mut FontCache,
    width: f32,
    pub items: Vec<DisplayItem>,
    y: f32,
    margin_x: f32,
    line: Vec<Word>,
    line_x: f32,
    pending_space: bool,
    list_stack: Vec<(bool, usize)>,
    pub height: f32,
}

impl<'a> Layouter<'a> {
    /// Layout `doc` into a display list `width` px wide, starting at (8,8).
    pub fn layout(
        doc: &'a Document,
        fonts: &'a mut FontCache,
        width: f32,
        scroll: f32,
    ) -> (Vec<DisplayItem>, f32) {
        let mut l = Layouter {
            doc,
            fonts,
            width,
            items: Vec::new(),
            y: 8.0 - scroll,
            margin_x: 8.0,
            line: Vec::new(),
            line_x: 0.0,
            pending_space: false,
            list_stack: Vec::new(),
            height: 0.0,
        };
        l.walk(doc.root, base_style());
        l.flush_line();
        l.height = l.y + scroll + 8.0;
        (l.items, l.height)
    }

    fn content_width(&self) -> f32 {
        (self.width - self.margin_x * 2.0).max(60.0)
    }

    fn walk(&mut self, node: NodeId, style: Style) {
        match self.doc.kind(node) {
            NodeKind::Document => {
                for &c in self.doc.children(node) {
                    self.walk(c, style);
                }
            }
            NodeKind::Text(t) => self.text(t, style),
            NodeKind::Comment(_) => {}
            NodeKind::Element { tag, .. } => {
                let (is_block, spec) = tag_spec(tag);
                if spec.display_none {
                    return;
                }
                let mut st = style;
                let mut marker = None;
                let ordered = self
                    .list_stack
                    .last()
                    .map(|(o, _)| *o)
                    .unwrap_or(false);
                // <ol> counters increment only on <li>
                let li_num = if tag == "li" {
                    match self.list_stack.last_mut() {
                        Some((_, n)) => {
                            *n += 1;
                            *n
                        }
                        None => 1,
                    }
                } else {
                    0
                };
                apply_inline(tag, &mut st, &mut marker, ordered, li_num);

                match tag.as_str() {
                    "ul" | "menu" | "dir" => self.list_stack.push((false, 0)),
                    "ol" => self.list_stack.push((true, 0)),
                    _ => {}
                }

                if is_block {
                    self.flush_line();
                    self.y += spec.margin_top * st.size;
                    let saved_mx = self.margin_x;
                    self.margin_x += spec.indent;
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
                            self.word(&m, st);
                        }
                        for &c in self.doc.children(node) {
                            self.walk(c, st);
                        }
                        self.flush_line();
                    }
                    self.margin_x = saved_mx;
                    self.y += spec.margin_bottom * st.size;
                } else {
                    // inline element — keep flowing, styles already applied
                    if let Some(m) = marker {
                        self.word(&m, st);
                    }
                    match tag.as_str() {
                        "br" => self.flush_line(),
                        "img" | "input" => {
                            let alt = self
                                .doc
                                .attr(node, "alt")
                                .map(|a| format!("[{a}]"))
                                .unwrap_or_else(|| "[image]".into());
                            self.word(&alt, st);
                        }
                        _ => {
                            for &c in self.doc.children(node) {
                                self.walk(c, st);
                            }
                        }
                    }
                }
                if tag == "ul" || tag == "ol" || tag == "menu" || tag == "dir" {
                    self.list_stack.pop();
                }
            }
        }
    }

    /// Add a piece of text as inline words.
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
                    self.word_with_space(word, st);
                    self.pending_space = true;
                }
                self.pending_space = false;
            }
            return;
        }
        // collapse whitespace to single spaces. A space belongs before a
        // word only when real whitespace preceded it.
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

    fn word_with_space(&mut self, word: &str, st: Style) {
        let font = self
            .fonts
            .resolve(st.family.name(), st.bold, st.italic)
            .or_else(|| self.fonts.resolve("liberation sans", false, false));
        let Some(font) = font else { return };
        let space_w = self
            .fonts
            .font(font)
            .map(|f| f.advance_units(f.glyph_id(' ')) as f32 * st.size / f.units_per_em() as f32)
            .unwrap_or(st.size * 0.3);
        if self.pending_space && !self.line.is_empty() {
            self.line_x += space_w;
            // the space belongs to the previous word's advance so the
            // emitted positions include it
            if let Some(last) = self.line.last_mut() {
                last.width += space_w;
            }
        }
        self.pending_space = false;
        self.push_word(word, st, font, space_w);
    }

    fn word(&mut self, word: &str, st: Style) {
        let font = self
            .fonts
            .resolve(st.family.name(), st.bold, st.italic)
            .or_else(|| self.fonts.resolve("liberation sans", false, false));
        if let Some(font) = font {
            self.push_word(word, st, font, 0.0);
        }
    }

    fn push_word(&mut self, word: &str, st: Style, font: usize, _space_w: f32) {
        let (w, ascent, line_h) = {
            let f = match self.fonts.font(font) {
                Some(f) => f,
                None => return,
            };
            let upem = f.units_per_em() as f32;
            (
                f.measure(word, st.size),
                f.ascender as f32 * st.size / upem,
                f.line_height(st.size),
            )
        };
        // wrap if this word doesn't fit
        if self.line_x + w > self.content_width() && !self.line.is_empty() {
            self.flush_line();
        }
        // word longer than a whole line: split it mid-word
        if w > self.content_width() && self.line.is_empty() {
            let mut chunk = String::new();
            let mut cw = 0.0;
            let mut it = word.chars().peekable();
            while let Some(c) = it.next() {
                let fw = self
                    .fonts
                    .font(font)
                    .map(|f| f.advance_units(f.glyph_id(c)) as f32 * st.size / f.units_per_em() as f32)
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
            width: w,
            ascent,
            line_h,
        });
        self.line_x += w;
    }

    /// Emit the current line: assign positions, advance y.
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
            .max(self.line.iter().map(|w| w.ascent + w.size * 0.25).fold(0.0, f32::max));
        let max_ascent = self.line.iter().map(|w| w.ascent).fold(0.0f32, f32::max);
        let baseline = self.y + max_ascent;
        let mut x = self.margin_x;
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

    #[test]
    fn produces_text_items() {
        let d = doc("<p>hello world</p>");
        let mut fonts = FontCache::new();
        let (items, h) = Layouter::layout(&d, &mut fonts, 800.0, 0.0);
        assert!(!items.is_empty());
        assert!(h > 10.0);
        assert!(matches!(items[0], DisplayItem::Text { .. }));
    }

    #[test]
    fn wraps_long_lines() {
        let long = "word ".repeat(500);
        let d = doc(&format!("<p>{long}</p>"));
        let mut fonts = FontCache::new();
        let (items, h) = Layouter::layout(&d, &mut fonts, 200.0, 0.0);
        // narrow viewport → many lines → tall
        assert!(h > 500.0, "height {h}");
        let _ = items;
    }

    #[test]
    fn headings_bigger() {
        let d = doc("<h1>big</h1><p>small</p>");
        let mut fonts = FontCache::new();
        let (items, _) = Layouter::layout(&d, &mut fonts, 800.0, 0.0);
        let h1_size = match &items[0] {
            DisplayItem::Text { size, .. } => *size,
            _ => panic!(),
        };
        assert!(h1_size > 30.0);
    }

    #[test]
    fn skips_head() {
        let d = doc("<head><title>x</title></head><body>visible</body>");
        let mut fonts = FontCache::new();
        let (items, _) = Layouter::layout(&d, &mut fonts, 800.0, 0.0);
        let all: String = items
            .iter()
            .filter_map(|i| match i {
                DisplayItem::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(!all.contains('x'), "{all}");
        assert!(all.contains("visible"));
    }
}
