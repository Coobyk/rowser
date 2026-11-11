//! Cascade completeness for rowser-css — original implementations, no
//! engine code ported.
//!
//! Covers: CSS-wide keywords (`initial`/`inherit`/`unset`/`revert`/
//! `revert-layer`), `var()`/`env()` substitution with nested + comma-
//! containing fallbacks and cycle detection, the `all` shorthand,
//! `@property` registrations (syntax gate / inherits / initial-value),
//! deferred var()-bearing shorthand expansion, the inherited-property
//! pass (with `font-size` resolved to absolute px so relative units don't
//! compound), and the extra media features evaluated at parse time.

use crate::{
    StrMap, StyleCtx, decl_wins, each_decl, inline_decls, matches, rightmost_ok,
    specificity,
};
use rowser_dom::NodeId;
use rustc_hash::{FxHashMap, FxHashSet};

/// Declaration ordering key: (important, origin, layer, spec, order).
pub(crate) type WinKey = (bool, u8, u32, (u32, u32, u32), usize);
/// prop → (value, important, origin, layer, spec, order) winner map.
pub(crate) type OutMap = StrMap<(String, bool, u8, u32, (u32, u32, u32), usize)>;

/// A `@property --name { ... }` registration.
#[derive(Debug, Clone)]
pub struct PropertyDef {
    pub name: String,
    /// Raw syntax string, e.g. `<length>`, `*`, `<color> | <length>`.
    pub syntax: String,
    pub inherits: bool,
    pub initial: Option<String>,
}

/// One node's fully resolved output: the public props map plus resolved
/// custom properties (descendants inherit + look them up for var()).
#[derive(Clone, Default)]
pub(crate) struct NodeOut {
    pub props: StrMap<String>,
    pub customs: StrMap<String>,
}

/// Accumulator handed from the match pass to [`StyleCtx::finish`].
#[derive(Default)]
pub(crate) struct Acc {
    pub out: OutMap,
    /// Raw `--*` declarations matching this node (rules + inline style).
    pub customs: StrMap<String>,
    /// `all: <wide>` decls in source order (value pre-parsed).
    pub all: Vec<(Wide, WinKey)>,
    /// Box shorthands whose value contains var()/env() — side expansion is
    /// deferred until the node's custom properties are resolved.
    pub pending: Vec<(String, String, WinKey)>,
}

/// A CSS-wide keyword usable as any property's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wide {
    Initial,
    Inherit,
    Unset,
    Revert,
    RevertLayer,
}

impl Wide {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Wide::Initial => "initial",
            Wide::Inherit => "inherit",
            Wide::Unset => "unset",
            Wide::Revert => "revert",
            Wide::RevertLayer => "revert-layer",
        }
    }
}

/// Is `v` exactly one of the CSS-wide keywords? (ASCII case-insensitive.)
pub(crate) fn css_wide(v: &str) -> Option<Wide> {
    let v = v.trim();
    Some(if v.eq_ignore_ascii_case("initial") {
        Wide::Initial
    } else if v.eq_ignore_ascii_case("inherit") {
        Wide::Inherit
    } else if v.eq_ignore_ascii_case("unset") {
        Wide::Unset
    } else if v.eq_ignore_ascii_case("revert") {
        Wide::Revert
    } else if v.eq_ignore_ascii_case("revert-layer") {
        Wide::RevertLayer
    } else {
        return None;
    })
}

/// Insert a decl into the winner map under cascade ordering.
pub(crate) fn merge(out: &mut OutMap, prop: &str, val: &str, key: WinKey) {
    match out.get_mut(prop) {
        Some(e) => {
            if decl_wins(key, (e.1, e.2, e.3, e.4, e.5)) {
                *e = (val.to_string(), key.0, key.1, key.2, key.3, key.4);
            }
        }
        None => {
            out.insert(
                prop.to_string(),
                (val.to_string(), key.0, key.1, key.2, key.3, key.4),
            );
        }
    }
}

/// Decls consumed on the way into `out`: `all` (kept aside for the
/// post-cascade merge) and var()-bearing box shorthands (deferred side
/// expansion). Returns true when the decl is fully handled.
pub(crate) fn feed(acc: &mut Acc, prop: &str, value: &str, key: WinKey) -> bool {
    if prop == "all" {
        // `all` only accepts the wide keywords; anything else is a dropped
        // invalid declaration.
        if let Some(w) = css_wide(value) {
            acc.all.push((w, key));
        }
        return true;
    }
    if shorthand_sides(prop).is_some() && has_fn(value) {
        // The shorthand name itself still lands in `out` (layout's
        // `sides()` fallback); its longhands are resolved later.
        merge(&mut acc.out, prop, value, key);
        acc.pending.push((prop.to_string(), value.to_string(), key));
        return true;
    }
    false
}

/// Box/logical shorthands → ordered physical longhands (TRBL or start,end).
pub(crate) fn shorthand_sides(prop: &str) -> Option<&'static [&'static str]> {
    Some(match prop {
        "margin" => &["margin-top", "margin-right", "margin-bottom", "margin-left"],
        "padding" => &[
            "padding-top",
            "padding-right",
            "padding-bottom",
            "padding-left",
        ],
        "inset" => &["top", "right", "bottom", "left"],
        "margin-inline" => &["margin-left", "margin-right"],
        "margin-block" => &["margin-top", "margin-bottom"],
        "padding-inline" => &["padding-left", "padding-right"],
        "padding-block" => &["padding-top", "padding-bottom"],
        "inset-inline" => &["left", "right"],
        "inset-block" => &["top", "bottom"],
        _ => return None,
    })
}

/// Distribute 1–4 (TRBL) or 1–2 (start,end) resolved tokens over `prop`'s
/// longhands — the same cycling `each_decl` uses for static values.
fn assign_sides<'v>(prop: &str, toks: &[&'v str], mut f: impl FnMut(&'static str, &'v str)) {
    let Some(sides) = shorthand_sides(prop) else { return };
    if sides.len() == 4 {
        let t = toks.first().copied().unwrap_or("0");
        let r = toks.get(1).copied().unwrap_or(t);
        let b = toks.get(2).copied().unwrap_or(t);
        let l = toks.get(3).copied().unwrap_or(r);
        f(sides[0], t);
        f(sides[1], r);
        f(sides[2], b);
        f(sides[3], l);
    } else {
        let a = toks.first().copied().unwrap_or("0");
        let b = toks.get(1).copied().unwrap_or(a);
        f(sides[0], a);
        f(sides[1], b);
    }
}

