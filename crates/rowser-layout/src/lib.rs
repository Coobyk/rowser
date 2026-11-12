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
mod bgimg;
mod borderfx;
pub mod colorfx;
mod flexbox;
mod multicol;
mod stacking;

mod generated;
pub mod svg;

use borderfx::{Borders, BoxShadow, Outline, RadiusSpec};
pub mod xform;

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

/// Which box edge a `DisplayItem::BorderSide` paints — dashes run along
/// the edge's length, bevels shade by orientation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderEdge {
    Top,
    Right,
    Bottom,
    Left,
}

/// `border-style`/`outline-style` — everything but `none`/`hidden`
/// (those zero the side's width during resolution instead).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderStyle {
    Solid,
    Dotted,
    Dashed,
    Double,
    Groove,
    Ridge,
    Inset,
    Outset,
}

/// `text-decoration-line` rendering style.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DecoStyle {
    Solid,
    Double,
    Dotted,
    Dashed,
    Wavy,
}

/// Resolved `text-decoration` for a text run (line bits + paint params).
#[derive(Debug, Clone)]
pub struct TextDeco {
    /// bit0 underline, bit1 overline, bit2 line-through
    pub lines: u8,
    pub style: DecoStyle,
    /// explicit text-decoration-color; None → the run's own color
    pub color: Option<Color>,
    /// text-decoration-thickness in px; None → derived from size
    pub thick: Option<f32>,
    /// text-underline-offset — extra px below the auto position
    pub offset: Option<f32>,
}

impl TextDeco {
    pub const NONE: TextDeco = TextDeco {
        lines: 0,
        style: DecoStyle::Solid,
        color: None,
        thick: None,
        offset: None,
    };
    pub fn underline(&self) -> bool {
        self.lines & 1 != 0
    }
    pub fn strike(&self) -> bool {
        self.lines & 4 != 0
    }
}

#[derive(Debug, Clone)]
pub enum DisplayItem {
    Text {
        x: f32,
        baseline: f32,
        text: String,
        font: usize,
        size: f32,
        color: Color,
        deco: TextDeco,
        /// user-select — false suppresses selection rects for this word
        sel: bool,
        /// letter-spacing — extra advance after every glyph
        ls: f32,
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
        /// image-rendering: pixelated → nearest-neighbor sampling
        near: bool,
    },
    /// Begin an effect group (filter/clip/blend/backdrop): items until the
    /// matching `PopEffect` form this element's painted subtree.
    PushEffect(colorfx::Effect),
    /// Close the innermost `PushEffect` group.
    PopEffect,
    /// One border/outline edge — `x,y,w,h` is the edge's own rect; `side`
    /// orients dashes (top/bottom run along x) and bevel shading.
    BorderSide {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        side: BorderEdge,
        style: BorderStyle,
        color: Color,
    },
    /// Filled rect with per-corner elliptical radii — corners in
    /// [top-left, top-right, bottom-right, bottom-left] order as (rx, ry).
    RoundedRect2 {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radii: [[f32; 2]; 4],
        color: Color,
    },
    /// box-shadow: alpha mask of the box — spread-expanded, radii-adjusted,
    /// offset by (dx,dy) — blurred, then blended with `color`. `inset`
    /// draws inside the box; outer shadows knock out the box interior.
    Shadow {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        dx: f32,
        dy: f32,
        blur: f32,
        spread: f32,
        radii: [[f32; 2]; 4],
        color: Color,
        inset: bool,
    },
    /// Begin clipping subsequent items to this (optionally rounded) rect —
    /// pairs with `PopClip` (overflow:hidden|clip|scroll|auto).
    PushClip {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radii: [[f32; 2]; 4],
    },
    /// End the most recent `PushClip`.
    PopClip,
    /// `background-image` layer / `object-fit` image: the decoded bitmap
    /// `src` is tiled over the clip rect (x,y,w,h). Tile geometry resolves
    /// in paint once the natural size is known:
    ///  - (px,py,pw,ph): positioning area (background-origin/object box)
    ///  - mode: 0 = tile size (sw,sh; <0 = auto/natural), 1 = cover,
    ///    2 = contain, 3 = scale-down
    ///  - tile origin per axis = pa + f*(pa - tile) + a
    ///  - rx/ry: 0 no-repeat, 1 repeat, 2 space(≈repeat), 3 round
    BgImage {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        px: f32,
        py: f32,
        pw: f32,
        ph: f32,
        mode: u8,
        sw: f32,
        sh: f32,
        fx: f32,
        ax: f32,
        fy: f32,
        ay: f32,
        rx: u8,
        ry: u8,
        src: String,
        /// image-rendering: pixelated → nearest-neighbor sampling
        near: bool,
    },
    /// Begin a 2D affine transform (xform::Mat) over the items up to the
    /// matching PopTransform. Nests; matrices compose in paint order.
    /// Coords stay in pre-transform space — transforms are visual-only.
    PushTransform { m: xform::Mat },
    /// Close the innermost PushTransform.
    PopTransform,
}

/// Hit-test record: rect in PRE-transform (laid-out) coordinates plus the
/// accumulated transform mapping those coords to painted position
/// (identity for untransformed elements). Hit-testing inverse-maps the
/// click point through the matrix, so rotated elements stay exact.
pub type Hit = (f32, f32, f32, f32, NodeId, xform::Mat);

#[derive(Clone, Copy, PartialEq)]
pub enum Align {
    Left,
    Center,
    Right,
    /// inter-word space stretched so the line fills the block
    Justify,
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
    deco: TextDeco,
    /// vertical-align
    va: Va,
    /// letter-spacing / word-spacing additions in px
    ls: f32,
    ws: f32,
    pre: bool,
    /// white-space:pre-line — whitespace collapses but \n breaks
    pre_line: bool,
    /// white-space:nowrap (and pre) — lines never break; text overflows
    nowrap: bool,
    /// tab-size in space widths (pre text)
    tab: f32,
    /// font-variant:small-caps — lowercase runs render as scaled caps
    small_caps: bool,
    /// hyphens:none — disables &shy; break opportunities
    hyphens_none: bool,
    /// text-indent (inherited) applied to a block's first line
    indent: f32,
    /// text-align-last (inherited); None → same as text-align
    align_last: Option<Align>,
    /// text-shadow list: (dx, dy, blur, color)
    shadow: Vec<(f32, f32, f32, Color)>,
    /// direction:rtl — flips start/end alignment and word order
    rtl: bool,
    /// user-select:none — words from this element can't be selected
    no_select: bool,
    /// line-clamp/-webkit-line-clamp: 0 = unclamped
    line_clamp: i32,
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
    /// resolved numeric font-weight (100–900); `bold` stays as the ≥600
    /// mirror for callers still keyed on the bool
    weight: u16,
    /// font-stretch percentage for face matching (100 = normal width)
    stretch: u16,
    /// accumulated `position:relative` inline offsets — painted position
    /// shifts; flow geometry is unaffected
    rel_x: f32,
    rel_y: f32,
    /// deepest inline element producing the current words — stamped onto
    /// each word so hit-testing sees inline elements, not just blocks.
    hit_owner: Option<NodeId>,
    /// `pointer-events:none` in effect (inherited) — the element and its
    /// descendants contribute no hit rects; `pointer-events:auto` resets it.
    pe_none: bool,
}

#[derive(Clone, Copy)]
enum LineH {
    Mul(f32),
    Px(f32),
}

/// `vertical-align` beyond the plain baseline: `Shift` is a baseline
/// offset as a fraction of font-size (sub/super/length/percent fold in);
/// the rest are line-box alignments, applied at emit where line metrics
/// exist. Top/Bottom double as text-top/text-bottom approximations.
#[derive(Clone, Copy)]
enum Va {
    Baseline,
    Shift(f32),
    Top,
    Middle,
    Bottom,
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
        deco: TextDeco::NONE,
        va: Va::Baseline,
        ls: 0.0,
        ws: 0.0,
        pre: false,
        pre_line: false,
        nowrap: false,
        tab: 8.0,
        small_caps: false,
        hyphens_none: false,
        indent: 0.0,
        align_last: None,
        shadow: Vec::new(),
        rtl: false,
        no_select: false,
        line_clamp: 0,
        break_word: false,
        align: Align::Left,
        href: None,
        line_h: None,
        inline_bg: None,
        weight: 400,
        stretch: 100,
        rel_x: 0.0,
        rel_y: 0.0,
        hit_owner: None,
        pe_none: false,
    }
}

const UA_CSS: &str = r#"
head,title,meta,link,style,script,template,noscript,base{display:none}
html,body,div,p,blockquote,pre,listing,h1,h2,h3,h4,h5,h6,ul,ol,menu,dir,dl,dd,dt,
section,article,nav,header,footer,main,aside,form,fieldset,figcaption,figure,
address,hr,center{display:block}
li{display:list-item}
table{display:table}
tr{display:table-row;vertical-align:middle}
thead{display:table-header-group}
tbody{display:table-row-group}
tfoot{display:table-footer-group}
td,th{display:table-cell}
caption{display:table-caption;text-align:center}
col{display:table-column}
colgroup{display:table-column-group}
body{margin:8px}
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
li{display:list-item}
q::before{content:open-quote}
q::after{content:close-quote}
blockquote{margin-top:1em;margin-bottom:1em;padding-left:40px;padding-right:40px}
pre,listing{font-family:monospace;white-space:pre;margin-top:1em;margin-bottom:1em}
b,strong{font-weight:bold}
i,em,dfn,var,cite,address{font-style:italic}
code,kbd,samp,tt{font-family:monospace}
a{color:#0000ee;text-decoration:underline}
a[href],area[href]{cursor:pointer}
input,textarea{cursor:text}
u,ins{text-decoration:underline}
s,strike,del{text-decoration:line-through}
small{font-size:.83em}
big{font-size:1.17em}
sub{vertical-align:sub;font-size:.8em}
sup{vertical-align:super;font-size:.8em}
hr{margin-top:.5em;margin-bottom:.5em}
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
        "darkgray" | "darkgrey" => bgra(169, 169, 169),
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
        "aliceblue" => bgra(240, 248, 255),
        "antiquewhite" => bgra(250, 235, 215),
        "aquamarine" => bgra(127, 255, 212),
        "azure" => bgra(240, 255, 255),
        "bisque" => bgra(255, 228, 196),
        "blanchedalmond" => bgra(255, 235, 205),
        "blueviolet" => bgra(138, 43, 226),
        "burlywood" => bgra(222, 184, 135),
        "cadetblue" => bgra(95, 158, 160),
        "chartreuse" => bgra(127, 255, 0),
        "chocolate" => bgra(210, 105, 30),
        "cornsilk" => bgra(255, 248, 220),
        "darkgoldenrod" => bgra(184, 134, 11),
        "darkkhaki" => bgra(189, 183, 107),
        "darkolivegreen" => bgra(85, 107, 47),
        "darksalmon" => bgra(233, 150, 122),
        "darkseagreen" => bgra(143, 188, 143),
        "darkslateblue" => bgra(72, 61, 139),
        "darkturquoise" => bgra(0, 206, 209),
        "darkviolet" => bgra(148, 0, 211),
        "deeppink" => bgra(255, 20, 147),
        "floralwhite" => bgra(255, 250, 240),
        "gainsboro" => bgra(220, 220, 220),
        "ghostwhite" => bgra(248, 248, 255),
        "goldenrod" => bgra(218, 165, 32),
        "greenyellow" => bgra(173, 255, 47),
        "honeydew" => bgra(240, 255, 240),
        "indianred" => bgra(205, 92, 92),
        "lavender" => bgra(230, 230, 250),
        "lavenderblush" => bgra(255, 240, 245),
        "lawngreen" => bgra(124, 252, 0),
        "lemonchiffon" => bgra(255, 250, 205),
        "lightcoral" => bgra(240, 128, 128),
        "lightcyan" => bgra(224, 255, 255),
        "lightgoldenrodyellow" => bgra(250, 250, 210),
        "lightpink" => bgra(255, 182, 193),
        "lightsalmon" => bgra(255, 160, 122),
        "lightseagreen" => bgra(32, 178, 170),
        "lightskyblue" => bgra(135, 206, 250),
        "lightslategray" | "lightslategrey" => bgra(119, 136, 153),
        "lightsteelblue" => bgra(176, 196, 222),
        "lightyellow" => bgra(255, 255, 224),
        "limegreen" => bgra(50, 205, 50),
        "linen" => bgra(250, 240, 230),
        "mediumaquamarine" => bgra(102, 205, 170),
        "mediumblue" => bgra(0, 0, 205),
        "mediumorchid" => bgra(186, 85, 211),
        "mediumseagreen" => bgra(60, 179, 113),
        "mediumslateblue" => bgra(123, 104, 238),
        "mediumspringgreen" => bgra(0, 250, 154),
        "mediumturquoise" => bgra(72, 209, 204),
        "mediumvioletred" => bgra(199, 21, 133),
        "mintcream" => bgra(245, 255, 250),
        "mistyrose" => bgra(255, 228, 225),
        "moccasin" => bgra(255, 228, 181),
        "navajowhite" => bgra(255, 222, 173),
        "oldlace" => bgra(253, 245, 230),
        "olivedrab" => bgra(107, 142, 35),
        "orangered" => bgra(255, 69, 0),
        "palegoldenrod" => bgra(238, 232, 170),
        "palegreen" => bgra(152, 251, 152),
        "paleturquoise" => bgra(175, 238, 238),
        "palevioletred" => bgra(219, 112, 147),
        "papayawhip" => bgra(255, 239, 213),
        "peachpuff" => bgra(255, 218, 185),
        "peru" => bgra(205, 133, 63),
        "powderblue" => bgra(176, 224, 230),
        "rosybrown" => bgra(188, 143, 143),
        "saddlebrown" => bgra(139, 69, 19),
        "sandybrown" => bgra(244, 164, 96),
        "seashell" => bgra(255, 245, 238),
        "slateblue" => bgra(106, 90, 205),
        "snow" => bgra(255, 250, 250),
        "thistle" => bgra(216, 191, 216),
        "wheat" => bgra(245, 222, 179),
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
    for (suf, k) in [
        ("grad", 0.9f32),
        ("turn", 360.0),
        ("deg", 1.0),
        ("rad", 57.295779513),
    ] {
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
/// 8-bit-free sRGB component (0..1) → linear-light.
fn srgb_un(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
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
    parse_color_ctx(v, None)
}

/// `parse_color` with a `currentColor` context: `cur` = the element's own
/// resolved color (the inherited color when resolving `color` itself).
pub fn parse_color_ctx(v: &str, cur: Option<Color>) -> Option<Color> {
    let v = v.trim().to_lowercase();
    if v == "transparent" {
        return None;
    }
    if v == "currentcolor" {
        return cur;
    }
    if let Some(inner) = v
        .strip_prefix("light-dark(")
        .and_then(|s| s.strip_suffix(')'))
    {
        // light mode only — take the first argument
        let (a, _) = top_comma(inner).unwrap_or((inner, ""));
        return parse_color_ctx(a.trim(), cur);
    }
    if v.starts_with("color-mix(") {
        let inner = v[v.find('(')? + 1..v.rfind(')')?].trim();
        // `in <space>,` prelude optional
        let mut space = "srgb";
        let inner = match top_comma(inner) {
            Some((pre, rest)) if pre.trim_start().starts_with("in ") => {
                space = pre.trim_start()[3..].trim();
                rest.trim()
            }
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
                        parse_color_ctx(col, cur)?
                    };
                    return Some((c, p / 100.0));
                }
            }
            Some((parse_color_ctx(s, cur)?, -1.0))
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
        let (na, nb) = if sum > 1.0 {
            (wa / sum, wb / sum)
        } else {
            (wa, wb)
        };
        if !matches!(space, "" | "srgb") {
            return colorfx::color_mix(space, ca, cb, na, nb);
        }
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
                let (r, g, b, a) = ((d >> 12) & 0xf, (d >> 8) & 0xf, (d >> 4) & 0xf, d & 0xf);
                bgra_a(
                    (r * 17) as u8,
                    (g * 17) as u8,
                    (b * 17) as u8,
                    (a * 17) as u8,
                )
            }
            6 => bgra(
                ((d >> 16) & 0xff) as u8,
                ((d >> 8) & 0xff) as u8,
                (d & 0xff) as u8,
            ),
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
        let fa = |i: usize| args.get(i).map(|s| falpha(s)).unwrap_or(1.0);
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
            "color" => {
                // color(<space> c1 c2 c3 [/ a]) — components 0..1
                if args.len() < 4 {
                    return None;
                }
                let cp = |s: &str| fpct(s, 1.0);
                let (x, y, z) = (cp(arg(1)), cp(arg(2)), cp(arg(3)));
                let (r, g, b) = match arg(0) {
                    "srgb" => return Some(fn_color_srgb(x, y, z, fa(4))),
                    "srgb-linear" => (x, y, z),
                    "display-p3" => {
                        // P3 shares the sRGB transfer curve; D65 → D65
                        let (x, y, z) = (srgb_un(x), srgb_un(y), srgb_un(z));
                        (
                            1.2249401762 * x - 0.2249401762 * y,
                            -0.0420569547 * x + 1.0420569547 * y,
                            -0.0196375546 * x - 0.0786360461 * y + 1.0982736007 * z,
                        )
                    }
                    "xyz" | "xyz-d65" => (
                        3.2409699419 * x - 1.5373831776 * y - 0.4986107603 * z,
                        -0.9692436363 * x + 1.8759675015 * y + 0.0415550574 * z,
                        0.0556300797 * x - 0.2039769589 * y + 1.0569715142 * z,
                    ),
                    "xyz-d50" => (
                        3.1341858371 * x - 1.6172094591 * y - 0.4906945842 * z,
                        -0.9787487286 * x + 1.9161301227 * y + 0.0334332507 * z,
                        0.0719529066 * x - 0.2289938547 * y + 1.4053853164 * z,
                    ),
                    _ => return None,
                };
                return Some(fn_color(r, g, b, fa(4)));
            }
            _ => return named_color(&v),
        }
    }
    named_color(&v)
}

impl Layouter<'_> {
    /// `background-color`/`background` or the legacy `bgcolor` attribute.
    /// `cur` = element's own color (for `currentcolor`).
    fn bg_of(&self, node: NodeId, c: &Props, cur: Option<Color>) -> Option<Color> {
        bg_color(c, cur).or_else(|| {
            self.doc
                .attr(node, "bgcolor")
                .and_then(|v| parse_color_ctx(v, cur))
        })
    }
}

