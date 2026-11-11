//! Generated content support: `content` value evaluation, CSS counters,
//! list markers, and `quotes`. Kept out of lib.rs — see `emit_pseudo`,
//! `push_list_ctx`, `make_marker` there for the call sites.
//!
//! Covered `content` forms: `<string>` literals, `attr(name)`, the four
//! `*-quote` keywords (driven by `quotes`; default “”‘’), `counter(name,
//! style?)`, `counters(name, sep, style?)`, and `none`/`normal`. url() and
//! image() content are parsed off and ignored (no fetch for content).

use crate::Props;
use rowser_dom::{Document, NodeId};
use rustc_hash::FxHashMap;

// ---------- CSS counters ----------

/// `counter-reset`/`counter-set`/`counter-increment` state.
///
/// Each name maps to a scope stack: `counter-reset` pushes a fresh scope;
/// `counter-set`/`counter-increment` modify the innermost, creating an
/// implicit scope when none exists. An element's `enter` returns the names
/// it pushed so `leave` pops exactly those — children open nested scopes;
/// a sibling's scope ends with the element that opened it.
#[derive(Default)]
pub(crate) struct Counters {
    map: FxHashMap<String, Vec<i32>>,
}

impl Counters {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Apply an element's counter props; returns the names this element
    /// pushed (hand back to `leave` when its box ends).
    pub(crate) fn enter(&mut self, c: &Props) -> Vec<String> {
        let mut pushed: Vec<String> = Vec::new();
        for (prop, mode, missing) in [
            ("counter-reset", 0u8, 0i32),
            ("counter-set", 1, 0),
            // `counter-increment: name` (no int) increments by 1
            ("counter-increment", 2, 1),
        ] {
            let Some(v) = c.get(prop) else { continue };
            let v = v.trim();
            if v.is_empty() || v == "none" {
                continue;
            }
            for (name, n) in counter_pairs(v) {
                let n = n.unwrap_or(missing);
                let stack = self.map.entry(name.clone()).or_default();
                match mode {
                    0 => {
                        // reset: always opens a fresh scope
                        stack.push(n);
                        pushed.push(name);
                    }
                    1 => {
                        // set: overwrite innermost; with none, an implicit
                        // scope opens — NOT recorded in `pushed`, so it
                        // survives this element and later siblings share it
                        // (approximates the spec's implicit reset-to-0 scope)
                        if let Some(top) = stack.last_mut() {
                            *top = n;
                        } else {
                            stack.push(n);
                        }
                    }
                    _ => {
                        // increment: innermost += n; implicit scope as above
                        if let Some(top) = stack.last_mut() {
                            *top += n;
                        } else {
                            stack.push(n);
                        }
                    }
                }
            }
        }
        pushed
    }

    /// Pop the scopes `enter` recorded (reverse order).
    pub(crate) fn leave(&mut self, pushed: Vec<String>) {
        for name in pushed.into_iter().rev() {
            if let Some(stack) = self.map.get_mut(&name) {
                stack.pop();
                if stack.is_empty() {
                    self.map.remove(&name);
                }
            }
        }
    }

    /// Innermost value of `name` (0 when the counter doesn't exist).
    pub(crate) fn value(&self, name: &str) -> i32 {
        self.map
            .get(name)
            .and_then(|s| s.last())
            .copied()
            .unwrap_or(0)
    }