/// Split on whitespace at paren depth 0 — `calc(1px + 2px)` and
/// `var(--x, 1px 2px)` stay single tokens.
pub(crate) fn split_ws_parens(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut st = 0usize;
    let is_ws = |c: u8| matches!(c, b' ' | b'\t' | b'\n' | 0x0c | b'\r');
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            c if is_ws(c) && depth <= 0 => {
                if st < i {
                    out.push(&s[st..i]);
                }
                st = i + 1;
            }
            _ => {}
        }
    }
    if st < b.len() {
        out.push(&s[st..]);
    }
    out
}

/// ASCII case-insensitive byte-slice equality.
fn eq_ci(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// Find the next `var(` / `env(` (case-insensitive). Returns
/// (fn-name start, index of the `(`).
pub(crate) fn find_fn(s: &str, from: usize) -> Option<(usize, usize)> {
    let b = s.as_bytes();
    let mut i = from;
    while i + 4 <= b.len() {
        if eq_ci(&b[i..i + 4], b"var(") || eq_ci(&b[i..i + 4], b"env(") {
            return Some((i, i + 3));
        }
        i += 1;
    }
    None
}

pub(crate) fn has_fn(s: &str) -> bool {
    find_fn(s, 0).is_some()
}

/// `inner` (var() args) → (name, optional fallback text) — split on the
/// first comma at paren depth 0, so fallbacks may contain commas and
/// nested function calls.
fn split_first_comma(s: &str) -> (&str, Option<&str>) {
    let b = s.as_bytes();
    let mut depth = 0i32;
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b',' if depth <= 0 => return (&s[..i], Some(&s[i + 1..])),
            _ => {}
        }
    }
    (s, None)
}

/// `var()`/`env()` substitution with memoized custom-property resolution
/// and cycle detection. `subst` returns `None` when the value is
/// "guaranteed-invalid" (unresolvable var() with no fallback, or a cycle).
pub(crate) struct VarResolver<'a> {
    customs: &'a StrMap<String>,
    memo: FxHashMap<String, Option<String>>,
    stack: Vec<String>,
    depth: usize,
}

impl<'a> VarResolver<'a> {
    pub(crate) fn new(customs: &'a StrMap<String>) -> Self {
        VarResolver {
            customs,
            memo: FxHashMap::default(),
            stack: Vec::new(),
            depth: 0,
        }
    }

    /// Resolved value of custom property `name`: `Some(text)` or `None`
    /// (missing, cyclic, or itself guaranteed-invalid).
    pub(crate) fn custom(&mut self, name: &str) -> Option<String> {
        if let Some(r) = self.memo.get(name) {
            return r.clone();
        }
        let raw = self.customs.get(name)?.clone();
        if self.stack.iter().any(|n| n == name) {
            self.memo.insert(name.to_string(), None);
            return None;
        }
        self.stack.push(name.to_string());
        let r = self.subst(&raw);
        self.stack.pop();
        self.memo.insert(name.to_string(), r.clone());
        r
    }

    /// Substitute all var()/env() calls in `value`. env() → `0px`
    /// (safe-area-inset-* are all zero for us).
    pub(crate) fn subst(&mut self, value: &str) -> Option<String> {
        if self.depth > 32 {
            return None;
        }
        let mut out = value.to_string();
        let mut guard = 0usize;
        loop {
            guard += 1;
            if guard > 1024 {
                return None;
            }
            let Some((start, open)) = find_fn(&out, 0) else {
                break;
            };
            let mut depth = 1i32;
            let mut end = open + 1;
            while end < out.len() && depth > 0 {
                match out.as_bytes()[end] {
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    _ => {}
                }
                end += 1;
            }
            if depth != 0 {
                return None; // unbalanced → invalid
            }
            let inner = out[open + 1..end - 1].trim().to_string();
            let rep = if out[start..open].eq_ignore_ascii_case("env") {
                "0px".to_string()
            } else {
                let (name, fb) = split_first_comma(&inner);
                let name = name.trim();
                if !name.starts_with("--") {
                    return None;
                }
                match self.custom(name) {
                    // trim outer ws — decls and fallbacks carry the
                    // space after the comma/name into the result
                    Some(v) => v.trim().to_string(),
                    None => match fb {
                        Some(fb) => {
                            self.depth += 1;
                            let r = self.subst(fb.trim());
                            self.depth -= 1;
                            r?
                        }
                        None => return None,
                    },
                }
            };
            out = format!("{}{}{}", &out[..start], rep, &out[end..]);
        }
        Some(out)
    }
}

/// Is `prop` in the inherited-property set? (Standard text/table set;
/// `font` and `text-decoration*` included approximately.)
pub(crate) fn is_inherited(prop: &str) -> bool {
    matches!(
        prop,
        "color"
            | "caret-color" | "font" | "font-family" | "font-size" | "font-style"
            | "font-variant" | "font-variant-caps" | "font-variant-ligatures"
            | "font-variant-numeric" | "font-weight" | "font-stretch"
            | "font-kerning" | "font-feature-settings" | "line-height"
            | "text-align" | "text-align-last" | "text-indent" | "text-transform"
            | "text-decoration" | "text-decoration-line" | "text-decoration-color"
            | "text-decoration-style" | "text-decoration-thickness"
            | "text-emphasis-color" | "text-emphasis-style" | "text-emphasis-position"
            | "text-shadow" | "text-justify" | "text-size-adjust"
            | "-webkit-text-size-adjust" | "-moz-tab-size" | "tab-size"
            | "letter-spacing" | "word-spacing" | "white-space" | "word-break"
            | "overflow-wrap" | "word-wrap" | "line-break" | "direction"
            | "visibility" | "cursor" | "list-style" | "list-style-type"
            | "list-style-position" | "list-style-image" | "border-collapse"
            | "caption-side" | "empty-cells" | "quotes" | "orphans" | "widows"
            | "pointer-events" | "speak" | "writing-mode" | "text-orientation"
    )
}

fn is_num(s: &str) -> bool {
    s.trim().parse::<f32>().is_ok()
}
fn is_int(s: &str) -> bool {
    s.trim().parse::<i64>().is_ok()
}
fn is_pct(s: &str) -> bool {
    s.trim().strip_suffix('%').is_some_and(is_num)
}

const LEN_UNITS: &[&str] = &[
    "px", "em", "rem", "ex", "ch", "cap", "ic", "lh", "rlh", "vw", "vh", "vi", "vb",
    "vmin", "vmax", "svw", "svh", "svi", "svb", "svmin", "svmax", "lvw", "lvh", "lvi",
    "lvb", "lvmin", "lvmax", "dvw", "dvh", "dvi", "dvb", "dvmin", "dvmax", "cm", "mm",
    "q", "in", "pc", "pt",
];