/// `background-color` or a color inside the `background` shorthand.
fn bg_color(c: &Props, cur: Option<Color>) -> Option<Color> {
    if let Some(v) = c.get("background-color") {
        if let Some(col) = parse_color_ctx(v, cur) {
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
            if let Some(col) = parse_color_ctx(w, cur) {
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
        if let Some(col) = parse_color_ctx(tok.trim_end_matches(';'), cur) {
            return Some(col);
        }
    }
    None
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
            } else if c.is_ascii_digit()
                || c == '.'
                || (c == '-' && i + 1 < b.len() && b[i + 1].is_ascii_digit())
            {
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
    let mut p = P {
        t,
        i: 0,
        rel,
        em,
        _m: std::marker::PhantomData,
    };
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
                if s[..i].trim().is_empty() || s[..i].trim_end().ends_with(['+', '-', '*', '/']) {
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

/// `text-shadow: dx dy [blur] [color] [, ...]` → (dx, dy, blur, color)
/// tuples. `none` yields an empty list; an absent color inherits the
/// run's `color` at parse time (close enough — currentColor is the rule).
fn parse_text_shadow(v: &str, size: f32, cur: Color) -> Vec<(f32, f32, f32, Color)> {
    if v.trim() == "none" {
        return Vec::new();
    }
    let mut out = Vec::new();
    for part in v.split(',') {
        let (mut dx, mut dy, mut blur) = (0.0f32, 0.0f32, 0.0f32);
        let mut color = cur;
        let mut n_len = 0;
        for tok in part.trim().split_whitespace() {
            if let Some(px) = parse_len(tok, size, size) {
                match n_len {
                    0 => dx = px,
                    1 => dy = px,
                    _ => blur = px,
                }
                n_len += 1;
            } else if let Some(c) = parse_color_ctx(tok, Some(cur)) {
                color = c;
            }
        }
        if n_len >= 2 {
            out.push((dx, dy, blur.max(0.0), color));
        }
    }
    out
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

/// font-weight → 100–900; `bolder`/`lighter` step from the parent's
/// resolved weight (CSS Fonts 4 coarse mapping).
fn font_weight_value(v: &str, parent: u16) -> u16 {
    match v.trim().to_lowercase().as_str() {
        "normal" => 400,
        "bold" => 700,
        "bolder" => match parent {
            ..=349 => 400,
            350..=549 => 700,
            _ => 900,
        },
        "lighter" => match parent {
            ..=549 => 100,
            550..=749 => 400,
            _ => 700,
        },
        other => other
            .parse::<u16>()
            .map(|n| n.clamp(1, 1000))
            .unwrap_or(400),
    }
}

/// font-stretch keyword or `n%` → percentage for face scoring.
fn font_stretch_value(v: &str) -> u16 {
    match v.trim().to_lowercase().as_str() {
        "ultra-condensed" => 50,
        "extra-condensed" => 63,
        "condensed" => 75,
        "semi-condensed" => 88,
        "normal" => 100,
        "semi-expanded" => 113,
        "expanded" => 125,
        "extra-expanded" => 150,
        "ultra-expanded" => 200,
        other => other.trim_end_matches('%').parse::<u16>().unwrap_or(100),
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

/// `dim` for heights — a %-value with no definite containing-block height
/// (`None` base) computes to `auto` in normal flow (CSS2.1 §10.5: an auto
/// CB height makes % heights evaluate to `auto`).
fn hdim_pct(c: &Props, name: &str, hrel: f32, em: f32, base: Option<f32>) -> Option<f32> {
    if base.is_none() && c.get(name).is_some_and(|v| v.contains('%')) {
        None
    } else {
        dim(c, name, hrel, em)
    }
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
    c.get("box-sizing")
        .is_some_and(|v| v.trim() == "border-box")
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
                side(c, &format!("{name}-top"), rel, em)
                    .or(sh[0])
                    .unwrap_or(0.0),
                side(c, &format!("{name}-right"), rel, em)
                    .or(sh[1])
                    .unwrap_or(0.0),
                side(c, &format!("{name}-bottom"), rel, em)
                    .or(sh[2])
                    .unwrap_or(0.0),
                side(c, &format!("{name}-left"), rel, em)
                    .or(sh[3])
                    .unwrap_or(0.0),
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
    if c.get(long)
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("auto"))
    {
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
    /// Intrinsic-breadth track (auto / minmax / fit-content / min-content /
    /// max-content). `min`/`max` are breadth kinds — 0 none, 1 min-content,
    /// 2 max-content; `cap` clamps growth (fit-content limit, minmax max);
    /// `stretch` marks an `auto` max — it shares container free space when
    /// *-content is normal|stretch.
    C {
        floor: f32,
        min: u8,
        max: u8,
        cap: f32,
        stretch: bool,
    },
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
        GTrack::C { floor, .. } => floor,
    }
}

/// Normalize a track into sizing semantics: (base floor, min-breadth kind,
/// growth kind, growth cap, stretchable). Kinds: 0 none, 1 min-content,
/// 2 max-content contribution. `Auto` is the legacy [min,max] clamp — an
/// uncapped max reads as an `auto` max (stretchable).
fn track_sem(t: &GTrack) -> (f32, u8, u8, f32, bool) {
    match *t {
        GTrack::Fixed(x) => (x, 0, 0, f32::MAX, false),
        GTrack::Fr(_, m) => (m, 0, 0, f32::MAX, false),
        GTrack::Auto(m, mx) => (m, 0, 2, mx, mx >= f32::MAX),
        GTrack::C {
            floor,
            min,
            max,
            cap,
            stretch,
        } => (floor, min, max, cap, stretch),
    }
}

/// One non-repeat track token → GTrack. `rel` is the %/em base (axis size).
fn track_spec(t: &str, rel: f32, em: f32, vh: f32, vw: f32) -> GTrack {
    let t = t.trim().to_lowercase();
    if let Some(f) = t.strip_suffix("fr") {
        return GTrack::Fr(f.trim().parse::<f32>().unwrap_or(1.0).max(0.0), 0.0);
    }
    match t.as_str() {
        // auto ≡ minmax(auto, auto): ~min-content floor, max-content max,
        // and it stretches to share container free space
        "auto" | "masonry" => {
            return GTrack::C {
                floor: 0.0,
                min: 1,
                max: 2,
                cap: f32::MAX,
                stretch: true,
            };
        }
        "min-content" => {
            return GTrack::C {
                floor: 0.0,
                min: 1,
                max: 1,
                cap: f32::MAX,
                stretch: false,
            };
        }
        "max-content" => {
            return GTrack::C {
                floor: 0.0,
                min: 2,
                max: 2,
                cap: f32::MAX,
                stretch: false,
            };
        }
        _ => {}
    }
    if let Some(inner) = t.strip_prefix("minmax(").and_then(|x| x.strip_suffix(')')) {
        let (a, b) = top_comma(inner).unwrap_or((inner, "auto"));
        let (floor, min) = match a.trim() {
            "auto" | "min-content" => (0.0, 1),
            "max-content" => (0.0, 2),
            a => (parse_len_vp(a, rel, em, vh, vw).unwrap_or(0.0), 0),
        };
        let b = b.trim();
        if let Some(f) = b.strip_suffix("fr") {
            return GTrack::Fr(f.trim().parse::<f32>().unwrap_or(1.0).max(0.0), floor);
        }
        return match b {
            "auto" => GTrack::C {
                floor,
                min,
                max: 2,
                cap: f32::MAX,
                stretch: true,
            },
            "min-content" => GTrack::C {
                floor,
                min,
                max: 1,
                cap: f32::MAX,
                stretch: false,
            },
            "max-content" => GTrack::C {
                floor,
                min,
                max: 2,
                cap: f32::MAX,
                stretch: false,
            },
            _ => {
                let cap = if let Some(x) = b
                    .strip_prefix("fit-content(")
                    .and_then(|x| x.strip_suffix(')'))
                {
                    parse_len_vp(x.trim(), rel, em, vh, vw).unwrap_or(f32::MAX)
                } else {
                    parse_len_vp(b, rel, em, vh, vw).unwrap_or(f32::MAX)
                };
                GTrack::C {
                    floor,
                    min,
                    max: 2,
                    cap,
                    stretch: false,
                }
            }
        };
    }
    if let Some(inner) = t
        .strip_prefix("fit-content(")
        .and_then(|x| x.strip_suffix(')'))
    {
        // max(min-content, min(max-content, limit))
        return GTrack::C {
            floor: 0.0,
            min: 1,
            max: 2,
            cap: parse_len_vp(inner.trim(), rel, em, vh, vw).unwrap_or(f32::MAX),
            stretch: false,
        };
    }
    GTrack::Fixed(parse_len_vp(&t, rel, em, vh, vw).unwrap_or(0.0))
}

/// `grid-template-*`/`grid-auto-*` value → expanded track list plus named
/// lines: `names[i]` holds the names of the line *before* track i (len =
/// tracks+1); `fit[i]` marks a track produced by repeat(auto-fit) — empty
/// ones collapse to 0 after placement. `none`/`subgrid`/`masonry`/empty →
/// empty (masonry ≈ auto rows; subgrid resolves to the parent's spanned
/// tracks at the call site). repeat() expands; auto-fill/auto-fit size
/// against `avail` using the repeat unit's floor width.
fn grid_track_list(
    v: Option<&String>,
    rel: f32,
    em: f32,
    vh: f32,
    vw: f32,
    avail: f32,
    gap: f32,
) -> (Vec<GTrack>, Vec<Vec<String>>, Vec<bool>) {
    let Some(v) = v else {
        return (Vec::new(), Vec::new(), Vec::new());
    };
    let v = v.trim().to_lowercase();
    if v.is_empty() || v == "none" || v == "subgrid" || v == "masonry" {
        return (Vec::new(), Vec::new(), Vec::new());
    }
    let mut out: Vec<GTrack> = Vec::new();
    let mut fit: Vec<bool> = Vec::new();
    let mut names: Vec<Vec<String>> = vec![Vec::new()];
    for tok in track_tokens(&v) {
        let t = tok.trim().to_lowercase();
        if t.starts_with('[') {
            // [name ...] — names for the line at the current track boundary
            let inner = t.trim_start_matches('[').trim_end_matches(']');
            names[out.len()].extend(inner.split_whitespace().map(String::from));
            continue;
        }
        if let Some(inner) = t.strip_prefix("repeat(").and_then(|x| x.strip_suffix(')')) {
            let (cnt, rest) = top_comma(inner).unwrap_or((inner, ""));
            let sub = expand_track_tokens(&track_tokens(rest), rel, em, vh, vw);
            if sub.is_empty() {
                continue;
            }
            let cnt = cnt.trim();
            let auto_fit = cnt == "auto-fit";
            let n = if cnt == "auto-fill" || auto_fit {
                let unit: f32 =
                    sub.iter().map(track_floor).sum::<f32>() + gap * (sub.len() - 1) as f32;
                (((avail + gap) / (unit + gap).max(1.0)).floor() as usize).max(1)
            } else {
                cnt.parse::<usize>().unwrap_or(1)
            };
            for _ in 0..n.min(1000) {
                out.extend_from_slice(&sub);
                fit.resize(out.len(), auto_fit);
            }
        } else {
            out.push(track_spec(&t, rel, em, vh, vw));
            fit.push(false);
        }
        while names.len() <= out.len() {
            names.push(Vec::new());
        }
    }
    (out, names, fit)
}

/// Expand a repeat() body (or any token list) into tracks: a numeric
/// `repeat(n, ...)` recurses — `repeat(auto-fill, 1fr repeat(2, 30px))`
/// and mixed minmax/fixed lists work. `[names]` inside repeats are
/// dropped, matching the flat list's behavior.
fn expand_track_tokens(toks: &[String], rel: f32, em: f32, vh: f32, vw: f32) -> Vec<GTrack> {
    fn go(toks: &[String], rel: f32, em: f32, vh: f32, vw: f32, depth: u8) -> Vec<GTrack> {
        let mut out = Vec::new();
        for tok in toks {
            let t = tok.trim().to_lowercase();
            if t.starts_with('[') {
                continue;
            }
            if depth < 8 {
                if let Some(inner) = t.strip_prefix("repeat(").and_then(|x| x.strip_suffix(')')) {
                    let (cnt, rest) = top_comma(inner).unwrap_or((inner, ""));
                    if let Ok(n) = cnt.trim().parse::<usize>() {
                        let sub = go(&track_tokens(rest), rel, em, vh, vw, depth + 1);
                        for _ in 0..n.min(1000) {
                            out.extend_from_slice(&sub);
                        }
                        continue;
                    }
                }
            }
            out.push(track_spec(&t, rel, em, vh, vw));
        }
        out
    }
    go(toks, rel, em, vh, vw, 0)
}

/// A grid line placement term: `auto`, an integer line, `span N`, a named
/// line (`content`/`content-start`), `<num> <name>` (the nth line carrying
/// the name), or `span [N] <name>` (to the Nth next/previous named line).
#[derive(Clone, PartialEq)]
enum GLine {
    Auto,
    Line(i32),
    Span(i32),
    Named(String),
    NamedLine(i32, String),
    NamedSpan(i32, String),
}

fn grid_line_val(t: &str) -> GLine {
    let t = t.trim().to_lowercase();
    let (mut span, mut num, mut name) = (false, None::<i32>, None::<String>);
    for tok in t.split_whitespace() {
        if tok == "span" {
            span = true;
        } else if let Ok(i) = tok.parse::<i32>() {
            if i != 0 && num.is_none() {
                num = Some(i);
            }
        } else if name.is_none() {
            name = Some(tok.to_string());
        }
    }
    match (span, num, name) {
        (true, _, Some(nm)) => GLine::NamedSpan(num.unwrap_or(1).max(1), nm),
        (true, n, None) => GLine::Span(n.unwrap_or(1)),
        (false, Some(i), Some(nm)) => GLine::NamedLine(i, nm),
        (false, Some(i), None) => GLine::Line(i),
        (false, None, Some(nm)) => {
            if nm == "auto" {
                GLine::Auto
            } else {
                GLine::Named(nm)
            }
        }
        (false, None, None) => GLine::Auto,
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
    // Named/NamedLine → numeric line. A bare `x` start prefers `x-start`
    // then `x`; `<ord> x` takes the ord-th line matching `x` or the
    // suffixed variant. Names exist only on explicit tracks.
    let named = |g: &GLine, is_start: bool| -> GLine {
        let suffix = if is_start { "-start" } else { "-end" };
        match g {
            GLine::NamedLine(ord, n) => {
                let mut count = 0i32;
                for (i, ns) in names.iter().enumerate() {
                    if ns.iter().any(|x| x == n || x == &format!("{n}{suffix}")) {
                        count += 1;
                        if count == *ord {
                            return GLine::Line(i as i32 + 1);
                        }
                    }
                }
                GLine::Auto
            }
            GLine::Named(n) => {
                for w in [format!("{n}{suffix}"), n.clone()] {
                    if let Some(i) = names.iter().position(|ns| ns.iter().any(|x| x == &w)) {
                        return GLine::Line(i as i32 + 1);
                    }
                }
                GLine::Auto
            }
            _ => g.clone(),
        }
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
        // `span <name>` end: the k-th `name`/`name-end` line past the
        // resolved start; fewer than k → span `k` tracks as a fallback.
        (GLine::Line(a), GLine::NamedSpan(k, ref nm)) => {
            let a_idx = idx(a).max(0) as usize;
            let span = ((a_idx + 1)..names.len())
                .filter(|&j| {
                    names[j]
                        .iter()
                        .any(|x| x == nm || x == &format!("{nm}-end"))
                })
                .nth(k.max(1) as usize - 1)
                .map(|j| j - a_idx)
                .unwrap_or(k.max(1) as usize);
            (Some(a_idx), span.max(1))
        }
        // `span <name>` start: the k-th `name`/`name-start` line before the
        // resolved end, walking backwards.
        (GLine::NamedSpan(k, ref nm), GLine::Line(b)) => {
            let e_idx = idx(b).max(0) as usize;
            let start = names
                .iter()
                .enumerate()
                .take(e_idx)
                .rev()
                .filter(|(_, ns)| ns.iter().any(|x| x == nm || x == &format!("{nm}-start")))
                .nth(k.max(1) as usize - 1)
                .map(|(j, _)| j);
            match start {
                Some(j) => (Some(j), (e_idx - j).max(1)),
                None => (
                    Some((e_idx as i64 - k as i64).max(0) as usize),
                    k.max(1) as usize,
                ),
            }
        }
        (GLine::Span(n), _) | (GLine::Auto, GLine::Span(n)) => (None, n.max(1) as usize),
        (GLine::NamedSpan(k, _), _) | (_, GLine::NamedSpan(k, _)) => (None, k.max(1) as usize),
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

// ---------- table box tree ----------

/// How an element participates in table-grid construction.
#[derive(Clone, Copy, PartialEq)]
enum TRole {
    /// display:none or a non-element — contributes nothing
    Skip,
    /// display:contents — its children act at this level
    Contents,
    Row,
    Cell,
    Caption,
    Col,
    ColGroup,
    /// ordering key: 0 header, 1 body, 2 footer
    RowGroup(u8),
    /// stray content — laid after the grid
    Other,
}

/// A cell's placement on the table occupancy grid.
struct TCell {
    node: NodeId,
    r: usize,
    c: usize,
    rs: usize,
    cs: usize,
}

/// Cell vertical alignment (td/th vertical-align + valign attr).
#[derive(Clone, Copy, PartialEq)]
enum TAlign {
    Top,
    Middle,
    Bottom,
    Baseline,
}

/// `top|middle|bottom|baseline` (+center) → TAlign.
fn talign(v: &str) -> Option<TAlign> {
    Some(match v.trim() {
        "top" | "text-top" => TAlign::Top,
        "middle" | "center" => TAlign::Middle,
        "bottom" | "text-bottom" => TAlign::Bottom,
        "baseline" => TAlign::Baseline,
        _ => return None,
    })
}

/// A column width source for table-layout:fixed: px or % of the table width.
#[derive(Clone, Copy)]
enum ColW {
    Px(f32),
    Pct(f32),
}

/// `<col width>`, cell `width` attr/CSS → fixed-layout column spec.
fn colw(v: &str) -> Option<ColW> {
    let v = v.trim();
    if let Some(p) = v.strip_suffix('%') {
        p.parse::<f32>().ok().map(ColW::Pct)
    } else {
        parse_len(v, 0.0, BASE_SIZE).map(ColW::Px)
    }
}

/// Parts gathered from a table element's children.
#[derive(Default)]
struct TableParts {
    /// (group order 0 thead/1 body/2 tfoot, row element, cell elements)
    rows: Vec<(u8, Option<NodeId>, Vec<NodeId>)>,
    captions: Vec<NodeId>,
    /// <col>/<colgroup> elements in document order
    cols: Vec<NodeId>,
    /// stray children laid after the grid
    extra: Vec<NodeId>,
}

/// End the current anonymous row of bare cells (display:table-cell
/// elements directly under a table/row-group form an implicit row).
fn flush_implicit(ord: u8, parts: &mut TableParts, implicit: &mut Vec<NodeId>) {
    if !implicit.is_empty() {
        parts.rows.push((ord, None, std::mem::take(implicit)));
    }
}

/// Per-cell measured state (styles, box metrics, intrinsic widths, laid
/// content). `items`/`hits`/`cell_h` are filled by the emit pass.
struct TCellInfo {
    cs: Rc<Props>,
    st: Style,
    pad: [f32; 4],
    border: Option<Borders>,
    bg: Option<Color>,
    valign: TAlign,
    /// cell's first-line baseline offset inside its content box
    first_ascent: f32,
    minw: f32,
    prefw: f32,
    empty: bool,
    items: Vec<DisplayItem>,
    hits: Vec<Hit>,
    cell_h: f32,
}

/// A fully gathered + measured table.
struct TGrid {
    parts: TableParts,
    tcells: Vec<TCell>,
    infos: Vec<TCellInfo>,
    /// occupancy: occ[r][c] = cell index covering that slot (usize::MAX free)
    occ: Vec<Vec<usize>>,
    /// row index → cell indices
    row_cells: Vec<Vec<usize>>,
    ncols: usize,
    nrows: usize,
    /// border-spacing (x, y) — 0 under border-collapse
    sp: (f32, f32),
    collapse: bool,
    /// per-column min-content and max-content widths
    col_min: Vec<f32>,
    col_pref: Vec<f32>,
}

/// border-collapse:collapse edge dedup — a shared edge paints once: the
/// wider adjoining border wins; ties go to the top/left cell. Returns the
/// border widths cell `ti` should actually paint.
fn collapse_edges(grid: &TGrid, ti: usize, bw: [f32; 4]) -> [f32; 4] {
    let tc = &grid.tcells[ti];
    let mut w = bw;
    let nb = |r: usize, c: usize| -> Option<usize> {
        grid.occ
            .get(r)?
            .get(c)
            .copied()
            .filter(|&i| i != usize::MAX)
    };
    // left: the left neighbour's right edge wins ties (wider always wins)
    if tc.c > 0 {
        if let Some(ni) = nb(tc.r, tc.c - 1) {
            let nw = grid.infos[ni].border.map(|b| b.w[1]).unwrap_or(0.0);
            if bw[3] <= nw {
                w[3] = 0.0;
            }
        }
    }
    // right: we win ties against the right neighbour's left edge
    if tc.c + tc.cs < grid.ncols {
        if let Some(ni) = nb(tc.r, tc.c + tc.cs) {
            let nw = grid.infos[ni].border.map(|b| b.w[3]).unwrap_or(0.0);
            if bw[1] < nw {
                w[1] = 0.0;
            }
        }
    }
    // top: the cell above's bottom edge wins ties
    if tc.r > 0 {
        if let Some(ni) = nb(tc.r - 1, tc.c) {
            let nw = grid.infos[ni].border.map(|b| b.w[2]).unwrap_or(0.0);
            if bw[0] <= nw {
                w[0] = 0.0;
            }
        }
    }
    // bottom: we win ties against the cell below's top edge
    if tc.r + tc.rs < grid.nrows {
        if let Some(ni) = nb(tc.r + tc.rs, tc.c) {
            let nw = grid.infos[ni].border.map(|b| b.w[0]).unwrap_or(0.0);
            if bw[2] < nw {
                w[2] = 0.0;
            }
        }
    }
    w
}

// ---------- the layouter ----------

struct Word {
    text: String,
    font: usize,
    size: f32,
    color: Color,
    deco: TextDeco,
    va: Va,
    /// user-select:none on the owning element
    sel: bool,
    /// whitespace gap before this word (word-spacing + letter-spacing
    /// folded in) — kept separate from `width` so `text-align:justify`
    /// can stretch it
    gap_before: f32,
    /// letter-spacing painted per glyph
    ls: f32,
    /// text-shadow copies to emit under the run
    shadow: Vec<(f32, f32, f32, Color)>,
    width: f32,
    ascent: f32,
    line_h: f32,
    href: Option<String>,
    /// inline background — FillRect emitted behind the text
    bg: Option<Color>,
    /// inline-block payload: display items in box-local coords (origin top-left)
    items: Option<Vec<DisplayItem>>,
    /// element boxes inside the payload — same coord space as `items`
    hits: Vec<Hit>,
    /// inline-block height (baseline alignment: box bottom sits on baseline)
    box_h: f32,
    /// painted-position shift from enclosing `position:relative` inlines —
    /// applied at emit; doesn't affect line geometry
    dx: f32,
    dy: f32,
    /// transform spec of the enclosing transformed inline-level element —
    /// a consecutive run of words sharing it wraps at flush_line
    xf: Option<Rc<xform::Spec>>,
    /// inline element that produced this word — flushed into `hits` so
    /// links/spans are hoverable/clickable (`None` = block-owned).
    owner: Option<NodeId>,
}

/// A floated box: horizontal margin-box extent [x0,x1] (emission coords)
/// intruding into line boxes while `top <= y < bottom`.
struct Float {
    right: bool,
    x0: f32,
    x1: f32,
    top: f32,
    bottom: f32,
}

/// Containing-block rect: a positioned ancestor's padding box in emission
/// coords (`h` None while the box's height is still content-driven).
#[derive(Clone, Copy)]
struct CbRect {
    x: f32,
    y: f32,
    w: f32,
    h: Option<f32>,
}

/// A `position:sticky` box's emitted range + flow edges, waiting for the
/// enclosing block's bottom to clamp the stick offset against.
struct StickyFix {
    i0: usize,
    i1: usize,
    h0: usize,
    h1: usize,
    /// border-box top/bottom at its flow position (emission coords)
    top: f32,
    bottom: f32,
    ins_t: Option<f32>,
    ins_b: Option<f32>,
}

/// Translate a display item by (dx, dy).
pub fn offset_item(it: DisplayItem, dx: f32, dy: f32) -> DisplayItem {
    match it {
        DisplayItem::Raster { x, y, w, h, pixels } => DisplayItem::Raster {
            x: x + dx,
            y: y + dy,
            w,
            h,
            pixels,
        },
        DisplayItem::RoundedRect {
            x,
            y,
            w,
            h,
            radius,
            color,
        } => DisplayItem::RoundedRect {
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
            deco,
            ls,
            href,
            sel,
        } => DisplayItem::Text {
            x: x + dx,
            baseline: baseline + dy,
            text,
            font,
            size,
            color,
            deco,
            ls,
            href,
            sel,
        },
        DisplayItem::FillRect { x, y, w, h, color } => DisplayItem::FillRect {
            x: x + dx,
            y: y + dy,
            w,
            h,
            color,
        },
        DisplayItem::Image {
            x,
            y,
            w,
            h,
            src,
            near,
        } => DisplayItem::Image {
            x: x + dx,
            y: y + dy,
            w,
            h,
            src,
            near,
        },
        DisplayItem::BorderSide {
            x,
            y,
            w,
            h,
            side,
            style,
            color,
        } => DisplayItem::BorderSide {
            x: x + dx,
            y: y + dy,
            w,
            h,
            side,
            style,
            color,
        },
        DisplayItem::RoundedRect2 {
            x,
            y,
            w,
            h,
            radii,
            color,
        } => DisplayItem::RoundedRect2 {
            x: x + dx,
            y: y + dy,
            w,
            h,
            radii,
            color,
        },
        DisplayItem::Shadow {
            x,
            y,
            w,
            h,
            dx: sdx,
            dy: sdy,
            blur,
            spread,
            radii,
            color,
            inset,
        } => DisplayItem::Shadow {
            x: x + dx,
            y: y + dy,
            w,
            h,
            dx: sdx,
            dy: sdy,
            blur,
            spread,
            radii,
            color,
            inset,
        },
        DisplayItem::PushClip { x, y, w, h, radii } => DisplayItem::PushClip {
            x: x + dx,
            y: y + dy,
            w,
            h,
            radii,
        },
        DisplayItem::PopClip => DisplayItem::PopClip,
        DisplayItem::BgImage {
            x,
            y,
            w,
            h,
            px,
            py,
            pw,
            ph,
            mode,
            sw,
            sh,
            fx,
            ax,
            fy,
            ay,
            rx,
            ry,
            src,
            near,
        } => DisplayItem::BgImage {
            x: x + dx,
            y: y + dy,
            w,
            h,
            px: px + dx,
            py: py + dy,
            pw,
            ph,
            mode,
            sw,
            sh,
            fx,
            ax,
            fy,
            ay,
            rx,
            ry,
            src,
            near,
        },
        DisplayItem::PushEffect(e) => DisplayItem::PushEffect(e.offset(dx, dy)),
        DisplayItem::PopEffect => DisplayItem::PopEffect, // moving stored points by (dx,dy) conjugates the authored matrix
        DisplayItem::PushTransform { m } => DisplayItem::PushTransform {
            m: m.conjugated(dx, dy),
        },
        DisplayItem::PopTransform => DisplayItem::PopTransform,
    }
}

/// Drop descendant hit rects poking outside a clip rect, intersecting the
/// rest — `hits[start..]` are the subtree's boxes in the same coords.
fn clip_hits(hits: &mut Vec<Hit>, start: usize, x: f32, y: f32, w: f32, h: f32) {
    let tail: Vec<_> = hits.drain(start..).collect();
    hits.extend(tail.into_iter().filter_map(|(hx, hy, hw, hh, n, m)| {
        let (x0, y0) = (hx.max(x), hy.max(y));
        let (x1, y1) = ((hx + hw).min(x + w), (hy + hh).min(y + h));
        (x1 > x0 && y1 > y0).then_some((x0, y0, x1 - x0, y1 - y0, n, m))
    }));
}

struct BlockPatch {
    /// index of first of `nslots` placeholders: outer shadows + bg +
    /// inset shadows + 4 border edges (+ PushClip when overflow clips —
    /// the clip marker must precede the children items).
    idx: usize,
    nslots: usize,
    x: f32,
    y: f32,
    w: f32,
    /// element font size — em/% resolution base for radii
    em: f32,
    bg: Option<Color>,
    border: Option<Borders>,
    radii: RadiusSpec,
    shadows: Vec<BoxShadow>,
    /// overflow clip — last slot is a PushClip on the padding box
    clip: bool,
    outline: Option<Outline>,
    /// parsed `background-*` layers + box metrics, emitted at
    /// `finish_patch` once the box height is known
    bgl: Option<bgimg::BoxBgs>,
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
    /// (family, weight, italic, first-char) → font index memo —
    /// FontCache::resolve scans every installed face per call, so
    /// resolving once per style is a must. u32::MAX = resolved-to-none.
    font_memo: FxHashMap<(String, u16, bool, u32), u32>,
    fonts: &'a mut FontCache,
    items: Vec<DisplayItem>,
    y: f32,
    /// left edge of the current content box
    margin_x: f32,
    /// width available to the current content box (before float insets)
    avail_w: f32,
    align: Align,
    /// text-align-last for forced line breaks; follows `align` saves
    align_last: Option<Align>,
    /// direction:rtl — flush_line emits words right-to-left
    rtl: bool,
    /// line-clamp: lines remaining before the block truncates (0 = off)
    clamp_left: i32,
    /// set once clamp_left runs out — drops all further content
    clamping: bool,
    line: Vec<Word>,
    line_x: f32,
    pending_space: bool,
    /// measured whitespace gap awaiting the next word (word-spacing and
    /// letter-spacing folded in by word_with_space)
    pending_gap: f32,
    /// Some(clip x) while laying a `text-overflow:ellipsis` +
    /// `overflow` clipped block — flush_line truncates + appends "…"
    ellipsis_clip: Option<f32>,
    /// text-indent for the *next* flushed line — consumed at emit so the
    /// first line shifts right while later lines return to the edge
    indent_pending: f32,
    /// open ul/ol/menu/dir contexts — drives ::marker type + numbering
    list_stack: Vec<generated::ListCtx>,
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
    hits: Vec<Hit>,
    /// color translucent paints composite over — the nearest ancestor
    /// background (body's dark page bg makes `bg-white/5` a dim veil)
    backdrop: Color,
    /// definite content height of the box currently laying its children —
    /// `%`/`h-full` heights on descendants (e.g. chess-piece imgs in fixed
    /// cells) resolve against it; propagated to auto-height children so a
    /// `h-full` chain still finds the nearest definite ancestor
    pct_h: Option<f32>,
    /// viewport width (the width passed to `layout`) for vw units and
    /// `background-attachment:fixed` positioning areas — also the initial
    /// containing block for `position:fixed`/`absolute` at the root
    viewport_w: f32,
    /// element whose background propagated to the canvas (its own bg-image
    /// layers are skipped — they're already painted viewport-wide)
    canvas_bg_node: Option<NodeId>,
    /// containing-block stack: padding box (emission coords) of each open
    /// positioned ancestor — `absolute` descendants resolve insets here
    cb_stack: Vec<CbRect>,
    /// sticky boxes awaiting the enclosing block's bottom to clamp against
    stickies: Vec<StickyFix>,
    /// transform spec of the innermost transformed inline-level element —
    /// words queued on `line` pick it up (see flush_line). Reset inside
    /// nested boxes (nest/flex/grid) whose content isn't part of the run.
    cur_xf: Option<Rc<xform::Spec>>,
    /// `grid-template-columns:subgrid` children inherit the parent grid's
    /// spanned track sizes — recorded as a px track list while laying out
    /// the parent item, read by the child's own grid pass
    subgrid_cols: FxHashMap<NodeId, String>,
    /// same for `grid-template-rows:subgrid` (explicit row floors only)
    subgrid_rows: FxHashMap<NodeId, String>,
    /// CSS counters — counter-reset scopes enter/leave with elements
    counters: generated::Counters,
    /// ::first-line style for the current block's not-yet-emitted first line
    fl_style: Option<Style>,
    /// ::first-letter style pending for the next text word
    flet: Option<Style>,
    /// open-quote nesting depth for `content:*-quote`
    quote_depth: i32,
    /// interaction state `:hover`/`:focus`/... selectors match against —
    /// cloned into each pre-resolve worker's StyleCtx
    pstate: css::PseudoState,
    /// `pointer-events:none` is in effect on the current walk path — hit
    /// pushes are dropped while set (an `auto` descendant re-enables).
    pe_blocked: bool,
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
    ) -> (Vec<DisplayItem>, f32, Vec<(String, f32)>, Vec<Hit>) {
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
    ) -> (Vec<DisplayItem>, f32, Vec<(String, f32)>, Vec<Hit>) {
        Self::layout_vh_st(
            doc,
            fonts,
            width,
            scroll,
            viewport_h,
            external,
            &css::PseudoState::default(),
        )
    }

    /// `layout_vh` + an interaction state — `:hover`/`:focus`/`:active`/
    /// `:target` selectors match against `pstate` for this pass.
    pub fn layout_vh_st(
        doc: &'a Document,
        fonts: &'a mut FontCache,
        width: f32,
        scroll: f32,
        viewport_h: f32,
        external: &[css::StyleSheet],
        pstate: &css::PseudoState,
    ) -> (Vec<DisplayItem>, f32, Vec<(String, f32)>, Vec<Hit>) {
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
            y: -scroll,
            margin_x: 0.0,
            avail_w: width,
            align: Align::Left,
            align_last: None,
            rtl: false,
            clamp_left: 0,
            clamping: false,
            line: Vec::new(),
            line_x: 0.0,
            pending_space: false,
            pending_gap: 0.0,
            ellipsis_clip: None,
            indent_pending: 0.0,
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
            viewport_w: width,
            canvas_bg_node: None,
            cb_stack: Vec::new(),
            stickies: Vec::new(),
            cur_xf: None,
            subgrid_cols: FxHashMap::default(),
            subgrid_rows: FxHashMap::default(),
            counters: generated::Counters::new(),
            fl_style: None,
            flet: None,
            quote_depth: 0,
            pstate: pstate.clone(),
            pe_blocked: false,
        };
        l.ctx.set_state(pstate.clone());
        l.preresolve_styles();
        // css-backgrounds §2.11: the canvas takes the root element's
        // background (body's when html's is transparent) — painted
        // edge-to-edge so the body margin doesn't show a white frame
        for &el in l.doc.children(l.doc.root) {
            if l.doc.tag(el) == Some("html") {
                let mut p = l.style(el);
                let mut canvas_bg = l.bg_of(el, &p, None);
                let mut src_el: Option<NodeId> = None;
                if canvas_bg.is_some() || bgimg::has_layers(&p) {
                    src_el = Some(el);
                } else {
                    for &b in l.doc.children(el) {
                        if l.doc.tag(b) == Some("body") {
                            let pb = l.style(b);
                            if l.bg_of(b, &pb, None).is_some() || bgimg::has_layers(&pb) {
                                canvas_bg = l.bg_of(b, &pb, None);
                                p = pb;
                                src_el = Some(b);
                            }
                        }
                    }
                }
                if let Some(se) = src_el {
                    l.canvas_bg_node = Some(se);
                    if let Some(col) = canvas_bg {
                        l.backdrop = col;
                        l.items.push(DisplayItem::FillRect {
                            x: 0.0,
                            y: 0.0,
                            w: width,
                            h: viewport_h,
                            color: col,
                        });
                    }
                    let em = p
                        .get("font-size")
                        .map(|v| parse_font_size(v, BASE_SIZE))
                        .unwrap_or(BASE_SIZE);
                    let cur = p.get("color").and_then(|v| parse_color(v)).unwrap_or(BLACK);
                    if let Some(bgl) =
                        bgimg::parse_box(&p, [0.0; 4], [0.0; 4], em, viewport_h, width, cur)
                    {
                        l.items.extend(bgimg::items_for(
                            &bgl,
                            0.0,
                            0.0,
                            width,
                            viewport_h,
                            (width, viewport_h),
                        ));
                    }
                }
            }
        }
        l.walk(doc.root, &base_style());
        l.flush_line();
        // floats don't consume vertical space at their site; make sure the
        // document is at least as tall as the deepest float
        for f in &l.floats {
            l.y = l.y.max(f.bottom);
        }
        l.drain_stickies(0, f32::NEG_INFINITY, f32::INFINITY);
        let h = l.y + scroll;
        (l.items, h, l.anchors, l.hits)
    }

    /// Horizontal space taken by floats on (left, right) at the current y:
    /// the intrusion into this block's line box [margin_x, margin_x+avail_w].
    /// Same-side floats can't overlap, so the deepest intrusion wins.
    fn float_insets(&self) -> (f32, f32) {
        let (mut l, mut r) = (0.0f32, 0.0f32);
        let (x0, x1) = (self.margin_x, self.margin_x + self.avail_w);
        for f in &self.floats {
            if self.y < f.top || self.y >= f.bottom {
                continue;
            }
            if f.right {
                if f.x0 < x1 {
                    r = r.max(x1 - f.x0);
                }
            } else if f.x1 > x0 {
                l = l.max(f.x1 - x0);
            }
        }
        (l, r)
    }

    /// Bottom of the deepest float on `right`'s side — the `clear` target.
    fn clear_below(&self, right: bool) -> f32 {
        self.floats
            .iter()
            .filter(|f| f.right == right)
            .map(|f| f.bottom)
            .fold(f32::NEG_INFINITY, f32::max)
    }

    /// Apply pending sticky offsets whose boxes are direct children of the
    /// block spanning [parent_top, parent_bottom] (emission coords).
    fn drain_stickies(&mut self, from: usize, parent_top: f32, parent_bottom: f32) {
        for si in from..self.stickies.len() {
            let (i0, i1, h0, h1, dy) = {
                let s = &self.stickies[si];
                let dy = if let Some(t) = s.ins_t {
                    (t - s.top).clamp(0.0, (parent_bottom - s.bottom).max(0.0))
                } else if let Some(b) = s.ins_b {
                    (self.viewport_h - b - s.bottom)
                        .min(0.0)
                        .max(parent_top - s.top)
                } else {
                    0.0
                };
                (
                    s.i0,
                    s.i1.min(self.items.len()),
                    s.h0,
                    s.h1.min(self.hits.len()),
                    dy,
                )
            };
            if dy != 0.0 {
                for i in i0..i1 {
                    let it = std::mem::replace(
                        &mut self.items[i],
                        DisplayItem::FillRect {
                            x: 0.0,
                            y: 0.0,
                            w: 0.0,
                            h: 0.0,
                            color: 0,
                        },
                    );
                    self.items[i] = offset_item(it, 0.0, dy);
                }
                for h in self.hits.iter_mut().skip(h0).take(h1 - h0) {
                    h.1 += dy;
                }
            }
        }
        self.stickies.truncate(from);
    }

    /// `position:relative` inset offsets → (dx, dy); (0,0) otherwise.
    fn rel_offset(&self, c: &Props, em: f32) -> (f32, f32) {
        if c.get("position").map(String::as_str) != Some("relative") {
            return (0.0, 0.0);
        }
        let hrel = self.cb_h.or(self.pct_h).unwrap_or(self.viewport_h);
        let [t, r, b, l] = stacking::inset_sides(c, em, self.avail_w, hrel);
        (
            l.unwrap_or(0.0) - r.unwrap_or(0.0),
            t.unwrap_or(0.0) - b.unwrap_or(0.0),
        )
    }

    /// Containing block for a positioned element's insets: `fixed` →
    /// viewport, `absolute` → nearest positioned ancestor's padding box,
    /// in-flow → current content box / nearest definite height.
    fn inset_cb(&self, fixed: bool, absolute: bool) -> CbRect {
        let vp = CbRect {
            x: 0.0,
            y: 0.0,
            w: self.viewport_w,
            h: Some(self.viewport_h),
        };
        if fixed {
            vp
        } else if absolute {
            self.cb_stack.last().copied().unwrap_or(vp)
        } else {
            CbRect {
                x: self.margin_x,
                y: self.y,
                w: self.avail_w,
                h: self.cb_h.or(self.pct_h),
            }
        }
    }

    /// Walk `node`'s children, cutting sibling paint-order segments at
    /// block-level/positioned/float boundaries and reordering the emitted
    /// runs into paint order (in-flow → floats → inline → positioned z).
    /// `sticky_top`/`sticky_bot` are the parent's content-top and projected
    /// content-bottom — explicit heights are applied after children are
    /// laid, so the caller passes the target along for sticky clamps.
    fn walk_children(&mut self, node: NodeId, st: &Style, sticky_top: f32, sticky_bot: f32) {
        let st0 = self.stickies.len();
        let mut segs: Vec<(usize, usize, i64)> =
            vec![(self.items.len(), self.hits.len(), stacking::L_INLINE)];
        for &ch in self.doc.children(node) {
            if self.clamping {
                break;
            }
            let key = if matches!(self.doc.kind(ch), NodeKind::Element { .. }) {
                let cc = self.style(ch);
                stacking::child_layer(&cc)
            } else {
                None
            };
            if let Some(key) = key {
                // flush the pending line into the open segment, then give
                // the block-level child a segment of its own
                self.flush_line();
                segs.push((self.items.len(), self.hits.len(), key));
                self.walk(ch, st);
                segs.push((self.items.len(), self.hits.len(), stacking::L_INLINE));
            } else {
                self.walk(ch, st);
            }
        }
        self.flush_line();
        // sticky fixups must apply before the reorder permutes ranges
        self.drain_stickies(st0, sticky_top, self.y.max(sticky_bot));
        stacking::reorder(&mut self.items, &mut self.hits, &segs);
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
        let pstate = &self.pstate;
        std::thread::scope(|s| {
            for (work, oc) in elems.chunks(chunk).zip(out.chunks_mut(chunk)) {
                s.spawn(move || {
                    let mut ctx = css::StyleCtx::new(doc, sheets);
                    ctx.set_state(pstate.clone());
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
        let mut props = self.ctx.computed(node);
        // paint containment (paint/strict/content include it) forces
        // clipping at the element's box — approximated as overflow:hidden
        if props.get("contain").is_some_and(|v| {
            v.split_whitespace()
                .any(|t| matches!(t, "paint" | "strict" | "content"))
        }) && props.get("overflow").is_none()
        {
            props.insert("overflow".to_string(), "hidden".to_string());
        }
        let c = Rc::new(props);
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
                let c = self.style(node);
                let mut st = style.clone();
                self.apply_computed(&c, &mut st);
                if let Some(id) = self.doc.attr(node, "id") {
                    if !id.is_empty() {
                        // scroll-margin-top shifts the anchor's scroll target
                        // up (the element wants breathing room when jumped to)
                        let sm = c
                            .get("scroll-margin-top")
                            .or_else(|| c.get("scroll-margin"))
                            .and_then(|v| parse_len(v.split_whitespace().next()?, 0.0, st.size))
                            .unwrap_or(0.0);
                        self.anchors
                            .push((id.to_string(), self.y + self.scroll - sm));
                    }
                }
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
                    "img"
                        | "svg"
                        | "input"
                        | "select"
                        | "textarea"
                        | "video"
                        | "canvas"
                        | "iframe"
                        | "object"
                        | "embed"
                        | "br"
                );
                // `opacity` folds into every emitted item: the painter
                // stores flattened opaque colors, so each item emitted for
                // this subtree is faded toward the backdrop beneath it.
                let op = c
                    .get("opacity")
                    .and_then(|v| v.trim().parse::<f32>().ok())
                    .unwrap_or(1.0)
                    .clamp(0.0, 1.0);
                // filter/clip-path/mix-blend-mode/backdrop-filter context —
                // st moves into the dispatch below
                let (fx_size, fx_cur) = (st.size, st.color);
                // visibility:hidden keeps its box but paints nothing — the
                // same truncation as opacity:0 (a `visibility:visible`
                // descendant that should re-show is rare and unsupported)
                let hidden = matches!(
                    c.get("visibility").map(String::as_str),
                    Some("hidden") | Some("collapse")
                );
                let disp = c.get("display").map(String::as_str);
                // Elements whose dispatch flushes the pending line and
                // writes straight into self.items — block-ish boxes,
                // positioned boxes (routed through `block`) and the
                // direct-emit replaced tags (img/svg flush internally).
                // Only those can wrap items[i0..] in a transform;
                // inline-level elements tag their queued words instead
                // (flush_line resolves the run's matrix).
                let direct = visually_hidden
                    || disp == Some("none")
                    || matches!(
                        disp,
                        Some("block") | Some("list-item") | Some("flex") | Some("grid")
                    )
                    || matches!(
                        c.get("position").map(String::as_str),
                        Some("absolute") | Some("fixed")
                    )
                    || matches!(tag.as_str(), "svg" | "img");
                if direct {
                    // the dispatch flushes anyway — flushing first keeps
                    // items[i0..] to THIS element's emission so a transform
                    // wrap (or the opacity fade below) can't sweep in
                    // earlier siblings' queued words
                    self.flush_line();
                }
                let (i0, h0, bd) = (self.items.len(), self.hits.len(), self.backdrop);
                // CSS 2D transform spec (xform.rs) — visual-only, applied
                // around the element's border box after its subtree emits
                let xf = xform::spec(&c, st.size);
                // transformed inline-level elements: stash the spec while
                // dispatching so push_word/push_box tag the queued words
                let saved_xf = if !direct && xf.is_some() {
                    Some(std::mem::replace(&mut self.cur_xf, xf.clone()))
                } else {
                    None
                };
                // CSS counters: resets open a scope popped when the
                // element's subtree ends (display:none — no box, no effect)
                let counter_scope = if visually_hidden || disp == Some("none") {
                    Vec::new()
                } else {
                    self.counters.enter(&c)
                };
                // pointer-events:none: suppress hits for the element and
                // its subtree — an `auto` descendant re-enables (st.pe_none
                // already folds inheritance into this element's flag)
                let pe_save = self.pe_blocked;
                self.pe_blocked = st.pe_none;
                match disp {
                    _ if visually_hidden => {
                        self.pe_blocked = pe_save;
                        return;
                    }
                    Some("none") => {
                        self.pe_blocked = pe_save;
                        return;
                    }
                    // display:contents — the element makes no box; children
                    // participate in the parent context (styles still apply
                    // to the element itself for inheritance)
                    Some("contents") => {
                        for &ch in self.doc.children(node) {
                            self.walk(ch, &st);
                        }
                    }
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
                    // floats blockify (CSS 2.1 §9.7); floated images go too —
                    // other replaced tags keep the inline path
                    _ if matches!(
                        c.get("float").map(String::as_str),
                        Some("left") | Some("right")
                    ) && (!replaced || matches!(tag.as_str(), "img" | "svg")) =>
                    {
                        self.block(node, tag, &c, st);
                    }
                    _ if replaced => self.inline(node, tag, &c, st),
                    Some("block") | Some("list-item") | Some("flex") | Some("grid")
                    | Some("flow-root") | Some("table") => {
                        self.block(node, tag, &c, st);
                    }
                    // internal table parts outside a table ⇒ block
                    // (anonymous-box generation beyond that unsupported)
                    Some("table-row")
                    | Some("table-cell")
                    | Some("table-caption")
                    | Some("table-row-group")
                    | Some("table-header-group")
                    | Some("table-footer-group") => self.block(node, tag, &c, st),
                    // columns generate no box of their own
                    Some("table-column") | Some("table-column-group") => return,
                    Some("inline-block") | Some("inline-flex") | Some("inline-grid")
                    | Some("inline-table") => self.inline_block(node, &c, st),
                    _ => self.inline(node, tag, &c, st),
                }
                if let Some(prev) = saved_xf {
                    self.cur_xf = prev;
                }
                self.pe_blocked = pe_save;
                if op <= 0.0 || hidden {
                    // fully transparent/hidden: nothing painted, nothing clickable
                    self.items.truncate(i0);
                    self.hits.truncate(h0);
                } else if op < 1.0 {
                    for it in &mut self.items[i0..] {
                        match it {
                            DisplayItem::Text { color, .. }
                            | DisplayItem::FillRect { color, .. }
                            | DisplayItem::RoundedRect { color, .. }
                            | DisplayItem::RoundedRect2 { color, .. }
                            | DisplayItem::BorderSide { color, .. }
                            | DisplayItem::Shadow { color, .. } => {
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
                            DisplayItem::Image { .. }
                            | DisplayItem::BgImage { .. }
                            | DisplayItem::PushClip { .. }
                            | DisplayItem::PopClip
                            | DisplayItem::PushEffect(_)
                            | DisplayItem::PopEffect
                            | DisplayItem::PushTransform { .. }
                            | DisplayItem::PopTransform => {}
                        }
                    }
                }
                // transform: wrap the element's emitted span; hit rects map
                // through the same matrix so clicks still land on it
                if direct {
                    if let Some(sp) = &xf {
                        self.wrap_span(sp, node, i0, h0);
                    }
                }
                // filter/clip-path/mix-blend-mode/backdrop-filter wrap this
                // element's emitted items as one paint-time group; rect =
                // own border box when it recorded a hit. (inline-block
                // items go to the line instead — see inline_block_inner.)
                let rect = self.hits[h0..]
                    .iter()
                    .find(|h| h.4 == node)
                    .map(|h| [h.0, h.1, h.2, h.3]);
                let fx = colorfx::element_fx(&c, fx_size, fx_cur, rect);
                if !fx.is_empty() {
                    colorfx::wrap_vec(&mut self.items, i0, fx);
                }
                self.counters.leave(counter_scope);
            }
        }
    }

    /// Record a hit rect unless `pointer-events:none` is in effect.
    fn push_hit(&mut self, h: Hit) {
        if !self.pe_blocked {
            self.hits.push(h);
        }
    }

    fn apply_computed(&mut self, c: &Props, st: &mut Style) {
        if let Some(v) = c.get("font-size") {
            st.size = parse_font_size(v, st.size);
        }
        if let Some(v) = c.get("font-weight") {
            st.weight = font_weight_value(v, st.weight);
            st.bold = st.weight >= 600;
        }
        if let Some(v) = c.get("font-style") {
            let v = v.trim().to_lowercase();
            st.italic = v.starts_with("italic")
                || (v.starts_with("oblique") && !v.starts_with("oblique 0"));
        }
        if let Some(v) = c.get("font-stretch") {
            st.stretch = font_stretch_value(v);
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
            // keep the whole comma list — FontCache::resolve tries each
            // name in order and expands generic keywords itself
            st.family = v.trim().to_lowercase();
        }
        if let Some(v) = c.get("color") {
            if let Some(col) = parse_color_ctx(v, Some(st.color)) {
                st.color = over(col, self.backdrop);
            }
        }
        // text-decoration shorthand + longhands → one spec. The shorthand
        // resets everything it omits; the longhands patch individual bits.
        let mut deco = st.deco.clone();
        if let Some(v) = c.get("text-decoration") {
            let v = v.trim().to_lowercase();
            if v == "none" {
                deco.lines = 0;
            } else {
                deco.lines = 0;
                for t in v.split_whitespace() {
                    match t {
                        "underline" => deco.lines |= 1,
                        "overline" => deco.lines |= 2,
                        "line-through" => deco.lines |= 4,
                        "solid" => deco.style = DecoStyle::Solid,
                        "double" => deco.style = DecoStyle::Double,
                        "dotted" => deco.style = DecoStyle::Dotted,
                        "dashed" => deco.style = DecoStyle::Dashed,
                        "wavy" => deco.style = DecoStyle::Wavy,
                        _ => {
                            if let Some(col) = parse_color_ctx(t, Some(st.color)) {
                                deco.color = Some(col);
                            } else if let Some(px) = parse_len(t, st.size, st.size) {
                                deco.thick = Some(px);
                            }
                        }
                    }
                }
            }
        }
        if let Some(v) = c.get("text-decoration-line") {
            let mut lines = 0;
            for t in v.split_whitespace() {
                match t {
                    "underline" => lines |= 1,
                    "overline" => lines |= 2,
                    "line-through" => lines |= 4,
                    _ => {}
                }
            }
            deco.lines = lines;
        }
        if let Some(v) = c.get("text-decoration-style") {
            deco.style = match v.trim() {
                "double" => DecoStyle::Double,
                "dotted" => DecoStyle::Dotted,
                "dashed" => DecoStyle::Dashed,
                "wavy" => DecoStyle::Wavy,
                _ => DecoStyle::Solid,
            };
        }
        if let Some(v) = c.get("text-decoration-color") {
            deco.color = parse_color_ctx(v, Some(st.color));
        }
        if let Some(v) = c.get("text-decoration-thickness") {
            let v = v.trim();
            deco.thick = if v == "auto" || v == "from-font" {
                None
            } else if let Some(px) = parse_len(v, st.size, st.size) {
                Some(px)
            } else {
                v.strip_suffix('%')
                    .and_then(|p| p.parse::<f32>().ok())
                    .map(|p| st.size * p / 100.0)
            };
        }
        if let Some(v) = c.get("text-underline-offset") {
            let v = v.trim();
            deco.offset = if v == "auto" {
                None
            } else if let Some(px) = parse_len(v, st.size, st.size) {
                Some(px)
            } else {
                v.strip_suffix('%')
                    .and_then(|p| p.parse::<f32>().ok())
                    .map(|p| st.size * p / 100.0)
            };
        }
        // text-underline-position: under ≈ larger offset from baseline
        if c.get("text-underline-position").map(String::as_str) == Some("under") {
            deco.offset = Some(deco.offset.unwrap_or(0.0) + st.size * 0.18);
        }
        st.deco = deco;
        if let Some(v) = c.get("text-align") {
            let rtl = st.rtl || c.get("direction").map(String::as_str) == Some("rtl");
            st.align = match (v.trim(), rtl) {
                ("center", _) | ("-webkit-center", _) | ("match-parent", _) => Align::Center,
                ("right", _) | ("-webkit-right", _) => Align::Right,
                ("start", true) | ("end", false) => Align::Right,
                ("end", true) => Align::Left,
                ("justify", _) | ("justify-all", _) => Align::Justify,
                _ => Align::Left,
            };
        }
        if let Some(v) = c.get("text-align-last") {
            st.align_last = match v.trim() {
                "center" => Some(Align::Center),
                "right" | "end" => Some(Align::Right),
                "justify" => Some(Align::Justify),
                "left" | "start" => Some(Align::Left),
                _ => None,
            };
        }
        if let Some(v) = c.get("text-indent") {
            let v = v
                .trim()
                .trim_end_matches(" hanging")
                .trim_end_matches(" each-line");
            st.indent = dim(c, "text-indent", self.avail_w, st.size)
                .or_else(|| parse_len(v, self.avail_w, st.size))
                .unwrap_or(0.0);
        }
        if let Some(v) = c.get("letter-spacing") {
            let v = v.trim();
            st.ls = if v == "normal" {
                0.0
            } else {
                parse_len(v, st.size, st.size).unwrap_or(0.0)
            };
        }
        if let Some(v) = c.get("word-spacing") {
            let v = v.trim();
            st.ws = if v == "normal" {
                0.0
            } else {
                parse_len(v, st.size, st.size).unwrap_or(0.0)
            };
        }
        if let Some(v) = c.get("tab-size") {
            let v = v.trim();
            st.tab = v
                .parse::<f32>()
                .ok()
                .or_else(|| parse_len(v, st.size, st.size).map(|px| px / (st.size * 0.3)))
                .unwrap_or(8.0)
                .max(0.0);
        }
        if let Some(v) = c.get("text-shadow") {
            st.shadow = parse_text_shadow(v, st.size, st.color);
        }
        if let Some(v) = c.get("font-variant").or_else(|| c.get("font-variant-caps")) {
            st.small_caps = v.trim() == "small-caps";
        }
        if let Some(v) = c.get("hyphens") {
            st.hyphens_none = v.trim() == "none";
        }
        st.rtl = c.get("direction").map(String::as_str) == Some("rtl");
        if st.rtl && c.get("text-align").is_none() {
            st.align = Align::Right;
        }
        if let Some(v) = c.get("text-wrap") {
            st.nowrap |= v.trim() == "nowrap";
        }
        if let Some(v) = c
            .get("user-select")
            .or_else(|| c.get("-webkit-user-select"))
            .or_else(|| c.get("-moz-user-select"))
        {
            st.no_select = matches!(v.trim(), "none" | "-moz-none");
        }
        if let Some(v) = c.get("line-clamp").or_else(|| c.get("-webkit-line-clamp")) {
            st.line_clamp = v.trim().parse::<i32>().unwrap_or(0);
        }
        if let Some(v) = c.get("unicode-bidi") {
            // bidi-override forces rtl reorder; isolate/plaintext keep
            // the direction's natural behavior (our approximation)
            st.rtl |= matches!(v.trim(), "bidi-override" | "isolate-override");
        }
        if let Some(v) = c.get("white-space") {
            let v = v.trim();
            st.pre = matches!(v, "pre" | "pre-wrap" | "break-spaces");
            st.pre_line = v == "pre-line";
            st.nowrap = matches!(v, "pre" | "nowrap");
        }
        if let Some(v) = c.get("overflow-wrap").or_else(|| c.get("word-wrap")) {
            st.break_word |= matches!(v.trim(), "break-word" | "anywhere");
        }
        if let Some(v) = c.get("word-break") {
            st.break_word |= matches!(v.trim(), "break-all" | "break-word");
        }
        if let Some(v) = c.get("vertical-align") {
            let v = v.trim();
            st.va = match v {
                "sub" => Va::Shift(-0.3),
                "super" => Va::Shift(0.35),
                "top" | "text-top" => Va::Top,
                "middle" => Va::Middle,
                "bottom" | "text-bottom" => Va::Bottom,
                "baseline" | "-moz-middle-with-baseline" => Va::Baseline,
                _ => {
                    // <length> and <percentage> (of line-height) fold into
                    // a fraction-of-size baseline shift
                    let px = parse_len(v, st.size, st.size).or_else(|| {
                        let lh = match st.line_h {
                            Some(LineH::Mul(m)) => st.size * m,
                            Some(LineH::Px(p)) => p,
                            None => st.size * 1.2,
                        };
                        v.strip_suffix('%')
                            .and_then(|p| p.parse::<f32>().ok())
                            .map(|p| lh * p / 100.0)
                    });
                    match px {
                        Some(px) => Va::Shift(px / st.size),
                        None => Va::Baseline,
                    }
                }
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
        if let Some(col) = bg_color(c, Some(st.color)) {
            st.inline_bg = Some(over(col, self.backdrop));
        }
        if let Some(v) = c.get("pointer-events") {
            st.pe_none = matches!(v.trim(), "none");
        }
    }

    fn block(&mut self, node: NodeId, tag: &str, c: &Props, mut st: Style) {
        self.flush_line();
        // a fresh block box isn't part of an enclosing inline's transformed
        // run — its own transform (if any) wraps via walk's wrap_span
        let saved_xf = self.cur_xf.take();
        // positioning: `absolute`/`fixed` leave flow (space restored at the
        // end); `sticky` stays in flow and clamps its painted y at the inset
        // edge while the parent is on screen (StickyFix below); `relative`
        // offsets post-layout. Insets resolve against the containing block:
        // fixed → viewport, absolute → nearest positioned ancestor's
        // padding box (cb_stack), in-flow → current box / definite height.
        let pos_kind = c.get("position").map(String::as_str);
        let fixed = pos_kind == Some("fixed");
        let oof = fixed || pos_kind == Some("absolute");
        let sticky = pos_kind == Some("sticky");
        let is_cb = matches!(
            pos_kind,
            Some("relative") | Some("absolute") | Some("fixed") | Some("sticky")
        );
        let cb = self.inset_cb(fixed, pos_kind == Some("absolute"));
        let cb_h_res = cb.h.or(self.cb_h).or(self.pct_h).unwrap_or(self.viewport_h);
        // `inset` shorthand + longhands + logical aliases → [t, r, b, l];
        // % resolves against the containing block's dims
        let [ins_t, ins_r, ins_b, ins_l] =
            stacking::inset_sides(c, st.size, cb.w.max(1.0), cb_h_res);
        // explicit `height` — definite containing-block height for positioned
        // descendants and a floor for this box's own extent. % resolves
        // against the containing block for positioned boxes, else the
        // nearest definite ancestor height, else the viewport.
        let h_prop = hdim_pct(
            c,
            "height",
            if oof {
                cb_h_res
            } else {
                self.pct_h.unwrap_or(self.viewport_h)
            },
            st.size,
            if oof { Some(cb_h_res) } else { self.pct_h },
        );
        // out-of-flow boxes leave no flow space: remember the flow position
        // and restore it after the children are laid (block-end restore)
        let flow = if oof {
            let f = (self.y, self.margin_x, self.avail_w);
            if let Some(t) = ins_t {
                // `top` = margin-edge distance below the cb's padding top
                self.y = cb.y + t;
            } else if fixed {
                // auto top: a fixed box keeps its static position in the
                // viewport — emitted coords have scroll baked in, so re-add
                // it to keep the box put at its original spot
                self.y = f.0 + self.scroll;
            }
            if let Some(l) = ins_l {
                self.margin_x = cb.x + l;
                self.avail_w = (cb.x + cb.w - self.margin_x).max(1.0);
            } else {
                self.avail_w = cb.w.max(1.0);
            }
            Some(f)
        } else {
            None
        };
        // `left`+`right`/`top`+`bottom` stretch an out-of-flow box to the
        // containing block — relative/sticky over-constrained insets just
        // ignore the block-end edge instead
        let inset_w = ins_l.zip(ins_r).map(|(l, r)| (cb.w - l - r).max(0.0));
        let inset_h = if oof {
            ins_t.zip(ins_b).map(|(t, b)| (cb_h_res - t - b).max(0.0))
        } else {
            None
        };
        let size = st.size;
        let [mt, mr, mb, ml] = box_sides(c, "margin", self.avail_w, size);
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
        // the block's own background paints behind the whole box, not per word
        st.inline_bg = None;
        // this element's text sits on its own bg — re-composite the color
        if let Some(b) = bg {
            if let Some(col) = c
                .get("color")
                .and_then(|v| parse_color_ctx(v, Some(st.color)))
            {
                st.color = over(col, b);
            }
        }
        let [bw_t, bw_r, bw_b, bw_l] = border.map(|b| b.w).unwrap_or([0.0; 4]);
        let bw_h = bw_l + bw_r; // horizontal border total
        let bw_v = bw_t + bw_b; // vertical border total
                                // `background-image` layers resolve to display items at finish_patch
                                // (the final box height isn't known yet). Skip a node whose
                                // background already propagated to the canvas.
        let bgl = if self.canvas_bg_node == Some(node) {
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
        };

        // floats blockify but out-of-flow boxes ignore `float` entirely
        if !oof
            && matches!(
                c.get("float").map(String::as_str),
                Some("left") | Some("right")
            )
        {
            self.float_block(node, tag, c, st, mt, mb);
            self.cur_xf = saved_xf;
            return;
        }

        // `display:table` lays its grid — a floated or out-of-flow table
        // already diverted above (floats blockify per CSS 2.1 §9.7)
        if tag == "table"
            || matches!(
                c.get("display").map(String::as_str),
                Some("table") | Some("inline-table")
            )
        {
            self.table(node, c, st);
            self.cur_xf = saved_xf;
            return;
        }

        // `width` caps the content box; `margin-?:auto` splits leftover space
        let mut ml = ml;
        let mut mr = mr;
        // inset-stretched boxes are sized by left+right, a border-box length
        let w_explicit = dim(c, "width", self.avail_w, size);
        let mut w_prop = w_explicit.or(inset_w);
        if let Some(mxw) = c
            .get("max-width")
            .and_then(|v| parse_len_vp(v, self.avail_w, size, self.viewport_h, self.avail_w))
        {
            w_prop = Some(w_prop.unwrap_or(self.avail_w).min(mxw));
        }
        if let Some(mnw) = c
            .get("min-width")
            .and_then(|v| parse_len_vp(v, self.avail_w, size, self.viewport_h, self.avail_w))
        {
            // min-width constrains the box — it must not *create* a width:
            // `min-width:0` on a width:auto block must stay full-width
            if mnw > 0.0 {
                w_prop = Some(w_prop.unwrap_or(self.avail_w).max(mnw));
            }
        }
        if oof && w_prop.is_none() {
            // shrink-to-fit (CSS 2.1 §10.3.7): max-content clamped to the
            // room from the box's left edge to the containing-block right
            let room = (cb.x + cb.w - self.margin_x).max(1.0);
            w_prop = Some(self.max_content_width(node, &st).min(room));
        }
        let w_prop = w_prop;
        // does `w_prop` already include padding+border? border-box, an inset
        // stretch (always a border-box length), or any non-oof auto width —
        // an abspos shrink-to-fit width is a content-box length
        let w_is_outer = bb || (w_explicit.is_none() && (inset_w.is_some() || !oof));
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

        // ul/ol/menu/dir open a ::marker numbering/type context
        match tag {
            "ul" | "menu" | "dir" => self.push_list_ctx(node, c, false),
            "ol" => self.push_list_ctx(node, c, true),
            _ => {}
        }
        // display:list-item boxes carry a ::marker
        let marker = if c.get("display").map(String::as_str) == Some("list-item") {
            self.make_marker(node, c, &st)
        } else {
            None
        };

        self.y += mt.max(0.0);
        // `clear`: the border edge drops below same-side floats (not oof)
        if !oof {
            match c.get("clear").map(String::as_str) {
                Some("left") | Some("inline-start") => self.y = self.y.max(self.clear_below(false)),
                Some("right") | Some("inline-end") => self.y = self.y.max(self.clear_below(true)),
                Some("both") => {
                    self.y = self
                        .y
                        .max(self.clear_below(false).max(self.clear_below(true)))
                }
                _ => {}
            }
        }
        let saved_mx = self.margin_x;
        let saved_avail = self.avail_w;
        let saved_align = self.align;
        let saved_align_last = self.align_last;
        let saved_rtl = self.rtl;
        let saved_ellip = self.ellipsis_clip;
        let saved_clamp = (self.clamp_left, self.clamping);
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
            let centered =
                saved_align == Align::Center || (margin_auto(c, true) && margin_auto(c, false));
            if centered && outer < self.avail_w {
                self.margin_x += ((self.avail_w - outer) / 2.0).max(0.0);
            }
        }
        self.margin_x += ml.max(0.0) + pl + bw_l;
        let content_mx = self.margin_x; // outside-::marker anchor
        self.avail_w = inner_w;
        self.align = st.align;
        self.align_last = st.align_last;
        self.rtl = st.rtl;
        if st.line_clamp > 0 && !self.clamping {
            self.clamp_left = st.line_clamp;
        }
        // text-indent shifts the first line only — it rides into
        // flush_line as a pending offset (wrap checks get it via line_x)
        if st.indent != 0.0 {
            self.indent_pending = st.indent;
            self.line_x += st.indent;
        }
        // text-overflow:ellipsis needs overflow clipping to be visible
        self.ellipsis_clip = if c.get("text-overflow").map(String::as_str) == Some("ellipsis")
            && borderfx::clips_overflow(c)
        {
            Some(inner_w)
        } else {
            None
        };

        // background/border need the final rect — reserve placeholders.
        // The patch covers the border box (padding included).
        // slots: outer shadows + bg + inset shadows + 4 border edges
        // (+ 1 PushClip when overflow clips, right before the children)
        let shadows = borderfx::parse_shadows(c, size, bd, st.color);
        let outline = borderfx::outline(c, size, bd, st.color);
        let clip = borderfx::clips_overflow(c);
        let radii = borderfx::radius_spec(c);
        let mut patch = if bg.is_some()
            || border.is_some()
            || !shadows.is_empty()
            || outline.is_some()
            || clip
            || !radii.none()
            || bgl.is_some()
        {
            let idx = self.items.len();
            let n_out = shadows.iter().filter(|s| !s.inset).count();
            let nslots = n_out + 1 + (shadows.len() - n_out) + 4 + clip as usize;
            for _ in 0..nslots {
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
                nslots,
                x: self.margin_x - pl - bw_l,
                y: self.y,
                w: pl + inner_w + pr + bw_h,
                em: size,
                bg,
                border,
                radii,
                shadows,
                clip,
                outline,
                bgl,
            })
        } else {
            None
        };

        // padding-top + border push content down inside the painted box
        self.y += pt + bw_t;
        let content_top = self.y;
        let mut children_items_start = self.items.len();
        let children_hits_start = self.hits.len();
        let st0 = self.stickies.len();
        // a positioned box is the containing block for positioned
        // descendants: remember its padding box (emission coords) — height
        // only when definite (explicit height or inset stretch)
        if is_cb {
            let pad_h = h_prop
                .map(|h| if bb { h - bw_v } else { h + pt + pb })
                .or_else(|| inset_h.map(|ih| ih - bw_v));
            self.cb_stack.push(CbRect {
                x: self.margin_x - pl,
                y: content_top - pt,
                w: pl + inner_w + pr,
                h: pad_h.map(|h| h.max(0.0)),
            });
        }
        // descendants composite translucent paints over this box's bg
        let saved_bd = self.backdrop;
        if let Some(b) = bg {
            self.backdrop = b;
        }
        // content-height target: min-height/height/max-height/aspect-ratio.
        // Under border-box these are border-box lengths → strip padding+border.
        let ht = |v: f32| if bb { v - pt - pb - bw_v } else { v };
        let hrel = self.pct_h.unwrap_or(self.viewport_h);
        let pct_h_definite = oof || self.pct_h.is_some();
        let mut mh_prop = h_prop;
        // `height`/`max-height` clamp the box to the target even when content
        // overflows; `min-height` (and aspect-ratio) only floor it
        let mut h_clamped = h_prop.is_some();
        if let Some(mn) = c
            .get("min-height")
            .filter(|v| pct_h_definite || !v.contains('%'))
            .and_then(|v| parse_len_vp(v, hrel, size, self.viewport_h, self.avail_w))
        {
            mh_prop = Some(mh_prop.map_or(mn, |h| h.max(mn)));
        }
        if let Some(mx) = c
            .get("max-height")
            .filter(|v| pct_h_definite || !v.contains('%'))
            .and_then(|v| parse_len_vp(v, hrel, size, self.viewport_h, self.avail_w))
        {
            mh_prop = Some(mh_prop.map_or(mx, |h| h.min(mx)));
            h_clamped = true;
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
            // ::first-line/::first-letter: the element's own pseudo style
            // overrides one propagated from a block ancestor; the propagated
            // flags restore at the end if they went unconsumed
            let prev_fl = self.fl_style.clone();
            let prev_flet = self.flet.clone();
            let fl_pp = self.ctx.computed_pseudo(node, css::PseudoElem::FirstLine);
            if !fl_pp.is_empty() {
                let mut fst = st.clone();
                self.apply_computed(&fl_pp, &mut fst);
                self.fl_style = Some(fst);
            }
            let flet_pp = self.ctx.computed_pseudo(node, css::PseudoElem::FirstLetter);
            if !flet_pp.is_empty() {
                let mut fst = st.clone();
                self.apply_computed(&flet_pp, &mut fst);
                self.flet = Some(fst);
            }
            // inside markers join the first line as a leading word (kept
            // out of ::first-letter); outside markers place post-layout
            if let Some((m, mst, true)) = &marker {
                let keep = self.flet.take();
                self.word(m, mst);
                self.flet = keep;
                self.pending_space = true;
            }
            // ::before generated box = the element's first child
            self.emit_pseudo(node, css::PseudoElem::Before, c, &st);
            // ::before text in a flex/grid container gets its own line
            // rather than tail-laying after the items
            if matches!(
                c.get("display").map(String::as_str),
                Some("flex") | Some("inline-flex") | Some("grid") | Some("inline-grid")
            ) {
                self.flush_line();
            }
            // containing-block height for positioned descendants: a box with
            // position + a definite height anchors `inset`-stretched children
            let saved_cb_h = self.cb_h;
            let saved_inset = self.inset_h;
            if is_cb {
                if let Some(h) = h_prop {
                    self.cb_h = Some(h);
                }
            }
            self.inset_h = inset_h;
            let floats_at_entry = self.floats.len();
            // sticky children clamp against this box's content bottom —
            // explicit heights apply after layout, so project the target now
            let sticky_bot = mh_prop
                .into_iter()
                .chain(inset_h.map(|ih| ih - pt - pb - bw_v))
                .fold(content_top, |b, h| b.max(content_top + h));
            let st_tail = st.clone();
            match c.get("display").map(String::as_str) {
                Some("flex") | Some("inline-flex") => self.flex_children(node, c, st),
                Some("grid") | Some("inline-grid") => self.grid_children(node, c, &st),
                _ if multicol::multicol_on(c) => self.multicol_children(node, c, st),
                _ => self.walk_children(node, &st, content_top, sticky_bot),
            }
            // display:flow-root establishes a block formatting context:
            // floats inside it extend its own height (margin-collapsing is
            // a no-op engine-wide, so nothing else to suppress)
            if c.get("display").map(String::as_str) == Some("flow-root") {
                for f in self.floats.iter().skip(floats_at_entry) {
                    self.y = self.y.max(f.bottom);
                }
            }
            self.cb_h = saved_cb_h;
            self.inset_h = saved_inset;
            if is_cb {
                self.cb_stack.pop();
            }
            // sticky children of flex/grid paths didn't pass walk_children —
            // catch them here; item ranges are still in emission order
            self.drain_stickies(st0, content_top, self.y.max(sticky_bot));
            // ::after generated box = the element's last child
            self.emit_pseudo(node, css::PseudoElem::After, c, &st_tail);
            self.flush_line();
            // un-consumed ::first-* flags propagate to the next sibling
            if self.fl_style.is_some() {
                self.fl_style = prev_fl;
            }
            if self.flet.is_some() {
                self.flet = prev_flet;
            }
            // `list-style-position:outside` marker in the left gutter
            if let Some((m, mst, false)) = &marker {
                self.place_outside_marker(m, mst, content_top, children_items_start, content_mx);
            }
            // settle the box at the content-height target: `height`/
            // `max-height` pin it exactly (content may overflow), a lone
            // `min-height` or aspect-ratio only raises the floor.
            // (flex alignment/justification is handled inside flex_layout —
            // no post-hoc shifting needed here)
            if let Some(mh) = mh_prop {
                self.y = if h_clamped {
                    content_top + mh
                } else {
                    self.y.max(content_top + mh)
                };
            }
        }
        self.backdrop = saved_bd;
        self.pct_h = saved_ph;
        // positioned box with `top`+`bottom` (e.g. `inset:0`): the box is
        // exactly the containing height minus insets — overflowing content
        // spills rather than stretching it. The inset extent is a
        // border-box length.
        if let Some(ih) = inset_h {
            self.y = content_top + ih - pt - pb - bw_v;
        }
        // padding-bottom + border extend the painted box below content
        self.y += pb + bw_b;

        if let Some(p) = patch.as_mut() {
            children_items_start += self.finish_patch(p);
            if p.clip {
                // descendant hits outside the padding box are dropped
                let h = self.y - p.y;
                let bw = p.border.map(|b| b.w).unwrap_or([0.0; 4]);
                let (cx, cy, cw, chh) = borderfx::padding_box(p.x, p.y, p.w, h, bw);
                clip_hits(&mut self.hits, children_hits_start, cx, cy, cw, chh);
            }
        }
        // element border box → click hit-test map
        self.push_hit((
            self.margin_x - pl - bw_l,
            content_top - pt - bw_t,
            inner_w + pl + pr + bw_h,
            self.y - content_top + pt + bw_t,
            node,
            xform::Mat::IDENTITY,
        ));
        // `translate`/`transform:translate*()` + `position:relative` offsets —
        // post-layout visual offset; flow position is unaffected. Elements
        // with a non-translate transform compose it into a matrix instead —
        // wrap_span emits PushTransform around their whole span.
        let (mut tx, mut ty) = if xform::has_matrix(c) {
            (0.0, 0.0)
        } else {
            translate_xy(
                c,
                inner_w + pl + pr + bw_h,
                self.y - (content_top - pt - bw_t),
                size,
            )
        };
        if pos_kind == Some("relative") {
            tx += ins_l.unwrap_or(0.0) - ins_r.unwrap_or(0.0);
            ty += ins_t.unwrap_or(0.0) - ins_b.unwrap_or(0.0);
        }
        if oof {
            // `right`/`bottom` without `left`/`top` anchor the far edge to
            // the containing block now that the box's extent is known
            if ins_l.is_none() {
                if let Some(r) = ins_r {
                    tx += cb.x + cb.w - r - (self.margin_x + inner_w + pr + bw_r);
                }
            }
            if ins_t.is_none() {
                if let Some(b) = ins_b {
                    ty += cb.y + cb_h_res - b - self.y;
                }
            }
        }
        if tx != 0.0 || ty != 0.0 {
            for i in children_items_start..self.items.len() {
                let it = std::mem::replace(
                    &mut self.items[i],
                    DisplayItem::FillRect {
                        x: 0.0,
                        y: 0.0,
                        w: 0.0,
                        h: 0.0,
                        color: 0,
                    },
                );
                self.items[i] = offset_item(it, tx, ty);
            }
            if let Some(p) = &patch {
                for i in p.idx..p.idx + p.nslots.min(self.items.len() - p.idx) {
                    let it = std::mem::replace(
                        &mut self.items[i],
                        DisplayItem::FillRect {
                            x: 0.0,
                            y: 0.0,
                            w: 0.0,
                            h: 0.0,
                            color: 0,
                        },
                    );
                    self.items[i] = offset_item(it, tx, ty);
                }
            }
            for h in self.hits.iter_mut().skip(children_hits_start) {
                h.0 += tx;
                h.1 += ty;
                h.5 = h.5.conjugated(tx, ty);
            }
        }
        if sticky {
            // stays in flow; painted y clamps at the inset edge while the
            // parent is on screen — applied by drain_stickies when the
            // enclosing block finishes
            self.stickies.push(StickyFix {
                i0: patch
                    .as_ref()
                    .map(|p| p.idx)
                    .unwrap_or(children_items_start),
                i1: self.items.len(),
                h0: children_hits_start,
                h1: self.hits.len(),
                top: content_top - pt - bw_t + ty,
                bottom: self.y + ty,
                ins_t,
                ins_b,
            });
        }
        self.margin_x = saved_mx;
        self.avail_w = saved_avail;
        self.align = saved_align;
        self.align_last = saved_align_last;
        self.rtl = saved_rtl;
        self.ellipsis_clip = saved_ellip;
        (self.clamp_left, self.clamping) = saved_clamp;
        // out-of-flow boxes reserve no flow space
        if let Some((fy, fmx, favail)) = flow {
            self.y = fy;
            self.margin_x = fmx;
            self.avail_w = favail;
        }
        self.y += mb.max(0.0);
        if tag == "ul" || tag == "ol" || tag == "menu" || tag == "dir" {
            self.list_stack.pop();
        }
        self.cur_xf = saved_xf;
    }

    /// `float:left|right` block: shrink-to-fit (or `width`-sized), placed at
    /// the margin edge; a float that doesn't fit beside earlier floats drops
    /// below them. Following lines inset via `self.floats` until `bottom`.
    /// The float is a BFC — it contains floats of its own.
    fn float_block(&mut self, node: NodeId, tag: &str, c: &Props, mut st: Style, mt: f32, mb: f32) {
        let size = st.size;
        let [pt, pr, pb, pl] = box_sides(c, "padding", self.avail_w, size).map(|v| v.max(0.0));
        let [_t, mr, _b, ml] = box_sides(c, "margin", self.avail_w, size);
        let bd = self.backdrop;
        let bg = self.bg_of(node, c, Some(st.color)).map(|col| over(col, bd));
        let border = borderfx::borders(c, size, st.color).map(|mut b| {
            for col in b.color.iter_mut() {
                *col = over(*col, bg.unwrap_or(bd));
            }
            b
        });
        st.inline_bg = None;
        let [bw_t, bw_r, bw_b, bw_l] = border.map(|b| b.w).unwrap_or([0.0; 4]);
        let bw_h = bw_l + bw_r;
        let bw_v = bw_t + bw_b;
        let right = c.get("float").is_some_and(|v| v.trim() == "right");
        let bgl = if self.canvas_bg_node == Some(node) {
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
        };

        // replaced floats (floated images) size from their payload box
        let w_prop = dim(c, "width", self.avail_w, size);
        let payload = self.replaced_payload(node, tag, c, &st);
        let inner = if let Some(w) = w_prop {
            let w = if border_box(c) { w - pl - pr - bw_h } else { w };
            w.max(1.0)
        } else if let Some((w, _, _, _)) = &payload {
            *w
        } else {
            // shrink-to-fit: max-content, clamped to the room below
            self.max_content_width(node, &st)
        };

        self.y += mt.max(0.0);
        // `clear`: drop below same-side floats first
        match c.get("clear").map(String::as_str) {
            Some("left") | Some("inline-start") => self.y = self.y.max(self.clear_below(false)),
            Some("right") | Some("inline-end") => self.y = self.y.max(self.clear_below(true)),
            Some("both") => {
                self.y = self
                    .y
                    .max(self.clear_below(false).max(self.clear_below(true)))
            }
            _ => {}
        }
        // a float that doesn't fit beside earlier floats drops below the
        // shallowest blocker (explicit/payload width must fit whole; auto
        // fits at min-content); an empty float list ends the descent
        let need = if w_prop.is_some() || payload.is_some() {
            inner + pl + pr + bw_h
        } else {
            (self.shrink_width(node, &st) + pl + pr + bw_h).min(inner + pl + pr + bw_h)
        };
        for _ in 0..16 {
            let (li, ri) = self.float_insets();
            if need <= self.avail_w - li - ri + 0.5 {
                break;
            }
            let next = self
                .floats
                .iter()
                .filter(|f| self.y >= f.top && self.y < f.bottom)
                .map(|f| f.bottom)
                .fold(f32::INFINITY, f32::min);
            if !next.is_finite() || next <= self.y {
                break;
            }
            self.y = next;
        }
        let (li, ri) = self.float_insets();
        let room = (self.avail_w - li - ri).max(1.0);
        let box_w = (inner + pl + pr + bw_h).min(room);
        let inner = (box_w - pl - pr - bw_h).max(1.0);

        // content-height target: height/min-height/max-height (border-box
        // strips padding+border) or aspect-ratio over the final width
        let ht = |v: f32| if border_box(c) { v - pt - pb - bw_v } else { v };
        let hrel = self.pct_h.unwrap_or(self.viewport_h);
        let pct_h_definite = self.pct_h.is_some();
        let mut h_prop = hdim_pct(c, "height", hrel, size, self.pct_h);
        let mut h_clamped = h_prop.is_some();
        if let Some(mn) = c
            .get("min-height")
            .filter(|v| pct_h_definite || !v.contains('%'))
            .and_then(|v| parse_len_vp(v, hrel, size, self.viewport_h, self.avail_w))
        {
            h_prop = Some(h_prop.map_or(mn, |h| h.max(mn)));
        }
        if let Some(mx) = c
            .get("max-height")
            .filter(|v| pct_h_definite || !v.contains('%'))
            .and_then(|v| parse_len_vp(v, hrel, size, self.viewport_h, self.avail_w))
        {
            h_prop = Some(h_prop.map_or(mx, |h| h.min(mx)));
            h_clamped = true;
        }
        let mh_prop = h_prop.map(ht).or_else(|| {
            aspect_ratio(c).map(|r| {
                if border_box(c) {
                    box_w / r - pt - pb - bw_v
                } else {
                    inner / r
                }
            })
        });

        let top = self.y;
        let f0 = self.floats.len();
        let saved = (
            self.margin_x,
            self.avail_w,
            self.align,
            self.align_last,
            self.rtl,
        );
        let outer_x = if right {
            self.margin_x + self.avail_w - ri - box_w - mr.max(0.0)
        } else {
            self.margin_x + li + ml.max(0.0)
        };
        self.margin_x = outer_x + pl + bw_l;
        self.avail_w = inner;
        self.align = st.align;
        self.align_last = st.align_last;
        self.rtl = st.rtl;
        if st.indent != 0.0 {
            self.indent_pending = st.indent;
            self.line_x += st.indent;
        }

        let shadows = borderfx::parse_shadows(c, size, bd, st.color);
        let outline = borderfx::outline(c, size, bd, st.color);
        let clip = borderfx::clips_overflow(c);
        let radii = borderfx::radius_spec(c);
        let patch = if bg.is_some()
            || border.is_some()
            || !shadows.is_empty()
            || outline.is_some()
            || clip
            || !radii.none()
            || bgl.is_some()
        {
            let idx = self.items.len();
            let n_out = shadows.iter().filter(|s| !s.inset).count();
            let nslots = n_out + 1 + (shadows.len() - n_out) + 4 + clip as usize;
            for _ in 0..nslots {
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
                nslots,
                x: outer_x,
                y: self.y,
                w: box_w,
                em: size,
                bg,
                border,
                radii,
                shadows,
                clip,
                outline,
                bgl,
            })
        } else {
            None
        };

        self.y += pt + bw_t;
        let content_top = self.y;
        // a positioned float is the containing block for positioned
        // descendants — its padding box (emission coords)
        let is_cb = matches!(
            c.get("position").map(String::as_str),
            Some("relative") | Some("absolute") | Some("fixed") | Some("sticky")
        );
        if is_cb {
            let pos_base = self.pct_h.or_else(|| {
                matches!(
                    c.get("position").map(String::as_str),
                    Some("absolute") | Some("fixed")
                )
                .then_some(self.viewport_h)
            });
            let ph = hdim_pct(
                c,
                "height",
                pos_base.unwrap_or(self.viewport_h),
                size,
                pos_base,
            )
            .map(|h| if border_box(c) { h - bw_v } else { h + pt + pb });
            self.cb_stack.push(CbRect {
                x: self.margin_x - pl,
                y: content_top - pt,
                w: pl + inner + pr,
                h: ph.map(|h| h.max(0.0)),
            });
        }
        let saved_bd = self.backdrop;
        if let Some(b) = bg {
            self.backdrop = b;
        }
        let children_hits_start = self.hits.len();
        // ::first-line/::first-letter don't cross float boundaries
        let saved_fl = self.fl_style.take();
        let saved_flet = self.flet.take();
        if let Some((pw, ph, items, hits)) = payload {
            let (tx, ty) = translate_xy(c, pw, ph, size);
            for it in items {
                self.items
                    .push(offset_item(it, self.margin_x + tx, self.y + ty));
            }
            for h in hits {
                self.hits
                    .push(xform::off_hit(h, self.margin_x + tx, self.y + ty));
            }
            self.y += ph;
        } else if tag == "table"
            || matches!(
                c.get("display").map(String::as_str),
                Some("table") | Some("inline-table")
            )
        {
            // a floated table still lays out as a table — the float only
            // contributes the outer box (position, width, margins)
            self.table_inner(node, c, st.clone(), true);
        } else {
            self.walk_children(node, &st, content_top, content_top + mh_prop.unwrap_or(0.0));
        }
        self.fl_style = saved_fl;
        self.flet = saved_flet;
        if is_cb {
            self.cb_stack.pop();
        }
        self.backdrop = saved_bd;
        if let Some(mh) = mh_prop {
            self.y = if h_clamped {
                content_top + mh
            } else {
                self.y.max(content_top + mh)
            };
        }
        self.y += pb + bw_b;
        if let Some(mut p) = patch {
            self.finish_patch(&mut p);
            if p.clip {
                let h = self.y - p.y;
                let bw = p.border.map(|b| b.w).unwrap_or([0.0; 4]);
                let (cx, cy, cw, chh) = borderfx::padding_box(p.x, p.y, p.w, h, bw);
                clip_hits(&mut self.hits, children_hits_start, cx, cy, cw, chh);
            }
        }
        // floats contain their own floats: the bottom covers nested ones
        let mut bottom = self.y;
        for f in &self.floats[f0..] {
            bottom = bottom.max(f.bottom);
        }
        let bottom = bottom + mb.max(0.0);

        self.margin_x = saved.0;
        self.avail_w = saved.1;
        self.align = saved.2;
        self.align_last = saved.3;
        self.rtl = saved.4;
        // floats take no vertical space in normal flow
        self.y = top;
        self.floats.push(Float {
            right,
            x0: outer_x - ml.max(0.0),
            x1: outer_x + box_w + mr.max(0.0),
            top,
            bottom,
        });
    }

    /// `display:inline-block`: a box sitting on the text line like a word.
    /// Content is laid out off-screen via `item_box`/`nest`, then emitted
    /// offset to the box's position on the line, sharing the line baseline.
    fn inline_block(&mut self, node: NodeId, c: &Props, st: Style) {
        self.inline_block_inner(node, c, st, None)
    }

    fn inline_block_inner(&mut self, node: NodeId, c: &Props, st: Style, hint: Option<f32>) {
        let size = st.size;
        let [_t, mr, mb, ml] = box_sides(c, "margin", self.avail_w, size);
        // shared box builder (`flexbox.rs`): same bg/border/padding/nest
        // geometry, plus deco info the flex path uses for height restretches
        let b = self.item_box(node, c, st.clone(), hint, None);
        // baseline: the first line's baseline inside the box; empty boxes
        // align on the bottom margin edge (≈ border bottom here)
        let ascent = b.baseline.unwrap_or(b.h);

        // a pending inter-word space becomes a gap before the box
        if self.pending_space && !self.line.is_empty() {
            let space_w = self
                .resolve_font(&st)
                .and_then(|f| {
                    self.fonts.font(f).map(|f| {
                        f.advance_units(f.glyph_id(' ')) as f32 * st.size / f.units_per_em() as f32
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
        // the box's own transform spec tags its line word — the word run's
        // marker applies it (translate or full matrix alike)
        let saved_xf = if b.xf.is_some() {
            Some(std::mem::replace(&mut self.cur_xf, b.xf.clone()))
        } else {
            None
        };
        self.push_box(
            b.w,
            b.h,
            b.items,
            b.hits,
            mb.max(0.0),
            self.rel_offset(c, size),
            ascent,
            Some(node),
        );
        if let Some(sv) = saved_xf {
            self.cur_xf = sv;
        }

        if mr > 0.0 {
            self.spacer(mr);
        }
    }

    /// Table layout: builds a cell occupancy grid (colspan/rowspan aware),
    /// sizes columns (auto min/max-content or `table-layout:fixed`), then
    /// lays each cell via `nest` at its column position. Handles the
    /// separate (border-spacing / cellspacing) and collapse (shared edges
    /// deduped, wider wins) border models, cell vertical-align, caption-side,
    /// empty-cells, and CSS display:table* boxes (grouped by display role —
    /// bare cells form anonymous rows; col/row-group/caption children get
    /// their roles from `display` or the matching HTML tag).
    fn table(&mut self, node: NodeId, c: &Props, st: Style) {
        self.table_inner(node, c, st, false)
    }

    /// Core table layout. `nested` = the element's own margin/bg/border box
    /// is already handled by the caller (inline-table inside an inline-block
    /// box, or a table element laid via `nest`).
    fn table_inner(&mut self, node: NodeId, c: &Props, mut st: Style, nested: bool) {
        let size = st.size;
        let [mt, _mr, mb, ml] = box_sides(c, "margin", self.avail_w, size);
        let bd = self.backdrop;
        let bg = self.bg_of(node, c, Some(st.color)).map(|col| over(col, bd));
        let border = borderfx::borders(c, size, st.color).map(|mut b| {
            for col in b.color.iter_mut() {
                *col = over(*col, bg.unwrap_or(bd));
            }
            b
        });
        st.inline_bg = None;
        if let Some(b) = bg {
            if let Some(col) = c
                .get("color")
                .and_then(|v| parse_color_ctx(v, Some(st.color)))
            {
                st.color = over(col, b);
            }
        }

        let mut grid = self.table_grid(node, c, &st);
        let (ncols, nrows) = (grid.ncols, grid.nrows);
        let (sp_x, sp_y) = grid.sp;

        if !nested {
            self.y += mt.max(0.0);
        }

        let saved = (
            self.margin_x,
            self.avail_w,
            self.align,
            self.align_last,
            self.rtl,
        );
        if !nested {
            self.margin_x += ml.max(0.0);
            self.avail_w -= ml.max(0.0);
        }
        self.align = st.align;
        self.align_last = st.align_last;
        self.rtl = st.rtl;
        let avail = self.avail_w.max(1.0);

        // `width` attribute (px/%) or CSS `width` sizes the table box
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

        // ---- column widths ----
        let mut col_w = vec![0.0f32; ncols];
        let sp_total = sp_x * (ncols + 1) as f32;
        let fixed = c.get("table-layout").map(String::as_str) == Some("fixed") && ncols > 0;
        if fixed && spec_w.is_some() {
            // fixed layout: <col>/<colgroup> widths → first-row cell widths →
            // equal split of what's left; content never affects columns
            let target = spec_w.unwrap_or(avail).min(avail).max(1.0);
            let mut fw: Vec<Option<ColW>> = vec![None; ncols];
            let mut next = 0usize;
            for &col in &grid.parts.cols {
                let is_group = self.table_role(col) == TRole::ColGroup;
                let kids: Vec<NodeId> = if is_group {
                    self.doc
                        .children(col)
                        .iter()
                        .copied()
                        .filter(|&g| self.table_role(g) == TRole::Col)
                        .collect()
                } else {
                    Vec::new()
                };
                let specs: Vec<NodeId> = if is_group && !kids.is_empty() {
                    kids
                } else {
                    vec![col]
                };
                for s in specs {
                    let span = self.span_attr(s, "span").min(64);
                    let wv = self.col_width_spec(s);
                    if let Some(wv) = wv {
                        for i in next..(next + span).min(ncols) {
                            fw[i] = Some(wv);
                        }
                    }
                    next += span;
                }
            }
            // unspecified columns take the first row's cell widths
            for (tc, ci) in grid.tcells.iter().zip(&grid.infos) {
                if tc.r == 0 && tc.cs == 1 && fw[tc.c].is_none() {
                    fw[tc.c] = self.cell_width_spec(tc.node, &ci.cs);
                }
            }
            let res = |w: ColW| match w {
                ColW::Px(p) => p,
                ColW::Pct(p) => target * p / 100.0,
            };
            let spec_sum: f32 = fw.iter().map(|w| w.map(res).unwrap_or(0.0)).sum();
            let n_free = fw.iter().filter(|w| w.is_none()).count();
            let left = (target - sp_total - spec_sum).max(0.0);
            for i in 0..ncols {
                col_w[i] = fw[i].map(res).unwrap_or(left / n_free.max(1) as f32);
            }
        } else if ncols > 0 {
            // auto layout: width = min(max-content, available) unless
            // specified; grow toward max-content shares, shrink evenly
            col_w.clone_from_slice(&grid.col_min);
            let total: f32 = col_w.iter().sum();
            let pref: f32 = grid.col_pref.iter().sum();
            let auto = (pref.max(total) + sp_total).min(avail);
            let target = spec_w
                .unwrap_or(auto)
                .min(avail)
                .max((total + sp_total).min(avail));
            let deficit = target - sp_total - total;
            if deficit > 0.01 && total > 0.0 {
                let pref_sum: f32 = grid.col_pref.iter().sum::<f32>().max(total);
                for (w, &p) in col_w.iter_mut().zip(&grid.col_pref) {
                    *w = w.max((target - sp_total) * p / pref_sum);
                }
                let cur: f32 = col_w.iter().sum();
                if target - sp_total - cur > 0.01 && cur > 0.0 {
                    for w in col_w.iter_mut() {
                        *w *= (target - sp_total) / cur;
                    }
                }
            } else if deficit < -0.01 && total > 0.0 {
                for w in col_w.iter_mut() {
                    *w *= (target - sp_total) / total;
                }
            }
        }

        // column left edges (separate model leads with one spacing unit)
        let mut col_x = vec![0.0f32; ncols];
        {
            let mut acc = sp_x;
            for (i, &w) in col_w.iter().enumerate() {
                col_x[i] = acc;
                acc += w + sp_x;
            }
        }
        let table_w: f32 = col_w.iter().sum::<f32>() + sp_total;

        if !nested {
            // align="center"|"right", margin:auto or parent text-align:center
            let center = self.doc.attr(node, "align") == Some("center")
                || (margin_auto(c, true) && margin_auto(c, false))
                || saved.2 == Align::Center;
            if center && table_w < avail {
                self.margin_x += (avail - table_w) / 2.0;
            } else if self.doc.attr(node, "align") == Some("right") && table_w < avail {
                self.margin_x += avail - table_w;
            }
        }

        // caption-side:top captions sit above the grid, at table width
        for &cap in &grid.parts.captions {
            if !self.caption_bottom(cap, c) {
                self.lay_caption(cap, &st, table_w);
            }
        }

        let shadows = borderfx::parse_shadows(c, size, bd, st.color);
        let outline = borderfx::outline(c, size, bd, st.color);
        let clip = borderfx::clips_overflow(c);
        let radii = borderfx::radius_spec(c);
        let tpad = box_sides(c, "padding", self.avail_w, size).map(|v| v.max(0.0));
        let tbgl = if self.canvas_bg_node == Some(node) {
            None
        } else {
            bgimg::parse_box(
                c,
                tpad,
                border.map(|b| b.w).unwrap_or([0.0; 4]),
                size,
                self.viewport_h,
                self.viewport_w,
                st.color,
            )
        };
        let patch = if !nested
            && (bg.is_some()
                || border.is_some()
                || !shadows.is_empty()
                || outline.is_some()
                || clip
                || !radii.none()
                || tbgl.is_some())
        {
            let idx = self.items.len();
            let n_out = shadows.iter().filter(|s| !s.inset).count();
            let nslots = n_out + 1 + (shadows.len() - n_out) + 4 + clip as usize;
            for _ in 0..nslots {
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
                nslots,
                x: self.margin_x,
                y: self.y,
                w: table_w,
                em: size,
                bg,
                border,
                radii,
                shadows,
                clip,
                outline,
                bgl: tbgl,
            })
        } else {
            None
        };
        let grid_top = self.y;
        let children_hits_start = self.hits.len();

        // ---- lay cells, measure row heights ----
        let empty_hide = c.get("empty-cells").map(String::as_str) == Some("hide");
        let mut row_h = vec![0.0f32; nrows];
        for (tc, ci) in grid.tcells.iter().zip(grid.infos.iter_mut()) {
            let cw: f32 = col_w[tc.c..tc.c + tc.cs].iter().sum::<f32>() + sp_x * (tc.cs - 1) as f32;
            let [pt, pr, pb, pl] = ci.pad;
            let [bw_t, bw_r, bw_b, bw_l] = ci.border.map(|b| b.w).unwrap_or([0.0; 4]);
            let inner = (cw - pl - pr - bw_l - bw_r).max(1.0);
            let (items, y_end, nhits) =
                self.nest(tc.node, &ci.st, pl + bw_l, pt + bw_t, inner, &ci.cs);
            let min_h = self.eff_line_h(&ci.st);
            ci.cell_h = (y_end + pb + bw_b).max(min_h);
            ci.items = items;
            ci.hits = nhits;
            if tc.rs == 1 {
                row_h[tc.r] = row_h[tc.r].max(ci.cell_h);
            }
        }
        // rowspan cells stretch their spanned rows when they need more room
        for (tc, ci) in grid.tcells.iter().zip(&grid.infos) {
            if tc.rs > 1 {
                let have: f32 =
                    row_h[tc.r..tc.r + tc.rs].iter().sum::<f32>() + sp_y * (tc.rs - 1) as f32;
                if ci.cell_h > have {
                    let add = (ci.cell_h - have) / tc.rs as f32;
                    for h in row_h[tc.r..tc.r + tc.rs].iter_mut() {
                        *h += add;
                    }
                }
            }
        }
        // per-row baseline offset for vertical-align:baseline cells
        let mut row_bl = vec![0.0f32; nrows];
        for (tc, ci) in grid.tcells.iter().zip(&grid.infos) {
            if tc.rs == 1 && ci.valign == TAlign::Baseline {
                let bl = ci.pad[0] + ci.border.map(|b| b.w[0]).unwrap_or(0.0) + ci.first_ascent;
                row_bl[tc.r] = row_bl[tc.r].max(bl);
            }
        }

        // ---- emit ----
        let mut row_y = grid_top + sp_y;
        // nrows may exceed declared rows when a rowspan extends below the
        // last row — the occupancy grid creates those rows implicitly
        for r in 0..grid.nrows {
            // row background (bgcolor attr / background-color on the row el)
            if let Some(Some(re)) = grid.parts.rows.get(r).map(|row| row.1) {
                let rc = self.style(re);
                let rbg = bg_color(&rc, Some(st.color))
                    .or_else(|| {
                        self.doc
                            .attr(re, "bgcolor")
                            .and_then(|v| parse_color_ctx(v, Some(st.color)))
                    })
                    .map(|col| over(col, bd));
                if let Some(col) = rbg {
                    self.items.push(DisplayItem::FillRect {
                        x: self.margin_x,
                        y: row_y,
                        w: table_w,
                        h: row_h[r],
                        color: col,
                    });
                }
            }
            for &ti in grid.row_cells.get(r).map_or(&[][..], Vec::as_slice) {
                let (tc, ci) = (&grid.tcells[ti], &grid.infos[ti]);
                let cx = self.margin_x + col_x[tc.c];
                let cw = col_w[tc.c..tc.c + tc.cs].iter().sum::<f32>() + sp_x * (tc.cs - 1) as f32;
                let ch = row_h[tc.r..tc.r + tc.rs].iter().sum::<f32>() + sp_y * (tc.rs - 1) as f32;
                if !(empty_hide && ci.empty) {
                    if let Some(col) = ci.bg {
                        self.items.push(DisplayItem::FillRect {
                            x: cx,
                            y: row_y,
                            w: cw,
                            h: ch,
                            color: col,
                        });
                    }
                    if let Some(b2) = &ci.border {
                        let w4 = if grid.collapse {
                            collapse_edges(&grid, ti, b2.w)
                        } else {
                            b2.w
                        };
                        let b = Borders {
                            w: w4,
                            style: b2.style,
                            color: b2.color,
                        };
                        borderfx::emit_borders(&mut self.items, cx, row_y, cw, ch, &b);
                    }
                }
                // vertical-align positions content inside the (possibly
                // taller) cell box; spanning cells top-align by default
                let dy = match ci.valign {
                    TAlign::Middle => ((ch - ci.cell_h) / 2.0).max(0.0),
                    TAlign::Bottom => (ch - ci.cell_h).max(0.0),
                    TAlign::Baseline if tc.rs == 1 => (row_bl[tc.r]
                        - (ci.pad[0] + ci.border.map(|b| b.w[0]).unwrap_or(0.0) + ci.first_ascent))
                        .max(0.0),
                    _ => 0.0,
                };
                for it in &ci.items {
                    self.items.push(offset_item(it.clone(), cx, row_y + dy));
                }
                for h in &ci.hits {
                    self.push_hit(xform::off_hit(*h, cx, row_y + dy));
                }
            }
            row_y += row_h[r] + sp_y;
        }
        self.y = row_y;

        if let Some(mut p) = patch {
            self.finish_patch(&mut p);
            if p.clip {
                let h = self.y - p.y;
                let bw = p.border.map(|b| b.w).unwrap_or([0.0; 4]);
                let (cx, cy, cw, chh) = borderfx::padding_box(p.x, p.y, p.w, h, bw);
                clip_hits(&mut self.hits, children_hits_start, cx, cy, cw, chh);
            }
        }
        // caption-side:bottom captions go under the grid
        for &cap in &grid.parts.captions {
            if self.caption_bottom(cap, c) {
                self.lay_caption(cap, &st, table_w);
            }
        }
        self.margin_x = saved.0;
        self.avail_w = saved.1;
        self.align = saved.2;
        self.align_last = saved.3;
        self.rtl = saved.4;
        if !nested {
            self.y += mb.max(0.0);
        }
        // stray children (nested tables, weird markup) laid after the grid
        for &e in &grid.parts.extra {
            self.walk(e, &st);
        }
    }

    /// Gather rows/cells/captions/cols, place cells on the rowspan-aware
    /// occupancy grid, and measure each cell's box metrics + intrinsic
    /// widths. Shared by the block table path and inline-table measuring.
    fn table_grid(&mut self, node: NodeId, c: &Props, st: &Style) -> TGrid {
        let collapse = c.get("border-collapse").map(String::as_str) == Some("collapse");
        let sp = if collapse {
            (0.0, 0.0)
        } else if let Some(v) = c.get("border-spacing") {
            let mut it = v.split_whitespace();
            let x = it
                .next()
                .and_then(|t| parse_len(t, self.avail_w, st.size))
                .unwrap_or(0.0);
            let y = it
                .next()
                .and_then(|t| parse_len(t, self.avail_w, st.size))
                .unwrap_or(x);
            (x.max(0.0), y.max(0.0))
        } else {
            match self
                .doc
                .attr(node, "cellspacing")
                .and_then(|v| v.trim().parse::<f32>().ok())
            {
                Some(v) => (v.max(0.0), v.max(0.0)),
                // HTML default cellspacing=2; CSS display:table starts at 0
                None => {
                    if self.doc.tag(node) == Some("table") {
                        (2.0, 2.0)
                    } else {
                        (0.0, 0.0)
                    }
                }
            }
        };
        let bd = self.backdrop;
        let bg = self.bg_of(node, c, Some(st.color)).map(|col| over(col, bd));
        // <table border=N> fallback cell borders (border="0" = none)
        let attr_border = self
            .doc
            .attr(node, "border")
            .and_then(|v| v.trim().parse::<f32>().ok())
            .filter(|w| *w > 0.0)
            .map(|w| Borders {
                w: [w; 4],
                style: [BorderStyle::Solid; 4],
                color: [BLACK; 4],
            });
        let cellpadding = self
            .doc
            .attr(node, "cellpadding")
            .and_then(|v| v.trim().parse::<f32>().ok());

        // gather rows/cells/captions/cols by display role
        let mut parts = TableParts::default();
        {
            let mut implicit = Vec::new();
            self.gather_level(node, 1, &mut parts, &mut implicit);
            flush_implicit(1, &mut parts, &mut implicit);
        }
        parts.rows.sort_by_key(|r| r.0); // thead → tbody → tfoot

        // occupancy grid: rowspan slots in later rows are marked occupied so
        // following cells keep column alignment
        let nrows_src = parts.rows.len();
        let mut occ: Vec<Vec<usize>> = Vec::new();
        let mut row_cells: Vec<Vec<usize>> = vec![Vec::new(); nrows_src];
        let mut tcells: Vec<TCell> = Vec::new();
        let mut ncols = 0usize;
        for (r, (_, _, cells)) in parts.rows.iter().enumerate() {
            while occ.len() <= r {
                occ.push(Vec::new());
            }
            let mut col = 0usize;
            for &cell in cells {
                // colspan=0 ("to end of group") is approximated as 1 — the
                // column count isn't known until rows are placed
                let cs = {
                    let a = self.span_attr(cell, "colspan");
                    if a == usize::MAX {
                        1
                    } else {
                        a
                    }
                };
                let rs = {
                    let a = self.span_attr(cell, "rowspan");
                    if a == usize::MAX {
                        nrows_src - r
                    } else {
                        a.min((nrows_src - r).max(1))
                    }
                };
                // first free column position for this cell
                loop {
                    let rowv = &occ[r];
                    let mut free = true;
                    for i in col..col + cs {
                        if i < rowv.len() && rowv[i] != usize::MAX {
                            free = false;
                            break;
                        }
                    }
                    if free {
                        break;
                    }
                    col += 1;
                }
                let idx = tcells.len();
                for rr in r..r + rs {
                    while occ.len() <= rr {
                        occ.push(Vec::new());
                    }
                    if occ[rr].len() < col + cs {
                        occ[rr].resize(col + cs, usize::MAX);
                    }
                    for i in col..col + cs {
                        occ[rr][i] = idx;
                    }
                }
                ncols = ncols.max(col + cs);
                row_cells[r].push(idx);
                tcells.push(TCell {
                    node: cell,
                    r,
                    c: col,
                    rs,
                    cs,
                });
                col += cs;
            }
        }
        let nrows = occ.len().max(nrows_src);

        // row-level vertical-align (tr valign attr / vertical-align)
        let row_va: Vec<Option<TAlign>> = parts
            .rows
            .iter()
            .map(|(_, re, _)| {
                re.and_then(|re| {
                    self.doc.attr(re, "valign").and_then(talign).or_else(|| {
                        let rs = self.style(re);
                        rs.get("vertical-align").and_then(|v| talign(v))
                    })
                })
            })
            .collect();

        // per-cell metrics + intrinsic widths
        let mut infos: Vec<TCellInfo> = Vec::with_capacity(tcells.len());
        let mut col_min = vec![0.0f32; ncols];
        let mut col_pref = vec![0.0f32; ncols];
        for tc in &tcells {
            let cs = self.style(tc.node);
            let mut cst = st.clone();
            self.apply_computed(&cs, &mut cst);
            cst.inline_bg = None; // cell bg paints the whole cell
            match self.doc.attr(tc.node, "align") {
                Some("right") => cst.align = Align::Right,
                Some("center") | Some("middle") => cst.align = Align::Center,
                _ => {}
            }
            let mut pad = box_sides(&cs, "padding", self.avail_w, cst.size).map(|v| v.max(0.0));
            if pad == [0.0; 4] {
                if let Some(cp) = cellpadding {
                    pad = [cp.max(0.0); 4];
                }
            }
            let cborder = borderfx::borders(&cs, cst.size, cst.color)
                .or(attr_border)
                .map(|mut b| {
                    for col in b.color.iter_mut() {
                        *col = over(*col, bg.unwrap_or(bd));
                    }
                    b
                });
            let [_, cbw_r, _, cbw_l] = cborder.map(|b| b.w).unwrap_or([0.0; 4]);
            let cbw_h = cbw_l + cbw_r;
            let cbg = cs
                .get("background-color")
                .and_then(|v| parse_color_ctx(v, Some(cst.color)))
                .or_else(|| {
                    self.doc
                        .attr(tc.node, "bgcolor")
                        .and_then(|v| parse_color_ctx(v, Some(cst.color)))
                })
                .map(|col| over(col, bg.unwrap_or(bd)));
            let valign = cs
                .get("vertical-align")
                .and_then(|v| talign(v))
                .or_else(|| self.doc.attr(tc.node, "valign").and_then(talign))
                .or(row_va.get(tc.r).copied().flatten())
                // UA gives tr vertical-align:middle (picked up via row_va);
                // bare display:table-cell boxes default to baseline
                .unwrap_or(TAlign::Baseline);
            let empty = !self.cell_has_content(tc.node);
            let first_ascent = self
                .resolve_font(&cst)
                .and_then(|fi| self.fonts.font(fi).map(|f| f.ascent(cst.size)))
                .unwrap_or(cst.size * 0.8);
            let mut w = self.shrink_width(tc.node, &cst) + pad[1] + pad[3] + cbw_h;
            let mut pw = self.max_content_width(tc.node, &cst) + pad[1] + pad[3] + cbw_h;
            if let Some(wp) = dim(&cs, "width", self.avail_w, cst.size) {
                let wp = if border_box(&cs) {
                    wp
                } else {
                    wp + pad[1] + pad[3] + cbw_h
                };
                w = w.max(wp);
                pw = pw.max(wp);
            }
            if tc.cs == 1 {
                col_min[tc.c] = col_min[tc.c].max(w);
                col_pref[tc.c] = col_pref[tc.c].max(pw);
            }
            infos.push(TCellInfo {
                cs,
                st: cst,
                pad,
                border: cborder,
                bg: cbg,
                valign,
                first_ascent,
                minw: w,
                prefw: pw,
                empty,
                items: Vec::new(),
                hits: Vec::new(),
                cell_h: 0.0,
            });
        }
        // colspan cells share their min/max-content widths across the
        // columns they span
        for (tc, ci) in tcells.iter().zip(&infos) {
            if tc.cs > 1 {
                for (acc, cell_w) in [(&mut col_min, ci.minw), (&mut col_pref, ci.prefw)] {
                    let cur: f32 = acc[tc.c..tc.c + tc.cs].iter().sum();
                    if cell_w > cur {
                        let add = (cell_w - cur) / tc.cs as f32;
                        for w in acc[tc.c..tc.c + tc.cs].iter_mut() {
                            *w += add;
                        }
                    }
                }
            }
        }

        TGrid {
            parts,
            tcells,
            infos,
            occ,
            row_cells,
            ncols,
            nrows,
            sp,
            collapse,
            col_min,
            col_pref,
        }
    }

    /// colspan/rowspan/span attribute (0 ⇒ "to the end"; clamped ≥1)
    fn span_attr(&self, cell: NodeId, name: &str) -> usize {
        match self.doc.attr(cell, name).map(|v| v.trim()) {
            Some("0") => usize::MAX,
            Some(v) => v.parse::<usize>().unwrap_or(1).max(1).min(1000),
            None => 1,
        }
    }

    /// Element's role in the table box tree: `display` wins, then the HTML
    /// tag. Used by both real <table> and CSS display:table structures.
    fn table_role(&mut self, node: NodeId) -> TRole {
        if !matches!(self.doc.kind(node), NodeKind::Element { .. }) {
            return TRole::Skip;
        }
        let c = self.style(node);
        match c.get("display").map(String::as_str) {
            Some("none") => TRole::Skip,
            Some("contents") => TRole::Contents,
            Some("table-row") => TRole::Row,
            Some("table-cell") => TRole::Cell,
            Some("table-caption") => TRole::Caption,
            Some("table-column") => TRole::Col,
            Some("table-column-group") => TRole::ColGroup,
            Some("table-header-group") => TRole::RowGroup(0),
            Some("table-footer-group") => TRole::RowGroup(2),
            Some("table-row-group") => TRole::RowGroup(1),
            Some("table") | Some("inline-table") => TRole::Other,
            _ => match self.doc.tag(node) {
                Some("tr") => TRole::Row,
                Some("td") | Some("th") => TRole::Cell,
                Some("caption") => TRole::Caption,
                Some("col") => TRole::Col,
                Some("colgroup") => TRole::ColGroup,
                Some("thead") => TRole::RowGroup(0),
                Some("tfoot") => TRole::RowGroup(2),
                Some("tbody") => TRole::RowGroup(1),
                _ => TRole::Other,
            },
        }
    }

    /// Collect table parts at one level: rows (with anonymous rows for bare
    /// cells), captions, column specs, stray content. `ord` is the row-group
    /// ordering key (0 thead, 1 body, 2 tfoot). `display:contents` recurses.
    fn gather_level(
        &mut self,
        node: NodeId,
        ord: u8,
        parts: &mut TableParts,
        implicit: &mut Vec<NodeId>,
    ) {
        for &ch in self.doc.children(node) {
            if !matches!(self.doc.kind(ch), NodeKind::Element { .. }) {
                parts.extra.push(ch); // text/comments render after the grid
                continue;
            }
            match self.table_role(ch) {
                TRole::Skip => {}
                TRole::Contents => self.gather_level(ch, ord, parts, implicit),
                TRole::Cell => implicit.push(ch),
                TRole::Row => {
                    flush_implicit(ord, parts, implicit);
                    let cells = self.row_cells(ch);
                    parts.rows.push((ord, Some(ch), cells));
                }
                TRole::RowGroup(o) => {
                    flush_implicit(ord, parts, implicit);
                    let mut inner = Vec::new();
                    self.gather_level(ch, o, parts, &mut inner);
                    flush_implicit(o, parts, &mut inner);
                }
                TRole::Caption => {
                    flush_implicit(ord, parts, implicit);
                    parts.captions.push(ch);
                }
                TRole::Col | TRole::ColGroup => {
                    flush_implicit(ord, parts, implicit);
                    parts.cols.push(ch);
                }
                TRole::Other => {
                    flush_implicit(ord, parts, implicit);
                    parts.extra.push(ch);
                }
            }
        }
    }

    /// Element children of a row box — every element child acts as a cell
    /// (anonymous-cell approximation); display:contents recurses.
    fn row_cells(&mut self, row: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        self.row_cells_into(row, &mut out);
        out
    }

    fn row_cells_into(&mut self, row: NodeId, out: &mut Vec<NodeId>) {
        for &ch in self.doc.children(row) {
            match self.table_role(ch) {
                TRole::Skip => {}
                TRole::Contents => self.row_cells_into(ch, out),
                _ => out.push(ch),
            }
        }
    }

    /// empty-cells support: whitespace-only text and nothing else ⇒ empty.
    /// NBSP counts as content; a lone <br> or display:none child doesn't.
    fn cell_has_content(&mut self, node: NodeId) -> bool {
        for &ch in self.doc.children(node) {
            match self.doc.kind(ch) {
                NodeKind::Text(t) => {
                    if t.chars().any(|ch| !ch.is_whitespace() || ch == '\u{a0}') {
                        return true;
                    }
                }
                NodeKind::Element { .. } => {
                    if self.doc.tag(ch) == Some("br") {
                        continue;
                    }
                    let cc = self.style(ch);
                    if cc.get("display").map(String::as_str) == Some("none") {
                        continue;
                    }
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    /// `<col>`/`<colgroup>` width attribute (px or %) or CSS width.
    fn col_width_spec(&mut self, col: NodeId) -> Option<ColW> {
        let raw = self
            .doc
            .attr(col, "width")
            .map(String::from)
            .or_else(|| self.style(col).get("width").cloned());
        raw.and_then(|v| colw(&v))
    }

    /// First-row cell width for table-layout:fixed (attr px/% or CSS width).
    fn cell_width_spec(&mut self, cell: NodeId, cs: &Props) -> Option<ColW> {
        let raw = self
            .doc
            .attr(cell, "width")
            .map(String::from)
            .or_else(|| cs.get("width").cloned());
        raw.and_then(|v| colw(&v))
    }

    /// caption-side resolution (caption's own value, else the table's).
    fn caption_bottom(&mut self, cap: NodeId, c: &Props) -> bool {
        let cs = self.style(cap);
        matches!(
            cs.get("caption-side")
                .or_else(|| c.get("caption-side"))
                .map(String::as_str),
            Some("bottom")
        )
    }

    /// Lay a caption box at table width, at the current y.
    fn lay_caption(&mut self, cap: NodeId, st: &Style, w: f32) {
        let cc = self.style(cap);
        let mut cst = st.clone();
        self.apply_computed(&cc, &mut cst);
        let (x, y) = (self.margin_x, self.y);
        let (items, y_end, hits) = self.nest(cap, &cst, 0.0, 0.0, w.max(1.0), &cc);
        for it in items {
            self.items.push(offset_item(it, x, y));
        }
        for h in hits {
            self.hits.push(xform::off_hit(h, x, y));
        }
        self.y += y_end;
    }

    /// Shrink-wrap width for `display:inline-table`: the auto-layout
    /// max-content width (columns + spacing).
    fn table_pref_width(&mut self, node: NodeId, c: &Props, st: &Style) -> f32 {
        let grid = self.table_grid(node, c, st);
        let total: f32 = grid.col_min.iter().sum();
        let pref: f32 = grid.col_pref.iter().sum();
        pref.max(total) + grid.sp.0 * (grid.ncols + 1) as f32
    }

    /// Lay out `node`'s children off-screen into a private item list.
    /// Content box: origin (x0, y0), `avail` wide. Returns (items, bottom y).
    /// `display:flex` — order-aware, wrapping-capable line layout.
    /// Implementation lives in `flexbox.rs` (`flex_layout`): items are
    /// sorted by `order`, packed into lines (`flex-wrap`), sized per line
    /// (grow/shrink/clamps/auto margins), aligned (`justify-content`,
    /// `align-items`/`align-self`, `align-content`) and emitted at explicit
    /// coordinates.
    fn flex_children(&mut self, node: NodeId, c: &Props, st: Style) {
        // flex items are atomic boxes, not part of an enclosing inline's
        // transform run — their own specs wrap their emitted spans
        let saved_xf = self.cur_xf.take();
        self.flex_layout(node, c, st);
        self.cur_xf = saved_xf;
    }

    /// `display:grid|inline-grid` children. Resolve the track templates,
    /// place items (explicit line numbers or row/column auto-flow), size
    /// tracks (fixed lengths, `fr` shares of leftover space, content-sized
    /// auto/minmax), then lay each item into its cell rect via `nest` and
    /// emit its box at the aligned offset.
    /// Subset: no named lines/areas/subgrid; non-element children skipped.
    fn grid_children(&mut self, node: NodeId, c: &Props, st: &Style) {
        // grid items are atomic boxes — not part of an enclosing inline's
        // transform run (their own spec wraps their emitted span below)
        let saved_xf = self.cur_xf.take();
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
            border: Option<Borders>,
            radius: RadiusSpec,
            shadows: Vec<BoxShadow>,
            outline: Option<Outline>,
            /// overflow clips descendants to the padding box
            clip: bool,
            bb: bool,
            w_prop: Option<f32>,
            h_prop: Option<f32>,
            min_h: Option<f32>,
            max_h: Option<f32>,
            intrinsic_w: f32,
            ratio: Option<f32>,
            just: GAlign,
            al: GAlign,
            bgl: Option<bgimg::BoxBgs>,
            laid: Vec<DisplayItem>,
            /// child element boxes laid relative to the item's content box —
            /// offset to (ix, iy+dy) at emit for click hit-testing
            hits: Vec<Hit>,
            /// border-box height of the laid content (pre-stretch)
            box_h_intr: f32,
            /// min-content content width — contribution to min-content /
            /// fit-content / auto-min tracks
            min_w: f32,
            /// content-box width floor: explicit min-width, else the
            /// min-width:auto min-content minimum (0 when overflow clips)
            min_wc: f32,
            /// content-box width ceiling from max-width
            max_wc: Option<f32>,
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
                .and_then(|t| {
                    parse_len_vp(t, cont_h.unwrap_or(avail), st.size, self.viewport_h, avail)
                })
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
            .map(String::from)
            .and_then(|v| {
                // `subgrid` adopts the parent grid's spanned tracks —
                // recorded on self by the parent's sizing pass; a non-grid
                // parent leaves it templateless (all auto)
                if v.trim() == "subgrid" {
                    self.subgrid_cols.get(&node).cloned()
                } else {
                    Some(v)
                }
            });
        let row_src = c
            .get("grid-template-rows")
            .map(String::as_str)
            .or(t_rows.as_deref())
            .map(String::from)
            .and_then(|v| {
                if v.trim() == "subgrid" {
                    self.subgrid_rows.get(&node).cloned()
                } else {
                    Some(v)
                }
            });
        let (col_specs, col_names, col_fit) = grid_track_list(
            col_src.as_ref(),
            avail,
            st.size,
            self.viewport_h,
            avail,
            avail,
            col_gap,
        );
        let (row_specs, row_names, row_fit) = grid_track_list(
            row_src.as_ref(),
            row_rel,
            st.size,
            self.viewport_h,
            avail,
            row_rel,
            row_gap,
        );
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
        let (auto_cols, _, _) = grid_track_list(
            c.get("grid-auto-columns"),
            avail,
            st.size,
            self.viewport_h,
            avail,
            avail,
            col_gap,
        );
        let (auto_rows, _, _) = grid_track_list(
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

        // container content distribution — resolved early: `stretch` (and
        // `normal`) inflates `auto`-max tracks during column/row sizing,
        // the other keywords shift/gap the finished track block at emit
        let (pc_a, pc_j) = c
            .get("place-content")
            .map(|v| {
                let mut it = v.split_whitespace();
                let a = it.next().unwrap_or("start").to_string();
                let j = it.next().map(String::from).unwrap_or_else(|| a.clone());
                (a, j)
            })
            .unwrap_or_default();
        let jc = c
            .get("justify-content")
            .or(if pc_j.is_empty() { None } else { Some(&pc_j) });
        let ac = c
            .get("align-content")
            .or(if pc_a.is_empty() { None } else { Some(&pc_a) });
        let stretch_cols = matches!(
            jc.map(|s| s.split_whitespace().last().unwrap_or("normal")),
            None | Some("normal") | Some("stretch") | Some("auto")
        );
        let stretch_rows = matches!(
            ac.map(|s| s.split_whitespace().last().unwrap_or("normal")),
            None | Some("normal") | Some("stretch") | Some("auto")
        );

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
                (Some(c0), c1 - c0 + 1, Some(r0), r1 - r0 + 1)
            } else {
                let (ecol, cs) = resolve_axis(grid_lines(&ic, "column"), exp_cols, &col_names);
                let (erow, rs) = resolve_axis(grid_lines(&ic, "row"), exp_rows, &row_names);
                (ecol, cs, erow, rs)
            };
            ncols = ncols.max(ecol.map(|cix| cix + cs).unwrap_or(cs));
            req.push((ch, ecol, cs, erow, rs));
        }

        let flow = c.get("grid-auto-flow").map(String::as_str).unwrap_or("");
        let col_flow = flow.contains("column");
        let dense = flow.contains("dense");
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
                        if dense {
                            // dense column flow: every column is rescanned
                            // from its first row — earlier holes backfill
                            let mut cix = 0usize;
                            'scan: loop {
                                if cix + cs > ncols {
                                    ncols = cix + cs; // implicit columns
                                    for row in occ.iter_mut() {
                                        row.resize(ncols, false);
                                    }
                                }
                                for r in 0..row_bound.saturating_sub(rs) + 1 {
                                    if grid_free(&occ, ncols, r, cix, rs, cs) {
                                        break 'scan (r, cix);
                                    }
                                }
                                cix += 1;
                            }
                        } else {
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
                        }
                    } else if dense {
                        // dense row flow: a row-major scan from the top
                        // backfills holes sparse flow leaves behind
                        let mut r = 0usize;
                        'scan: loop {
                            for cix in 0..ncols {
                                if cix + cs <= ncols && grid_free(&occ, ncols, r, cix, rs, cs) {
                                    break 'scan (r, cix);
                                }
                            }
                            r += 1;
                        }
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

        // repeat(auto-fit): expanded tracks no item lands in collapse to
        // 0px — auto-fill's stay (same count, only collapse differs);
        // their adjacent gutters fold away at emit time
        let mut col_collapsed = vec![false; ncols];
        for (i, f) in col_fit.iter().enumerate().take(ncols) {
            if *f && !placed.iter().any(|p| (p.2..p.2 + p.4).contains(&i)) {
                col_collapsed[i] = true;
            }
        }
        let mut row_collapsed = vec![false; nrows];
        for (i, f) in row_fit.iter().enumerate().take(nrows) {
            if *f && !placed.iter().any(|p| (p.1..p.1 + p.3).contains(&i)) {
                row_collapsed[i] = true;
            }
        }

        // ---- column track sizes ----
        let col_spec: Vec<GTrack> = (0..ncols)
            .map(|i| spec_at(&col_specs, &auto_cols, i))
            .collect();
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
            let bg = self
                .bg_of(ch, &cc, Some(cst.color))
                .map(|col| over(col, bd));
            let border = borderfx::borders(&cc, cst.size, cst.color).map(|mut b| {
                for col in b.color.iter_mut() {
                    *col = over(*col, bg.unwrap_or(bd));
                }
                b
            });
            let bw = border.map(|b| b.w).unwrap_or([0.0; 4]);
            let radius = borderfx::radius_spec(&cc);
            let shadows = borderfx::parse_shadows(&cc, cst.size, bd, cst.color);
            let outline = borderfx::outline(&cc, cst.size, bd, cst.color);
            let clip = borderfx::clips_overflow(&cc);
            let w_prop = dim(&cc, "width", avail, cst.size).map(|w| {
                let mut w = w;
                if let Some(mx) = cc
                    .get("max-width")
                    .and_then(|v| parse_len_vp(v, avail, cst.size, self.viewport_h, avail))
                {
                    w = w.min(mx);
                }
                if let Some(mn) = cc
                    .get("min-width")
                    .and_then(|v| parse_len_vp(v, avail, cst.size, self.viewport_h, avail))
                {
                    w = w.max(mn);
                }
                // border-box `width` → strip padding+border for content size
                if bb {
                    w - p[1] - p[3] - bw[1] - bw[3]
                } else {
                    w
                }
            });
            // auto tracks size to the item's max-content contribution
            // (minmax(0,X) clamps at X) — min-content would wrap every word
            let intrinsic_w = w_prop
                .unwrap_or_else(|| self.max_content_width(ch, &cst).min(avail))
                .max(0.0);
            // min-content contribution: floors min-content / fit-content /
            // auto-min tracks and the `min-width:auto` item minimum
            let min_w = self.min_content_w(ch, &cst);
            let wconv = |v: f32| {
                if bb {
                    v - p[1] - p[3] - bw[1] - bw[3]
                } else {
                    v
                }
            };
            let overflow_clips = matches!(
                cc.get("overflow-x")
                    .or_else(|| cc.get("overflow"))
                    .map(String::as_str),
                Some("hidden") | Some("scroll") | Some("auto") | Some("clip")
            );
            let min_wc = cc
                .get("min-width")
                .and_then(|v| parse_len_vp(v, avail, cst.size, self.viewport_h, avail))
                .map(wconv)
                .unwrap_or(if overflow_clips { 0.0 } else { min_w })
                .max(0.0);
            let max_wc = cc
                .get("max-width")
                .and_then(|v| parse_len_vp(v, avail, cst.size, self.viewport_h, avail))
                .map(wconv);
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
            let hconv = |v: f32| {
                if bb {
                    v - p[0] - p[2] - bw[0] - bw[2]
                } else {
                    v
                }
            };
            let ihbase = cont_h.or(self.pct_h);
            let ihrel = ihbase.unwrap_or(avail);
            let h_prop = hdim_pct(&cc, "height", ihrel, cst.size, ihbase).map(hconv);
            let min_h = cc
                .get("min-height")
                .filter(|v| ihbase.is_some() || !v.contains('%'))
                .and_then(|v| parse_len_vp(v, ihrel, cst.size, self.viewport_h, avail))
                .map(hconv);
            let max_h = cc
                .get("max-height")
                .filter(|v| ihbase.is_some() || !v.contains('%'))
                .and_then(|v| parse_len_vp(v, ihrel, cst.size, self.viewport_h, avail))
                .map(hconv);
            let ratio = aspect_ratio(&cc);
            let bgl = if self.canvas_bg_node == Some(ch) {
                None
            } else {
                bgimg::parse_box(
                    &cc,
                    p,
                    bw,
                    cst.size,
                    self.viewport_h,
                    self.viewport_w,
                    cst.color,
                )
            };
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
                shadows,
                outline,
                clip,
                bb,
                w_prop,
                h_prop,
                min_h,
                max_h,
                intrinsic_w,
                ratio,
                just,
                al,
                bgl,
                laid: Vec::new(),
                hits: Vec::new(),
                box_h_intr: 0.0,
                min_w,
                min_wc,
                max_wc,
            });
        }

        // tracks grow to fit items: a single-span item contributes its
        // min-content and max-content margin-box sizes at the track's min
        // and max breadths (growth capped — fit-content limits, minmax max)
        for it in &items {
            if it.cs == 1 {
                let sides = it.p[1] + it.p[3] + it.bw[1] + it.bw[3] + it.m[1] + it.m[3];
                let c_min = it.min_w + sides;
                let c_max = it.intrinsic_w + sides;
                let (_, mn, mxk, cap, _) = track_sem(&col_spec[it.c]);
                let lo = match mn {
                    1 => c_min,
                    2 => c_max,
                    _ => 0.0,
                };
                let grow = match mxk {
                    1 => c_min,
                    2 => c_max,
                    _ => 0.0,
                };
                cw[it.c] = cw[it.c].max(lo).max(grow.min(cap));
            }
        }
        for it in &items {
            if it.cs > 1 {
                let need =
                    it.intrinsic_w + it.p[1] + it.p[3] + it.bw[1] + it.bw[3] + it.m[1] + it.m[3];
                let have: f32 =
                    cw[it.c..it.c + it.cs].iter().sum::<f32>() + col_gap * (it.cs - 1) as f32;
                if need > have {
                    // growable (intrinsic) tracks absorb the deficit; a
                    // fixed track never grows — the content overflows it
                    let targets: Vec<usize> = (it.c..it.c + it.cs)
                        .filter(|&i| {
                            matches!(
                                col_spec[i],
                                GTrack::Auto(..) | GTrack::C { .. } | GTrack::Fr(..)
                            )
                        })
                        .collect();
                    if !targets.is_empty() {
                        let add = (need - have) / targets.len() as f32;
                        for i in targets {
                            cw[i] += add;
                        }
                    }
                }
            }
        }
        // collapsed auto-fit tracks are inert — 0px, no fr share, no stretch
        for (i, col) in col_collapsed.iter().enumerate() {
            if *col {
                cw[i] = 0.0;
            }
        }
        // fr columns share the leftover space
        let total_fr: f32 = col_spec
            .iter()
            .enumerate()
            .map(|(i, t)| {
                if col_collapsed[i] {
                    0.0
                } else if let GTrack::Fr(f, _) = t {
                    *f
                } else {
                    0.0
                }
            })
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
                    if !col_collapsed[i] {
                        cw[i] = (*m).max(leftover * *f / total_fr);
                    }
                }
            }
        }
        // justify-content normal|stretch: leftover space inflates
        // `auto`-max columns evenly (fr already ate what it wanted)
        if stretch_cols {
            let stretchable: Vec<usize> = (0..ncols)
                .filter(|&i| track_sem(&col_spec[i]).4 && !col_collapsed[i])
                .collect();
            if !stretchable.is_empty() {
                let used: f32 = cw.iter().sum::<f32>() + col_gap * ncols.saturating_sub(1) as f32;
                let extra = (avail - used).max(0.0);
                if extra > 0.0 {
                    let add = extra / stretchable.len() as f32;
                    for i in stretchable {
                        cw[i] += add;
                    }
                }
            }
        }
        // `subgrid` children adopt the sizes of the parent tracks their
        // cell spans — rows only get explicit floors here (content-sized
        // row heights aren't known until items are measured)
        for it in &items {
            if matches!(
                it.cc.get("display").map(String::as_str),
                Some("grid") | Some("inline-grid")
            ) {
                if it
                    .cc
                    .get("grid-template-columns")
                    .is_some_and(|v| v.trim() == "subgrid")
                {
                    let s: Vec<String> = cw[it.c..it.c + it.cs]
                        .iter()
                        .map(|w| format!("{w}px"))
                        .collect();
                    self.subgrid_cols.insert(it.id, s.join(" "));
                }
                if it
                    .cc
                    .get("grid-template-rows")
                    .is_some_and(|v| v.trim() == "subgrid")
                {
                    let s: Vec<String> = (it.r..it.r + it.rs)
                        .map(|i| format!("{}px", track_floor(&spec_at(&row_specs, &auto_rows, i))))
                        .collect();
                    self.subgrid_rows.insert(it.id, s.join(" "));
                }
            }
        }
        // ---- measure items at their final content width ----
        for it in items.iter_mut() {
            let cell_w: f32 =
                cw[it.c..it.c + it.cs].iter().sum::<f32>() + col_gap * (it.cs - 1) as f32;
            let inner_w =
                (cell_w - it.m[1] - it.m[3] - it.p[1] - it.p[3] - it.bw[1] - it.bw[3]).max(1.0);
            let content_w = it
                .w_prop
                .map(|w| w.min(inner_w))
                .unwrap_or(match it.just {
                    GAlign::Stretch => inner_w,
                    _ => it.intrinsic_w.min(inner_w),
                })
                .min(it.max_wc.unwrap_or(f32::MAX))
                .max(it.min_wc)
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
        let row_spec: Vec<GTrack> = (0..nrows)
            .map(|i| spec_at(&row_specs, &auto_rows, i))
            .collect();
        // fr rows with an indefinite container height behave as auto —
        // they floor at 0 and grow to content below
        let mut rh: Vec<f32> = row_spec
            .iter()
            .map(|t| match *t {
                GTrack::Fr(..) if cont_h.is_none() => 0.0,
                _ => track_floor(t),
            })
            .collect();
        for it in &items {
            let need = it.box_h_intr + it.m[0] + it.m[2];
            if it.rs == 1 {
                // row min/max breadths share one content height — either
                // kind contributes `need` (growth still honors the cap)
                let (mn, mxk, cap) = match row_spec[it.r] {
                    GTrack::Auto(_, mx) => (0u8, 2u8, mx),
                    GTrack::C { min, max, cap, .. } => (min, max, cap),
                    GTrack::Fr(..) if cont_h.is_none() => (0, 2, f32::MAX),
                    _ => (0, 0, f32::MAX),
                };
                let lo = if mn > 0 { need } else { 0.0 };
                let grow = if mxk > 0 { need.min(cap) } else { 0.0 };
                rh[it.r] = rh[it.r].max(lo).max(grow);
            }
        }
        for it in &items {
            if it.rs > 1 {
                let need = it.box_h_intr + it.m[0] + it.m[2];
                let have: f32 =
                    rh[it.r..it.r + it.rs].iter().sum::<f32>() + row_gap * (it.rs - 1) as f32;
                if need > have {
                    let mut targets: Vec<usize> = (it.r..it.r + it.rs)
                        .filter(|&i| match row_spec[i] {
                            GTrack::Auto(..) | GTrack::C { .. } => true,
                            GTrack::Fr(..) => cont_h.is_none(),
                            GTrack::Fixed(_) => false,
                        })
                        .collect();
                    if targets.is_empty() {
                        targets = (it.r..it.r + it.rs).collect();
                    }
                    let add = (need - have) / targets.len() as f32;
                    for i in targets {
                        rh[i] += add;
                    }
                }
            }
        }
        // collapsed auto-fit rows are inert — 0px, no fr share, no stretch
        for (i, col) in row_collapsed.iter().enumerate() {
            if *col {
                rh[i] = 0.0;
            }
        }
        if let Some(ch) = cont_h {
            let total_fr: f32 = row_spec
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    if row_collapsed[i] {
                        0.0
                    } else if let GTrack::Fr(f, _) = t {
                        *f
                    } else {
                        0.0
                    }
                })
                .sum();
            if total_fr > 0.0 {
                let non_fr: f32 = rh
                    .iter()
                    .zip(&row_spec)
                    .map(|(h, t)| if matches!(t, GTrack::Fr(..)) { 0.0 } else { *h })
                    .sum();
                let leftover = (ch - row_gap * nrows.saturating_sub(1) as f32 - non_fr).max(0.0);
                for (i, t) in row_spec.iter().enumerate() {
                    if let GTrack::Fr(f, m) = t {
                        if !row_collapsed[i] {
                            rh[i] = (*m).max(leftover * *f / total_fr);
                        }
                    }
                }
            }
            // align-content normal|stretch inflates `auto`-max rows —
            // implicit (auto) rows included
            if stretch_rows {
                let stretchable: Vec<usize> = (0..nrows)
                    .filter(|&i| track_sem(&row_spec[i]).4 && !row_collapsed[i])
                    .collect();
                if !stretchable.is_empty() {
                    let used: f32 =
                        rh.iter().sum::<f32>() + row_gap * nrows.saturating_sub(1) as f32;
                    let extra = (ch - used).max(0.0);
                    if extra > 0.0 {
                        let add = extra / stretchable.len() as f32;
                        for i in stretchable {
                            rh[i] += add;
                        }
                    }
                }
            }
        }

        // ---- emit ----
        // the gutter after track i folds to 0 when either adjacent track
        // is a collapsed auto-fit track
        let gapx = |i: usize| -> f32 {
            if i + 1 < ncols && (col_collapsed[i] || col_collapsed[i + 1]) {
                0.0
            } else {
                col_gap
            }
        };
        let gapy = |i: usize| -> f32 {
            if i + 1 < nrows && (row_collapsed[i] || row_collapsed[i + 1]) {
                0.0
            } else {
                row_gap
            }
        };
        let gaps_w: f32 = (0..ncols.saturating_sub(1)).map(|i| gapx(i)).sum();
        let gaps_h: f32 = (0..nrows.saturating_sub(1)).map(|i| gapy(i)).sum();
        let tracks_w = cw.iter().sum::<f32>() + gaps_w;
        let tracks_h = rh.iter().sum::<f32>() + gaps_h;
        let (egap_x, off_x) = content_offset(jc, (avail - tracks_w).max(0.0), ncols);
        let (egap_y, off_y) =
            content_offset(ac, (cont_h.unwrap_or(tracks_h) - tracks_h).max(0.0), nrows);
        let mut col_x = vec![0.0f32; ncols + 1];
        for i in 0..ncols {
            col_x[i + 1] = col_x[i]
                + cw[i]
                + if i + 1 < ncols {
                    gapx(i) + egap_x
                } else {
                    col_gap + egap_x
                };
        }
        let mut row_y = vec![0.0f32; nrows + 1];
        for i in 0..nrows {
            row_y[i + 1] = row_y[i]
                + rh[i]
                + if i + 1 < nrows {
                    gapy(i) + egap_y
                } else {
                    row_gap + egap_y
                };
        }

        // each item emits a sibling paint-order segment for the post-pass
        let mut gsegs: Vec<(usize, usize, i64)> = Vec::new();
        for it in items.iter_mut() {
            gsegs.push((
                self.items.len(),
                self.hits.len(),
                stacking::item_layer(&it.cc),
            ));
            let cell_x = x0 + off_x + col_x[it.c];
            let cell_w: f32 = cw[it.c..it.c + it.cs].iter().sum::<f32>()
                + (it.c..it.c + it.cs - 1)
                    .map(|i| gapx(i) + egap_x)
                    .sum::<f32>();
            let cell_y = y0 + off_y + row_y[it.r];
            let cell_h: f32 = rh[it.r..it.r + it.rs].iter().sum::<f32>()
                + (it.r..it.r + it.rs - 1)
                    .map(|i| gapy(i) + egap_y)
                    .sum::<f32>();
            let inner_w =
                (cell_w - it.m[1] - it.m[3] - it.p[1] - it.p[3] - it.bw[1] - it.bw[3]).max(1.0);
            let content_w = it
                .w_prop
                .map(|w| w.min(inner_w).max(1.0))
                .unwrap_or(match it.just {
                    GAlign::Stretch => inner_w,
                    _ => it.intrinsic_w.min(inner_w).max(1.0),
                })
                .min(it.max_wc.unwrap_or(f32::MAX))
                .max(it.min_wc)
                .max(1.0);
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
            // translate/transform + position:relative shift the painted
            // box + its hit rects; a matrix transform wraps the emitted span
            let gspec = xform::spec(&it.cc, it.st.size);
            let wrapping = gspec.as_ref().is_some_and(|s| s.wrap);
            let gi0 = self.items.len();
            let (i0, h0) = (gi0, self.hits.len());
            let (tx, ty) = if wrapping {
                (0.0, 0.0)
            } else {
                translate_xy(&it.cc, box_w, box_h, it.st.size)
            };
            let (rx, ry) = self.rel_offset(&it.cc, it.st.size);
            let (ix, iy) = (ix + tx + rx, iy + ty + ry);
            let radii = it.radius.resolve(box_w, box_h, it.st.size);
            let layers = it
                .bgl
                .as_ref()
                .map(|bgl| {
                    bgimg::items_for(
                        bgl,
                        ix,
                        iy,
                        box_w,
                        box_h,
                        (self.viewport_w, self.viewport_h),
                    )
                })
                .unwrap_or_default();

            // `align-items:center` on the item centers its content inside the
            // (possibly stretched) box
            let dy = if matches!(
                it.cc
                    .get("align-items")
                    .or_else(|| it.cc.get("place-items"))
                    .map(String::as_str),
                Some("center")
            ) {
                ((box_h - it.box_h_intr) / 2.0).max(0.0)
            } else {
                0.0
            };
            let laid: Vec<DisplayItem> = it
                .laid
                .drain(..)
                .map(|di| offset_item(di, ix, iy + dy))
                .collect();
            borderfx::emit_box(
                &mut self.items,
                ix,
                iy,
                box_w,
                box_h,
                it.bg,
                it.border.as_ref(),
                &radii,
                &it.shadows,
                it.clip,
                it.outline.as_ref(),
                layers,
                laid,
            );
            self.hits
                .push((ix, iy, box_w, box_h, it.id, xform::Mat::IDENTITY));
            let mut hv: Vec<_> = it
                .hits
                .drain(..)
                .map(|h| xform::off_hit(h, ix, iy + dy))
                .collect();
            if it.clip {
                let bw = it.border.map(|b| b.w).unwrap_or([0.0; 4]);
                let (cx, cy, cw, chh) = borderfx::padding_box(ix, iy, box_w, box_h, bw);
                clip_hits(&mut hv, 0, cx, cy, cw, chh);
            }
            // hits join the list before wrap_span tags this span's [h0..]
            for h in hv.drain(..) {
                self.push_hit(h);
            }
            if let Some(sp) = gspec {
                self.wrap_span(&sp, it.id, i0, h0);
            }
            let fx = colorfx::element_fx(
                &it.cc,
                it.st.size,
                it.st.color,
                Some([ix, iy, box_w, box_h]),
            );
            if !fx.is_empty() {
                colorfx::wrap_vec(&mut self.items, gi0, fx);
            }
            self.hits.extend(hv);
        }
        stacking::reorder(&mut self.items, &mut self.hits, &gsegs);

        // the container consumes max(content, declared height) in flow
        self.y = y0 + tracks_h.max(cont_h.unwrap_or(0.0));
        self.cur_xf = saved_xf;
    }

    fn nest(
        &mut self,
        node: NodeId,
        st: &Style,
        x0: f32,
        y0: f32,
        avail: f32,
        c: &Props,
    ) -> (Vec<DisplayItem>, f32, Vec<Hit>) {
        let saved_items = std::mem::take(&mut self.items);
        let saved_hits = std::mem::take(&mut self.hits);
        let saved_line = std::mem::take(&mut self.line);
        let saved_floats = std::mem::take(&mut self.floats);
        // box interiors aren't part of an enclosing inline's transform run
        let saved_xf = self.cur_xf.take();
        let saved = (
            self.y,
            self.margin_x,
            self.avail_w,
            self.align,
            self.align_last,
            self.ellipsis_clip,
            self.indent_pending,
            self.rtl,
            self.clamp_left,
            self.clamping,
        );
        let saved_lx = self.line_x;
        let saved_ps = self.pending_space;
        let saved_pg = self.pending_gap;
        self.y = y0;
        self.margin_x = x0;
        self.avail_w = avail.max(1.0);
        self.align = st.align;
        self.align_last = st.align_last;
        self.rtl = st.rtl;
        self.clamp_left = 0;
        self.clamping = false;
        if st.line_clamp > 0 {
            self.clamp_left = st.line_clamp;
        }
        self.ellipsis_clip = if c.get("text-overflow").map(String::as_str) == Some("ellipsis")
            && borderfx::clips_overflow(c)
        {
            Some(self.avail_w)
        } else {
            None
        };
        self.indent_pending = st.indent;
        self.line_x = 0.0;
        self.pending_space = false;
        self.pending_gap = 0.0;
        let st0 = self.stickies.len();
        // containing-block height for positioned descendants
        let saved_cb_h = self.cb_h;
        let is_cb = matches!(
            c.get("position").map(String::as_str),
            Some("relative") | Some("absolute") | Some("fixed") | Some("sticky")
        );
        if is_cb {
            let h = dim(c, "height", self.viewport_h, st.size);
            if let Some(h) = h {
                self.cb_h = Some(h);
            }
            // positioned descendants resolve insets against the padding box
            // (x0/y0 are the content origin in box-local coords)
            let [cpt, cpr, _cpb, cpl] = box_sides(c, "padding", avail, st.size).map(|v| v.max(0.0));
            self.cb_stack.push(CbRect {
                x: x0 - cpl,
                y: y0 - cpt,
                w: cpl + avail.max(1.0) + cpr,
                h,
            });
        }
        // the enclosing block's inset stretch must not leak into nested
        // containers — a positioned descendant sets its own
        let saved_ih = self.inset_h;
        self.inset_h = None;
        // ::first-line/::first-letter of the enclosing block don't cross
        // into nested boxes (inline-block/cell/item = own flow root)
        let saved_fl = self.fl_style.take();
        let saved_flet = self.flet.take();
        self.emit_pseudo(node, css::PseudoElem::Before, c, st);
        if matches!(
            c.get("display").map(String::as_str),
            Some("flex") | Some("inline-flex") | Some("grid") | Some("inline-grid")
        ) {
            self.flush_line();
        }
        match c.get("display").map(String::as_str) {
            Some("flex") | Some("inline-flex") => self.flex_children(node, c, st.clone()),
            Some("grid") | Some("inline-grid") => self.grid_children(node, c, st),
            // inline-table / tables inside cells: the outer box is the
            // caller's, so margins/bg/border/centering are skipped
            Some("table") | Some("inline-table") => self.table_inner(node, c, st.clone(), true),
            _ if multicol::multicol_on(c) => self.multicol_children(node, c, st.clone()),
            _ => {
                for &ch in self.doc.children(node) {
                    self.walk(ch, st);
                }
            }
        }
        self.emit_pseudo(node, css::PseudoElem::After, c, st);
        self.cb_h = saved_cb_h;
        self.inset_h = saved_ih;
        if is_cb {
            self.cb_stack.pop();
        }
        // sticky children clamp against this box's bottom — project the
        // explicit-height target since `self.y` only saw the content
        let nest_bot = self
            .y
            .max(y0 + dim(c, "height", self.viewport_h, st.size).unwrap_or(0.0));
        self.drain_stickies(st0, y0, nest_bot);
        self.fl_style = saved_fl;
        self.flet = saved_flet;
        self.flush_line();
        // a nested box is a BFC: it contains its own floats (they're dropped
        // from the outer float list on restore, but extend this box's bottom)
        let mut bottom = self.y;
        for f in &self.floats {
            bottom = bottom.max(f.bottom);
        }
        let items = std::mem::replace(&mut self.items, saved_items);
        let hits = std::mem::replace(&mut self.hits, saved_hits);
        self.y = saved.0;
        self.margin_x = saved.1;
        self.avail_w = saved.2;
        self.align = saved.3;
        self.align_last = saved.4;
        self.ellipsis_clip = saved.5;
        self.indent_pending = saved.6;
        self.rtl = saved.7;
        self.clamp_left = saved.8;
        self.clamping = saved.9;
        self.line = saved_line;
        self.line_x = saved_lx;
        self.pending_space = saved_ps;
        self.pending_gap = saved_pg;
        self.floats = saved_floats;
        self.cur_xf = saved_xf;
        (items, bottom, hits)
    }

    /// Wrap the emitted span items[i0..] in PushTransform/PopTransform when
    /// `sp` carries a non-translate transform, and tag hits[h0..] with the
    /// matrix (composed after any already-recorded inner transform) so
    /// hit-testing can inverse-map clicks exactly. `node`'s own hit rect
    /// (pushed during its layout) is the transform-origin border box;
    /// elements without one (table, floats) fall back to the items' bbox.
    fn wrap_span(&mut self, sp: &xform::Spec, node: NodeId, i0: usize, h0: usize) {
        if !sp.wrap || (self.items.len() <= i0 && self.hits.len() <= h0) {
            return;
        }
        let bbox = self.hits[h0..]
            .iter()
            .rev()
            .find(|h| h.4 == node)
            .map(|h| (h.0, h.1, h.2, h.3))
            .or_else(|| xform::items_bbox(&self.items[i0..]))
            .unwrap_or((0.0, 0.0, 0.0, 0.0));
        let m = sp.matrix(bbox);
        if m.is_identity() {
            return;
        }
        // an element emitting no items (childless, unstyled) still needs
        // its hits tagged — its painted shape just happens to be empty
        if self.items.len() > i0 {
            self.items.insert(i0, DisplayItem::PushTransform { m });
            self.items.push(DisplayItem::PopTransform);
        }
        for h in &mut self.hits[h0..] {
            h.5 = m.mul(&h.5);
        }
    }

    /// Emit one line word's items (and hit rects) at pen position `x`.
    /// `m` is the transform wrapping the run it belongs to — hit rects
    /// compose it after their own (inner) transforms.
    fn emit_word(
        &mut self,
        w: Word,
        x: f32,
        baseline: f32,
        line_top: f32,
        line_h: f32,
        m: Option<&xform::Mat>,
    ) {
        // hit for the owning inline element (e.g. <a> over its text)
        if !self.pe_blocked {
            if let Some(o) = w.owner {
                self.hits.push((
                    x,
                    line_top,
                    w.width,
                    line_h,
                    o,
                    m.copied().unwrap_or(xform::Mat::IDENTITY),
                ));
            }
        }
        if let Some(items) = w.items {
            // inline-block vertical-align: default bottom-on-baseline;
            // line-box keywords anchor to the line box edges
            let dy = match w.va {
                Va::Top => line_top,
                Va::Middle => line_top + line_h / 2.0 - w.box_h / 2.0,
                Va::Bottom => line_top + line_h - w.box_h,
                Va::Shift(f) => baseline - w.box_h - f * w.size,
                Va::Baseline => baseline - w.box_h,
            };
            for it in items {
                self.items.push(offset_item(it, x, dy));
            }
            for h in w.hits {
                let mut h = xform::off_hit(h, x, dy);
                if let Some(m) = m {
                    h.5 = m.mul(&h.5);
                }
                self.hits.push(h);
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
            let baseline = match w.va {
                Va::Baseline => baseline,
                Va::Shift(f) => baseline - f * w.size,
                // top/middle/bottom on a text run — run edges vs the line
                // box (text-top/text-bottom approximate as top/bottom)
                Va::Top => line_top + w.ascent,
                Va::Middle => line_top + line_h / 2.0 + w.ascent / 2.0,
                Va::Bottom => line_top + line_h - w.size * 0.25,
            };
            // text-shadow: offset clones under the run, blurred via a
            // filter group when a blur radius is set
            for &(sdx, sdy, blur, scol) in &w.shadow {
                if blur > 0.5 {
                    self.items
                        .push(DisplayItem::PushEffect(colorfx::Effect::Filter(
                            colorfx::parse_filter(&format!("blur({blur:.1}px)"), w.size, scol),
                        )));
                }
                self.items.push(DisplayItem::Text {
                    x: x + sdx,
                    baseline: baseline + sdy,
                    text: w.text.clone(),
                    font: w.font,
                    size: w.size,
                    color: scol,
                    deco: TextDeco::NONE,
                    ls: w.ls,
                    href: None,
                    sel: w.sel,
                });
                if blur > 0.5 {
                    self.items.push(DisplayItem::PopEffect);
                }
            }
            self.items.push(DisplayItem::Text {
                x,
                baseline,
                text: w.text,
                font: w.font,
                size: w.size,
                color: w.color,
                deco: w.deco,
                ls: w.ls,
                href: w.href,
                sel: w.sel,
            });
        }
        // empty words (inline-margin spacers) just advance x
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
                let [_t, pr, _b, pl] =
                    box_sides(&c, "padding", self.avail_w, st.size).map(|v| v.max(0.0));
                ew - pl - pr
            } else {
                ew
            };
            w = w.max(ew);
        } else {
            match c.get("display").map(String::as_str) {
                Some("flex") | Some("inline-flex") => {
                    if matches!(
                        c.get("flex-direction").map(String::as_str),
                        Some("column") | Some("column-reverse")
                    ) {
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
                Some("table") | Some("inline-table") => {
                    w = w.max(self.table_pref_width(node, &c, st));
                }
                Some("grid") | Some("inline-grid") => {
                    // fixed track sum beats text (repeat(8,64px) → 512)
                    let (tracks, _, _) = grid_track_list(
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
                            GTrack::C { floor, .. } => *floor,
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

    /// min-content width approximation: the longest unbreakable word, or a
    /// descendant's own minimum (explicit width, nested track sums).
    /// Unlike `shrink_width` an empty subtree contributes 0 — it feeds
    /// `min-width:auto` floors and min-content/fit-content tracks where
    /// the 4em empty-content default would be wrong.
    fn min_content_w(&mut self, node: NodeId, st: &Style) -> f32 {
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
        let c = self.style(node);
        if let Some(ew) = dim_intrinsic(&c, "width", self.avail_w, st.size) {
            let ew = if border_box(&c) {
                let [_t, pr, _b, pl] =
                    box_sides(&c, "padding", self.avail_w, st.size).map(|v| v.max(0.0));
                ew - pl - pr
            } else {
                ew
            };
            w = w.max(ew);
        }
        // a child's own minimum can out-measure our words
        for &ch in self.doc.children(node) {
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
                continue; // hidden or out-of-flow: no contribution
            }
            let mut cst = st.clone();
            self.apply_computed(&cc, &mut cst);
            let cw = dim_intrinsic(&cc, "width", self.avail_w, cst.size)
                .unwrap_or_else(|| self.min_content_w(ch, &cst));
            w = w.max(cw);
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
        // column direction: max-content width is the widest item, and the
        // main-axis gap runs vertically — neither contributes to width
        let col_dir = matches!(
            c.get("flex-direction").map(String::as_str),
            Some("column") | Some("column-reverse")
        );
        let gap = if col_dir {
            0.0
        } else {
            c.get("column-gap")
                .and_then(|v| parse_len(v, self.avail_w, st.size))
                .or_else(|| {
                    c.get("gap")
                        .and_then(|v| parse_len(v, self.avail_w, st.size))
                })
                .unwrap_or(0.0)
        };
        let mut w = 0.0f32;
        let mut n = 0usize;
        for &ch in self.doc.children(node) {
            if let NodeKind::Text(t) = self.doc.kind(ch) {
                if let Some(fi) = self.resolve_font(st) {
                    if let Some(f) = self.fonts.font(fi) {
                        let tw =
                            f.measure(&t.split_whitespace().collect::<Vec<_>>().join(" "), st.size);
                        if col_dir {
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
            let item = cw + pl.max(0.0) + pr.max(0.0) + ml + mr;
            if col_dir {
                w = w.max(item);
            } else {
                w += item;
            }
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
                        "img"
                            | "svg"
                            | "input"
                            | "select"
                            | "textarea"
                            | "video"
                            | "canvas"
                            | "iframe"
                            | "object"
                            | "embed"
                    ) || matches!(
                        disp,
                        Some("inline-block") | Some("inline-flex") | Some("inline-grid")
                    );
                    if atomic {
                        *line += dim_intrinsic(&cc, "width", self.avail_w, cst.size)
                            .unwrap_or_else(|| self.max_content_width(ch, &cst));
                    } else if tag == "br"
                        || matches!(
                            disp,
                            Some("block")
                                | Some("list-item")
                                | Some("flex")
                                | Some("grid")
                                | Some("table")
                                | Some("flow-root")
                                | Some("inline-table")
                                | Some("table-row")
                                | Some("table-cell")
                                | Some("table-caption")
                                | Some("table-row-group")
                                | Some("table-header-group")
                                | Some("table-footer-group")
                        )
                    {
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
        // column direction: items stack vertically — the cross-size
        // contribution is the widest item, no horizontal gaps
        let col_dir = matches!(
            c.get("flex-direction").map(String::as_str),
            Some("column") | Some("column-reverse")
        );
        let gap = if col_dir {
            0.0
        } else {
            c.get("column-gap")
                .and_then(|v| parse_len(v, self.avail_w, st.size))
                .or_else(|| {
                    c.get("gap")
                        .and_then(|v| parse_len(v, self.avail_w, st.size))
                })
                .unwrap_or(0.0)
        };
        // flex-wrap:wrap rows can break between items — the min-content
        // contribution is the widest single item, not the whole line
        let wrap = col_dir
            || matches!(
                c.get("flex-wrap").map(String::as_str),
                Some("wrap") | Some("wrap-reverse")
            );
        let mut w = 0.0f32;
        let mut n = 0usize;
        for &ch in self.doc.children(node) {
            if let NodeKind::Text(t) = self.doc.kind(ch) {
                if let Some(fi) = self.resolve_font(st) {
                    if let Some(f) = self.fonts.font(fi) {
                        let tw =
                            f.measure(&t.split_whitespace().collect::<Vec<_>>().join(" "), st.size);
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
        let gaps = if wrap {
            0.0
        } else {
            gap * n.saturating_sub(1) as f32
        };
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
        let gap = self.take_gap();
        self.line.push(Word {
            text: String::new(),
            font: usize::MAX,
            size: 0.0,
            color: 0,
            deco: TextDeco::NONE,
            va: Va::Baseline,
            sel: true,
            gap_before: gap,
            ls: 0.0,
            shadow: Vec::new(),
            width: w,
            ascent: 0.0,
            line_h: 0.0,
            href: None,
            bg: None,
            items: None,
            hits: Vec::new(),
            box_h: 0.0,
            dx: 0.0,
            dy: 0.0,
            xf: self.cur_xf.clone(),
            owner: None,
        });
        self.line_x += w;
    }

    /// Push an inline-block box (pre-rendered items) onto the line like a word.
    /// `ascent` = the box's first-baseline offset below its border-box top —
    /// the baseline the box shares with the line's other words.
    fn push_box(
        &mut self,
        w: f32,
        h: f32,
        items: Vec<DisplayItem>,
        hits: Vec<Hit>,
        mb: f32,
        rel: (f32, f32),
        ascent: f32,
        owner: Option<NodeId>,
    ) {
        if self.line_x + w > self.content_width() + 0.5 && !self.line.is_empty() {
            self.flush_line();
        }
        let gap = self.take_gap();
        self.line.push(Word {
            text: String::new(),
            font: usize::MAX,
            size: 0.0,
            color: 0,
            deco: TextDeco::NONE,
            va: Va::Baseline,
            sel: true,
            gap_before: gap,
            ls: 0.0,
            shadow: Vec::new(),
            width: w,
            ascent: ascent.min(h),
            line_h: h + mb,
            href: None,
            bg: None,
            items: Some(items),
            hits,
            box_h: h,
            dx: rel.0,
            dy: rel.1,
            xf: self.cur_xf.clone(),
            owner,
        });
        self.line_x += w;
    }

    fn finish_patch(&mut self, p: &mut BlockPatch) -> usize {
        let h = self.y - p.y;
        let radii = p.radii.resolve(p.w, h, p.em);
        let mut slot = p.idx;
        // paint order = slot order: outer shadows under the box (listed
        // order reversed — first CSS shadow topmost), bg, inset shadows,
        // border edges, then the clip marker right before the children.
        for s in p.shadows.iter().filter(|s| !s.inset).rev() {
            self.items[slot] = borderfx::shadow_item(s, p.x, p.y, p.w, h, &radii);
            slot += 1;
        }
        if let Some(bg) = p.bg {
            self.items[slot] = borderfx::bg_item(p.x, p.y, p.w, h, bg, &radii);
            slot += 1;
        }
        // bg-image/gradient layers paint between the bg color and the
        // inset shadows/borders — spliced items shift the slots after.
        let mut n = 0usize;
        if let Some(bgl) = &p.bgl {
            let vp = (self.viewport_w, self.viewport_h);
            let items = bgimg::items_for(bgl, p.x, p.y, p.w, h, vp);
            n = items.len();
            self.items.splice(slot..slot, items);
            slot += n;
        }
        for s in p.shadows.iter().filter(|s| s.inset).rev() {
            self.items[slot] = borderfx::shadow_item(s, p.x, p.y, p.w, h, &radii);
            slot += 1;
        }
        if let Some(b) = &p.border {
            let mut tmp = Vec::with_capacity(4);
            borderfx::emit_borders(&mut tmp, p.x, p.y, p.w, h, b);
            for it in tmp {
                self.items[slot] = it;
                slot += 1;
            }
        }
        if p.clip {
            // overflow clips descendants to the padding box — pull the
            // border-box radii in by the border widths. The clip marker is
            // the last reserved slot; PopClip + outline land after the
            // children items appended since.
            let bw = p.border.map(|b| b.w).unwrap_or([0.0; 4]);
            let (cx, cy, cw, ch) = borderfx::padding_box(p.x, p.y, p.w, h, bw);
            let pr = radii.inset(bw);
            self.items[p.idx + p.nslots + n - 1] = DisplayItem::PushClip {
                x: cx,
                y: cy,
                w: cw,
                h: ch,
                radii: pr.0,
            };
            self.items.push(DisplayItem::PopClip);
        }
        if let Some(o) = &p.outline {
            borderfx::emit_outline(&mut self.items, p.x, p.y, p.w, h, o);
        }
        p.nslots += n;
        n
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
    ) -> Option<(f32, f32, Vec<DisplayItem>, Vec<Hit>)> {
        // % heights resolve against the definite containing height
        let hrel = self.pct_h.unwrap_or(self.avail_w);
        match tag {
            "video" => {
                // <video> lays out at its box size; its `poster` bitmap is
                // the painted payload (object-fit applies like <img>)
                let w = dim(c, "width", self.avail_w, st.size)
                    .or_else(|| {
                        self.doc
                            .attr(node, "width")
                            .and_then(|v| v.trim_end_matches("px").parse().ok())
                    })
                    .unwrap_or(300.0f32);
                let h = hdim_pct(c, "height", hrel, st.size, self.pct_h)
                    .or_else(|| {
                        self.doc
                            .attr(node, "height")
                            .and_then(|v| v.trim_end_matches("px").parse().ok())
                    })
                    .or_else(|| aspect_ratio(c).map(|r| w / r))
                    .unwrap_or(150.0f32);
                let items = self
                    .doc
                    .attr(node, "poster")
                    .map(|src| vec![self.fit_image_item(c, st.size, w, h, src)])
                    .unwrap_or_default();
                Some((
                    w,
                    h,
                    items,
                    vec![(0.0, 0.0, w, h, node, xform::Mat::IDENTITY)],
                ))
            }
            "svg" => {
                let w = dim(c, "width", self.avail_w, st.size)
                    .or_else(|| hdim_pct(c, "height", hrel, st.size, self.pct_h))
                    .unwrap_or(24.0);
                let h = hdim_pct(c, "height", hrel, st.size, self.pct_h).unwrap_or(w);
                let color = st.color;
                let pixels =
                    svg::render(self.doc, node, w.max(1.0) as u32, h.max(1.0) as u32, color);
                Some((
                    w,
                    h,
                    vec![DisplayItem::Raster {
                        x: 0.0,
                        y: 0.0,
                        w,
                        h,
                        pixels,
                    }],
                    vec![(0.0, 0.0, w, h, node, xform::Mat::IDENTITY)],
                ))
            }
            "img" => {
                // CSS width/height beat the attributes (inline style and
                // utility classes size almost every modern <img>)
                let w = dim(c, "width", self.avail_w, st.size)
                    .or_else(|| {
                        self.doc
                            .attr(node, "width")
                            .and_then(|v| v.trim_end_matches("px").parse().ok())
                    })
                    .or_else(|| {
                        // only height given + aspect-ratio/intrinsic guess
                        hdim_pct(c, "height", hrel, st.size, self.pct_h)
                            .filter(|_| aspect_ratio(c).is_some())
                            .map(|h| h * aspect_ratio(c).unwrap())
                    })
                    .unwrap_or(64.0f32);
                let h = hdim_pct(c, "height", hrel, st.size, self.pct_h)
                    .or_else(|| {
                        self.doc
                            .attr(node, "height")
                            .and_then(|v| v.trim_end_matches("px").parse().ok())
                    })
                    .or_else(|| aspect_ratio(c).map(|r| w / r))
                    .unwrap_or(w.min(48.0).max(16.0));
                let items = self
                    .img_src(node, w)
                    .map(|src| vec![self.fit_image_item(c, st.size, w, h, &src)])
                    .unwrap_or_default();
                Some((
                    w,
                    h,
                    items,
                    vec![(0.0, 0.0, w, h, node, xform::Mat::IDENTITY)],
                ))
            }
            _ => None,
        }
    }

    /// `srcset` candidate picker: `w` descriptors resolve against the
    /// element's layout width, `x` against 1 dpr — prefer the smallest
    /// candidate that covers `want`, else the largest available.
    fn img_src(&self, node: NodeId, want: f32) -> Option<String> {
        // <picture>: first preceding <source> sibling whose `media` matches
        // supplies its srcset (spec: sources are evaluated in document order)
        if let Some(par) = self.doc.parent(node) {
            if self.doc.tag(par) == Some("picture") {
                for &sib in self.doc.children(par) {
                    if sib == node {
                        break;
                    }
                    if self.doc.tag(sib) != Some("source") {
                        continue;
                    }
                    let ok = self
                        .doc
                        .attr(sib, "media")
                        .map(|m| css::media_query(m, self.viewport_w))
                        .unwrap_or(true);
                    if ok {
                        if let Some(ss) = self.doc.attr(sib, "srcset") {
                            if let Some(u) = self.pick_srcset(ss, want) {
                                return Some(u);
                            }
                        }
                    }
                }
            }
        }
        if let Some(ss) = self.doc.attr(node, "srcset") {
            if let Some(u) = self.pick_srcset(ss, want) {
                return Some(u);
            }
        }
        self.doc.attr(node, "src").map(String::from)
    }

    /// Pick the best candidate out of a `srcset` list for `want` css px.
    fn pick_srcset(&self, ss: &str, want: f32) -> Option<String> {
        {
            let mut best: Option<(f32, String)> = None;
            for cand in ss.split(',') {
                let mut it = cand.trim().split_whitespace();
                let Some(url) = it.next() else { continue };
                let d = it.next().unwrap_or("1x");
                let score = if let Some(w) = d.strip_suffix('w') {
                    w.parse::<f32>().unwrap_or(0.0)
                } else {
                    d.trim_end_matches('x').parse::<f32>().unwrap_or(1.0) * want
                };
                let better = match &best {
                    None => true,
                    Some((bs, _)) => {
                        if score >= want && *bs < want {
                            true
                        } else if score >= want && *bs >= want {
                            score < *bs
                        } else {
                            score > *bs
                        }
                    }
                };
                if better && !url.is_empty() {
                    best = Some((score, url.to_string()));
                }
            }
            best.map(|(_, u)| u)
        }
    }

    /// `object-fit`-aware image item for a replaced element in box-local
    /// coords: `fill` (default) keeps the plain stretch `Image`; any other
    /// fit emits a `BgImage` whose tile resolves against the decoded size.
    fn fit_image_item(&self, c: &Props, size: f32, w: f32, h: f32, src: &str) -> DisplayItem {
        let near = matches!(
            c.get("image-rendering").map(String::as_str),
            Some("pixelated")
                | Some("crisp-edges")
                | Some("-moz-crisp-edges")
                | Some("optimizeSpeed")
        );
        match bgimg::object_fit(c, size, self.viewport_h, self.viewport_w) {
            Some((mode, xx, yy)) => DisplayItem::BgImage {
                x: 0.0,
                y: 0.0,
                w,
                h,
                px: 0.0,
                py: 0.0,
                pw: w,
                ph: h,
                mode,
                sw: -1.0,
                sh: -1.0,
                fx: xx.0,
                ax: xx.1,
                fy: yy.0,
                ay: yy.1,
                rx: 0,
                ry: 0,
                src: src.to_string(),
                near,
            },
            None => DisplayItem::Image {
                x: 0.0,
                y: 0.0,
                w,
                h,
                src: src.to_string(),
                near,
            },
        }
    }

    fn inline(&mut self, node: NodeId, tag: &str, c: &Props, mut st: Style) {
        if tag == "a" {
            st.href = self.doc.attr(node, "href").map(String::from);
        }
        // position:relative on an inline box shifts its painted runs only —
        // the offset rides each emitted word; flow geometry is unaffected
        let (rx, ry) = self.rel_offset(c, st.size);
        st.rel_x += rx;
        st.rel_y += ry;
        // this element owns the words it emits — hit rects let links and
        // other inline elements be hovered/clicked over their text
        st.hit_owner = Some(node);
        // inline margins become spacing around the element's runs
        let [_t, mr, _b, ml] = box_sides(c, "margin", self.avail_w, st.size);
        if ml > 0.0 {
            self.spacer(ml);
        }
        match tag {
            "br" => self.flush_line(),
            "svg" | "img" | "video" => {
                self.flush_line();
                if let Some((w, h, items, hits)) = self.replaced_payload(node, tag, c, &st) {
                    // svg|img are always `direct` in walk — a wrapping
                    // transform wraps the span there; here just gate the
                    // translate fast path
                    let sp = xform::spec(c, st.size);
                    let (tx, ty) = if sp.as_ref().is_some_and(|s| s.wrap) {
                        (0.0, 0.0)
                    } else {
                        translate_xy(c, w, h, st.size)
                    };
                    let x = self.line_origin();
                    let y = self.y;
                    for it in items {
                        self.items.push(offset_item(it, x + tx, y + ty));
                    }
                    for hd in hits {
                        self.push_hit(xform::off_hit(hd, x + tx, y + ty));
                    }
                    let lh = self.eff_line_h(&st);
                    self.y += h.max(lh);
                }
            }
            "input" | "textarea" | "select" => {
                // Childless controls: show their value/placeholder, else a
                // generic control label. ::placeholder styles the hint.
                let (label, ph) = match self.doc.attr(node, "value") {
                    Some(v) => (v, false),
                    None => match self.doc.attr(node, "placeholder") {
                        Some(p) => (p, true),
                        None => ("[control]", false),
                    },
                };
                if ph {
                    let pp = self.ctx.computed_pseudo(node, css::PseudoElem::Placeholder);
                    let mut pst = st.clone();
                    self.apply_computed(&pp, &mut pst);
                    self.word(label, &pst);
                } else {
                    self.word(label, &st);
                }
            }
            _ => {
                self.emit_pseudo(node, css::PseudoElem::Before, c, &st);
                for &ch in self.doc.children(node) {
                    self.walk(ch, &st);
                }
                self.emit_pseudo(node, css::PseudoElem::After, c, &st);
            }
        }
        if mr > 0.0 {
            self.spacer(mr);
        }
    }

    fn text(&mut self, text: &str, st: &Style) {
        if self.clamping {
            return;
        }
        if st.pre {
            // whitespace preserved verbatim; \n forces a line break,
            // \t expands to `tab-size` space widths
            for (i, seg) in text.split('\n').enumerate() {
                if i > 0 {
                    self.flush_line();
                }
                let seg = seg.replace('\t', &" ".repeat(st.tab.round() as usize));
                let mut first = i == 0;
                for word in seg.split(' ') {
                    if word.is_empty() {
                        // every preserved space earns its own gap
                        if !first && !self.line.is_empty() {
                            if let Some(fi) = self.resolve_font(st) {
                                let gw = self.space_w(fi, st);
                                self.pending_gap += gw + st.ws + st.ls;
                            }
                        }
                        continue;
                    }
                    if !first {
                        self.pending_space = true;
                    }
                    self.word_with_space(word, st);
                    first = false;
                }
            }
            self.pending_space = false;
            return;
        }
        if st.pre_line {
            // whitespace collapses like normal text but \n still breaks
            for (i, seg) in text.split('\n').enumerate() {
                if i > 0 {
                    self.flush_line();
                    self.pending_space = false;
                    self.pending_gap = 0.0;
                }
                self.text_collapse(seg, st);
            }
            return;
        }
        self.text_collapse(text, st);
    }

    /// normal whitespace-collapse path: runs of whitespace become one
    /// pending space, words split on any whitespace
    fn text_collapse(&mut self, text: &str, st: &Style) {
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
        self.resolve_font_ch(st, None)
    }

    /// Font resolution with the run's first char for unicode-range gating
    /// (@font-face approximation — per-run, no mid-run segmentation).
    /// `ch` None resolves purely on family/weight/stretch/style.
    fn resolve_font_ch(&mut self, st: &Style, ch: Option<char>) -> Option<usize> {
        // FontCache::resolve scans every installed face; memoize per
        // (family, weight, italic, first char). u32::MAX = resolved-none.
        let key = (
            st.family.clone(),
            st.weight,
            st.italic,
            ch.map_or(0, |c| c as u32),
        );
        if let Some(&v) = self.font_memo.get(&key) {
            return if v == u32::MAX {
                None
            } else {
                Some(v as usize)
            };
        }
        let r = self
            .fonts
            .resolve_face(&st.family, st.weight, st.stretch, st.italic, ch)
            .or_else(|| self.fonts.resolve("liberation sans", false, false));
        self.font_memo
            .insert(key, r.map(|i| i as u32).unwrap_or(u32::MAX));
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
        let primary = self.resolve_font_ch(st, word.chars().next())?;
        if covers(&mut self.fonts, primary, word) {
            return Some(primary);
        }
        const ALT: &[&str] = &[
            "noto sans symbols2",
            "noto sans symbols",
            "noto sans math",
            "symbola",
            "dejavu sans",
            "noto sans",
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

    /// advance of one space in this font+size
    fn space_w(&mut self, font: usize, st: &Style) -> f32 {
        self.fonts
            .font(font)
            .map(|f| f.advance_units(f.glyph_id(' ')) as f32 * st.size / f.units_per_em() as f32)
            .unwrap_or(st.size * 0.3)
    }

    /// Take the pending whitespace gap — stamped on the next pushed word
    /// and counted in `line_x` for wrap checks.
    fn take_gap(&mut self) -> f32 {
        let g = std::mem::take(&mut self.pending_gap);
        self.line_x += g;
        g
    }

    fn word_with_space(&mut self, word: &str, st: &Style) {
        let Some(font) = self.resolve_font_covering(word, st) else {
            return;
        };
        if self.pending_space && !self.line.is_empty() {
            // whitespace collapses to one gap; word-spacing widens it and
            // letter-spacing applies to the space char itself
            self.pending_gap += self.space_w(font, st) + st.ws + st.ls;
        }
        self.pending_space = false;
        self.push_word(word, st, font);
    }

    /// one small-caps segment: lowercase-source runs painted as caps at
    /// reduced size, uppercase-source runs at full size
    fn push_case_run(&mut self, run: &str, st: &Style, font: usize, small: bool) {
        let size = st.size * if small { 0.78 } else { 1.0 };
        let (w, ascent, line_h) = {
            let Some(f) = self.fonts.font(font) else {
                return;
            };
            let upem = f.units_per_em() as f32;
            (
                f.measure(run, size) + st.ls * run.chars().count() as f32,
                f.ascender as f32 * size / upem,
                match st.line_h {
                    Some(LineH::Mul(m)) => size * m,
                    Some(LineH::Px(p)) => p,
                    None => f.line_height(size),
                },
            )
        };
        if !st.nowrap && self.line_x + w > self.content_width() + 0.5 && !self.line.is_empty() {
            self.flush_line_wrap();
        }
        let gap = self.take_gap();
        self.line.push(Word {
            text: run.to_string(),
            font,
            size,
            color: st.color,
            deco: st.deco.clone(),
            va: st.va,
            sel: !st.no_select,
            gap_before: gap,
            ls: st.ls,
            shadow: st.shadow.clone(),
            href: st.href.clone(),
            width: w,
            ascent,
            line_h,
            bg: st.inline_bg,
            items: None,
            hits: Vec::new(),
            box_h: 0.0,
            dx: st.rel_x,
            dy: st.rel_y,
            xf: self.cur_xf.clone(),
            owner: st.hit_owner,
        });
        self.line_x += w;
    }

    fn word(&mut self, word: &str, st: &Style) {
        if let Some(font) = self.resolve_font_ch(st, word.chars().next()) {
            self.push_word(word, st, font);
        }
    }

    fn push_word(&mut self, word: &str, st: &Style, font: usize) {
        if self.clamping {
            return;
        }
        let owned: String;
        let word = match st.transform {
            1 => {
                owned = word.to_uppercase();
                &owned
            }
            2 => {
                owned = word.to_lowercase();
                &owned
            }
            3 => {
                owned = word
                    .chars()
                    .enumerate()
                    .map(|(i, c)| {
                        if i == 0 {
                            c.to_uppercase().next().unwrap_or(c)
                        } else {
                            c
                        }
                    })
                    .collect();
                &owned
            }
            _ => word,
        };
        // ::first-letter: the leading char becomes its own run under the
        // pseudo's style (approximation — no drop-cap float)
        let word = if let Some(fst) = self.flet.take() {
            let mut chs = word.chars();
            match chs.next() {
                Some(fc) => {
                    let fcs = fc.to_string();
                    if let Some(ffont) = self.resolve_font_covering(&fcs, &fst) {
                        let (fw, fascent, flh) = {
                            let Some(f) = self.fonts.font(ffont) else {
                                return;
                            };
                            let upem = f.units_per_em() as f32;
                            (
                                f.measure(&fcs, fst.size),
                                f.ascender as f32 * fst.size / upem,
                                match fst.line_h {
                                    Some(LineH::Mul(m)) => fst.size * m,
                                    Some(LineH::Px(p)) => p,
                                    None => f.line_height(fst.size),
                                },
                            )
                        };
                        if !st.nowrap
                            && self.line_x + fw > self.content_width() + 0.5
                            && !self.line.is_empty()
                        {
                            self.flush_line_wrap();
                        }
                        let gap = self.take_gap();
                        self.line.push(Word {
                            text: fcs,
                            font: ffont,
                            size: fst.size,
                            color: fst.color,
                            deco: fst.deco.clone(),
                            va: fst.va,
                            sel: !fst.no_select,
                            gap_before: gap,
                            ls: fst.ls,
                            shadow: fst.shadow.clone(),
                            href: fst.href.clone(),
                            width: fw,
                            ascent: fascent,
                            line_h: flh,
                            bg: fst.inline_bg,
                            items: None,
                            hits: Vec::new(),
                            box_h: 0.0,
                            dx: 0.0,
                            dy: 0.0,
                            xf: self.cur_xf.clone(),
                            owner: fst.hit_owner,
                        });
                        self.line_x += fw;
                        chs.as_str()
                    } else {
                        word
                    }
                }
                None => word,
            }
        } else {
            word
        };
        if word.is_empty() {
            return;
        }
        self.push_word_body(word, st, font);
    }

    /// Wrap+push a (transformed, first-letter-stripped) word. `&shy;`
    /// splits at the last fitting hyphenation point; small-caps segments
    /// the word by source case; letter-spacing widens the measure.
    fn push_word_body(&mut self, word: &str, st: &Style, font: usize) {
        // soft hyphens: strip for the fit check; only a wrap break shows
        // them (hyphens:manual — the default; `none` disables entirely)
        let clean: String;
        let raw = word;
        let word = if word.contains('\u{ad}') {
            clean = word.replace('\u{ad}', "");
            &clean
        } else {
            word
        };
        if word.is_empty() {
            return;
        }
        if st.small_caps && word.chars().any(|c| c.is_lowercase()) {
            // fake small-caps: lowercase-source runs render as uppercase
            // at ~0.78em — each contiguous run becomes its own word
            let mut seg = String::new();
            let mut seg_lower = false;
            for c in word.chars() {
                let lower = c.is_lowercase();
                if !seg.is_empty() && lower != seg_lower {
                    let seg = std::mem::take(&mut seg);
                    self.push_case_run(&seg, st, font, seg_lower);
                }
                seg_lower = lower;
                if lower {
                    seg.extend(c.to_uppercase());
                } else {
                    seg.push(c);
                }
            }
            if !seg.is_empty() {
                self.push_case_run(&seg, st, font, seg_lower);
            }
            return;
        }
        let (w, ascent, line_h) = {
            let Some(f) = self.fonts.font(font) else {
                return;
            };
            let upem = f.units_per_em() as f32;
            (
                f.measure(word, st.size) + st.ls * word.chars().count() as f32,
                f.ascender as f32 * st.size / upem,
                match st.line_h {
                    Some(LineH::Mul(m)) => st.size * m,
                    Some(LineH::Px(p)) => p,
                    None => f.line_height(st.size),
                },
            )
        };
        if !st.nowrap && self.line_x + w > self.content_width() + 0.5 && !self.line.is_empty() {
            // &shy; split first: last opportunity that fits earns a hyphen
            if !st.hyphens_none && raw.contains('\u{ad}') {
                let mut cut = None;
                let mut frag = String::new();
                for (bi, c) in raw.char_indices() {
                    if c == '\u{ad}' {
                        if frag.is_empty() {
                            continue;
                        }
                        let mut t = frag.clone();
                        t.push('-');
                        let fw = self
                            .fonts
                            .font(font)
                            .map(|f| f.measure(&t, st.size))
                            .unwrap_or(0.0)
                            + st.ls * t.chars().count() as f32;
                        if self.line_x + fw <= self.content_width() + 0.5 {
                            cut = Some(bi);
                        } else {
                            break;
                        }
                    } else {
                        frag.push(c);
                    }
                }
                if let Some(bi) = cut {
                    let mut head: String = raw[..bi].replace('\u{ad}', "");
                    head.push('-');
                    let rest: String = raw[bi + '\u{ad}'.len_utf8()..].to_string();
                    self.push_word_body(&head, st, font);
                    self.flush_line_wrap();
                    self.push_word_body(&rest, st, font);
                    return;
                }
            }
            self.flush_line_wrap();
        }
        if st.break_word && !st.nowrap && w > self.content_width() + 0.5 && self.line.is_empty() {
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
                    let gap = self.take_gap();
                    self.line.push(Word {
                        text: chunk.clone(),
                        font,
                        size: st.size,
                        color: st.color,
                        deco: st.deco.clone(),
                        va: st.va,
                        sel: !st.no_select,
                        gap_before: gap,
                        ls: st.ls,
                        shadow: st.shadow.clone(),
                        href: st.href.clone(),
                        width: cw,
                        ascent,
                        line_h,
                        bg: st.inline_bg,
                        items: None,
                        hits: Vec::new(),
                        box_h: 0.0,
                        dx: st.rel_x,
                        dy: st.rel_y,
                        xf: self.cur_xf.clone(),
                        owner: st.hit_owner,
                    });
                    self.flush_line_wrap();
                    chunk.clear();
                    cw = 0.0;
                }
                chunk.push(c);
                cw += fw;
            }
            if !chunk.is_empty() {
                let gap = self.take_gap();
                self.line.push(Word {
                    text: chunk,
                    font,
                    size: st.size,
                    color: st.color,
                    deco: st.deco.clone(),
                    va: st.va,
                    sel: !st.no_select,
                    gap_before: gap,
                    ls: st.ls,
                    shadow: st.shadow.clone(),
                    href: st.href.clone(),
                    width: cw,
                    ascent,
                    line_h,
                    bg: st.inline_bg,
                    items: None,
                    hits: Vec::new(),
                    box_h: 0.0,
                    dx: st.rel_x,
                    dy: st.rel_y,
                    xf: self.cur_xf.clone(),
                    owner: st.hit_owner,
                });
                self.line_x += cw;
            }
            return;
        }
        let gap = self.take_gap();
        self.line.push(Word {
            text: word.to_string(),
            font,
            size: st.size,
            color: st.color,
            deco: st.deco.clone(),
            va: st.va,
            sel: !st.no_select,
            gap_before: gap,
            ls: st.ls,
            shadow: st.shadow.clone(),
            href: st.href.clone(),
            width: w,
            ascent,
            line_h,
            bg: st.inline_bg,
            items: None,
            hits: Vec::new(),
            box_h: 0.0,
            dx: st.rel_x,
            dy: st.rel_y,
            xf: self.cur_xf.clone(),
            owner: st.hit_owner,
        });
        self.line_x += w;
    }

    fn flush_line(&mut self) {
        // forced break — end of block, <br>, explicit newline: the last
        // line isn't justified (text-align-last applies instead)
        self.flush_line_inner(true)
    }

    /// wrap-point flush — the line is justified when text-align:justify
    fn flush_line_wrap(&mut self) {
        self.flush_line_inner(false)
    }

    fn flush_line_inner(&mut self, forced: bool) {
        if self.line.is_empty() {
            self.pending_space = false;
            return;
        }
        // ::first-line: the first emitted line box re-resolves under the
        // pseudo's style (approximation — style applies to the whole line
        // box, not the truncated CSS-spec first line)
        if let Some(fst) = self.fl_style.take() {
            let mut line = std::mem::take(&mut self.line);
            for w in line.iter_mut() {
                if w.text.is_empty() || w.items.is_some() {
                    continue;
                }
                let Some(fi) = self.resolve_font_covering(&w.text, &fst) else {
                    continue;
                };
                let Some(f) = self.fonts.font(fi) else {
                    continue;
                };
                let upem = f.units_per_em() as f32;
                w.font = fi;
                w.size = fst.size;
                w.color = fst.color;
                w.deco = fst.deco.clone();
                w.va = fst.va;
                w.ls = fst.ls;
                w.shadow = fst.shadow.clone();
                w.width = f.measure(&w.text, fst.size);
                w.ascent = f.ascender as f32 * fst.size / upem;
                w.line_h = match fst.line_h {
                    Some(LineH::Mul(m)) => fst.size * m,
                    Some(LineH::Px(p)) => p,
                    None => f.line_height(fst.size),
                };
                if fst.inline_bg.is_some() {
                    w.bg = fst.inline_bg;
                }
            }
            self.line = line;
        }
        let line_h = self
            .line
            .iter()
            .map(|w| w.line_h)
            .fold(0.0f32, f32::max)
            .max(
                self.line
                    .iter()
                    .map(|w| w.ascent + w.size * 0.25)
                    .fold(0.0, f32::max),
            );
        let max_ascent = self.line.iter().map(|w| w.ascent).fold(0.0f32, f32::max);
        let baseline = self.y + max_ascent;

        // text-overflow:ellipsis — the block clipped its content box and
        // asked for "…": drop tail words, then truncate the last one to
        // fit the ellipsis inside the content edge
        if let Some(clip) = self.ellipsis_clip {
            let limit = self.line_origin() + clip;
            let mut used = self
                .line
                .iter()
                .map(|w| w.gap_before + w.width)
                .sum::<f32>();
            while used > clip + 0.5 && self.line.len() > 1 {
                let w = self.line.pop().unwrap();
                used -= w.gap_before + w.width;
            }
            if used > clip + 0.5 {
                if let Some(w) = self.line.last_mut() {
                    let _ = limit;
                    let avail = (clip - w.gap_before).max(0.0);
                    if let Some(f) = self.fonts.font(w.font) {
                        let ew = f.measure("\u{2026}", w.size);
                        let mut keep = 0usize;
                        let mut acc = 0.0f32;
                        for (i, c) in w.text.char_indices() {
                            let cw = f.advance(f.glyph_id(c), w.size) + w.ls;
                            if acc + cw + ew > avail {
                                break;
                            }
                            acc += cw;
                            keep = i + c.len_utf8();
                        }
                        w.text.truncate(keep);
                        w.text.push('\u{2026}');
                        w.width = acc + ew;
                        w.deco = TextDeco::NONE;
                    }
                }
            }
        }

        let used = self
            .line
            .iter()
            .map(|w| w.gap_before + w.width)
            .sum::<f32>();
        // forced (block-end/br) lines aren't justified — text-align-last
        // applies; auto falls back to start for justified text
        let align = if forced {
            match self.align_last {
                Some(a) => a,
                None => {
                    if self.align == Align::Justify {
                        Align::Left
                    } else {
                        self.align
                    }
                }
            }
        } else {
            self.align
        };
        let n_gaps = self.line.iter().filter(|w| w.gap_before > 0.0).count() as f32;
        // text-align:justify shares the leftover width across word gaps
        let justify_extra = if align == Align::Justify && !forced && n_gaps > 0.0 {
            ((self.content_width() - used) / n_gaps).max(0.0)
        } else {
            0.0
        };
        let off = match align {
            Align::Left | Align::Justify => 0.0,
            Align::Center => ((self.content_width() - used) / 2.0).max(0.0),
            Align::Right => (self.content_width() - used).max(0.0),
        };
        let mut x = self.line_origin() + off + std::mem::take(&mut self.indent_pending);
        // per-word x positions on this line — gap_before lands *before*
        // the word, widened by justify_extra
        let n = self.line.len();
        let mut xs = vec![0.0; n];
        // direction:rtl emits words right-to-left — the first word lands
        // at the line's right edge
        let emit_order: Vec<usize> = if self.rtl {
            (0..n).rev().collect()
        } else {
            (0..n).collect()
        };
        for &k in &emit_order {
            let w = &self.line[k];
            x += w.gap_before
                + if w.gap_before > 0.0 {
                    justify_extra
                } else {
                    0.0
                };
            xs[k] = x;
            x += w.width;
        }
        // a consecutive run of words sharing one transform spec wraps in
        // PushTransform/PopTransform — the run's extent is the element's
        // box (transform-origin anchors there)
        let mut line: Vec<Option<Word>> = std::mem::take(&mut self.line)
            .into_iter()
            .map(Some)
            .collect();
        let mut i = 0;
        while i < n {
            let end = match &line[i].as_ref().unwrap().xf {
                Some(sp) => {
                    let mut j = i + 1;
                    while j < n
                        && line[j]
                            .as_ref()
                            .unwrap()
                            .xf
                            .as_ref()
                            .is_some_and(|o| Rc::ptr_eq(o, sp))
                    {
                        j += 1;
                    }
                    j
                }
                None => i + 1,
            };
            let m = line[i]
                .as_ref()
                .unwrap()
                .xf
                .as_ref()
                .map(|sp| {
                    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
                    for k in i..end {
                        let w = line[k].as_ref().unwrap();
                        let (wx, wy, wh) = if w.items.is_some() {
                            (xs[k] + w.dx, baseline + w.dy - w.box_h, w.box_h)
                        } else {
                            (
                                xs[k] + w.dx,
                                baseline + w.dy - w.ascent,
                                w.ascent + w.size * 0.3,
                            )
                        };
                        x0 = x0.min(wx);
                        y0 = y0.min(wy);
                        x1 = x1.max(wx + w.width);
                        y1 = y1.max(wy + wh);
                    }
                    sp.matrix((x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0)))
                })
                .filter(|m| !m.is_identity());
            if let Some(m) = m {
                self.items.push(DisplayItem::PushTransform { m });
            }
            for k in i..end {
                let w = line[k].take().unwrap();
                let (dx, dy) = (w.dx, w.dy);
                self.emit_word(
                    w,
                    xs[k] + dx,
                    baseline + dy,
                    self.y + dy,
                    line_h,
                    m.as_ref(),
                );
            }
            if m.is_some() {
                self.items.push(DisplayItem::PopTransform);
            }
            i = end;
        }
        self.y += line_h;
        self.line_x = 0.0;
        self.pending_space = false;
        self.pending_gap = 0.0;
        // line-clamp — count the emitted line; a wrap flush exhausting
        // the budget appends "\u{2026}" and drops all further content
        if self.clamp_left > 0 {
            self.clamp_left -= 1;
            if self.clamp_left == 0 {
                self.clamping = true;
                if !forced {
                    for it in self.items.iter_mut().rev() {
                        if let DisplayItem::Text { text, .. } = it {
                            if !text.is_empty() {
                                text.push(char::from_u32(0x2026).unwrap());
                                break;
                            }
                        }
                        if !matches!(it, DisplayItem::PopEffect | DisplayItem::PopClip) {
                            break;
                        }
                    }
                }
            }
        }
    }

    /// `::before`/`::after` generated content — emitted as the element's
    /// first/last child inside block(), inline() and nest(). The pseudo
    /// box inherits the element's text style plus its own computed props;
    /// `quotes` is read from the element too (that's where it's declared).
    /// `content` forms: strings, attr(), *-quote, counter()/counters();
    /// none/normal → no box.
    fn emit_pseudo(&mut self, node: NodeId, kind: css::PseudoElem, ec: &Props, st: &Style) {
        // replaced elements can't host generated content
        if matches!(
            self.doc.tag(node),
            Some(
                "img"
                    | "svg"
                    | "input"
                    | "select"
                    | "textarea"
                    | "video"
                    | "canvas"
                    | "iframe"
                    | "object"
                    | "embed"
                    | "br"
                    | "hr"
            )
        ) {
            return;
        }
        let mut pp = self.ctx.computed_pseudo(node, kind);
        if pp.is_empty() || pp.get("display").map(String::as_str) == Some("none") {
            return;
        }
        if !pp.contains_key("quotes") {
            if let Some(q) = ec.get("quotes") {
                pp.insert("quotes".to_string(), q.clone());
            }
        }
        let mut pst = st.clone();
        self.apply_computed(&pp, &mut pst);
        let mut depth = self.quote_depth;
        let txt = generated::content_text(self.doc, node, &pp, &self.counters, &mut depth);
        self.quote_depth = depth;
        let Some(txt) = txt else { return };
        // a block-level pseudo box gets its own line
        let blockish = matches!(
            pp.get("display").map(String::as_str),
            Some("block") | Some("list-item") | Some("flex") | Some("grid") | Some("flow-root")
        );
        if blockish {
            self.flush_line();
        }
        self.text(&txt, &pst);
        if blockish {
            self.flush_line();
        }
    }

    /// Open a list context for ul/menu/dir (unordered) / ol (ordered).
    /// Marker type: the element's list-style-type, else disc→circle→square
    /// by unordered depth (decimal for ol). Honors start=, reversed, and
    /// the `list-style[-type]:none` suppression.
    fn push_list_ctx(&mut self, node: NodeId, c: &Props, ordered: bool) {
        let hide = c.get("list-style-type").is_some_and(|v| v.trim() == "none")
            || c.get("list-style").is_some_and(|v| v.contains("none"));
        let (typ, pos) = generated::list_props(c);
        let (mut next, mut step) = (1i32, 1i32);
        if ordered {
            let reversed = self.doc.attr(node, "reversed").is_some();
            let n_items = self
                .doc
                .children(node)
                .iter()
                .filter(|&&ch| self.doc.tag(ch) == Some("li"))
                .count() as i32;
            let start = self
                .doc
                .attr(node, "start")
                .and_then(|v| v.trim().parse::<i32>().ok());
            next = start.unwrap_or(if reversed { n_items.max(1) } else { 1 });
            step = if reversed { -1 } else { 1 };
        }
        let typ = typ.unwrap_or_else(|| {
            if ordered {
                generated::LType::Decimal
            } else {
                let depth = self.list_stack.iter().filter(|l| !l.ordered).count();
                match depth % 3 {
                    0 => generated::LType::Disc,
                    1 => generated::LType::Circle,
                    _ => generated::LType::Square,
                }
            }
        });
        self.list_stack.push(generated::ListCtx {
            hide,
            ordered,
            typ,
            inside: pos.unwrap_or(false),
            next,
            step,
        });
    }

    /// A list-item's ::marker: (text, style, inside). Marker style = the
    /// element's text style + ::marker computed props; `content` on
    /// ::marker overrides the list glyph.
    fn make_marker(
        &mut self,
        node: NodeId,
        c: &Props,
        st: &Style,
    ) -> Option<(String, Style, bool)> {
        let (own_typ, own_pos) = generated::list_props(c);
        if own_typ == Some(generated::LType::None) {
            return None;
        }
        // a numeric list-style-type (inherited from the list or set on the
        // item) numbers the marker; glyph types show their glyph
        let numbered = |t: &generated::LType| {
            matches!(
                t,
                generated::LType::Decimal
                    | generated::LType::DecimalZero
                    | generated::LType::LowerAlpha
                    | generated::LType::UpperAlpha
                    | generated::LType::LowerRoman
                    | generated::LType::UpperRoman
            )
        };
        let (typ, inside, num, numeric) = match self.list_stack.last_mut() {
            Some(lc) if lc.hide => return None,
            Some(lc) => {
                let t = own_typ.clone().unwrap_or_else(|| lc.typ.clone());
                let numeric = lc.ordered || numbered(&t);
                let num = if numeric {
                    // <li value=N> overrides the running count
                    let v = self
                        .doc
                        .attr(node, "value")
                        .and_then(|v| v.trim().parse::<i32>().ok())
                        .unwrap_or(lc.next);
                    lc.next = v + lc.step;
                    v
                } else {
                    0
                };
                (t, own_pos.unwrap_or(lc.inside), num, numeric)
            }
            // a list-item outside any list: UA default disc; a numeric
            // list-style-type still numbers it (count = 1)
            None => {
                let t = own_typ.clone().unwrap_or(generated::LType::Disc);
                let numeric = numbered(&t);
                let num = if numeric {
                    self.doc
                        .attr(node, "value")
                        .and_then(|v| v.trim().parse::<i32>().ok())
                        .unwrap_or(1)
                } else {
                    0
                };
                (t, own_pos.unwrap_or(false), num, numeric)
            }
        };
        let Some(mut text) = generated::marker_text(&typ, num, numeric) else {
            return None;
        };
        let mut mst = st.clone();
        let mp = self.ctx.computed_pseudo(node, css::PseudoElem::Marker);
        if !mp.is_empty() {
            self.apply_computed(&mp, &mut mst);
            // ::marker{content:...} overrides the list glyph
            let mut depth = self.quote_depth;
            if let Some(t) =
                generated::content_text(self.doc, node, &mp, &self.counters, &mut depth)
            {
                self.quote_depth = depth;
                text = t;
            }
        }
        Some((text, mst, inside))
    }

    /// `list-style-position:outside`: the marker box sits in the gutter to
    /// the left of the content edge, aligned on the first line's baseline.
    fn place_outside_marker(
        &mut self,
        m: &str,
        mst: &Style,
        content_top: f32,
        items_start: usize,
        content_mx: f32,
    ) {
        let baseline = self.items[items_start..]
            .iter()
            .find_map(|i| match i {
                DisplayItem::Text { baseline, .. } => Some(*baseline),
                _ => None,
            })
            .unwrap_or(content_top + mst.size);
        let Some(font) = self.resolve_font_covering(m, mst) else {
            return;
        };
        let Some(f) = self.fonts.font(font) else {
            return;
        };
        let w = f.measure(m, mst.size);
        // right-align ending ~0.4em left of the content edge
        let x = content_mx - w - 0.4 * mst.size;
        self.items.push(DisplayItem::Text {
            x,
            baseline,
            text: m.to_string(),
            font,
            size: mst.size,
            color: mst.color,
            deco: mst.deco.clone(),
            ls: mst.ls,
            href: None,
            sel: true,
        });
    }

    /// `::selection` background-color for the document root, composited —
    /// the renderer tints the selection highlight with it. `None` when no
    /// ::selection rule supplies a background.
    pub fn selection_tint(doc: &Document, external: &[css::StyleSheet], vw: f32) -> Option<Color> {
        let author = css::document_sheets_vw(doc, vw);
        let mut sheets: Vec<&css::StyleSheet> = vec![ua_sheet()];
        sheets.extend(external.iter());
        sheets.extend(author.iter());
        let mut ctx = css::StyleCtx::new(doc, &sheets);
        for &el in doc.children(doc.root) {
            if doc.tag(el) == Some("html") {
                let sp = ctx.computed_pseudo(el, css::PseudoElem::Selection);
                if let Some(col) = bg_color(&sp, None) {
                    return Some(over(col, WHITE));
                }
                for &b in doc.children(el) {
                    if doc.tag(b) == Some("body") {
                        let sp = ctx.computed_pseudo(b, css::PseudoElem::Selection);
                        if let Some(col) = bg_color(&sp, None) {
                            return Some(over(col, WHITE));
                        }
                    }
                }
            }
        }
        None
    }
}

/// Computed `cursor` keyword for `node` under `pstate`, resolved with the
/// same sheet stack as layout (UA + external + author). `cursor` is
/// inherited — ancestors are climbed until one declares it. "auto"/""
/// means the default pointer.
pub fn cursor_for(
    doc: &Document,
    width: f32,
    external: &[css::StyleSheet],
    node: NodeId,
    pstate: &css::PseudoState,
) -> String {
    let author = css::document_sheets_vw(doc, width);
    let mut sheets: Vec<&css::StyleSheet> = vec![ua_sheet()];
    sheets.extend(external.iter());
    sheets.extend(author.iter());
    let mut ctx = css::StyleCtx::new(doc, &sheets);
    ctx.set_state(pstate.clone());
    let mut cur = Some(node);
    while let Some(n) = cur {
        if doc.tag(n).is_none() {
            break;
        }
        if let Some(v) = ctx.computed(n).get("cursor") {
            return v.trim().to_string();
        }
        cur = doc.parent(n);
    }
    "auto".to_string()
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

    /// (x, baseline, size, color, text) for a Text item — test extractor
    fn tinfo(i: &DisplayItem) -> (f32, f32, f32, Color, &str) {
        match i {
            DisplayItem::Text {
                x,
                baseline,
                size,
                color,
                text,
                ..
            } => (*x, *baseline, *size, *color, text),
            _ => unreachable!(),
        }
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
        let (items, _, _) =
            lay("<style>.big{font-size:40px}</style><p class=big>huge</p><p>small</p>");
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
        let (items, _, _) =
            lay("<style>.hide{display:none}</style><p class=hide>gone</p><p>here</p>");
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
        let (items, _, _) = lay("<style>p{text-align:center}</style><p>mid</p>");
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
            if let DisplayItem::Text {
                x, baseline, text, ..
            } = i
            {
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
        assert!(
            xb - xa > 100.0,
            "b should sit past the 100px box: {xa} -> {xb}"
        );
    }

    #[test]
    fn inline_block_bg_and_border() {
        let (items, _, _) = lay("<style>.ib{display:inline-block;width:50px;height:20px;\
             background-color:#ff0000;border:2px solid blue}</style>\
             <p><span class=ib></span></p>");
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
        let (items, _, _) = lay("<table><tr><td>a1</td><td>bbb1</td></tr>\
             <tr><td>a2</td><td>b2</td></tr></table>");
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
        let (items, _, _) = lay("<style>td{border:2px solid #ff0000}</style>\
             <table><tr><td>x</td></tr></table>");
        let reds = items
            .iter()
            .filter(
                |i| matches!(i, DisplayItem::FillRect { color, .. } if *color == bgra(255, 0, 0)),
            )
            .count();
        assert_eq!(reds, 4, "four border edges around the cell");
    }

    #[test]
    fn float_narrows_following_lines() {
        let (items, _, _) = lay("<style>.fl{float:left;width:200px}</style>\
             <div class=fl>nav</div><p>hello world</p>");
        let (xnav, _) = text_pos(&items, "nav");
        let (xhello, _) = text_pos(&items, "hello");
        assert!(xnav < 30.0, "float content at left edge, x={xnav}");
        assert!(
            xhello >= xnav + 190.0,
            "text flows beside the float: {xhello}"
        );
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
        assert!(
            hs[1] - hs[0] >= 30.0,
            "padding grows bg: {:?} vs {:?}",
            hs[0],
            hs[1]
        );
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
        let (items, _, _) = lay("<style>.lh{line-height:3}</style><p class=lh>a</p><p>b</p>");
        let (_, ya) = text_pos(&items, "a");
        let (_, yb) = text_pos(&items, "b");
        assert!(
            yb - ya > 40.0,
            "line-height 3x16 separates baselines: {yb} - {ya}"
        );
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
        assert!(
            (xb1 - xa1 - 100.0).abs() < 1.0,
            "col step 100: {xa1} -> {xb1}"
        );
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
        assert!(
            (xb - xa - fr - 20.0).abs() < 2.0,
            "fr column step: {xa} -> {xb}"
        );
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
        assert!(
            (xc - xb - 110.0).abs() < 1.0,
            "col step incl gap: {xb} -> {xc}"
        );
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
            assert!(
                (w - 64.0).abs() < 1.0 && (h - 64.0).abs() < 1.0,
                "square cell {w}x{h}"
            );
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
        let r = items.iter().find(
            |i| matches!(i, DisplayItem::FillRect { color, .. } if *color == bgra(255, 0, 0)),
        );
        let Some(DisplayItem::FillRect { x, y, w, h, .. }) = r else {
            panic!("cell bg not found")
        };
        assert!((*x - 8.0 - 75.0).abs() < 1.0, "centered x {x}");
        assert!((*y - 8.0 - 40.0).abs() < 1.0, "centered y {y}");
        assert!((*w - 50.0).abs() < 1.0 && (*h - 20.0).abs() < 1.0);
    }

    #[test]
    fn grid_explicit_placement() {
        let (items, _, _) = lay("<style>.g{display:grid;grid-template-columns:100px 100px}\
             .x{grid-column:2;grid-row:1}</style>\
             <div class=g><div>a</div><div class=x>b</div><div>c</div></div>");
        let (xa, ya) = text_pos(&items, "a");
        let (xb, yb) = text_pos(&items, "b");
        let (_xc, yc) = text_pos(&items, "c");
        assert!(
            (xb - xa - 100.0).abs() < 1.0,
            "b pinned to col 2: {xa} -> {xb}"
        );
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
        assert!(
            (xb - xa - 50.0).abs() < 1.0,
            "nested grid columns: {xa} -> {xb}"
        );
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
            assert!(
                (r.2 - 64.0).abs() < 1.0 && (r.3 - 64.0).abs() < 1.0,
                "square cell"
            );
        }
    }

    #[test]
    fn width_caps_block_content() {
        let (items, _, _) = lay("<style>.w{width:150px}</style>\
             <div class=w>aa bb cc dd ee ff gg hh ii jj kk ll</div>");
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
                assert!(
                    *x < 8.0 + 150.0 + 1.0,
                    "word starts inside 150px box, x={x}"
                );
            }
        }
    }

    fn has_text(items: &[DisplayItem], needle: &str) -> bool {
        items.iter().any(|i| match i {
            DisplayItem::Text { text, .. } => text.contains(needle),
            _ => false,
        })
    }

    #[test]
    fn before_after_content() {
        let (items, _, _) = lay(
            "<style>.tag::before{content:'['}.tag::after{content:' ' attr(title) ']'}</style>             <p class=tag title=x>mid</p>",
        );
        assert!(has_text(&items, "["), "::before string missing");
        assert!(has_text(&items, "x]"), "::after attr() missing: {items:?}");
        // content:none generates nothing
        let (items, _, _) = lay("<style>p::before{content:none}</style><p>mid</p>");
        assert_eq!(texts(&items).len(), 1);
        // unknown pseudo-elements still produce no box
        let (items, _, _) = lay("<style>p::-webkit-x{content:'x'}</style><p>mid</p>");
        assert_eq!(texts(&items).len(), 1);
    }

    #[test]
    fn pseudo_styles_generated_text() {
        let (items, _, _) =
            lay("<style>p::before{content:'b ';color:red;font-size:40px}</style><p>rest</p>");
        let t = texts(&items);
        let (text, color, size) = match t[0] {
            DisplayItem::Text {
                text, color, size, ..
            } => (text, *color, *size),
            _ => unreachable!(),
        };
        assert_eq!(text, "b");
        assert_eq!(color, bgra(255, 0, 0));
        assert!((size - 40.0).abs() < 0.5);
    }

    #[test]
    fn markers_emit() {
        // ol: numbered markers; ul: bullet glyph
        let (items, _, _) = lay("<ol><li>a</li><li>b</li></ol><ul><li>c</li></ul>");
        assert!(has_text(&items, "1."), "{items:?}");
        assert!(has_text(&items, "2."), "{items:?}");
        assert!(has_text(&items, "\u{2022}"), "{items:?}");
        // start= / value= / reversed honored
        let (items, _, _) = lay(
            "<ol start=4><li>a</li></ol>             <ol><li value=9>b</li></ol>             <ol reversed><li>x</li><li>y</li></ol>",
        );
        assert!(has_text(&items, "4."));
        assert!(has_text(&items, "9."));
        // nested ul cycles disc → circle
        let (items, _, _) = lay("<ul><li>a<ul><li>b</li></ul></li></ul>");
        assert!(has_text(&items, "\u{2022}"));
        assert!(has_text(&items, "\u{25e6}"), "nested marker should be ◦");
        // list-style-type variants on the item
        let (items, _, _) =
            lay("<style>li{list-style-type:upper-roman}</style><ul><li>a</li><li>b</li></ul>");
        assert!(has_text(&items, "I."), "{items:?}");
        assert!(has_text(&items, "II."), "{items:?}");
        // list-style:none suppresses
        let (items, _, _) = lay("<ul style='list-style:none'><li>a</li></ul>");
        assert!(!has_text(&items, "\u{2022}"));
        // ::marker content + style
        let (items, _, _) =
            lay("<style>li::marker{content:'» ';color:red}</style><ul><li>a</li></ul>");
        assert!(has_text(&items, "»"), "{items:?}");
    }

    #[test]
    fn css_counters() {
        let (items, _, _) = lay(
            "<style>h1{counter-increment:sec}h1::before{content:counter(sec)'.'}</style>             <body style='counter-reset:sec'>             <h1>a</h1><h1>b</h1></body>",
        );
        assert!(has_text(&items, "1."), "{items:?}");
        assert!(has_text(&items, "2."), "{items:?}");
        // counters() joins nested scopes (the classic nested-ol example)
        let (items, _, _) = lay(
            "<style>ol{counter-reset:c}li{counter-increment:c}             li::marker{content:counters(c,'.') '. '}</style>             <ol><li>a<ol><li>b</li></ol></li></ol>",
        );
        assert!(has_text(&items, "1.1"), "{items:?}");
        // counter-set and counter formats (4 → d)
        let (items, _, _) = lay(
            "<style>div{counter-set:n 4}div::before{content:counter(n,upper-alpha)}</style>             <div>x</div>",
        );
        assert!(has_text(&items, "D"), "{items:?}");
    }

    #[test]
    fn first_letter_styles() {
        let (items, _, _) =
            lay("<style>p::first-letter{color:red;font-size:40px}</style><p>Hi there</p>");
        let t = texts(&items);
        let (mut fl, mut rest) = (None, None);
        for i in &t {
            if let DisplayItem::Text {
                text, color, size, ..
            } = i
            {
                if text == "H" {
                    fl = Some((*color, *size));
                } else {
                    rest = Some((*color, *size));
                }
            }
        }
        let (fc, fs) = fl.expect("first-letter run");
        assert_eq!(fc, bgra(255, 0, 0));
        assert!((fs - 40.0).abs() < 0.5);
        let (rc, rs) = rest.expect("rest run");
        assert_eq!(rc, bgra(0x11, 0x11, 0x11));
        assert!((rs - 16.0).abs() < 1.0);
    }

    // ---------- tables ----------

    #[test]
    fn table_rowspan_keeps_column_alignment() {
        // rowspan cell in (0,0); row 1's cell must land in column 1
        let (items, _, _) = lay("<table><tr><td rowspan=2>A</td><td>b1</td></tr>\
             <tr><td>b2</td></tr></table>");
        let (_xa, ya) = text_pos(&items, "A");
        let (xb1, yb1) = text_pos(&items, "b1");
        let (xb2, yb2) = text_pos(&items, "b2");
        assert!(
            (xb2 - xb1).abs() < 1.0,
            "b2 aligns under b1: {xb1} vs {xb2}"
        );
        assert!(yb2 > yb1, "b2 on the second row");
        assert!(ya < yb2, "rowspan cell starts at the top");
    }

    #[test]
    fn table_colspan_shares_columns() {
        let (items, _, _) = lay("<table><tr><td colspan=2>widecell</td></tr>\
             <tr><td>c1</td><td>c2</td></tr></table>");
        let (xw, _) = text_pos(&items, "widecell");
        let (xc1, yc1) = text_pos(&items, "c1");
        let (xc2, yc2) = text_pos(&items, "c2");
        assert!(xc2 > xc1, "two columns still separate");
        assert!(yc2 - yc1 < 1.0, "c1/c2 on one row");
        assert!(xw <= xc2 + 1.0, "spanning cell inside the table");
    }

    #[test]
    fn table_fixed_layout_ignores_content() {
        // col widths: first cell 100px, second splits the rest (400-100-3*2)
        let (items, _, _) = lay("<table style=\"table-layout:fixed;width:400px\">\
             <tr><td style=\"width:100px\">x</td><td>yyyyyyyyyyyyyyyyyyyy</td></tr></table>");
        let (_xx, _) = text_pos(&items, "x");
        let (xy, _) = text_pos(&items, "yyyyyyyyyyyyyyyyyyyy");
        // second column starts right after the 100px column + 2px spacings
        assert!(
            (xy - 8.0 - 2.0 - 100.0 - 2.0).abs() < 2.0,
            "fixed col boundary, x={xy}"
        );
    }

    #[test]
    fn table_border_spacing_offsets_cells() {
        let (items, _, _) =
            lay("<table style=\"border-spacing:10px\"><tr><td>a</td><td>b</td></tr></table>");
        let (xa, _) = text_pos(&items, "a");
        let (xb, _) = text_pos(&items, "b");
        // a's column content can't start before the leading 10px spacing
        assert!(xa >= 8.0 + 10.0 - 0.5, "leading spacing before a: {xa}");
        // the column gap is at least the 10px spacing
        assert!(xb - xa > 10.0, "spacing separates cells: {xa} -> {xb}");
    }

    #[test]
    fn table_border_collapse_dedupes_edges() {
        // collapse: shared edge between two 4px cells draws once (not twice)
        let (items, _, _) = lay("<style>td{border:4px solid #ff0000}</style>\
             <table style=\"border-collapse:collapse\"><tr><td>a</td><td>b</td></tr></table>");
        let reds = items
            .iter()
            .filter(
                |i| matches!(i, DisplayItem::FillRect { color, .. } if *color == bgra(255, 0, 0)),
            )
            .count();
        // two cells: a paints l/t/b/r minus lost edges; b paints r/t/b minus
        // shared left — expect < 8 red edges (dedup) and > 4
        assert!(reds < 8, "shared edge deduped: {reds} red edges");
        assert!(reds >= 6, "perimeter still drawn: {reds} red edges");
    }

    #[test]
    fn caption_above_table() {
        let (items, _, _) = lay("<table><caption>cap</caption><tr><td>cell</td></tr></table>");
        let (_xc, yc) = text_pos(&items, "cap");
        let (_xs, ys) = text_pos(&items, "cell");
        assert!(yc < ys, "caption above the table: {yc} vs {ys}");
    }

    #[test]
    fn caption_side_bottom() {
        let (items, _, _) = lay(
            "<table><caption style=\"caption-side:bottom\">cap</caption>\
             <tr><td>cell</td></tr></table>",
        );
        let (_xc, yc) = text_pos(&items, "cap");
        let (_xs, ys) = text_pos(&items, "cell");
        assert!(yc > ys, "caption below the table: {yc} vs {ys}");
    }

    #[test]
    fn empty_cells_hide_background() {
        let (items, _, _) = lay("<table style=\"empty-cells:hide\"><tr>\
             <td style=\"background-color:#ff0000\"> </td>\
             <td style=\"background-color:#00ff00\">x</td></tr></table>");
        assert!(
            !items.iter().any(
                |i| matches!(i, DisplayItem::FillRect { color, .. } if *color == bgra(255, 0, 0))
            ),
            "empty cell bg hidden"
        );
        assert!(
            items.iter().any(
                |i| matches!(i, DisplayItem::FillRect { color, .. } if *color == bgra(0, 255, 0))
            ),
            "filled cell bg shown"
        );
    }

    #[test]
    fn cell_vertical_align_bottom() {
        // tall first cell makes a high row; second cell bottom-aligns
        let (items, _, _) = lay("<table><tr><td><div style=\"height:60px\"></div></td>\
             <td style=\"vertical-align:bottom\">low</td></tr></table>");
        let (_xl, yl) = text_pos(&items, "low");
        // top-aligned would sit at ~10px (body margin + spacing + padding);
        // bottom-aligned lands near the bottom of the ~80px row
        assert!(yl > 60.0, "bottom-aligned cell sits low: yl={yl}");
    }

    #[test]
    fn css_display_table_from_divs() {
        let (items, _, _) = lay("<style>.t{display:table}.r{display:table-row}\
             .c{display:table-cell}</style>\
             <div class=t><div class=r><div class=c>a</div><div class=c>b</div></div>\
             <div class=r><div class=c>d</div><div class=c>e</div></div></div>");
        let (xa, ya) = text_pos(&items, "a");
        let (xb, yb) = text_pos(&items, "b");
        let (xd, yd) = text_pos(&items, "d");
        let (xe, _ye) = text_pos(&items, "e");
        assert!((ya - yb).abs() < 0.01, "same row");
        assert!(xb > xa, "second cell right of first");
        assert!(yd > ya, "second row below");
        assert!((xd - xa).abs() < 1.0, "column 0 aligned");
        assert!((xe - xb).abs() < 1.0, "column 1 aligned");
    }

    #[test]
    fn display_contents_children_flow_in_parent() {
        let (items, _, _) = lay("<style>.w{display:contents}</style>\
             <div><p>before</p><div class=w><p>inside</p></div><p>after</p></div>");
        let t = texts(&items);
        let all: String = t
            .iter()
            .filter_map(|i| match i {
                DisplayItem::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(all.contains("before") && all.contains("inside") && all.contains("after"));
        let (_xb, yb) = text_pos(&items, "before");
        let (_xi, yi) = text_pos(&items, "inside");
        assert!(yi > yb, "contents child flows in the parent's order");
    }

    #[test]
    fn multicol_splits_into_columns_with_rule() {
        let (items, _, _) = lay(
            "<style>.m{column-count:2;column-gap:20px;column-rule:2px solid #ff0000}</style>\
             <div class=m><p>a</p><p>b</p></div>",
        );
        let (xa, _) = text_pos(&items, "a");
        let (xb, _) = text_pos(&items, "b");
        assert!(
            xb - xa > 300.0,
            "b lands in the second column: {xa} -> {xb}"
        );
        assert!(
            items.iter().any(|i| matches!(
                i,
                DisplayItem::FillRect { color, h, .. } if *color == bgra(255, 0, 0) && *h > 1.0
            )),
            "column-rule painted"
        );
    }

    #[test]
    fn multicol_column_span_all() {
        let (items, _, _) = lay("<style>.m{columns:2}.s{column-span:all}</style>\
             <div class=m><p>a</p><p>b</p><p class=s>SPAN</p><p>c</p><p>d</p></div>");
        let (xs, _) = text_pos(&items, "SPAN");
        let (xc, _) = text_pos(&items, "c");
        // the spanner sits at the container's left edge, second segment's
        // first column starts there too
        assert!(xs < 20.0, "spanner at left edge: {xs}");
        assert!(xc < 20.0, "post-span column 0 at left edge: {xc}");
    }

    #[test]
    fn display_flow_root_block() {
        // flow-root lays like a block: child below it follows normally
        let (items, _, _) = lay("<style>.r{display:flow-root}</style>\
             <div class=r><p>in</p></div><p>out</p>");
        let t = texts(&items);
        assert!(t.len() >= 2);
    }

    #[test]
    fn display_list_item_gets_marker() {
        let (items, _, _) = lay("<style>.i{display:list-item}</style>\
             <ul><div class=i>one</div></ul>");
        let t = texts(&items);
        let has_marker = t.iter().any(|i| match i {
            DisplayItem::Text { text, .. } => text.contains('•'),
            _ => false,
        });
        assert!(has_marker, "display:list-item gets a bullet marker");
    }

    #[test]
    fn grid_auto_fill_expands_tracks() {
        // repeat(auto-fill,100px) in ~784px avail → 7 explicit tracks;
        // `grid-column:-1` = the last explicit line → track 7 → x = 8+700
        let (items, _, _) = lay(
            "<style>.g{display:grid;grid-template-columns:repeat(auto-fill,100px)}\
             .x{grid-column:-1}</style>\
             <div class=g><div>a</div><div class=x>b</div></div>",
        );
        let (xa, _) = text_pos(&items, "a");
        let (xb, _) = text_pos(&items, "b");
        assert!(
            (xb - xa - 700.0).abs() < 2.0,
            "auto-fill expands 7 tracks, b in the implicit 8th: {xa} -> {xb}"
        );
    }

    #[test]
    fn grid_auto_fit_collapses_empty() {
        // repeat(auto-fit,100px) → same 7 tracks, but the 5 empty ones
        // collapse to 0: an item pinned at track 7 lands at +100 not +600
        let (items, _, _) = lay(
            "<style>.g{display:grid;grid-template-columns:repeat(auto-fit,100px)}\
             .x{grid-column:7}</style>\
             <div class=g><div>a</div><div class=x>b</div></div>",
        );
        let (xa, _) = text_pos(&items, "a");
        let (xb, _) = text_pos(&items, "b");
        assert!(
            (xb - xa - 100.0).abs() < 2.0,
            "empty auto-fit tracks collapse: {xa} -> {xb}"
        );
    }

    #[test]
    fn grid_auto_flow_dense_backfills() {
        // a,b span 2 of 3 columns → b wraps to row 1 leaving r0c2 free;
        // dense puts c in the hole instead of after b
        let (items, _, _) = lay(
            "<style>.g{display:grid;grid-template-columns:repeat(3,100px);\
             grid-auto-flow:dense}.w{grid-column:span 2}</style>\
             <div class=g><div class=w>a</div><div class=w>b</div><div>c</div><div>d</div></div>",
        );
        let (_xa, ya) = text_pos(&items, "a");
        let (_xb, yb) = text_pos(&items, "b");
        let (_xc, yc) = text_pos(&items, "c");
        let (_xd, yd) = text_pos(&items, "d");
        assert!(
            (yc - ya).abs() < 1.0,
            "dense c backfills row 0: ya={ya} yc={yc}"
        );
        assert!(yb > ya, "b lands on row 1");
        assert!((yd - yb).abs() < 1.0, "d backfills r1c2 beside b");
    }

    #[test]
    fn grid_auto_flow_column() {
        // 2 explicit rows + column flow → implicit 60px columns
        let (items, _, _) = lay("<style>.g{display:grid;grid-template-rows:20px 20px;\
             grid-auto-flow:column;grid-template-columns:100px;\
             grid-auto-columns:60px}</style>\
             <div class=g><div>a</div><div>b</div><div>c</div><div>d</div><div>e</div></div>");
        let (xa, ya) = text_pos(&items, "a");
        let (_xb, yb) = text_pos(&items, "b");
        let (xc, yc) = text_pos(&items, "c");
        let (xe, _ye) = text_pos(&items, "e");
        assert!(yb > ya, "column flow fills rows first");
        assert!((yc - ya).abs() < 1.0, "c wraps to the next column's top");
        assert!(
            (xc - xa - 100.0).abs() < 2.0,
            "explicit col 100px: {xa} -> {xc}"
        );
        assert!(
            (xe - xc - 60.0).abs() < 2.0,
            "implicit col 60px: {xc} -> {xe}"
        );
    }

    #[test]
    fn grid_fit_content_clamps() {
        // fit-content(80px): a ~200px max-content contribution clamps at 80
        let (items, _, _) = lay(
            "<style>.g{display:grid;grid-template-columns:fit-content(80px)}\
             .i{background:#ff0000}</style>\
             <div class=g><div class=i>aaa bbb ccc ddd eee fff ggg hhh iii</div></div>",
        );
        let r = items.iter().find(
            |i| matches!(i, DisplayItem::FillRect { color, .. } if *color == bgra(255, 0, 0)),
        );
        let Some(DisplayItem::FillRect { w, .. }) = r else {
            panic!("item bg not found")
        };
        assert!((*w - 80.0).abs() < 2.0, "fit-content clamps at 80px: w={w}");
    }

    #[test]
    fn grid_justify_content_center() {
        // 200px of tracks inside ~784 avail → the block centers: off ~292
        let (items, _, _) = lay("<style>.g{display:grid;grid-template-columns:100px 100px;\
             justify-content:center}</style>\
             <div class=g><div>a</div><div>b</div></div>");
        let (xa, _) = text_pos(&items, "a");
        let want = 8.0 + (784.0 - 200.0) / 2.0;
        assert!(
            (xa - want).abs() < 3.0,
            "track block centered: x={xa} want {want}"
        );
    }

    #[test]
    fn grid_named_numeric_lines() {
        // [a]100px[b]100px[a]100px[b] — `2 a` = the 2nd `a` line = line 3
        let (items, _, _) = lay("<style>.g{display:grid;\
             grid-template-columns:[a] 100px [b] 100px [a] 100px [b]}\
             .x{grid-column:2 a}</style>\
             <div class=g><div class=x>bb</div></div>");
        let (xb, _) = text_pos(&items, "bb");
        assert!(
            (xb - 8.0 - 200.0).abs() < 2.0,
            "line '2 a' → column 3: x={xb}"
        );
    }

    #[test]
    fn grid_span_to_named_line() {
        // `1 / span a` spans to the next `a` line (line 3) → 2 columns wide
        let (items, _, _) = lay("<style>.g{display:grid;\
             grid-template-columns:[a] 100px [b] 100px [a] 100px [b]}\
             .x{grid-column:1 / span a;background:#ff0000}</style>\
             <div class=g><div class=x></div></div>");
        let r = items.iter().find(
            |i| matches!(i, DisplayItem::FillRect { color, .. } if *color == bgra(255, 0, 0)),
        );
        let Some(DisplayItem::FillRect { w, .. }) = r else {
            panic!("item bg not found")
        };
        assert!((*w - 200.0).abs() < 2.0, "span a → 2 tracks: w={w}");
    }

    #[test]
    fn grid_shorthand_template_and_autoflow() {
        // `grid: 40px / 100px 100px` → template rows + columns
        let (items, _, _) = lay("<style>.g{display:grid;grid:40px / 100px 100px}</style>\
             <div class=g><div>a</div><div>b</div><div>c</div></div>");
        let (xa, ya) = text_pos(&items, "a");
        let (xb, _) = text_pos(&items, "b");
        let (xc, yc) = text_pos(&items, "c");
        assert!(
            (xb - xa - 100.0).abs() < 1.0,
            "shorthand cols: {xa} -> {xb}"
        );
        assert!(
            (yc - ya - 40.0).abs() < 2.0,
            "shorthand row 40px: {ya} -> {yc}"
        );
        assert!((xc - xa).abs() < 1.0, "c wraps to column 1");
        // `grid: auto-flow 40px / 100px 100px` → row flow, 40px auto rows
        let (items, _, _) = lay(
            "<style>.g{display:grid;grid:auto-flow 40px / 100px 100px}</style>\
             <div class=g><div>a</div><div>b</div><div>c</div></div>",
        );
        let (_, ya) = text_pos(&items, "a");
        let (_, yc) = text_pos(&items, "c");
        assert!((yc - ya - 40.0).abs() < 2.0, "auto row 40px: {ya} -> {yc}");
    }

    #[test]
    fn grid_auto_tracks_stretch() {
        // justify-content:normal → the two auto columns stretch evenly to
        // fill the ~784px container (col 1 starts ~halfway, not at ~16px
        // of content-sized tracks)
        let (items, _, _) = lay(
            "<style>.g{display:grid;grid-template-columns:auto auto}</style>\
             <div class=g><div>a</div><div>b</div></div>",
        );
        let (xa, _) = text_pos(&items, "a");
        let (xb, _) = text_pos(&items, "b");
        assert!(
            xb - xa > 300.0,
            "auto columns stretch to fill: {xa} -> {xb}"
        );
    }

    fn fill_rect(items: &[DisplayItem], color: u32) -> (f32, f32, f32, f32) {
        for i in items {
            if let DisplayItem::FillRect {
                x,
                y,
                w,
                h,
                color: c,
            } = i
            {
                if *c == color {
                    return (*x, *y, *w, *h);
                }
            }
        }
        panic!("no FillRect of color {color:#x}");
    }

    #[test]
    fn flex_order_reorders_items() {
        // `order` sorts items before placement: -1 first, 0 middle, 1 last
        let (items, _, _) = lay(
            "<style>.f{display:flex}.b{order:1}.a{order:-1}</style>\
             <div class=f><span class=b>one</span><span class=a>two</span><span>three</span></div>",
        );
        let (xa, _) = text_pos(&items, "one");
        let (xb, _) = text_pos(&items, "two");
        let (xc, _) = text_pos(&items, "three");
        assert!(
            xb < xc && xc < xa,
            "order -1,0,1 → two < three < one: {xb} {xc} {xa}"
        );
    }

    #[test]
    fn flex_shorthand_grows_item() {
        // `flex:1` → grow 1, basis 0% → the item absorbs all leftover width
        let (items, _, _) = lay("<style>.f{display:flex}.a{flex:1;background:#ff0000}\
             .b{width:100px}</style>\
             <div class=f><div class=b>side</div><div class=a></div></div>");
        let (x, _, w, _) = fill_rect(&items, bgra(255, 0, 0));
        assert!(
            (x - 8.0 - 100.0).abs() < 1.0,
            "grow item starts after 100px sibling, x={x}"
        );
        assert!(
            (w - 684.0).abs() < 2.0,
            "grow item fills remaining 784-100, w={w}"
        );
    }

    #[test]
    fn flex_wrap_makes_two_lines() {
        // 3×300px items in a 784px container wrap to two lines
        let (items, _, _) = lay(
            "<style>.f{display:flex;flex-wrap:wrap}.i{width:300px}</style>\
             <div class=f><div class=i>aa</div><div class=i>bb</div><div class=i>cc</div></div>",
        );
        let (_, ya) = text_pos(&items, "aa");
        let (_, yb) = text_pos(&items, "bb");
        let (_, yc) = text_pos(&items, "cc");
        assert!(
            (yb - ya).abs() < 0.5,
            "aa and bb share line 1: {ya} vs {yb}"
        );
        assert!(yc > ya + 10.0, "cc wraps to line 2: {ya} -> {yc}");
    }

    #[test]
    fn flex_align_content_space_between() {
        // two wrapped lines in a 200px container spread to the cross edges
        let (items, _, _) = lay("<style>.f{display:flex;flex-wrap:wrap;height:200px;\
             align-content:space-between}.i{width:300px}</style>\
             <div class=f><div class=i>aa</div><div class=i>bb</div><div class=i>cc</div></div>");
        let (_, ya) = text_pos(&items, "aa");
        let (_, yc) = text_pos(&items, "cc");
        assert!(
            yc - ya > 150.0,
            "lines spread to container edges: {ya} -> {yc}"
        );
    }

    #[test]
    fn flex_align_items_stretch_fills_line() {
        // auto-height item stretches to the line's cross size (80px)
        let (items, _, _) = lay("<style>.f{display:flex}.a{background:#ff0000}</style>\
             <div class=f><div class=a></div><div style='height:80px'>tall</div></div>");
        let (_, _, _, h) = fill_rect(&items, bgra(255, 0, 0));
        assert!((h - 80.0).abs() < 1.0, "stretched item height {h}");
    }

    #[test]
    fn flex_column_justify_content_center() {
        // column direction: justify-content centers along the vertical axis
        let (items, _, _) = lay("<style>.f{display:flex;flex-direction:column;height:200px;\
             justify-content:center}.i{height:20px}</style>\
             <div class=f><div class=i>mid</div></div>");
        let (_, yb) = text_pos(&items, "mid");
        // box top ≈ 8 + (200-20)/2 = 98; baseline a bit below the top
        assert!(
            yb > 8.0 + 95.0 && yb < 8.0 + 125.0,
            "vertically centered baseline {yb}"
        );
    }

    #[test]
    fn flex_min_width_floors_shrink() {
        // two 600px-basis items overflow 784px; min-width:550 stops b's
        // shrink while a shrinks to its share (an explicit `width` is its
        // own min-content, so the test uses flex-basis for the base size)
        let (items, _, _) = lay(
            "<style>.f{display:flex}.a{flex-basis:600px;background:#ff0000}\
             .b{flex-basis:600px;min-width:550px;background:#00ff00}</style>\
             <div class=f><div class=a></div><div class=b></div></div>",
        );
        let (_, _, wa, _) = fill_rect(&items, bgra(255, 0, 0));
        let (_, _, wb, _) = fill_rect(&items, bgra(0, 255, 0));
        assert!(wa < 500.0, "a shrank below its 600px base, w={wa}");
        assert!(
            (wb - 550.0).abs() < 1.0,
            "b floored at min-width 550, w={wb}"
        );
    }

    #[test]
    fn flex_basis_sizes_item() {
        // flex-basis:120px (no grow) sets the item's main size
        let (items, _, _) = lay(
            "<style>.f{display:flex}.a{flex-basis:120px;background:#ff0000}</style>\
             <div class=f><div class=a></div></div>",
        );
        let (_, _, w, _) = fill_rect(&items, bgra(255, 0, 0));
        assert!((w - 120.0).abs() < 1.0, "flex-basis width {w}");
    }

    #[test]
    fn flex_row_reverse_packs_from_right() {
        // row-reverse mirrors the pack direction: DOM order a,b → a on the right
        let (items, _, _) = lay(
            "<style>.f{display:flex;flex-direction:row-reverse}.i{width:100px}</style>\
             <div class=f><div class=i>aa</div><div class=i>bb</div></div>",
        );
        let (xa, _) = text_pos(&items, "aa");
        let (xb, _) = text_pos(&items, "bb");
        assert!(
            xa > xb,
            "row-reverse puts first item last: aa {xa}, bb {xb}"
        );
        assert!(
            (xa - 8.0 - 684.0).abs() < 1.5,
            "aa hugs the right edge, x={xa}"
        );
    }

    #[test]
    fn flex_space_between_spreads_items() {
        let (items, _, _) = lay(
            "<style>.f{display:flex;justify-content:space-between}.i{width:100px}</style>\
             <div class=f><div class=i>aa</div><div class=i>bb</div></div>",
        );
        let (xa, _) = text_pos(&items, "aa");
        let (xb, _) = text_pos(&items, "bb");
        assert!(
            (xa - 8.0).abs() < 1.5,
            "first item at the start edge, x={xa}"
        );
        assert!(
            (xb - 8.0 - 684.0).abs() < 1.5,
            "last item at the end edge, x={xb}"
        );
    }

    #[test]
    fn flex_auto_margin_pushes_item() {
        // margin-left:auto absorbs the leftover main space
        let (items, _, _) = lay(
            "<style>.f{display:flex}.i{width:100px}.b{margin-left:auto}</style>\
             <div class=f><div class=i>aa</div><div class='i b'>bb</div></div>",
        );
        let (xb, _) = text_pos(&items, "bb");
        assert!(
            (xb - 8.0 - 684.0).abs() < 1.5,
            "auto margin pushes bb to the right edge, x={xb}"
        );
    }

    #[test]
    fn flex_gap_separates_items() {
        let (items, _, _) = lay("<style>.f{display:flex;gap:20px}.i{width:100px}</style>\
             <div class=f><div class=i>aa</div><div class=i>bb</div></div>");
        let (xb, _) = text_pos(&items, "bb");
        assert!(
            (xb - 8.0 - 120.0).abs() < 1.5,
            "gap:20 → bb at x=128, x={xb}"
        );
    }

    #[test]
    fn flex_baseline_aligns_first_lines() {
        // different font sizes align on their first text baselines
        let (items, _, _) = lay("<style>.f{display:flex;align-items:baseline}\
             .a{font-size:32px}.b{font-size:12px}</style>\
             <div class=f><div class=a>big</div><div class=b>small</div></div>");
        let (_, ya) = text_pos(&items, "big");
        let (_, yb) = text_pos(&items, "small");
        assert!((yb - ya).abs() < 1.0, "shared first baseline: {ya} vs {yb}");
    }

    fn lay_fc(html: &str, fonts: &mut FontCache, external: &[css::StyleSheet]) -> Vec<DisplayItem> {
        let d = doc(html);
        let (items, _, _, _) = Layouter::layout(&d, fonts, 800.0, 0.0, external);
        items
    }

    fn first_font(items: &[DisplayItem]) -> Option<usize> {
        items.iter().find_map(|i| match i {
            DisplayItem::Text { font, .. } => Some(*font),
            _ => None,
        })
    }

    #[test]
    fn family_fallback_order() {
        // first unresolvable name is skipped; the second wins
        let mut fonts = FontCache::new();
        let want = fonts.resolve("liberation serif", false, false);
        let items = lay_fc(
            "<p style=\"font-family:'no such family xyz','Liberation Serif',monospace\">hi</p>",
            &mut fonts,
            &[],
        );
        if let Some(w) = want {
            assert_eq!(first_font(&items), Some(w));
        }
    }

    #[test]
    fn registered_webfont_wins() {
        let bytes =
            std::fs::read("/usr/share/fonts/liberation/LiberationSerif-Regular.ttf").unwrap();
        let mut fonts = FontCache::new();
        let idx = fonts
            .register_bytes("My Web Face", &bytes, 400, false, vec![])
            .unwrap();
        let items = lay_fc(
            "<p style=\"font-family:'My Web Face',serif\">hi</p>",
            &mut fonts,
            &[],
        );
        // declared family resolves to the registered bytes, not generics
        assert_eq!(first_font(&items), Some(idx));
    }

    #[test]
    fn unicode_range_picks_per_run_face() {
        let bytes =
            std::fs::read("/usr/share/fonts/liberation/LiberationSans-Regular.ttf").unwrap();
        let mut fonts = FontCache::new();
        let up = fonts
            .register_bytes("Ranged", &bytes, 400, false, vec![(0x41, 0x5a)])
            .unwrap();
        let lo = fonts
            .register_bytes("Ranged", &bytes, 400, false, vec![(0x61, 0x7a)])
            .unwrap();
        let items = lay_fc(
            "<style>p{font-family:Ranged}</style><p>ABC</p><p>abc</p>",
            &mut fonts,
            &[],
        );
        let got: Vec<usize> = items
            .iter()
            .filter_map(|i| match i {
                DisplayItem::Text { font, .. } => Some(*font),
                _ => None,
            })
            .collect();
        assert_eq!(got, vec![up, lo]);
    }

    #[test]
    fn weight_and_stretch_map_to_numbers() {
        assert_eq!(font_weight_value("bold", 400), 700);
        assert_eq!(font_weight_value("600", 400), 600);
        assert_eq!(font_weight_value("bolder", 400), 700);
        assert_eq!(font_weight_value("lighter", 900), 700);
        assert_eq!(font_stretch_value("condensed"), 75);
        assert_eq!(font_stretch_value("120%"), 120);
    }

    #[test]
    fn dashed_border_emits_border_side() {
        let (items, _, _) = lay("<style>.b{border:4px dashed #ff0000}</style><div class=b>x</div>");
        assert!(items.iter().any(|i| matches!(
            i,
            DisplayItem::BorderSide { style, .. } if *style == BorderStyle::Dashed
        )));
    }

    #[test]
    fn per_side_border_styles() {
        let (items, _, _) = lay("<style>.b{border-width:2px;border-style:solid double;\
             border-color:#ff0000}</style><div class=b>x</div>");
        // solid sides stay FillRects; double sides become BorderSide items
        assert!(items.iter().any(|i| matches!(
            i,
            DisplayItem::BorderSide { style, .. } if *style == BorderStyle::Double
        )));
        assert!(items.iter().any(|i| matches!(
            i,
            DisplayItem::FillRect { color, .. } if *color == bgra(255, 0, 0)
        )));
    }

    #[test]
    fn corner_radius_emits_rounded2() {
        let (items, _, _) = lay("<style>.r{border-top-left-radius:20px;background:#ff0000;\
             width:100px;height:100px}</style><div class=r></div>");
        assert!(items.iter().any(|i| matches!(
            i,
            DisplayItem::RoundedRect2 { radii, .. } if radii[0][0] > 10.0
        )));
    }

    #[test]
    fn slash_radius_resolves_per_corner() {
        let (items, _, _) = lay("<style>.r{border-radius:10px 20px/5px;background:#ff0000;\
             width:100px;height:100px}</style><div class=r></div>");
        let r = items.iter().find_map(|i| match i {
            DisplayItem::RoundedRect2 { radii, .. } => Some(*radii),
            _ => None,
        });
        let Some(radii) = r else {
            panic!("no RoundedRect2 emitted")
        };
        assert!((radii[0][0] - 10.0).abs() < 0.5 && (radii[0][1] - 5.0).abs() < 0.5);
        assert!((radii[1][0] - 20.0).abs() < 0.5 && (radii[1][1] - 5.0).abs() < 0.5);
    }

    #[test]
    fn box_shadow_multiple_and_inset() {
        let (items, _, _) = lay("<style>.s{box-shadow:2px 3px red,inset 0 0 4px blue;\
             width:10px;height:10px}</style><div class=s>x</div>");
        let sh: Vec<_> = items
            .iter()
            .filter(|i| matches!(i, DisplayItem::Shadow { .. }))
            .collect();
        assert_eq!(sh.len(), 2);
        let ins = sh.iter().any(|i| {
            matches!(
                i,
                DisplayItem::Shadow { inset: true, blur, .. } if *blur > 0.0
            )
        });
        assert!(ins, "one inset shadow with blur");
    }

    #[test]
    fn overflow_hidden_clips_children() {
        let (items, _, _) = lay("<style>.o{overflow:hidden;width:100px;height:10px}</style>\
             <div class=o><div>inner text</div></div>");
        let pos = |pred: &dyn Fn(&DisplayItem) -> bool| items.iter().position(pred);
        let push = pos(&|i| matches!(i, DisplayItem::PushClip { .. })).expect("PushClip");
        let pop = pos(&|i| matches!(i, DisplayItem::PopClip)).expect("PopClip");
        let txt = pos(&|i| matches!(i, DisplayItem::Text { .. })).expect("text");
        assert!(push < txt && txt < pop, "children inside the clip");
    }

    #[test]
    fn outline_paints_outside_border() {
        let (items, _, _) = lay(
            "<style>.o{outline:2px solid #ff0000;width:50px;height:20px}</style>\
             <div class=o>x</div>",
        );
        let reds = items
            .iter()
            .filter(|i| {
                matches!(
                    i,
                    DisplayItem::FillRect { color, .. } if *color == bgra(255, 0, 0)
                )
            })
            .count();
        assert!(reds >= 4, "four outline edges");
    }

    #[test]
    fn gradient_background_emits_raster() {
        let (items, _, _) = lay(
            "<style>div{height:40px;background:linear-gradient(90deg,#ff0000,#0000ff)}</style><div></div>",
        );
        let r = items
            .iter()
            .find_map(|it| match it {
                DisplayItem::Raster { x, y, w, h, pixels } => Some((*x, *y, *w, *h, pixels)),
                _ => None,
            })
            .expect("gradient raster item");
        assert_eq!(r.0, 8.0);
        assert_eq!(r.2, 784.0); // body-margin edge to edge
        assert_eq!(r.3, 40.0);
        let px = r.4;
        assert!(
            px[2] > 240 && px[0] < 20,
            "left pixel ~red: {:?}",
            &px[0..4]
        );
        let ri = 783 * 4;
        assert!(
            px[ri] > 240 && px[ri + 2] < 20,
            "right pixel ~blue: {:?}",
            &px[ri..ri + 4]
        );
    }

    #[test]
    fn background_image_emits_bgimage() {
        let (items, _, _) = lay(
            "<style>div{height:50px;background-image:url(a.png);background-repeat:no-repeat}</style><div></div>",
        );
        match items
            .iter()
            .find(|it| matches!(it, DisplayItem::BgImage { .. }))
        {
            Some(DisplayItem::BgImage {
                x,
                y,
                w,
                h,
                rx,
                ry,
                src,
                ..
            }) => {
                assert_eq!((*x, *y, *w, *h), (8.0, 8.0, 784.0, 50.0));
                assert_eq!((*rx, *ry), (0, 0));
                assert_eq!(src, "a.png");
            }
            _ => panic!("BgImage missing"),
        }
    }

    #[test]
    fn background_cover_contain_modes() {
        let (items, _, _) = lay(
            "<style>div{height:50px;background:url(a) center/cover no-repeat}</style><div></div>",
        );
        assert!(
            items
                .iter()
                .any(|it| matches!(it, DisplayItem::BgImage { mode: 1, .. })),
            "cover BgImage missing"
        );
        let (items, _, _) = lay(
            "<style>div{height:50px;background-image:url(a);background-size:contain}</style><div></div>",
        );
        assert!(
            items
                .iter()
                .any(|it| matches!(it, DisplayItem::BgImage { mode: 2, .. })),
            "contain BgImage missing"
        );
    }

    #[test]
    fn background_position_four_value() {
        let (items, _, _) = lay(
            "<style>div{height:50px;background:url(a) right 10px bottom 20px no-repeat}</style><div></div>",
        );
        match items.iter().find_map(|it| match it {
            DisplayItem::BgImage { fx, ax, fy, ay, .. } => Some((*fx, *ax, *fy, *ay)),
            _ => None,
        }) {
            Some(v) => assert_eq!(v, (1.0, -10.0, 1.0, -20.0)),
            None => panic!("BgImage missing"),
        }
    }

    #[test]
    fn object_fit_cover_uses_bgimage() {
        let (items, _, _) = lay("<img src=p.png width=100 height=50 style='object-fit:cover'>");
        match items
            .iter()
            .find(|it| matches!(it, DisplayItem::BgImage { .. }))
        {
            Some(DisplayItem::BgImage {
                mode, w, h, src, ..
            }) => {
                assert_eq!(*mode, 1);
                assert_eq!((*w, *h), (100.0, 50.0));
                assert_eq!(src, "p.png");
            }
            _ => panic!("object-fit BgImage missing"),
        }
    }

    #[test]
    fn body_background_image_paints_canvas() {
        let (items, _, _) = lay("<style>body{background:url(bg.png) no-repeat}</style>");
        match items
            .iter()
            .find(|it| matches!(it, DisplayItem::BgImage { .. }))
        {
            Some(DisplayItem::BgImage { x, y, w, .. }) => {
                assert_eq!((*x, *y, *w), (0.0, 0.0, 800.0)); // viewport-wide canvas rect
            }
            _ => panic!("canvas BgImage missing"),
        }
    }

    fn lay_scroll(html: &str, scroll: f32) -> Vec<DisplayItem> {
        let d = doc(html);
        let mut fonts = FontCache::new();
        Layouter::layout(&d, &mut fonts, 800.0, scroll, &[]).0
    }

    fn rect_index(items: &[DisplayItem], color: u32) -> Option<usize> {
        items.iter().position(|i| match i {
            DisplayItem::FillRect { color: c, .. } | DisplayItem::RoundedRect { color: c, .. } => {
                *c == color
            }
            _ => false,
        })
    }

    #[test]
    fn float_wraps_inline_text() {
        // a left float indents the lines it overlaps but not lines below it
        let (items, _, _) = lay("<style>.fl{float:left;width:200px;height:30px}</style>\
             <div class=fl></div>\
             <p>aa bb cc dd ee ff gg hh ii jj kk ll mm nn oo pp qq rr ss tt uu vv ww xx yy zz \
                aa bb cc dd ee ff gg hh ii jj kk ll mm nn oo pp qq rr ss tt uu vv ww xx yy zz</p>");
        let t = texts(&items);
        let mut by_x: Vec<(f32, f32)> = t
            .iter()
            .filter_map(|i| match i {
                DisplayItem::Text { x, baseline, .. } => Some((*x, *baseline)),
                _ => None,
            })
            .collect();
        by_x.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        // first line starts right of the float (body margin 8 + float 200)
        assert!(
            by_x[0].0 > 200.0,
            "line 0 insets past the float, x={}",
            by_x[0].0
        );
        // the deepest line is back at the left margin once past the float's
        // bottom — check the leftmost word on that line
        let deep_y = by_x.last().unwrap().1;
        let deep_x = by_x
            .iter()
            .filter(|w| (w.1 - deep_y).abs() < 1.0)
            .map(|w| w.0)
            .fold(f32::INFINITY, f32::min);
        assert!(
            deep_x < 20.0,
            "line below the float returns to the left edge"
        );
    }

    #[test]
    fn clear_drops_below_float() {
        let (items, _, _) = lay(
            "<style>.fl{float:left;width:200px;height:80px}.cl{clear:left}</style>\
             <div class=fl></div><div class=cl>below</div>",
        );
        let (_, by) = text_pos(&items, "below");
        assert!(
            by >= 60.0,
            "cleared block below the 80px float, baseline={by}"
        );
    }

    #[test]
    fn float_drops_when_no_room() {
        let (items, _, _) = lay("<style>.f{float:left;width:60%;height:50px}</style>\
             <div class=f style=background:#111></div>\
             <div class=f style=background:#222></div>");
        let a = rect_index(&items, bgra(17, 17, 17)).expect("first float bg");
        let b = rect_index(&items, bgra(34, 34, 34)).expect("second float bg");
        let (xa, ya, xb, yb) = match (&items[a], &items[b]) {
            (
                DisplayItem::FillRect { x: xa, y: ya, .. },
                DisplayItem::FillRect { x: xb, y: yb, .. },
            ) => (*xa, *ya, *xb, *yb),
            _ => panic!("float bgs are fill rects"),
        };
        assert!(
            (xb - xa).abs() < 1.0,
            "second float returns to the left edge"
        );
        assert!(yb > ya + 40.0, "second float drops below the first");
    }

    #[test]
    fn float_shrink_to_fit() {
        // auto-width float hugs its max-content, not the longest word
        let (items, _, _) = lay("<div style=\"float:left;background:#112233\">aa bbbbbbbb</div>");
        let i = rect_index(&items, bgra(0x11, 0x22, 0x33)).expect("float bg");
        let w = match items[i] {
            DisplayItem::FillRect { w, .. } | DisplayItem::RoundedRect { w, .. } => w,
            _ => panic!(),
        };
        assert!(w > 60.0, "float width covers both words, w={w}");
        assert!(w < 400.0, "shrink-wrapped, not full width, w={w}");
    }

    #[test]
    fn sticky_clamps_at_top_on_scroll() {
        let html = "<style>.wrap{height:2000px}.hd{position:sticky;top:0;height:40px}</style>\
                    <div class=wrap><div class=hd>hdr</div></div>";
        let at0 = lay_scroll(html, 0.0);
        let (_, y0) = text_pos(&at0, "hdr");
        let at200 = lay_scroll(html, 200.0);
        let (_, y200) = text_pos(&at200, "hdr");
        assert!(y200 >= 0.0, "sticky stays at/under the top inset, y={y200}");
        assert!(y200 < 60.0, "sticky pins near the viewport top, y={y200}");
        // without sticking the header would have scrolled far off-screen
        assert!(y200 > y0 - 60.0, "stuck below its flow position");
    }

    #[test]
    fn sticky_unsticks_past_parent() {
        // the stick is bounded by the parent's bottom — scroll far past and
        // the header rides up with the wrapper's end
        let html = "<style>.wrap{height:100px}.hd{position:sticky;top:0;height:40px}</style>\
                    <div class=wrap><div class=hd>hdr</div></div>\
                    <div style=height:2000px></div>";
        let items = lay_scroll(html, 500.0);
        let (_, y) = text_pos(&items, "hdr");
        assert!(y < -20.0, "sticky scrolls off with its parent, y={y}");
    }

    #[test]
    fn z_index_reorders_paint() {
        let (items, _, _) = lay("<style>\
             .a{position:absolute;top:0;left:0;width:100px;height:100px;background:#111111}\
             .b{position:absolute;top:0;left:0;width:50px;height:50px;background:#222222;z-index:2}\
             </style>\
             <div class=a></div><div class=b></div><p>text here</p>");
        let a = rect_index(&items, bgra(0x11, 0x11, 0x11)).expect("a rect");
        let b = rect_index(&items, bgra(0x22, 0x22, 0x22)).expect("b rect");
        assert!(b > a, "z-index:2 paints after z-auto even though DOM-later");
        let ti = items
            .iter()
            .position(|i| matches!(i, DisplayItem::Text { .. }))
            .expect("text");
        assert!(
            a > ti && b > ti,
            "positioned boxes paint above in-flow text"
        );
    }

    #[test]
    fn negative_z_below_flow() {
        let (items, _, _) = lay(
            "<div>\
             <div style=\"background:#222222;height:50px\"></div>\
             <div style=\"position:absolute;z-index:-1;background:#111111;top:0;left:0;width:100px;height:100px\"></div>\
             </div>",
        );
        let flow = rect_index(&items, bgra(0x22, 0x22, 0x22)).expect("flow bg");
        let neg = rect_index(&items, bgra(0x11, 0x11, 0x11)).expect("neg-z bg");
        assert!(neg < flow, "z-index:-1 paints below in-flow siblings");
    }

    #[test]
    fn absolute_resolves_against_positioned_ancestor() {
        let (items, _, _) = lay("<style>\
             .rel{position:relative;margin:100px 0 0 50px;width:300px;height:200px}\
             .ab{position:absolute;top:10px;left:20px;width:30px;height:30px;background:#123456}\
             </style>\
             <div class=rel><div class=ab></div></div>");
        let i = rect_index(&items, bgra(0x12, 0x34, 0x56)).expect("abspos bg");
        let (x, y) = match items[i] {
            DisplayItem::FillRect { x, y, .. } | DisplayItem::RoundedRect { x, y, .. } => (x, y),
            _ => panic!(),
        };
        // body margin 8 + rel margin-left 50 + left 20 = 78
        assert!(
            (x - 78.0).abs() < 2.0,
            "abspos x in the ancestor's cb, x={x}"
        );
        // body margin 8 + rel margin-top 100 + top 10 = 118
        assert!(
            (y - 118.0).abs() < 2.0,
            "abspos y in the ancestor's cb, y={y}"
        );
    }

    fn hit_at(hits: &[Hit], x: f32, y: f32) -> Option<NodeId> {
        hits.iter()
            .filter(|(hx, hy, hw, hh, _, m)| {
                let Some(inv) = m.inverse() else { return false };
                let (px, py) = inv.apply(x, y);
                px >= *hx && px <= hx + hw && py >= *hy && py <= hy + hh
            })
            .min_by(|a, b| {
                (a.2 * a.3)
                    .partial_cmp(&(b.2 * b.3))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|h| h.4)
    }

    fn lay_hits(html: &str) -> (Vec<DisplayItem>, Vec<Hit>) {
        let d = doc(html);
        let mut fonts = FontCache::new();
        let (items, _, _, hits) = Layouter::layout(&d, &mut fonts, 800.0, 0.0, &[]);
        (items, hits)
    }

    #[test]
    fn translate_stays_on_direct_offset_path() {
        // pure translate() → no transform markers; items + hits just shift
        let (items, hits) = lay_hits(
            "<div style=\"transform:translate(10px,20px);width:50px;height:50px;\
             background:#ff0000\"></div>",
        );
        assert!(
            !items.iter().any(|i| matches!(
                i,
                DisplayItem::PushTransform { .. } | DisplayItem::PopTransform
            )),
            "translate-only transform must not emit matrix markers"
        );
        let r = items.iter().find(
            |i| matches!(i, DisplayItem::FillRect { color, .. } if *color == bgra(255, 0, 0)),
        );
        let Some(DisplayItem::FillRect { x, y, w, h, .. }) = r else {
            panic!("missing bg rect")
        };
        assert!(
            (*x - 18.0).abs() < 1.0 && (*y - 28.0).abs() < 1.0,
            "shifted bg at ({x},{y})"
        );
        assert!((*w - 50.0).abs() < 1.0 && (*h - 50.0).abs() < 1.0);
        // the element's hit moved with it and stays identity-mapped
        let h50 = hits
            .iter()
            .find(|h| (h.2 - 50.0).abs() < 1.0 && (h.3 - 50.0).abs() < 1.0)
            .expect("50x50 hit");
        assert!((h50.0 - 18.0).abs() < 1.0 && (h50.1 - 28.0).abs() < 1.0);
        assert!(h50.5.is_identity());
        // a click on the shifted spot finds the moved element
        assert!(hit_at(&hits, 30.0, 40.0).is_some());
    }

    #[test]
    fn rotate_emits_transform_wrapper() {
        let (items, hits) = lay_hits(
            "<div id=r style=\"transform:rotate(90deg);width:200px;height:20px\
             ;background:#ff0000\"></div>",
        );
        let pi = items
            .iter()
            .position(|i| matches!(i, DisplayItem::PushTransform { .. }))
            .expect("rotation must emit PushTransform");
        assert!(
            items[pi + 1..]
                .iter()
                .any(|i| matches!(i, DisplayItem::PopTransform)),
            "PushTransform needs a matching PopTransform"
        );
        let DisplayItem::PushTransform { m } = items[pi] else {
            unreachable!()
        };
        assert!(!m.is_identity());
        // wrapped items keep pre-transform coords: the bg still sits at the
        // laid-out spot (8,8)-ish, NOT rotated
        let r = items.iter().find(
            |i| matches!(i, DisplayItem::FillRect { color, .. } if *color == bgra(255, 0, 0)),
        );
        let Some(DisplayItem::FillRect { x, y, w, h, .. }) = r else {
            panic!("missing bg rect")
        };
        assert!((*w - 200.0).abs() < 1.0 && (*h - 20.0).abs() < 1.0);
        assert!(
            *x < 20.0 && *y < 20.0,
            "pre-transform coords kept ({x},{y})"
        );
        // the element's hit carries the same matrix
        let hrot = hits
            .iter()
            .find(|h| (h.2 - 200.0).abs() < 1.0 && (h.3 - 20.0).abs() < 1.0)
            .expect("200x20 hit");
        assert!(!hrot.5.is_identity(), "transformed hit keeps its matrix");
    }

    #[test]
    fn rotated_hit_inverse_maps_clicks() {
        // 200x20 bar rotated 90° paints as a vertical strip; the click map
        // must follow the real shape, not the laid-out rect
        let (_, hits) =
            lay_hits("<div id=r style=\"transform:rotate(90deg);width:200px;height:20px\"></div>");
        let hrot = hits
            .iter()
            .find(|h| !h.5.is_identity() && (h.2 - 200.0).abs() < 1.0)
            .copied()
            .expect("rotated hit");
        let node = hrot.4;
        // painted position inside the rect's top-left corner → click hits
        // (a point 1px in avoids the boundary's float jitter)
        let (cx, cy) = hrot.5.apply(hrot.0 + 1.0, hrot.1 + 1.0);
        assert_eq!(hit_at(&hits, cx, cy), Some(node), "painted corner");
        let (cx, cy) = hrot.5.apply(hrot.0 + hrot.2 / 2.0, hrot.1 + hrot.3 / 2.0);
        assert_eq!(hit_at(&hits, cx, cy), Some(node), "painted center");
        // a click inside the laid-out rect but outside the rotated shape
        // must NOT reach the element (no mere-AABB approximation)
        let (bx, by) = (hrot.0 + 2.0, hrot.1 + 2.0);
        let (ix, iy) = hrot.5.inverse().unwrap().apply(bx, by);
        let inside = ix >= hrot.0 && ix <= hrot.0 + hrot.2 && iy >= hrot.1 && iy <= hrot.1 + hrot.3;
        assert!(!inside, "un-rotated spot must inverse-map outside");
        assert_ne!(hit_at(&hits, bx, by), Some(node));
    }

    #[test]
    fn nested_transform_composes_on_hits() {
        // outer translate + inner rotate: the inner element's hit matrix is
        // the composition — inverse-mapping still lands exactly
        let (_, hits) = lay_hits(
            "<div style=\"transform:translate(50px,0px) rotate(90deg)\
             ;width:200px;height:100px\">             <div style=\"transform:rotate(90deg);width:40px;height:10px\"></div>             </div>",
        );
        let h_in = hits
            .iter()
            .find(|h| !h.5.is_identity() && (h.2 - 40.0).abs() < 1.0)
            .copied()
            .expect("inner hit");
        // outer list is translate·rotate → wraps too (rotate present)
        let (cx, cy) = h_in.5.apply(h_in.0 + 1.0, h_in.1 + 1.0);
        assert_eq!(hit_at(&hits, cx, cy), Some(h_in.4));
    }
    #[test]
    fn letter_spacing_widens_words() {
        let (a, _, _) = lay("<p>hello</p>");
        let (b, _, _) = lay("<style>p{letter-spacing:2px}</style><p>hello</p>");
        let (t, b) = (texts(&a), texts(&b));
        if let (
            DisplayItem::Text { ls: l0, .. },
            DisplayItem::Text {
                ls: l1, text: t1, ..
            },
        ) = (t[0], b[0])
        {
            assert_eq!(*l0, 0.0);
            assert_eq!(*l1, 2.0);
            assert_eq!(t1, "hello");
        }
    }

    #[test]
    fn word_spacing_widens_gap() {
        let (a, _, _) = lay("<p>one two</p>");
        let (b, _, _) = lay("<style>p{word-spacing:20px}</style><p>one two</p>");
        let get2 = |items: &[DisplayItem]| -> f32 { tinfo(texts(items)[1]).0 };
        assert!(get2(&b) - get2(&a) > 19.0, "gap grew by ~20px");
    }

    #[test]
    fn text_align_justify_stretches() {
        let (items, _, _) = lay(
            "<style>p{text-align:justify;width:160px}</style>             <p>alpha beta gamma delta epsilon zeta</p>",
        );
        let t = texts(&items);
        let b1 = tinfo(t[0]).1;
        let b2 = tinfo(t[t.len() - 1]).1;
        assert!(b2 > b1, "text wrapped to a second line");
        // a justified wrap stretches gaps: words on line 1 sit further
        // apart than under plain left alignment
        let xs: Vec<f32> = t
            .iter()
            .map(|i| tinfo(i))
            .filter(|i| i.1 == b1)
            .map(|i| i.0)
            .collect();
        assert!(xs.len() >= 3, "3+ words on line 1: {xs:?}");
        let (plain, _, _) = lay(
            "<style>p{width:160px}</style>             <p>alpha beta gamma delta epsilon zeta</p>",
        );
        let tp = texts(&plain);
        let pb1 = tinfo(tp[0]).1;
        let pxs: Vec<f32> = tp
            .iter()
            .map(|i| tinfo(i))
            .filter(|i| i.1 == pb1)
            .map(|i| i.0)
            .collect();
        assert!(
            xs[1] > pxs[1] + 1.0,
            "justify stretched the gap: {xs:?} vs {pxs:?}"
        );
    }

    #[test]
    fn text_indent_first_line_only() {
        let (items, _, _) = lay(
            "<style>p{text-indent:40px;width:120px}</style>             <p>aaaa bbbb cccc dddd eeee ffff</p>",
        );
        let t = texts(&items);
        let (x0, b0, _, _, _) = tinfo(t[0]);
        assert!(x0 >= 39.0, "first line indented: x={x0}");
        // a word on the second line returns to the content edge
        let second = t.iter().find(|i| tinfo(i).1 > b0);
        if let Some(i) = second {
            let (sx, _, _, _, _) = tinfo(i);
            assert!(sx < 20.0, "second line not indented: x={sx}");
        }
    }

    #[test]
    fn pre_line_breaks_and_collapses() {
        let (items, _, _) =
            lay("<style>p{white-space:pre-line;width:400px}</style>             <p>a   b\nc</p>");
        let t = texts(&items);
        // 'a' and 'b' share line 1 (spaces collapsed), 'c' on line 2
        let b0 = tinfo(t[0]).1;
        assert!(
            t.iter().any(|i| tinfo(i).1 > b0 + 1.0),
            "\\n broke the line"
        );
    }

    #[test]
    fn decoration_lines_and_style() {
        let (items, _, _) = lay(
            "<style>.u{text-decoration:underline wavy red 2px}             .o{text-decoration:overline}             .n{text-decoration:none}</style>             <p><span class=u>a</span><span class=o>b</span><u><span class=n>c</span></u></p>",
        );
        let t = texts(&items);
        if let DisplayItem::Text { deco, .. } = t[0] {
            assert!(deco.underline() && deco.style == DecoStyle::Wavy);
            assert_eq!(deco.thick, Some(2.0));
        }
        if let DisplayItem::Text { deco, .. } = t[1] {
            assert!(deco.lines & 2 != 0, "overline bit");
            assert!(!deco.underline());
        }
        if let DisplayItem::Text { deco, .. } = t[2] {
            assert_eq!(deco.lines, 0, "none clears inherited underline");
        }
    }

    #[test]
    fn text_shadow_emits_copies() {
        let (items, _, _) = lay("<style>p{text-shadow:5px 3px 0 #f00}</style><p>hi</p>");
        let t = texts(&items);
        assert_eq!(t.len(), 2, "shadow copy + main run");
        let (sx, sb, _, sc, _) = tinfo(t[0]);
        let (mx, mb, _, _, _) = tinfo(t[1]);
        assert_eq!(sx - mx, 5.0);
        assert_eq!(sb - mb, 3.0);
        assert_eq!(sc, bgra(255, 0, 0));
    }

    #[test]
    fn text_overflow_ellipsis() {
        let (items, _, _) = lay(
            "<style>p{overflow:hidden;text-overflow:ellipsis;             white-space:nowrap;width:80px}</style>             <p>supercalifragilisticexpialidocious</p>",
        );
        let t = texts(&items);
        let text = tinfo(t[t.len() - 1]).4;
        assert!(text.ends_with('\u{2026}'), "ends with ellipsis: {text}");
        assert!(text.len() < 40, "truncated: {text}");
    }

    #[test]
    fn small_caps_scales_lowercase() {
        let (items, _, _) = lay("<style>p{font-variant:small-caps}</style><p>Ab</p>");
        let t = texts(&items);
        let strs: Vec<(&str, f32)> = t
            .iter()
            .map(|i| {
                let (_, _, sz, _, tx) = tinfo(i);
                (tx, sz)
            })
            .collect();
        assert!(
            strs.iter().any(|(t, s)| *t == "B" && *s < 16.0),
            "b → small cap: {strs:?}"
        );
    }

    #[test]
    fn vertical_align_sub_shift() {
        let (items, _, _) = lay("<p>a<sub>b</sub></p>");
        let t = texts(&items);
        let (ba, bb) = (tinfo(t[0]).1, tinfo(t[1]).1);
        assert!(bb > ba + 2.0, "sub sits lower");
    }

    #[test]
    fn tab_size_expands() {
        let (a, _, _) = lay("<pre>x\ty</pre>");
        let (b, _, _) = lay("<style>pre{tab-size:2}</style><pre>x\ty</pre>");
        let gx = |items: &[DisplayItem]| -> f32 { tinfo(texts(items)[1]).0 };
        assert!(
            gx(&a) - gx(&b) > 10.0,
            "tab 8 vs 2: {} vs {}",
            gx(&a),
            gx(&b)
        );
    }

    #[test]
    fn soft_hyphen_breaks_with_hyphen() {
        let (items, _, _) =
            lay("<style>p{width:90px}</style>             <p>aaaaa bbbb\u{ad}cccc\u{ad}dddd</p>");
        let t = texts(&items);
        let strs: Vec<String> = t.iter().map(|i| tinfo(i).4.to_string()).collect();
        assert!(
            strs.iter().all(|s| !s.contains('\u{ad}')),
            "no raw shy: {strs:?}"
        );
        assert!(
            strs.iter().any(|s| s.ends_with('-')),
            "a shy break shows a hyphen: {strs:?}"
        );
    }

    #[test]
    fn rtl_direction_reverses_word_order() {
        let (items, _, _) = lay(r#"<style>p{direction:rtl}</style><p>AA BB CC DD EE FF GG HH</p>"#);
        let xs: Vec<f32> = texts(&items).iter().map(|i| tinfo(i).0).collect();
        let (items2, _, _) = lay(r#"<p>AA BB CC DD EE FF GG HH</p>"#);
        let xs2: Vec<f32> = texts(&items2).iter().map(|i| tinfo(i).0).collect();
        assert!(xs2.windows(2).all(|w| w[0] < w[1]), "ltr ascends: {xs2:?}");
        assert!(xs.windows(2).all(|w| w[0] > w[1]), "rtl descends: {xs:?}");
    }

    #[test]
    fn logical_sizes_map_to_physical() {
        let (items, _, _) = lay(
            r#"<style>div{inline-size:120px;block-size:40px;background:#f00}</style>             <div><span>x</span></div>"#,
        );
        let b = items
            .iter()
            .find_map(|i| match i {
                DisplayItem::FillRect { w, h, color, .. }
                    if *w > 100.0 && *color == bgra(255, 0, 0) =>
                {
                    Some((*w, *h))
                }
                _ => None,
            })
            .expect("div bg rect");
        assert_eq!(b, (120.0, 40.0));
    }

    #[test]
    fn text_wrap_nowrap_maps_to_nowrap() {
        let (items, _, _) = lay(
            r#"<style>p{text-wrap:nowrap}</style><p>one two three four five six seven eight</p>"#,
        );
        let t = texts(&items);
        let y0 = tinfo(t[0]).1;
        assert!(t.iter().all(|i| tinfo(i).1 == y0), "nowrap single line");
    }

    #[test]
    fn user_select_none_marks_words() {
        let (items, _, _) = lay(
            r#"<style>.nope{user-select:none;-webkit-user-select:none}</style>             <p>yes <span class=nope>hidden</span> tail</p>"#,
        );
        let mut saw_yes = false;
        let mut saw_hidden = false;
        for i in texts(&items) {
            if let DisplayItem::Text { text, sel, .. } = i {
                if text.trim() == "yes" {
                    assert!(*sel);
                    saw_yes = true;
                }
                if text.trim() == "hidden" {
                    assert!(!*sel, "user-select:none word");
                    saw_hidden = true;
                }
            }
        }
        assert!(saw_yes && saw_hidden);
    }

    #[test]
    fn srcset_picks_covering_candidate() {
        let (items, _, _) = lay(
            r#"<img width="300" src="fallback.png"              srcset="small.png 200w, mid.png 400w, big.png 1200w">"#,
        );
        let src = items.iter().find_map(|i| match i {
            DisplayItem::Image { src, .. } | DisplayItem::BgImage { src, .. } => Some(src.as_str()),
            _ => None,
        });
        // want=300 → smallest covering is mid (400w)
        assert_eq!(src, Some("mid.png"));
    }

    #[test]
    fn line_clamp_truncates_with_ellipsis() {
        let (items, _, _) = lay(
            r#"<style>p{width:60px;-webkit-line-clamp:2}</style>             <p>one two three four five six seven eight nine ten</p>"#,
        );
        let t = texts(&items);
        let strs: Vec<String> = t.iter().map(|i| tinfo(i).4.to_string()).collect();
        assert!(
            strs.iter().any(|s| s.ends_with('\u{2026}')),
            "ellipsis: {strs:?}"
        );
        for w in ["eight", "nine", "ten"] {
            assert!(!strs.iter().any(|s| s.contains(w)), "{w} leaked: {strs:?}");
        }
    }

    #[test]
    fn webkit_box_maps_to_flex() {
        let (items, _, _) = lay(
            r#"<style>d{display:-webkit-box}i{width:20px;height:10px;background:#f00}</style>             <d><i></i><i></i></d>"#,
        );
        let xs: Vec<f32> = items
            .iter()
            .filter_map(|i| match i {
                DisplayItem::FillRect { x, color, .. } if *color == bgra(255, 0, 0) => Some(*x),
                _ => None,
            })
            .collect();
        assert_eq!(xs.len(), 2);
        assert!((xs[1] - xs[0] - 20.0).abs() < 1.0, "flex row: {xs:?}");
    }

    #[test]
    fn image_rendering_pixelated_flags_item() {
        let (items, _, _) = lay(
            r#"<style>img{image-rendering:pixelated;width:32px;height:32px}</style>             <img src="a.png">"#,
        );
        let near = items.iter().find_map(|i| match i {
            DisplayItem::Image { near, .. } => Some(*near),
            _ => None,
        });
        assert_eq!(near, Some(true));
        let (items2, _, _) = lay(r#"<img src="a.png" width="32" height="32">"#);
        let near2 = items2.iter().find_map(|i| match i {
            DisplayItem::Image { near, .. } => Some(*near),
            _ => None,
        });
        assert_eq!(near2, Some(false), "auto must smooth");
    }

    #[test]
    fn mask_image_emits_effect() {
        let (items, _, _) = lay(
            r#"<style>d{-webkit-mask-image:url(m.png);width:20px;height:20px;             background:#000}</style><d></d>"#,
        );
        assert!(
            items.iter().any(|i| matches!(
                i,
                DisplayItem::PushEffect(colorfx::Effect::Mask { src }) if src == "m.png"
            )),
            "mask effect missing"
        );
    }
}
