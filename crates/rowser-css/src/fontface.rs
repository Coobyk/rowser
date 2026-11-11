//! `@font-face` descriptor parsing.
//!
//! A `@font-face { ... }` block holds descriptors, not style rules, so the
//! main parser hands its declaration list here and gets back a [`FontFace`]
//! the stylesheet carries for the renderer's font loader. Unknown
//! descriptors (font-display, size-adjust, ascent-override, ...) are
//! parsed past and ignored — they never make the face invalid.

use crate::{split_top, Decl};

/// One entry of a `src:` list, in declaration order.
#[derive(Debug, Clone, PartialEq)]
pub enum FontSrc {
    /// `url("…")` with optional `format("…")`/`tech("…")` hints.
    Url {
        url: String,
        /// format() keyword lowercased ("truetype", "woff2", ...).
        format: Option<String>,
        /// tech() keywords lowercased.
        tech: Vec<String>,
    },
    /// `local("Family Name")` — an already-installed face.
    Local(String),
}

/// `font-style` descriptor of a face.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FontFaceStyle {
    Normal,
    Italic,
    /// `oblique` optionally followed by a slant angle in degrees
    /// (default 14 per spec).
    Oblique(f32),
}

/// A parsed `@font-face` block: which family it claims, where to get the
/// bytes, and which (weight, style, stretch, codepoint) space it covers.
#[derive(Debug, Clone)]
pub struct FontFace {
    pub family: String,
    pub src: Vec<FontSrc>,
    /// `font-weight` range (min, max) — a plain weight is (w, w).
    pub weight: (u16, u16),
    pub style: FontFaceStyle,
    /// `font-stretch` percentage range — 100 = normal width.
    pub stretch: (u16, u16),
    /// `unicode-range` codepoint ranges; empty = unrestricted.
    pub unicode_range: Vec<(u32, u32)>,
}

impl FontFace {
    /// Whether the face's unicode-range admits `cp` (unrestricted = yes).
    pub fn covers(&self, cp: u32) -> bool {
        self.unicode_range.is_empty() || self.unicode_range.iter().any(|&(a, b)| cp >= a && cp <= b)
    }
}

/// Strip one layer of " or ' quoting plus surrounding whitespace.
fn unquote(s: &str) -> String {
    let s = s.trim();
    let inner = if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        &s[1..s.len() - 1]
    } else {
        s
    };
    inner.trim().to_string()
}

/// Extract `name(arg)` function calls from a css value fragment, in order.
/// Parens nest; quotes shield parens inside strings.
fn functions(s: &str) -> Vec<(String, String)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        if b[i].is_ascii_alphabetic() || b[i] == b'-' {
            let st = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'-') {
                i += 1;
            }
            let name = s[st..i].to_lowercase();
            let mut j = i;
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < b.len() && b[j] == b'(' {
                let mut depth = 0i32;
                let mut quote = 0u8;
                let mut k = j;
                while k < b.len() {
                    let c = b[k];
                    if quote != 0 {
                        if c == quote {
                            quote = 0;
                        }
                    } else if c == b'"' || c == b'\'' {
                        quote = c;
                    } else if c == b'(' {
                        depth += 1;
                    } else if c == b')' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    k += 1;
                }
                out.push((name, s[j + 1..k.min(s.len())].trim().to_string()));
                i = k + 1;
            } else {
                i = j.max(i + 1);
            }
        } else {
            i += 1;
        }
    }
    out
}