fn is_len(s: &str) -> bool {
    let s = s.trim().to_lowercase();
    if s == "0" || s.starts_with("calc(") || s.starts_with("min(") || s.starts_with("max(") {
        return true;
    }
    for u in LEN_UNITS {
        if let Some(n) = s.strip_suffix(u) {
            return is_num(n);
        }
    }
    false
}

/// Lenient `<color>`: `#hex`, any color function, or any bare identifier
/// (named colors incl. currentcolor/transparent pass through).
fn is_color(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    if let Some(hex) = s.strip_prefix('#') {
        return !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit());
    }
    let l = s.to_lowercase();
    if l.contains('(') {
        return [
            "rgb", "hsl", "hwb(", "lab(", "lch(", "oklab(", "oklch(", "color(",
            "color-mix(", "light-dark(", "device-cmyk(", "hsla(", "rgba(",
        ]
        .iter()
        .any(|f| l.starts_with(f));
    }
    l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn is_ident(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty()
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '-' || c == '_')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// One `@property` syntax component vs one token.
fn component_ok(ty: &str, tok: &str) -> bool {
    let tok = tok.trim();
    if tok.is_empty() {
        return false;
    }
    if !ty.starts_with('<') {
        // bare keyword alternative ("auto | <length>")
        return tok.eq_ignore_ascii_case(ty);
    }
    match ty {
        "<length>" => is_len(tok),
        "<length-percentage>" => is_len(tok) || is_pct(tok),
        "<percentage>" => is_pct(tok),
        "<number>" => is_num(tok),
        "<integer>" => is_int(tok),
        "<angle>" => {
            let l = tok.to_lowercase();
            ["deg", "rad", "grad", "turn"]
                .iter()
                .any(|u| l.strip_suffix(u).is_some_and(is_num))
        }
        "<time>" => {
            let l = tok.to_lowercase();
            l.strip_suffix("ms")
                .or_else(|| l.strip_suffix("s"))
                .is_some_and(is_num)
        }
        "<resolution>" => parse_dppx(tok).is_some(),
        "<color>" => is_color(tok),
        "<string>" => {
            (tok.starts_with('"') && tok.ends_with('"'))
                || (tok.starts_with('\'') && tok.ends_with('\''))
        }
        "<custom-ident>" => is_ident(tok),
        "<url>" => tok.to_lowercase().starts_with("url("),
        "<image>" => {
            let l = tok.to_lowercase();
            l.contains("gradient(")
                || l.starts_with("url(")
                || l.starts_with("image(")
                || l.starts_with("image-set(")
                || l.starts_with("cross-fade(")
                || l.starts_with("paint(")
        }
        // unknown components → lenient accept
        _ => true,
    }
}

/// `@property` syntax check: alternatives separated by `|`; `+` (space
/// list) and `#` (comma list) multipliers; `*` accepts anything.
pub(crate) fn syntax_ok(syntax: &str, value: &str) -> bool {
    let value = value.trim();
    let syntax = syntax.trim();
    if syntax == "*" || syntax.is_empty() {
        return true;
    }
    for alt in syntax.split('|') {
        let alt = alt.trim();
        if alt.is_empty() {
            continue;
        }
        let (base, kind) = if let Some(b) = alt.strip_suffix('#') {
            (b.trim(), b'#')
        } else if let Some(b) = alt.strip_suffix('+') {
            (b.trim(), b'+')
        } else {
            (alt, 0u8)
        };
        let ok = match kind {
            b'#' => value.split(',').all(|t| component_ok(base, t.trim())),
            b'+' => {
                let toks = split_ws_parens(value);
                !toks.is_empty() && toks.iter().all(|t| component_ok(base, t))
            }
            _ => component_ok(base, value),
        };
        if ok {
            return true;
        }
    }
    false
}

/// `font-size` value → absolute px against `base` (the parent's resolved
/// size). Keywords follow the same table layout's parse_font_size uses.
/// Returns None for unresolvable values (calc()/viewport units) — callers
/// keep the raw string and let layout resolve it.
pub(crate) fn font_size_px(v: &str, base: f32) -> Option<f32> {
    let v = v.trim();
    let n = match v {
        "xx-small" => 8.0,
        "x-small" => 9.6,
        "small" => 12.8,
        "medium" => 16.0,
        "large" => 19.2,
        "x-large" => 24.0,
        "xx-large" => 32.0,
        "larger" => base * 1.2,
        "smaller" => base / 1.2,
        _ => {
            let lv = v.to_lowercase();
            if let Some(x) = lv.strip_suffix("px") {
                x.trim().parse::<f32>().ok()?
            } else if let Some(x) = lv.strip_suffix("rem") {
                x.trim().parse::<f32>().ok()? * 16.0
            } else if let Some(x) = lv.strip_suffix("em") {
                x.trim().parse::<f32>().ok()? * base
            } else if let Some(x) = lv.strip_suffix("ex") {
                x.trim().parse::<f32>().ok()? * base / 2.0
            } else if let Some(x) = lv.strip_suffix("ch") {
                x.trim().parse::<f32>().ok()? * base / 2.0
            } else if let Some(x) = lv.strip_suffix("pt") {
                x.trim().parse::<f32>().ok()? * 96.0 / 72.0
            } else if let Some(x) = lv.strip_suffix('%') {
                x.trim().parse::<f32>().ok()? * base / 100.0
            } else {
                return lv.parse::<f32>().ok();
            }
        }
    };
    Some(n.max(4.0))
}

pub(crate) fn fmt_px(n: f32) -> String {
    format!("{n}px")
}

/// `a/b` or bare number → ratio (for aspect-ratio media features).
pub(crate) fn parse_ratio(s: &str) -> Option<f32> {
    let s = s.trim();
    if let Some((a, b)) = s.split_once('/') {
        let a = a.trim().parse::<f32>().ok()?;
        let b = b.trim().parse::<f32>().ok()?;
        if b == 0.0 { None } else { Some(a / b) }
    } else {
        s.parse::<f32>().ok()
    }
}

/// resolution → dppx (1dppx = 96dpi; `x` and bare numbers = dppx).
pub(crate) fn parse_dppx(s: &str) -> Option<f32> {
    let s = s.trim().to_lowercase();
    for (suf, k) in [("dppx", 1.0f32), ("dpcm", 96.0 / 2.54), ("dpi", 1.0 / 96.0), ("x", 1.0)] {
        if let Some(n) = s.strip_suffix(suf) {
            return n.trim().parse::<f32>().ok().map(|v| v * k);
        }
    }
    s.parse::<f32>().ok()
}