    /// All scope values of `name`, outermost → innermost.
    pub(crate) fn values(&self, name: &str) -> &[i32] {
        self.map.get(name).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// `name [int]` pairs, whitespace separated (`counter-reset: a 2 b`).
/// `None` int = the declaration didn't specify one (defaults differ per
/// property — see `enter`).
fn counter_pairs(v: &str) -> Vec<(String, Option<i32>)> {
    let mut out: Vec<(String, Option<i32>)> = Vec::new();
    let mut it = v.split_whitespace().peekable();
    while let Some(tok) = it.next() {
        if tok.parse::<i32>().is_ok() {
            continue; // orphan number — ignore
        }
        let n = match it.peek().and_then(|t| t.parse::<i32>().ok()) {
            Some(n) => {
                it.next();
                Some(n)
            }
            None => None,
        };
        out.push((tok.to_string(), n));
    }
    out
}

// ---------- counter / marker number formatting ----------

/// Format `n` in a counter/list style: decimal, decimal-leading-zero,
/// lower-alpha, upper-alpha, lower-roman, upper-roman, none.
pub(crate) fn fmt_counter(n: i32, style: &str) -> String {
    match style {
        "decimal-leading-zero" => {
            if n < 0 {
                format!("-{n:02}")
            } else {
                format!("{n:02}")
            }
        }
        "lower-alpha" | "lower-latin" => alpha(n, false),
        "upper-alpha" | "upper-latin" => alpha(n, true),
        "lower-roman" => roman(n).to_lowercase(),
        "upper-roman" => roman(n),
        "none" => String::new(),
        _ => n.to_string(),
    }
}

/// Alphabetic counter: 1→a, 26→z, 27→aa (0/negatives fall back to decimal).
fn alpha(mut n: i32, upper: bool) -> String {
    if n <= 0 {
        return n.to_string();
    }
    let mut s = Vec::new();
    while n > 0 {
        n -= 1;
        s.push((b'a' + (n % 26) as u8) as char);
        n /= 26;
    }
    let s: String = s.into_iter().rev().collect();
    if upper {
        s.to_uppercase()
    } else {
        s
    }
}

/// Roman numerals for 1..=3999; outside that range, decimal fallback.
fn roman(n: i32) -> String {
    if n <= 0 || n > 3999 {
        return n.to_string();
    }
    const T: &[(i32, &str)] = &[
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut out = String::new();
    let mut n = n;
    for &(v, s) in T {
        while n >= v {
            out.push_str(s);
            n -= v;
        }
    }
    out
}

// ---------- content value evaluation ----------

/// Evaluate a pseudo box's `content` value to text. `None` = no box
/// (`none`/`normal`, empty result, or no `content` declaration at all).
/// `depth` is the document-wide open-quote nesting depth.
pub(crate) fn content_text(
    doc: &Document,
    node: NodeId,
    c: &Props,
    counters: &Counters,
    depth: &mut i32,
) -> Option<String> {
    let v = c.get("content")?;
    let v = v.trim();
    if v.is_empty() || v == "none" || v == "normal" {
        return None;
    }
    let pairs = quote_pairs(c);
    let clamp = |d: i32| -> usize {
        d.clamp(0, pairs.len().saturating_sub(1) as i32) as usize
    };
    let mut out = String::new();
    for tok in content_tokens(v) {
        match tok {
            Tok::Str(s) => out.push_str(&s),
            Tok::Attr(name) => {
                if let Some(a) = doc.attr(node, &name) {
                    out.push_str(a);
                }
            }
            Tok::OpenQ => {
                if let Some((o, _)) = pairs.get(clamp(*depth)) {
                    out.push_str(o);
                }
                *depth += 1;
            }
            Tok::CloseQ => {
                *depth = (*depth - 1).max(0);
                if let Some((_, cl)) = pairs.get(clamp(*depth)) {
                    out.push_str(cl);
                }
            }
            Tok::NoOpenQ => *depth += 1,
            Tok::NoCloseQ => *depth = (*depth - 1).max(0),
            Tok::Counter(name, style) => {
                out.push_str(&fmt_counter(
                    counters.value(&name),
                    style.as_deref().unwrap_or(""),
                ));
            }
            Tok::Counters(name, sep, style) => {
                let st = style.as_deref().unwrap_or("");
                let vals: Vec<String> = counters
                    .values(&name)
                    .iter()
                    .map(|&n| fmt_counter(n, st))
                    .collect();
                out.push_str(&vals.join(&sep));
            }
            Tok::Skip => {}
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// `quotes` → open/close pairs (outer level first). `none`/an empty list
/// produces no quote glyphs; unspecified → the UA default “”‘’.
fn quote_pairs(c: &Props) -> Vec<(String, String)> {
    if let Some(v) = c.get("quotes") {
        if v.trim() == "none" {
            return Vec::new();
        }
        let strs: Vec<String> = quoted_strings(v).into_iter().map(|s| s.1).collect();
        let mut out = Vec::new();
        let mut it = strs.into_iter();
        while let Some(o) = it.next() {
            // a dangling open quote pairs with itself
            let cl = it.next().unwrap_or_else(|| o.clone());
            out.push((o, cl));
        }
        return out;
    }
    vec![
        ("\u{201c}".to_string(), "\u{201d}".to_string()),
        ("\u{2018}".to_string(), "\u{2019}".to_string()),
    ]
}

enum Tok {
    Str(String),
    Attr(String),
    OpenQ,
    CloseQ,
    NoOpenQ,
    NoCloseQ,
    Counter(String, Option<String>),
    Counters(String, String, Option<String>),
    /// url()/image()/leader()/none/normal/unknown — contributes nothing.
    Skip,
}

/// `(byte_index_of_open_quote, contents)` per quoted string in `v`.
fn quoted_strings(v: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let b = v.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'"' || b[i] == b'\'' {
            let q = b[i];
            let st = i + 1;
            let mut j = st;
            let mut s = String::new();
            while j < b.len() && b[j] != q {
                if b[j] == b'\\' && j + 1 < b.len() {
                    j += 1;
                }
                s.push(b[j] as char);
                j += 1;
            }
            out.push((i, s));
            i = j + 1;
        } else {
            i += 1;
        }
    }
    out
}

/// Tokenize a `content` value: quoted strings, `fn(...)`, keywords.
fn content_tokens(v: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let b = v.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'"' || c == b'\'' {
            let q = c;
            let mut j = i + 1;
            let mut s = String::new();
            while j < b.len() && b[j] != q {
                if b[j] == b'\\' && j + 1 < b.len() {
                    j += 1;
                }
                s.push(b[j] as char);
                j += 1;
            }
            out.push(Tok::Str(s));
            i = j + 1;
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'-' || c >= 0x80 {
            let st = i;
            while i < b.len()
                && (b[i].is_ascii_alphanumeric() || matches!(b[i], b'-' | b'_') || b[i] >= 0x80)
            {
                i += 1;
            }
            let word = &v[st..i];
            // whitespace may precede '('
            let mut j = i;
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < b.len() && b[j] == b'(' {
                let mut depth = 0i32;
                let mut k = j;
                while k < b.len() {
                    match b[k] {
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    k += 1;
                }
                let inner = &v[j + 1..k.min(v.len())];
                let args: Vec<&str> =
                    crate::top_args(inner).into_iter().map(|s| s.trim()).collect();
                let arg = |n: usize| args.get(n).copied().unwrap_or("");
                let unquote = |s: &str| s.trim().trim_matches('"').trim_matches('\'').to_string();
                out.push(match word {
                    "attr" => Tok::Attr(unquote(arg(0))),
                    "counter" => Tok::Counter(
                        arg(0).to_string(),
                        (args.len() > 1).then(|| arg(1).to_string()),
                    ),
                    "counters" => Tok::Counters(
                        arg(0).to_string(),
                        unquote(arg(1)),
                        (args.len() > 2).then(|| arg(2).to_string()),
                    ),
                    _ => Tok::Skip, // url(), image(), leader(), ...
                });
                i = k + 1;
                continue;
            }
            out.push(match word {
                "open-quote" => Tok::OpenQ,
                "close-quote" => Tok::CloseQ,
                "no-open-quote" => Tok::NoOpenQ,
                "no-close-quote" => Tok::NoCloseQ,
                _ => Tok::Skip, // none/normal/unknown idents
            });
            continue;
        }
        i += 1;
    }
    out
}

// ---------- list markers ----------

/// list-style-type resolved to a marker kind.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum LType {
    Disc,
    Circle,
    Square,
    Decimal,
    DecimalZero,
    LowerAlpha,
    UpperAlpha,
    LowerRoman,
    UpperRoman,
    /// `list-style-type: "*"` — a custom marker string.
    Custom(String),
    /// `none` (marker suppressed).
    None,
}

/// One open list context on the list stack.
pub(crate) struct ListCtx {
    /// `list-style:none`-style suppression from the list itself.
    pub hide: bool,
    /// ordered (ol): markers are formatted numbers; unordered: glyphs.
    pub ordered: bool,
    /// marker type for items that don't override it.
    pub typ: LType,
    /// list-style-position:inside → marker joins the first line.
    pub inside: bool,
    /// next marker number (ordered lists only).
    pub next: i32,
    /// +1 normally, -1 for `ol reversed`.
    pub step: i32,
}

/// One token as a list-style-type, or None when it isn't one (e.g.
/// `inside`, `url(...)`).
fn type_token(tok: &str) -> Option<LType> {
    Some(match tok {
        "disc" => LType::Disc,
        "circle" => LType::Circle,
        "square" => LType::Square,
        "decimal" => LType::Decimal,
        "decimal-leading-zero" => LType::DecimalZero,
        "lower-alpha" | "lower-latin" => LType::LowerAlpha,
        "upper-alpha" | "upper-latin" => LType::UpperAlpha,
        "lower-roman" => LType::LowerRoman,
        "upper-roman" => LType::UpperRoman,
        "none" => LType::None,
        "inside" | "outside" => return None,
        _ if tok.starts_with("url(") => return None,
        _ if tok.starts_with('"') || tok.starts_with('\'') => {
            LType::Custom(tok.trim_matches('"').trim_matches('\'').to_string())
        }
        _ => LType::Custom(tok.to_string()),
    })
}

/// `(type, inside)` an element's own list props specify.
/// `Some(LType::None)` = explicit `none` (marker suppressed).
pub(crate) fn list_props(c: &Props) -> (Option<LType>, Option<bool>) {
    let mut typ = None;
    let mut pos = None;
    if let Some(v) = c.get("list-style") {
        // quoted strings are custom marker text; everything else is a
        // type/position/image token
        for tok in v.split_whitespace() {
            match tok {
                "inside" => pos = Some(true),
                "outside" => pos = Some(false),
                _ => {
                    if let Some(t) = type_token(tok) {
                        typ = Some(t);
                    }
                }
            }
        }
        // multi-token custom strings like `" - "` split on whitespace —
        // recover them whole
        for (_, s) in quoted_strings(v) {
            typ = Some(LType::Custom(s));
        }
    }
    if let Some(v) = c.get("list-style-type") {
        typ = type_token(v.trim());
    }
    if let Some(v) = c.get("list-style-position") {
        pos = Some(v.trim() == "inside");
    }
    (typ, pos)
}

/// Marker text for a list item: `Some` when a marker paints.
/// `num` is meaningful for ordered/numeric types only.
pub(crate) fn marker_text(typ: &LType, num: i32, ordered: bool) -> Option<String> {
    match typ {
        LType::None => None,
        LType::Custom(s) => Some(s.clone()),
        _ if ordered => Some(format!("{}.", fmt_counter(num, type_name(typ)))),
        _ => Some(
            match typ {
                LType::Circle => "\u{25e6}", // ◦
                LType::Square => "\u{25aa}", // ▪
                _ => "\u{2022}",             // •
            }
            .to_string(),
        ),
    }
}

fn type_name(t: &LType) -> &'static str {
    match t {
        LType::DecimalZero => "decimal-leading-zero",
        LType::LowerAlpha => "lower-alpha",
        LType::UpperAlpha => "upper-alpha",
        LType::LowerRoman => "lower-roman",
        LType::UpperRoman => "upper-roman",
        _ => "decimal",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(kvs: &[(&str, &str)]) -> Props {
        kvs.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn counter_pairs_parse() {
        assert_eq!(
            counter_pairs("a 2 b c -3"),
            vec![
                ("a".to_string(), Some(2)),
                ("b".to_string(), None),
                ("c".to_string(), Some(-3))
            ]
        );
    }

    #[test]
    fn counter_scopes() {
        let mut ct = Counters::new();
        let parent = props(&[("counter-reset", "sec 0")]);
        let child = props(&[("counter-increment", "sec"), ("counter-reset", "sub 5")]);
        let p = ct.enter(&parent);
        let c1 = ct.enter(&child);
        assert_eq!(ct.value("sec"), 1);
        assert_eq!(ct.value("sub"), 5);
        ct.leave(c1);
        let c2 = ct.enter(&child);
        assert_eq!(ct.value("sec"), 2, "sibling shares the parent scope");
        ct.leave(c2);
        ct.leave(p);
        assert_eq!(ct.value("sec"), 0, "scope popped at element end");
    }

    #[test]
    fn implicit_counter_scope() {
        // increment with no enclosing reset opens a root-level scope
        // that later siblings share
        let mut ct = Counters::new();
        let inc = props(&[("counter-increment", "c")]);
        let a = ct.enter(&inc);
        assert_eq!(ct.value("c"), 1);
        ct.leave(a);
        let b = ct.enter(&inc);
        assert_eq!(ct.value("c"), 2);
        ct.leave(b);
    }

    #[test]
    fn counter_formats() {
        assert_eq!(fmt_counter(4, "decimal"), "4");
        assert_eq!(fmt_counter(4, "decimal-leading-zero"), "04");
        assert_eq!(fmt_counter(1, "lower-alpha"), "a");
        assert_eq!(fmt_counter(27, "upper-alpha"), "AA");
        assert_eq!(fmt_counter(4, "lower-roman"), "iv");
        assert_eq!(fmt_counter(1987, "upper-roman"), "MCMLXXXVII");
        assert_eq!(fmt_counter(0, "lower-roman"), "0");
    }

    #[test]
    fn list_style_parsing() {
        let c = props(&[("list-style", "inside square")]);
        assert_eq!(list_props(&c), (Some(LType::Square), Some(true)));
        let c = props(&[("list-style-type", "lower-roman")]);
        assert_eq!(list_props(&c), (Some(LType::LowerRoman), None));
        let c = props(&[("list-style", "\"*\" inside")]);
        assert_eq!(
            list_props(&c),
            (Some(LType::Custom("*".to_string())), Some(true))
        );
        let c = props(&[("list-style", "none")]);
        assert_eq!(list_props(&c), (Some(LType::None), None));
    }
}