/// Parse one `src:` value into ordered [`FontSrc`] entries.
pub fn parse_src_list(v: &str) -> Vec<FontSrc> {
    let mut out = Vec::new();
    for item in split_top(v, ",") {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (mut url, mut local) = (None, None);
        let (mut format, mut tech) = (None, Vec::new());
        for (name, arg) in functions(item) {
            match name.as_str() {
                "url" => url = Some(unquote(&arg)),
                "local" => local = Some(unquote(&arg)),
                "format" => {
                    format = Some(unquote(&arg).to_lowercase());
                }
                "tech" => {
                    for t in arg.split(|c: char| c == ',' || c.is_whitespace()) {
                        let t = t.trim();
                        if !t.is_empty() {
                            tech.push(unquote(t).to_lowercase());
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(name) = local {
            out.push(FontSrc::Local(name));
        } else if let Some(url) = url {
            out.push(FontSrc::Url { url, format, tech });
        }
    }
    out
}

/// `font-weight` descriptor: `normal`|`bold`|number or a `low high` range.
fn parse_weight(v: &str) -> (u16, u16) {
    fn one(t: &str) -> Option<u16> {
        match t.to_lowercase().as_str() {
            "normal" => Some(400),
            "bold" => Some(700),
            _ => t.parse::<u16>().ok().map(|n| n.clamp(1, 1000)),
        }
    }
    let mut toks = v.split_whitespace();
    let lo = toks.next().and_then(one).unwrap_or(400);
    let hi = toks.next().and_then(one).unwrap_or(lo);
    (lo.min(hi), lo.max(hi))
}

/// `font-style` descriptor: normal | italic | oblique [<angle>].
fn parse_style(v: &str) -> FontFaceStyle {
    let v = v.trim().to_lowercase();
    if v.starts_with("italic") {
        FontFaceStyle::Italic
    } else if let Some(rest) = v.strip_prefix("oblique") {
        let angle = rest
            .trim()
            .trim_end_matches("deg")
            .trim()
            .parse::<f32>()
            .unwrap_or(14.0);
        FontFaceStyle::Oblique(angle)
    } else {
        FontFaceStyle::Normal
    }
}

/// `font-stretch` descriptor: keyword or `n%`, or a `low high` range.
fn parse_stretch(v: &str) -> (u16, u16) {
    fn one(t: &str) -> Option<u16> {
        match t.to_lowercase().as_str() {
            "ultra-condensed" => Some(50),
            "extra-condensed" => Some(63),
            "condensed" => Some(75),
            "semi-condensed" => Some(88),
            "normal" => Some(100),
            "semi-expanded" => Some(113),
            "expanded" => Some(125),
            "extra-expanded" => Some(150),
            "ultra-expanded" => Some(200),
            _ => t.trim_end_matches('%').parse::<u16>().ok(),
        }
    }
    let mut toks = v.split_whitespace();
    let lo = toks.next().and_then(one).unwrap_or(100);
    let hi = toks.next().and_then(one).unwrap_or(lo);
    (lo.min(hi), lo.max(hi))
}

/// `unicode-range` descriptor: `U+XXXX`, `U+XXXX-YYYY`, `U+X??` wildcard
/// lists (each `?` spans a hex digit), comma separated.
pub fn parse_unicode_range(v: &str) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for item in v.split(',') {
        let item = item.trim();
        let Some(hex) = item.strip_prefix("u+").or_else(|| item.strip_prefix("U+")) else {
            continue;
        };
        if let Some((a, b)) = hex.split_once('-') {
            if let (Ok(lo), Ok(hi)) = (
                u32::from_str_radix(a.trim(), 16),
                u32::from_str_radix(b.trim(), 16),
            ) {
                out.push((lo.min(hi), lo.max(hi)));
            }
            continue;
        }
        let hex = hex.trim();
        if hex.contains('?') {
            let lo = hex.replace('?', "0");
            let hi = hex.replace('?', "f");
            if let (Ok(lo), Ok(hi)) = (u32::from_str_radix(&lo, 16), u32::from_str_radix(&hi, 16)) {
                out.push((lo, hi));
            }
        } else if let Ok(cp) = u32::from_str_radix(hex, 16) {
            out.push((cp, cp));
        }
    }
    out
}

/// Build a [`FontFace`] from an `@font-face` block's declarations. Last
/// declaration of a descriptor wins, as in normal rule blocks. A face
/// with no `font-family` is invalid and dropped; a missing `src` simply
/// never resolves (kept — the renderer skips it).
pub fn font_face_from_decls(decls: &[Decl]) -> Option<FontFace> {
    let get = |name: &str| -> Option<&str> {
        decls
            .iter()
            .rev()
            .find(|d| d.prop == name)
            .map(|d| d.value.as_str())
    };
    let family = get("font-family")
        .and_then(|v| v.split(',').next())
        .map(unquote)
        .filter(|f| !f.is_empty())?;
    Some(FontFace {
        family,
        src: get("src").map(parse_src_list).unwrap_or_default(),
        weight: get("font-weight").map(parse_weight).unwrap_or((400, 400)),
        style: get("font-style")
            .map(parse_style)
            .unwrap_or(FontFaceStyle::Normal),
        stretch: get("font-stretch").map(parse_stretch).unwrap_or((100, 100)),
        unicode_range: get("unicode-range")
            .map(parse_unicode_range)
            .unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse, parse_vw};

    #[test]
    fn parses_font_face_block() {
        let s = parse(
            "@font-face { font-family: 'My Face'; \
             src: local('Helvetica'), url(f.woff2) format(\"woff2\"), \
             url(f.ttf) format('truetype'); \
             font-weight: 200 900; font-style: oblique 10deg; \
             font-stretch: 75% 125%; unicode-range: U+0025-00FF, U+4??; }",
        );
        assert_eq!(s.fonts.len(), 1);
        let f = &s.fonts[0];
        assert_eq!(f.family, "My Face");
        assert_eq!(f.src.len(), 3);
        assert_eq!(f.src[0], FontSrc::Local("Helvetica".to_string()));
        match &f.src[1] {
            FontSrc::Url { url, format, .. } => {
                assert_eq!(url, "f.woff2");
                assert_eq!(format.as_deref(), Some("woff2"));
            }
            _ => panic!("expected url src"),
        }
        match &f.src[2] {
            FontSrc::Url { url, format, .. } => {
                assert_eq!(url, "f.ttf");
                assert_eq!(format.as_deref(), Some("truetype"));
            }
            _ => panic!("expected url src"),
        }
        assert_eq!(f.weight, (200, 900));
        assert_eq!(f.style, FontFaceStyle::Oblique(10.0));
        assert_eq!(f.stretch, (75, 125));
        assert_eq!(f.unicode_range, vec![(0x25, 0xff), (0x400, 0x4ff)]);
        assert!(f.covers(0x30));
        assert!(f.covers(0x450));
        assert!(!f.covers(0x300));
    }

    #[test]
    fn font_face_defaults_and_multi() {
        let s = parse(
            "@font-face { font-family: A; src: url(a.ttf) } \
             @font-face { font-family: B; src: url(b.woff) format(woff) }",
        );
        assert_eq!(s.fonts.len(), 2);
        assert_eq!(s.fonts[0].weight, (400, 400));
        assert_eq!(s.fonts[0].style, FontFaceStyle::Normal);
        assert_eq!(s.fonts[0].stretch, (100, 100));
        assert!(s.fonts[0].unicode_range.is_empty());
        assert!(s.fonts[0].covers(0x1f600)); // unrestricted covers all
    }

    #[test]
    fn font_face_ignores_extra_descriptors() {
        let s = parse(
            "@font-face { font-family: A; src: url(a.ttf); \
             font-display: swap; size-adjust: 90%; ascent-override: 80%; \
             font-weight: bold }",
        );
        assert_eq!(s.fonts.len(), 1);
        assert_eq!(s.fonts[0].weight, (700, 700));
    }

    #[test]
    fn font_face_inside_media() {
        let s = parse_vw(
            "@media (min-width: 100px) { \
             @font-face { font-family: Gated; src: url(g.ttf) } \
             p { color: red } }",
            800.0,
        );
        assert_eq!(s.fonts.len(), 1);
        assert_eq!(s.fonts[0].family, "Gated");
        assert_eq!(s.rules.len(), 1);
        // gated OUT → face (and rule) skipped with the block
        let s = parse_vw(
            "@media (min-width: 2000px) { \
             @font-face { font-family: Gated; src: url(g.ttf) } \
             p { color: red } }",
            800.0,
        );
        assert!(s.fonts.is_empty());
        assert!(s.rules.is_empty());
    }

    #[test]
    fn font_face_does_not_eat_following_rule() {
        let s = parse("@font-face { font-family: A; src: url(a.ttf) } p { x: y }");
        assert_eq!(s.fonts.len(), 1);
        assert_eq!(s.rules.len(), 1);
        assert_eq!(s.rules[0].decls[0].prop, "x");
    }
}