/// Extra `name: value` media features not handled inline in
/// `media_feature`. Fixed device: hover-capable, fine pointer, sRGB,
/// 1dppx, 8-bit color, no forced/inverted colors.
pub(crate) fn media_extra(name: &str, val: &str, vw: f32) -> bool {
    let vh = crate::VH;
    match name {
        "prefers-contrast" | "prefers-reduced-data" => val == "no-preference",
        "display-mode" => val == "browser",
        "color-gamut" | "video-color-gamut" => val == "srgb",
        "dynamic-range" | "video-dynamic-range" => val == "standard",
        "aspect-ratio" => parse_ratio(val).is_some_and(|r| (vw / vh - r).abs() < 0.01),
        "min-aspect-ratio" => parse_ratio(val).is_some_and(|r| vw / vh >= r - 0.01),
        "max-aspect-ratio" => parse_ratio(val).is_some_and(|r| vw / vh <= r + 0.01),
        "resolution" => parse_dppx(val).is_some_and(|r| (r - 1.0).abs() < 0.01),
        "min-resolution" => parse_dppx(val).is_some_and(|r| r <= 1.01),
        "max-resolution" => parse_dppx(val).is_some_and(|r| r >= 1.0 - 0.01),
        "color" => val.trim().parse::<f32>().is_ok_and(|n| (1.0..=8.0).contains(&n)),
        "min-color" => val.trim().parse::<f32>().is_ok_and(|n| n <= 8.0),
        "max-color" => val.trim().parse::<f32>().is_ok_and(|n| n >= 8.0),
        "color-index" | "min-color-index" | "max-color-index" => val.trim() == "0",
        "scan" => val == "progressive",
        "update" => val == "fast",
        "overflow-block" | "overflow-inline" => val == "scroll",
        "nav-controls" => val.starts_with("no"),
        _ => true, // lenient: unknown features resolve true
    }
}

/// Nearest element ancestor (skipping the Document node).
fn element_parent(doc: &rowser_dom::Document, n: NodeId) -> Option<NodeId> {
    let mut p = doc.parent(n)?;
    while doc.tag(p).is_none() {
        p = doc.parent(p)?;
    }
    Some(p)
}

/// Winner-map merge helper for `cascaded_prop` (kept as a free fn so
/// nested closures don't fight over a captured `&mut best`).
fn take_best(best: &mut Option<(String, WinKey)>, v: &str, key: WinKey) {
    match best {
        Some(e) => {
            if decl_wins(key, e.1) {
                *e = (v.to_string(), key);
            }
        }
        None => *best = Some((v.to_string(), key)),
    }
}

impl<'a> StyleCtx<'a> {
    /// Parent element's resolved output, memoized in `node_memo` so chains
    /// of inherit/unset/all:inherit lookups compute each ancestor once.
    fn parent_entry(&mut self, node: NodeId) -> Option<NodeOut> {
        let p = element_parent(self.doc, node)?;
        if !self.node_memo.contains_key(&p) {
            let o = self.node_style(p);
            self.node_memo.insert(p, o);
        }
        self.node_memo.get(&p).cloned()
    }

    /// Narrowed re-cascade of a single property — used by `revert`
    /// (earlier origins only) and `revert-layer` (earlier layers only).
    /// `keep(origin, layer)` gates each rule. Sheets only; `all` decls
    /// contribute their keyword (the caller resolves one more level).
    fn cascaded_prop(
        &mut self,
        node: NodeId,
        prop: &str,
        customs: &StrMap<String>,
        keep: &dyn Fn(u8, u32) -> bool,
    ) -> Option<String> {
        let doc = self.doc;
        let mut best: Option<(String, WinKey)> = None;
        let mut order = 0usize;
        for (origin, sheet) in self.sheets.iter().enumerate() {
            sheet.candidates_into(doc, node, &mut self.buf);
            for &ri in &self.buf {
                let rule = &sheet.rules[ri as usize];
                let layer = sheet.layers.get(ri as usize).copied().unwrap_or(u32::MAX);
                if !keep(origin as u8, layer) {
                    continue;
                }
                let mut spec = (0u32, 0u32, 0u32);
                let mut matched = false;
                for s in &rule.selectors {
                    if rightmost_ok(doc, node, s) && matches(doc, node, s) {
                        matched = true;
                        let sp = specificity(s);
                        if sp > spec {
                            spec = sp;
                        }
                    }
                }
                if !matched {
                    continue;
                }
                for d in &rule.decls {
                    if d.prop.starts_with("--") {
                        continue;
                    }
                    let nk = (d.important, origin as u8, layer, spec, order);
                    order += 1;
                    if d.prop == "all" {
                        if css_wide(&d.value).is_some() {
                            take_best(&mut best, &d.value, nk);
                        }
                    } else if shorthand_sides(&d.prop).is_some() && has_fn(&d.value) {
                        // var()-shorthand → resolve and pick `prop`'s side
                        let mut r = VarResolver::new(customs);
                        if let Some(rv) = r.subst(&d.value) {
                            let toks = split_ws_parens(&rv);
                            assign_sides(&d.prop, &toks, |sp, tv| {
                                if sp == prop {
                                    take_best(&mut best, tv, nk);
                                }
                            });
                        }
                    } else {
                        each_decl(&d.prop, &d.value, |p, v| {
                            if p == prop {
                                take_best(&mut best, v, nk);
                            }
                        });
                    }
                }
            }
        }
        best.map(|b| b.0)
    }

    /// The match pass: rules + inline style → winner map + raw customs +
    /// `all`/pending side queues, then [`Self::finish`] resolves.
    /// (Body of the former `computed` — `all`/pending are intercepted
    /// before `each_decl`, and `order` now ticks per declaration.)
    pub(crate) fn node_style(&mut self, node: NodeId) -> NodeOut {
        let doc = self.doc;
        let mut acc = Acc::default();
        // props whose winning decl is !important — the inline style
        // attribute may only override those with !important of its own
        let mut important_props: FxHashSet<String> = FxHashSet::default();
        let mut order = 0usize;
        for (origin, sheet) in self.sheets.iter().enumerate() {
            // origin 0 = UA (weakest), later = author
            sheet.candidates_into(doc, node, &mut self.buf);
            for &ri in &self.buf {
                let rule = &sheet.rules[ri as usize];
                let layer = sheet
                    .layers
                    .get(ri as usize)
                    .copied()
                    .unwrap_or(u32::MAX);
                // best specificity across matching selectors — and whether
                // any matched at all — in one pass over the selector list
                let mut spec = (0u32, 0u32, 0u32);
                let mut matched = false;
                for s in &rule.selectors {
                    if rightmost_ok(doc, node, s) && matches(doc, node, s) {
                        matched = true;
                        let sp = specificity(s);
                        if sp > spec {
                            spec = sp;
                        }
                    }
                }
                if !matched {
                    continue;
                }
                for d in &rule.decls {
                    if d.prop.starts_with("--") {
                        acc.customs.insert(d.prop.clone(), d.value.clone());
                    }
                    // per-decl order (not per-rule) so `all` in the middle
                    // of a block interleaves correctly with other decls
                    let nk = (d.important, origin as u8, layer, spec, order);
                    order += 1;
                    if feed(&mut acc, &d.prop, &d.value, nk) {
                        if d.important {
                            if let Some(sides) = shorthand_sides(&d.prop) {
                                for s in sides {
                                    important_props.insert(s.to_string());
                                }
                            }
                        }
                        continue;
                    }
                    each_decl(&d.prop, &d.value, |prop, val| {
                        if d.important {
                            important_props.insert(prop.to_string());
                        }
                        merge(&mut acc.out, prop, val, nk);
                    });
                }
            }
        }
        // The inline `style` attribute is the strongest normal source: it
        // beats author rules but loses to author !important; an !important
        // decl inside it behaves as an author-origin !important (and so
        // still loses to a UA !important, per Cascade 5).
        if let Some(s) = doc.attr(node, "style") {
            for d in inline_decls(s) {
                if d.prop.starts_with("--") {
                    acc.customs.insert(d.prop.clone(), d.value.clone());
                    continue;
                }
                let nk = (
                    d.important,
                    if d.important { 1u8 } else { u8::MAX },
                    u32::MAX,
                    (u32::MAX, u32::MAX, u32::MAX),
                    usize::MAX,
                );
                if feed(&mut acc, &d.prop, &d.value, nk) {
                    continue;
                }
                if d.important {
                    // Merge through decl_wins rather than stomping: a UA
                    // !important (origin 0) and layered author !important
                    // still beat the unlayered author-level inline decl.
                    each_decl(&d.prop, &d.value, |prop, val| {
                        important_props.insert(prop.to_string());
                        merge(&mut acc.out, prop, val, nk);
                    });
                } else {
                    // An `all: * !important` decl outranks every normal
                    // inline decl (it expands to important on each prop).
                    let any_imp_all = acc.all.iter().any(|(_, k)| k.0);
                    each_decl(&d.prop, &d.value, |prop, val| {
                        if !important_props.contains(prop) && !any_imp_all {
                            acc.out.insert(
                                prop.to_string(),
                                (
                                    val.to_string(),
                                    false,
                                    u8::MAX,
                                    u32::MAX,
                                    (u32::MAX, u32::MAX, u32::MAX),
                                    usize::MAX,
                                ),
                            );
                        }
                    });
                }
            }
        }
        let mut out = self.finish(node, acc);
        // Emulate animation end-state: a named animation rests at its
        // keyframes' end frame (approximates `forwards`/completed anims —
        // sites using `opacity-0` + `animate-*` still become visible).
        if let Some(v) = out
            .props
            .get("animation-name")
            .or_else(|| out.props.get("animation"))
            .cloned()
        {
            if let Some(name) = v
                .split([',', ' '])
                .map(str::trim)
                .find(|t| !t.is_empty())
            {
                for sheet in &self.sheets {
                    if let Some(decls) = sheet.keyframes.get(name) {
                        for d in decls {
                            if d.prop.starts_with("--") {
                                continue;
                            }
                            let val = crate::resolve_vars(&d.value, &out.customs);
                            each_decl(&d.prop, &val, |p, v| {
                                out.props.insert(p.to_string(), v.to_string());
                            });
                        }
                    }
                }
            }
        }
        out
    }

    /// Post-cascade resolution: `all`, pending var()-shorthands,
    /// var()/env() substitution, CSS-wide keywords, custom-property
    /// inheritance + `@property`, the inherited-property pass, and
    /// `font-size` normalization to absolute px.
    pub(crate) fn finish(&mut self, node: NodeId, mut acc: Acc) -> NodeOut {
        // `@property` registrations, last-wins per name across sheets
        // (owned: sheets can't be borrowed across &mut self calls).
        let mut defs: FxHashMap<String, PropertyDef> = FxHashMap::default();
        for sh in &self.sheets {
            for (n, d) in &sh.properties {
                defs.insert(n.clone(), d.clone());
            }
        }
        // Parent's resolved output — the source for inherit/unset/
        // all:inherit and the inherited-property pass.
        let parent = self.parent_entry(node);

        // ---------- custom properties ----------
        let need_customs = !acc.customs.is_empty()
            || !defs.is_empty()
            || !acc.pending.is_empty()
            || parent.as_ref().is_some_and(|p| !p.customs.is_empty())
            || acc.out.values().any(|e| has_fn(&e.0));
        let mut resolved_customs: StrMap<String> = StrMap::default();
        if need_customs {
            let mut raw: StrMap<String> = StrMap::default();
            // :root globals — the base fallback (detached nodes, and the
            // html element itself whose parent chain ends at Document)
            for (k, v) in self.root_customs().clone() {
                raw.insert(k, v);
            }
            // inherited custom props (unless registered inherits:false)
            if let Some(par) = &parent {
                for (k, v) in &par.customs {
                    let inh = defs.get(k.as_str()).map(|d| d.inherits).unwrap_or(true);
                    if inh {
                        raw.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
            }
            // own declarations (keywords resolved against the base map)
            for (k, v) in &acc.customs {
                match css_wide(v) {
                    Some(Wide::Initial) | Some(Wide::Revert) | Some(Wide::RevertLayer) => {
                        raw.remove(k);
                    }
                    // inherit/unset: whatever the base map holds stands
                    // (custom properties inherit by default)
                    Some(Wide::Inherit) | Some(Wide::Unset) => {}
                    None => {
                        raw.insert(k.clone(), v.clone());
                    }
                }
            }
            // registered initial-values fill absent names
            for (n, d) in &defs {
                if !raw.contains_key(n.as_str()) {
                    if let Some(init) = &d.initial {
                        raw.insert(n.clone(), init.clone());
                    }
                }
            }
            {
                let mut r = VarResolver::new(&raw);
                for k in raw.keys() {
                    if let Some(v) = r.custom(k) {
                        resolved_customs.insert(k.clone(), v);
                    }
                }
            }
            // syntax gate: a mismatched registered value is invalid → the
            // registered initial (if any) replaces it
            for (n, d) in &defs {
                let bad = resolved_customs
                    .get(n.as_str())
                    .is_some_and(|v| !syntax_ok(&d.syntax, v));
                if bad {
                    match &d.initial {
                        Some(init) => {
                            let mut r2 = VarResolver::new(&resolved_customs);
                            match r2.subst(init) {
                                Some(iv) => {
                                    resolved_customs.insert(n.clone(), iv);
                                }
                                None => {
                                    resolved_customs.remove(n.as_str());
                                }
                            }
                        }
                        None => {
                            resolved_customs.remove(n.as_str());
                        }
                    }
                }
            }
        }

        // ---------- `all` shorthand ----------
        let mut all_kw: Option<(Wide, WinKey)> = None;
        for (w, k) in &acc.all {
            let better = match &all_kw {
                None => true,
                Some((_, ok)) => decl_wins(*k, *ok),
            };
            if better {
                all_kw = Some((*w, *k));
            }
        }
        let mut all_losers: FxHashSet<String> = FxHashSet::default();
        if let Some((w, ak)) = all_kw {
            let kws = w.as_str();
            for (prop, e) in acc.out.iter_mut() {
                if prop.starts_with("--") {
                    continue; // `all` does not touch custom properties
                }
                let ek = (e.1, e.2, e.3, e.4, e.5);
                if decl_wins(ak, ek) {
                    // the all-decl becomes the prop's winner — keeps its
                    // own layer/origin for revert-layer accounting
                    *e = (kws.to_string(), ak.0, ak.1, ak.2, ak.3, ak.4);
                    all_losers.insert(prop.clone());
                }
            }
        }

        // ---------- pending var()-bearing shorthands ----------
        {
            let mut r = VarResolver::new(&resolved_customs);
            for (prop, value, key) in &acc.pending {
                let Some(rv) = r.subst(value) else { continue };
                let toks = split_ws_parens(&rv);
                assign_sides(prop, &toks, |sp, tv| {
                    // `all` outranks a losing shorthand decl
                    let (val, k2) = match all_kw {
                        Some((w, ak)) if decl_wins(ak, *key) => (w.as_str(), ak),
                        _ => (tv, *key),
                    };
                    merge(&mut acc.out, sp, val, k2);
                });
            }
        }

        let decl_keys: FxHashSet<String> = acc.out.keys().cloned().collect();

        // ---------- var()/env() substitution on surviving values ----------
        {
            let mut r = VarResolver::new(&resolved_customs);
            let mut bad: Vec<String> = Vec::new();
            for (k, e) in acc.out.iter_mut() {
                if has_fn(&e.0) {
                    match r.subst(&e.0) {
                        Some(v) => {
                            // empty result on a normal prop is
                            // guaranteed-invalid → unset semantics
                            if v.trim().is_empty() && !k.starts_with("--") {
                                bad.push(k.clone());
                            } else {
                                e.0 = v;
                            }
                        }
                        None => bad.push(k.clone()),
                    }
                }
            }
            for k in bad {
                acc.out.remove(&k); // not `reset` — unset may still inherit
            }
        }

        // ---------- CSS-wide keywords ----------
        let unset_val = |prop: &str| -> Option<String> {
            if is_inherited(prop) {
                parent.as_ref().and_then(|p| p.props.get(prop).cloned())
            } else {
                None
            }
        };
        let mut reset: FxHashSet<String> = FxHashSet::default();
        let wides: Vec<(String, Wide, u8, u32)> = acc
            .out
            .iter()
            .filter_map(|(p, e)| css_wide(&e.0).map(|w| (p.clone(), w, e.2, e.3)))
            .collect();
        for (prop, wide, orig, layer) in wides {
            let mut resolved: Option<String> = match wide {
                Wide::Initial => None,
                Wide::Inherit => {
                    parent.as_ref().and_then(|p| p.props.get(&prop).cloned())
                }
                Wide::Unset => unset_val(&prop),
                Wide::Revert => {
                    // as if the decl came from an earlier origin (author → UA)
                    self.cascaded_prop(node, &prop, &resolved_customs, &|o, _| o < orig)
                }
                Wide::RevertLayer => {
                    // drop decls from same-or-later @layers
                    self.cascaded_prop(node, &prop, &resolved_customs, &|_, l| l < layer)
                }
            };
            // one more level if the cascaded value is itself a keyword
            if let Some(w2) = resolved.as_deref().and_then(css_wide) {
                resolved = match w2 {
                    Wide::Inherit => {
                        parent.as_ref().and_then(|p| p.props.get(&prop).cloned())
                    }
                    Wide::Initial => None,
                    _ => unset_val(&prop),
                };
            }
            // revert/revert-layer with nothing earlier → unset semantics
            if resolved.is_none() && matches!(wide, Wide::Revert | Wide::RevertLayer) {
                resolved = unset_val(&prop);
            }
            match resolved {
                Some(v) => {
                    if let Some(e) = acc.out.get_mut(&prop) {
                        e.0 = v;
                    }
                }
                None => {
                    acc.out.remove(&prop);
                    // explicit initial — block the inherited pass
                    reset.insert(prop);
                }
            }
        }

        // ---------- sync custom props into the output ----------
        {
            let mut dropc: Vec<String> = Vec::new();
            for (k, e) in acc.out.iter_mut() {
                if k.starts_with("--") {
                    match resolved_customs.get(k) {
                        Some(v) => e.0 = v.clone(),
                        None => dropc.push(k.clone()),
                    }
                }
            }
            for k in dropc {
                acc.out.remove(&k);
            }
        }

        // ---------- extract ----------
        let mut props: StrMap<String> =
            acc.out.into_iter().map(|(k, e)| (k, e.0)).collect();

        // ---------- inherited-property pass ----------
        // Only props with no surviving decl inherit (the cascade already
        // gave any matched decl precedence) — and `reset` props keep
        // their explicit initial/keyword resolution.
        if let Some(par) = &parent {
            for (k, v) in &par.props {
                if is_inherited(k) && !props.contains_key(k) && !reset.contains(k) {
                    props.insert(k.clone(), v.clone());
                }
            }
            // `all: inherit` applies to *every* non-custom property —
            // including ones this node had no declarations for.
            if matches!(all_kw, Some((Wide::Inherit, _))) {
                for (k, v) in &par.props {
                    if k.starts_with("--") || props.contains_key(k) || reset.contains(k) {
                        continue;
                    }
                    if decl_keys.contains(k) && !all_losers.contains(k) {
                        continue; // own decl beat `all` — keep it
                    }
                    props.insert(k.clone(), v.clone());
                }
            }
        }

        // ---------- font-size → absolute px ----------
        // Inherited em/% sizes must not re-resolve against the child's
        // own (already-resolved) size — normalize once here.
        let base_px = parent
            .as_ref()
            .and_then(|p| p.props.get("font-size"))
            .and_then(|v| {
                v.trim()
                    .strip_suffix("px")
                    .and_then(|n| n.trim().parse::<f32>().ok())
            })
            .unwrap_or(16.0);
        if let Some(v) = props.get("font-size").cloned() {
            if let Some(px) = font_size_px(&v, base_px) {
                props.insert("font-size".to_string(), fmt_px(px));
            }
        }

        NodeOut {
            props,
            customs: resolved_customs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{computed, parse};
    use rowser_dom::NodeId;

    fn props(html: &str, css: &str, tag: &str) -> StrMap<String> {
        let doc = rowser_html::parse(html);
        let sh = parse(css);
        let n = doc.find_deep(doc.root, tag).unwrap();
        computed(&doc, n, &[&sh])
    }

    #[test]
    fn wide_inherit() {
        let c = props(
            "<body><div><p></p></div></body>",
            "div { color: rgb(1,2,3) } p { color: inherit }",
            "p",
        );
        assert_eq!(c.get("color").unwrap(), "rgb(1,2,3)");
        // inherit on a non-inherited prop also pulls the parent's value
        let c = props(
            "<body><div><p></p></div></body>",
            "div { margin-top: 7px } p { margin-top: inherit }",
            "p",
        );
        assert_eq!(c.get("margin-top").unwrap(), "7px");
    }

    #[test]
    fn wide_unset() {
        let c = props(
            "<body><div><p></p></div></body>",
            "div { color: blue; margin-top: 9px } p { color: unset; margin-top: unset }",
            "p",
        );
        // inherited prop → parent's value; non-inherited → initial (absent)
        assert_eq!(c.get("color").unwrap(), "blue");
        assert!(c.get("margin-top").is_none());
    }

    #[test]
    fn wide_revert() {
        let doc = rowser_html::parse("<body><p></p></body>");
        let p: NodeId = doc.find_deep(doc.root, "p").unwrap();
        let ua = parse("p { color: gray; x-uaprop: 1 }");
        let author = parse("p { color: revert; x-uaprop: revert; margin-top: revert }");
        let c = computed(&doc, p, &[&ua, &author]);
        assert_eq!(c.get("color").unwrap(), "gray"); // author dropped → UA
        assert_eq!(c.get("x-uaprop").unwrap(), "1");
        // nothing earlier → unset semantics (margin-top: not inherited → absent)
        assert!(c.get("margin-top").is_none());
    }

    #[test]
    fn wide_revert_layer() {
        let doc = rowser_html::parse("<body><p></p></body>");
        let p = doc.find_deep(doc.root, "p").unwrap();
        // layer b's decl wins, but revert-layer drops same-or-later layers
        // → layer a's value stands
        let sh = parse("@layer a { p { color: red } } @layer b { p { color: revert-layer } }");
        assert_eq!(computed(&doc, p, &[&sh]).get("color").unwrap(), "red");
        // with no earlier layer at all → unset semantics → absent
        let sh = parse("@layer b { p { x-frob: revert-layer } }");
        assert!(computed(&doc, p, &[&sh]).get("x-frob").is_none());
    }

    #[test]
    fn wide_keywords_inline() {
        let doc = rowser_html::parse("<body><div><p style=\"color:inherit\"></p></div></body>");
        let p = doc.find_deep(doc.root, "p").unwrap();
        let sh = parse("div { color: teal }");
        assert_eq!(computed(&doc, p, &[&sh]).get("color").unwrap(), "teal");
    }

    #[test]
    fn var_fallbacks() {
        let c = props(
            "<body><p></p></body>",
            "p { color: var(--a, var(--b, red)); x-c: var(--nope, rgb(1, 2, 3)) }",
            "p",
        );
        assert_eq!(c.get("color").unwrap(), "red"); // nested fallback
        assert_eq!(c.get("x-c").unwrap(), "rgb(1, 2, 3)"); // comma in fallback
        // chained: --a unset → --b defined → green
        let c = props(
            "<body><p></p></body>",
            ":root { --b: green } p { color: var(--a, var(--b, red)) }",
            "p",
        );
        assert_eq!(c.get("color").unwrap(), "green");
        // `var(--x,)` — empty fallback → guaranteed-invalid → prop dropped
        let c = props("<body><p></p></body>", "p { color: var(--z,) }", "p");
        assert!(c.get("color").is_none());
    }

    #[test]
    fn var_cycle() {
        let c = props(
            "<body><p></p></body>",
            ":root { --a: var(--b); --b: var(--a) } p { color: var(--a) }",
            "p",
        );
        // cyclic custom props are invalid; with no fallback the decl drops
        assert!(c.get("color").is_none());
        // self-reference with fallback → falls back
        let c = props(
            "<body><p></p></body>",
            ":root { --a: var(--a) } p { color: var(--a, blue) }",
            "p",
        );
        assert_eq!(c.get("color").unwrap(), "blue");
    }

    #[test]
    fn custom_prop_inheritance() {
        // --x declared on a mid-tree ancestor reaches descendants
        let c = props(
            "<body><div><p></p></div></body>",
            "div { --g: 3px } p { margin-top: var(--g) }",
            "p",
        );
        assert_eq!(c.get("margin-top").unwrap(), "3px");
        // own decl overrides inherited
        let c = props(
            "<body><div><p></p></div></body>",
            "div { --g: 3px } p { --g: 9px; margin-top: var(--g) }",
            "p",
        );
        assert_eq!(c.get("margin-top").unwrap(), "9px");
    }

    #[test]
    fn var_in_shorthand() {
        // pending expansion: resolved multi-token value splits over sides
        let c = props(
            "<body><p></p></body>",
            ":root { --m: 2px 4px } p { margin: var(--m) }",
            "p",
        );
        assert_eq!(c.get("margin-top").unwrap(), "2px");
        assert_eq!(c.get("margin-right").unwrap(), "4px");
        // paren-containing fallback tokens don't split early
        let c = props(
            "<body><p></p></body>",
            "p { margin: var(--a, 3px) var(--b, 1px 2px) }",
            "p",
        );
        assert_eq!(c.get("margin-top").unwrap(), "3px");
        assert_eq!(c.get("margin-bottom").unwrap(), "2px");
    }

    #[test]
    fn all_keyword() {
        // all:unset — inherited props take parent's, others reset
        let c = props(
            "<body><div><p></p></div></body>",
            "div { color: red; margin-top: 7px } p { all: unset }",
            "p",
        );
        assert_eq!(c.get("color").unwrap(), "red");
        assert!(c.get("margin-top").is_none());
        // same block, later decl beats all (source order)
        let c = props("<body><p></p></body>", "p { all: unset; color: blue }", "p");
        assert_eq!(c.get("color").unwrap(), "blue");
        // all:inherit — every prop, incl. non-inherited, takes parent's
        let c = props(
            "<body><div><p></p></div></body>",
            "div { margin-top: 7px; color: green } p { all: inherit }",
            "p",
        );
        assert_eq!(c.get("margin-top").unwrap(), "7px");
        assert_eq!(c.get("color").unwrap(), "green");
    }

    #[test]
    fn property_registration() {
        let doc = rowser_html::parse("<body><div><p></p></div></body>");
        let p = doc.find_deep(doc.root, "p").unwrap();
        // inherits:false — the parent's 20px does not reach p; the
        // registered initial-value does
        let sh = parse(
            "@property --gap { syntax: \"<length>\"; inherits: false; initial-value: 4px } \
             div { --gap: 20px } p { margin-top: var(--gap) }",
        );
        assert_eq!(computed(&doc, p, &[&sh]).get("margin-top").unwrap(), "4px");
        // syntax mismatch → invalid at computed time → initial-value
        let sh = parse(
            "@property --gap { syntax: \"<length>\"; inherits: true; initial-value: 8px } \
             p { --gap: red; margin-top: var(--gap) }",
        );
        assert_eq!(computed(&doc, p, &[&sh]).get("margin-top").unwrap(), "8px");
        // registered inherits:true flows like a normal custom prop
        let sh = parse(
            "@property --gap { syntax: \"<length>\"; inherits: true } \
             div { --gap: 20px } p { margin-top: var(--gap) }",
        );
        assert_eq!(computed(&doc, p, &[&sh]).get("margin-top").unwrap(), "20px");
    }

    #[test]
    fn env_safe_area() {
        let c = props(
            "<body><p></p></body>",
            "p { padding-left: env(safe-area-inset-left, 5px) }",
            "p",
        );
        assert_eq!(c.get("padding-left").unwrap(), "0px");
    }

    #[test]
    fn font_size_px_normalization() {
        // own decl resolves against the parent's resolved size
        let doc = rowser_html::parse("<body><div><p></p></div></body>");
        let div = doc.find_deep(doc.root, "div").unwrap();
        let p = doc.find_deep(doc.root, "p").unwrap();
        let sh = parse("div { font-size: 2em }");
        let mut ctx = StyleCtx::new(&doc, &[&sh]);
        assert_eq!(ctx.computed(div).get("font-size").unwrap(), "32px");
        // p has no decl → inherits the resolved px, not "2em" (no doubling)
        assert_eq!(ctx.computed(p).get("font-size").unwrap(), "32px");
        // p's own em resolves against the inherited base
        let sh = parse("div { font-size: 2em } p { font-size: 1.5em }");
        let mut ctx = StyleCtx::new(&doc, &[&sh]);
        assert_eq!(ctx.computed(p).get("font-size").unwrap(), "48px");
    }

    #[test]
    fn inherited_set() {
        // inherited prop set only on an ancestor reaches the node
        let c = props(
            "<body><div><p></p></div></body>",
            "div { color: fuchsia; font-weight: bold; text-align: center }",
            "p",
        );
        assert_eq!(c.get("color").unwrap(), "fuchsia");
        assert_eq!(c.get("font-weight").unwrap(), "bold");
        assert_eq!(c.get("text-align").unwrap(), "center");
        // ...but a node's own matched decl still wins over inheritance
        let c = props(
            "<body><div><p></p></div></body>",
            "div { color: fuchsia } p { color: blue }",
            "p",
        );
        assert_eq!(c.get("color").unwrap(), "blue");
        // non-inherited props do NOT propagate
        let c = props(
            "<body><div><p></p></div></body>",
            "div { margin-top: 7px; display: flex }",
            "p",
        );
        assert!(c.get("margin-top").is_none());
        assert!(c.get("display").is_none());
    }

    #[test]
    fn media_misc() {
        let n = |css: &str| parse(css).rules.len();
        assert_eq!(n("@media (prefers-reduced-motion: reduce) { p { a: 1 } }"), 0);
        assert_eq!(n("@media (prefers-reduced-motion: no-preference) { p { a: 1 } }"), 1);
        assert_eq!(n("@media (prefers-contrast: more) { p { a: 1 } }"), 0);
        assert_eq!(n("@media (prefers-contrast: no-preference) { p { a: 1 } }"), 1);
        assert_eq!(n("@media (forced-colors: active) { p { a: 1 } }"), 0);
        assert_eq!(n("@media (forced-colors: none) { p { a: 1 } }"), 1);
        assert_eq!(n("@media (hover: hover) { p { a: 1 } }"), 1);
        assert_eq!(n("@media (hover: none) { p { a: 1 } }"), 0);
        assert_eq!(n("@media (pointer: fine) { p { a: 1 } }"), 1);
        assert_eq!(n("@media (pointer: coarse) { p { a: 1 } }"), 0);
        assert_eq!(n("@media (pointer: none) { p { a: 1 } }"), 0);
        assert_eq!(n("@media (orientation: landscape) { p { a: 1 } }"), 1);
        assert_eq!(n("@media (aspect-ratio: 4/3) { p { a: 1 } }"), 1); // 1024/768
        assert_eq!(n("@media (min-aspect-ratio: 1/1) { p { a: 1 } }"), 1);
        assert_eq!(n("@media (max-aspect-ratio: 1/1) { p { a: 1 } }"), 0);
        assert_eq!(n("@media (resolution: 96dpi) { p { a: 1 } }"), 1);
        assert_eq!(n("@media (resolution: 1dppx) { p { a: 1 } }"), 1);
        assert_eq!(n("@media (min-resolution: 2dppx) { p { a: 1 } }"), 0);
        assert_eq!(n("@media (max-resolution: 150dpi) { p { a: 1 } }"), 1);
        assert_eq!(n("@media (color) { p { a: 1 } }"), 1);
        assert_eq!(n("@media (color: 0) { p { a: 1 } }"), 0);
        assert_eq!(n("@media (min-color: 4) { p { a: 1 } }"), 1);
        assert_eq!(n("@media (color-gamut: srgb) { p { a: 1 } }"), 1);
        assert_eq!(n("@media (color-gamut: p3) { p { a: 1 } }"), 0);
        assert_eq!(n("@media (monochrome) { p { a: 1 } }"), 0);
        assert_eq!(n("@media (inverted-colors: inverted) { p { a: 1 } }"), 0);
        assert_eq!(n("@media (display-mode: browser) { p { a: 1 } }"), 1);
        assert_eq!(n("@media (display-mode: fullscreen) { p { a: 1 } }"), 0);
        assert_eq!(n("@media (min-height: 700px) { p { a: 1 } }"), 1);
        assert_eq!(n("@media (max-height: 500px) { p { a: 1 } }"), 0);
    }
}
