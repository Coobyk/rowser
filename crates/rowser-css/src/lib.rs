//! rowser-css: CSS parsing and matching, from scratch.
//!
//! Pragmatic subset of CSS Syntax Level 3 / Selectors 4: rule lists,
//! qualified rules with selector lists + declaration blocks, `!important`,
//! comments, @rules (`@media`/`@supports`/`@layer` unwrapped, others skipped).
//! Selectors: `*`, tag, `.class`, `#id`, `[attr]` with `= ~= |= ^= $= *=`
//! and `i`/`s` flags, descendant/`>`/`+`/`~` combinators, structural
//! pseudo-classes (`:nth-*`, `:first/last/only-*`, `:empty`, `:checked`,
//! `:disabled`, `:enabled`), functional `:is`/`:where`/`:not`/`:has`, `:root`,
//! `:host`, `&` (nesting ref — matches any element). Dynamic-state pseudos
//! (`:hover`, `:focus`, `:active`, `:target`, `:link`...) match against a
//! [`PseudoState`] carried by [`StyleCtx`]; `::before`/`::after`/`::marker`/
//! `::first-letter`/`::first-line`/`::selection`/`::placeholder` parse to
//! [`Simple::PseudoElem`] and style generated boxes via
//! [`StyleCtx::computed_pseudo`]. Other `::pseudo-elements` never match.
//!
//! The cascade: user-agent sheet < author sheets, then !important author >
//! !important UA; ties broken by specificity then source order.

use rowser_dom::{Document, NodeId};
use rustc_hash::FxHashMap;

pub mod cascade;
mod pseudo;

/// Property maps keyed by CSS property name — Fx-hashed (string keys are
/// small and lookups/inserts dominate the cascade).
pub type StrMap<V> = FxHashMap<String, V>;

mod fontface;
pub use fontface::{FontFace, FontFaceStyle, FontSrc};

#[derive(Debug, Clone)]
pub struct Decl {
    pub prop: String,
    pub value: String,
    pub important: bool,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub selectors: Vec<Selector>,
    pub decls: Vec<Decl>,
}

/// Right-to-left matching list: last element is the rightmost compound.
#[derive(Debug, Clone, PartialEq)]
pub struct Selector {
    /// (combinator-before-compound, compound). For absolute selectors the
    /// first entry's combinator is None; parts[i].0 relates parts[i-1] to
    /// parts[i]. For `:has` relative selectors, parts[0].0 is the lead
    /// combinator binding compound 0 to the anchor (`>`/`+`/`~`, or
    /// Descendant when omitted).
    pub parts: Vec<(Combinator, Vec<Simple>)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Combinator {
    Descendant,
    Child,
    /// `a + b` — next element sibling.
    NextSibling,
    /// `a ~ b` — any subsequent element sibling.
    Sibling,
    None,
}

/// Attribute-selector operators: `= ~= |= ^= $= *=`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AttrOp {
    Eq,
    /// `~=` whitespace-separated word list.
    Includes,
    /// `|=` exact or `v-` prefix (lang-style).
    DashMatch,
    /// `^=` prefix.
    Prefix,
    /// `$=` suffix.
    Suffix,
    /// `*=` substring.
    Substring,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Simple {
    Universal,
    /// `&` nesting reference (top-level → any element).
    Nest,
    Tag(String),
    Class(String),
    Id(String),
    AttrExists(String),
    AttrEquals(String, String),
    /// `[attr op v]` with flag for ASCII case-insensitive compare.
    AttrOp(String, AttrOp, String, bool),
    FirstChild,
    LastChild,
    FirstOfType,
    LastOfType,
    OnlyChild,
    OnlyOfType,
    /// `:nth-child(An+B)` — (a, b) coefficients.
    NthChild(i32, i32),
    NthLastChild(i32, i32),
    NthOfType(i32, i32),
    NthLastOfType(i32, i32),
    /// `:empty` — no element children and no non-whitespace text.
    Empty,
    /// `:checked` — `checked`/`selected` attribute present.
    Checked,
    Disabled,
    Enabled,
    /// `:root` — the html element.
    Root,
    /// `:is(list)` — matches any selector in list, max-arg specificity.
    Is(Vec<Selector>),
    /// `:where(list)` — like :is but zero specificity.
    Where(Vec<Selector>),
    /// `:not(list)` — matches when no selector in list matches.
    Not(Vec<Selector>),
    /// `:has(rel-list)` — relative selectors anchored on this element.
    Has(Vec<Selector>),
    /// `:hover` — node is the hovered element or an ancestor of it.
    Hover,
    /// `:focus` — node is the focused element.
    Focus,
    /// `:focus-within` — node is the focused element or an ancestor of it.
    FocusWithin,
    /// `:focus-visible` — approximated as `:focus` (no focus modality yet).
    FocusVisible,
    /// `:active` — node is the activated element or an ancestor of it.
    Active,
    /// `:link` — `<a>`/`<area>` with `href` (no history → always unvisited).
    Link,
    /// `:visited` — never matches: no history database.
    Visited,
    /// `:target` — element whose `id` equals the current URL fragment.
    Target,
    /// Valid-but-never-matching: `::pseudo-elements` we don't generate
    /// boxes for, and functional pseudos we don't support.
    Never,
    /// `::pseudo-element` in the rightmost compound — never matches a real
    /// element; [`StyleCtx::computed_pseudo`] finds the rules styling an
    /// element's generated box.
    PseudoElem(PseudoElem),
    /// Unsupported pseudo — selector fails quietly.
    Unsupported,
}

/// Pseudo-elements that generate/style boxes in layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PseudoElem {
    Before,
    After,
    Marker,
    FirstLetter,
    FirstLine,
    Selection,
    Placeholder,
}

/// Dynamic-interaction state the `:hover`/`:focus`/`:active`/`:target`
/// selectors match against. One per rendered page: the event loop updates
/// it (pointer move/down/up, click-to-focus, `#frag` navigation) and hands
/// it to [`StyleCtx::set_state`] before each layout.
#[derive(Debug, Clone, Default)]
pub struct PseudoState {
    /// Element under the pointer — `:hover` also matches its ancestors.
    pub hovered: Option<NodeId>,
    /// Element holding focus — `:focus`/`:focus-visible` match it,
    /// `:focus-within` matches its ancestors too.
    pub focused: Option<NodeId>,
    /// Element the pointer is pressed on — `:active` also matches ancestors.
    pub active: Option<NodeId>,
    /// URL fragment (`#x`) for `:target`.
    pub target_frag: Option<String>,
}

pub struct StyleSheet {
    pub rules: Vec<Rule>,
    /// Cascade-layer index per rule (parallel to `rules`); `u32::MAX` =
    /// unlayered, which beats every layered rule of the same importance.
    layers: Vec<u32>,
    /// Rightmost-compound index built at parse time: rules bucketed by a
    /// tag/class/id their rightmost compound requires. `plain` holds rules
    /// whose rightmost compound needs no such key (`*`, `[attr]`, pseudos).
    /// The buckets are a superset of real matches — full matching still runs.
    by_tag: FxHashMap<String, Vec<u32>>,
    by_class: FxHashMap<String, Vec<u32>>,
    by_id: FxHashMap<String, Vec<u32>>,
    plain: Vec<u32>,
    /// `@keyframes` name → declarations of its end (`to`/`100%`/last)
    /// frame, used to emulate animation-fill-mode resting state.
    pub keyframes: FxHashMap<String, Vec<Decl>>,
    /// `@property --x {}` registrations: custom-prop name → descriptors.
    pub properties: FxHashMap<String, cascade::PropertyDef>,
    /// `@font-face` blocks collected while parsing — consumed by the
    /// renderer's font loader, never by the cascade.
    pub fonts: Vec<FontFace>,
}

pub fn parse(css: &str) -> StyleSheet {
    parse_vw(css, 1024.0)
}

/// Parse a single complex selector (e.g. `div.a > .b[x]`, `a b`) for
/// `querySelector`-style matching via [`matches`]. Selector lists (`a, b`)
/// are not handled — split on commas and match each.
pub fn parse_selector(s: &str) -> Option<Selector> {
    let mut p = CssParser {
        s: s.as_bytes(),
        pos: 0,
        vw: 1024.0,
        dark: false,
        cur_layer: None,
        layer_map: FxHashMap::default(),
        layer_count: 0,
        keyframes: Vec::new(),
        properties: Vec::new(),
        font_faces: Vec::new(),
    };
    p.skip_ws_comments();
    let sel = p.selector()?;
    p.skip_ws_comments();
    if p.eof() {
        Some(sel)
    } else {
        None
    }
}

pub fn parse_vw(css: &str, vw: f32) -> StyleSheet {
    let mut p = CssParser {
        s: css.as_bytes(),
        pos: 0,
        vw,
        dark: dark_mode(),
        cur_layer: None,
        layer_map: FxHashMap::default(),
        layer_count: 0,
        keyframes: Vec::new(),
        properties: Vec::new(),
        font_faces: Vec::new(),
    };
    let mut pairs: Vec<(u32, Rule)> = Vec::new();
    p.skip_ws_comments();
    while !p.eof() {
        if p.peek() == Some(b'@') {
            if let Some(mut inner) = p.at_rule_rules() {
                pairs.append(&mut inner);
            }
        } else if let Some(r) = p.rule() {
            pairs.push(r);
        }
        p.skip_ws_comments();
    }
    let (rules, layers): (Vec<Rule>, Vec<u32>) = pairs.into_iter().map(|(l, r)| (r, l)).unzip();
    let mut sheet = StyleSheet {
        rules,
        layers,
        by_tag: FxHashMap::default(),
        by_class: FxHashMap::default(),
        by_id: FxHashMap::default(),
        plain: Vec::new(),
        keyframes: p.keyframes.into_iter().collect(),
        properties: p
            .properties
            .into_iter()
            .map(|d| (d.name.clone(), d))
            .collect(),
        fonts: p.font_faces,
    };
    sheet.reindex();
    sheet
}

/// Bucket key a selector's rightmost compound is indexed under. Picking id >
/// class > tag > none keeps buckets selective; any required key is correct
/// because the full matcher still runs on candidates.
fn rightmost_key(sel: &Selector) -> Option<(&str, &str)> {
    let (_, simples) = sel.parts.last()?;
    for s in simples {
        if let Simple::Id(i) = s {
            return Some(("id", i));
        }
    }
    for s in simples {
        if let Simple::Class(c) = s {
            return Some(("class", c));
        }
    }
    for s in simples {
        if let Simple::Tag(t) = s {
            return Some(("tag", t));
        }
    }
    None
}

impl StyleSheet {
    /// (Re)build the rightmost-compound index after `rules` changes.
    pub fn reindex(&mut self) {
        self.layers.resize(self.rules.len(), u32::MAX);
        self.layers.truncate(self.rules.len());
        self.by_tag.clear();
        self.by_class.clear();
        self.by_id.clear();
        self.plain.clear();
        for (i, rule) in self.rules.iter().enumerate() {
            let mut bucketed = false;
            for sel in &rule.selectors {
                match rightmost_key(sel) {
                    Some(("id", k)) => {
                        self.by_id.entry(k.to_string()).or_default().push(i as u32);
                        bucketed = true;
                    }
                    Some(("class", k)) => {
                        self.by_class
                            .entry(k.to_string())
                            .or_default()
                            .push(i as u32);
                        bucketed = true;
                    }
                    Some(("tag", k)) => {
                        self.by_tag.entry(k.to_string()).or_default().push(i as u32);
                        bucketed = true;
                    }
                    _ => {}
                }
            }
            if !bucketed {
                self.plain.push(i as u32);
            }
        }
    }

    /// Positions of rules that could match `node`, in sheet order, appended
    /// to `out` (cleared first). Union of every bucket the node keys into,
    /// deduplicated — a superset of the rules whose selectors actually match.
    /// Each bucket is already sorted (rule indices are pushed in order), so
    /// the final sort is over a mostly-sorted vec.
    fn candidates_into(&self, doc: &Document, node: NodeId, out: &mut Vec<u32>) {
        out.clear();
        out.extend_from_slice(&self.plain);
        if let Some(t) = doc.tag(node) {
            if let Some(ix) = self.by_tag.get(t) {
                out.extend_from_slice(ix);
            }
        }
        if let Some(id) = doc.attr(node, "id") {
            if let Some(ix) = self.by_id.get(id) {
                out.extend_from_slice(ix);
            }
        }
        if let Some(cls) = doc.attr(node, "class") {
            for c in cls.split_whitespace() {
                if let Some(ix) = self.by_class.get(c) {
                    out.extend_from_slice(ix);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
    }
}

struct CssParser<'a> {
    s: &'a [u8],
    pos: usize,
    vw: f32,
    /// `prefers-color-scheme: dark` environment state.
    dark: bool,
    /// Cascade layer of the enclosing `@layer` block, if any.
    cur_layer: Option<u32>,
    /// Layer name → index, assigned by first mention (block or statement).
    layer_map: FxHashMap<String, u32>,
    layer_count: u32,
    /// `@keyframes` name → decls of the end (`to`/`100%`) frame, captured
    /// for animation end-state emulation (non-animating engine).
    keyframes: Vec<(String, Vec<Decl>)>,
    /// `@property` registrations collected while parsing.
    properties: Vec<cascade::PropertyDef>,
    /// `@font-face` descriptors collected for `StyleSheet::fonts`.
    font_faces: Vec<FontFace>,
}

impl<'a> CssParser<'a> {
    fn eof(&self) -> bool {
        self.pos >= self.s.len()
    }
    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }
    fn bump(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.pos += 1;
        Some(c)
    }
    fn is_ws(c: u8) -> bool {
        matches!(c, b' ' | b'\t' | b'\n' | 0x0c | b'\r')
    }
    fn skip_ws_comments(&mut self) {
        // Defensive: malformed input elsewhere may have pushed pos past len.
        self.pos = self.pos.min(self.s.len());
        loop {
            while self.peek().is_some_and(Self::is_ws) {
                self.pos += 1;
            }
            if self.s[self.pos..].starts_with(b"/*") {
                while self.pos + 1 < self.s.len() && &self.s[self.pos..self.pos + 2] != b"*/" {
                    self.pos += 1;
                }
                self.pos = (self.pos + 2).min(self.s.len());
            } else {
                break;
            }
        }
    }
    /// @media/@supports/@layer are unwrapped — inner rules returned (with
    /// their cascade-layer index) when the condition passes. Others are
    /// skipped. Returns (layer, rule) pairs to append.
    fn at_rule_rules(&mut self) -> Option<Vec<(u32, Rule)>> {
        self.pos += 1; // '@'
        let name = self.ident().to_lowercase();
        self.skip_ws_comments();
        let st = self.pos;
        while !self.eof() && self.peek() != Some(b'{') && self.peek() != Some(b';') {
            self.pos += 1;
        }
        let prelude = String::from_utf8_lossy(&self.s[st..self.pos])
            .trim()
            .to_lowercase();
        let raw_prelude = String::from_utf8_lossy(&self.s[st..self.pos])
            .trim()
            .to_string();
        let has_block = self.peek() == Some(b'{');
        if has_block {
            self.pos += 1;
        }
        let include = match name.as_str() {
            "media" => media_matches(&prelude, self.vw, self.dark),
            "supports" => supports_matches(&prelude),
            "layer" => true,
            _ => false,
        };
        // `@keyframes name { <offset> { decls } ... }` — capture the end
        // frame's declarations (the animation's resting state) instead of
        // skipping the block wholesale.
        if name == "keyframes" && has_block {
            let kf_name = prelude.trim().to_string();
            let mut end_decls: Vec<Decl> = Vec::new();
            loop {
                self.skip_ws_comments();
                match self.peek() {
                    None | Some(b'}') => {
                        if self.peek() == Some(b'}') {
                            self.pos += 1;
                        }
                        break;
                    }
                    _ => {}
                }
                // offset prelude (`from`/`to`/`50%`) until the decl block
                while !self.eof() && !matches!(self.peek(), Some(b'{') | Some(b'}')) {
                    self.pos += 1;
                }
                if self.peek() == Some(b'{') {
                    self.pos += 1;
                    end_decls = self.decl_block(); // last frame wins
                }
            }
            if !kf_name.is_empty() {
                self.keyframes.push((kf_name, end_decls));
            }
            return Some(Vec::new());
        }
        // `@property --x { syntax: "..."; inherits: bool; initial-value: v }`
        // — a declaration block, not a rule list; register and move on.
        if name == "property" && has_block {
            let decls = self.decl_block();
            let get = |n: &str| decls.iter().find(|d| d.prop == n).map(|d| d.value.clone());
            let pname = raw_prelude.trim().to_string();
            if pname.starts_with("--") {
                self.properties.push(cascade::PropertyDef {
                    name: pname,
                    syntax: get("syntax")
                        .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                        .unwrap_or_else(|| "*".to_string()),
                    inherits: get("inherits").is_some_and(|v| v.trim() == "true"),
                    initial: get("initial-value"),
                });
            }
            return Some(Vec::new());
        }
        // `@font-face { ... }` is a descriptor block — parse it into the
        // sheet's font list instead of rule-parsing it or skipping it.
        if name == "font-face" && has_block {
            let decls = self.decl_block();
            if let Some(ff) = fontface::font_face_from_decls(&decls) {
                self.font_faces.push(ff);
            }
            return Some(Vec::new());
        }
        // At-rules we don't unwrap (@font-face, @keyframes, @charset...)
        // hold declarations/frames, not rules — rule-parsing their block
        // produces garbage selectors and can consume past the closing
        // brace, eating the next real rule. Skip by brace depth instead.
        if !include {
            if has_block {
                self.pos -= 1; // hand '{' back to the skip scanner
                self.skip_at_rule();
            }
            return Some(Vec::new());
        }
        // @layer: statement form (`@layer a, b;`) only registers ordering;
        // block form wraps inner rules with the layer's index.
        let prev_layer = self.cur_layer;
        if name == "layer" {
            if has_block {
                self.cur_layer = Some(self.layer_index(prelude.trim()));
            } else {
                for nm in prelude.split(',') {
                    self.layer_index(nm.trim());
                }
            }
        }
        let mut rules = Vec::new();
        if has_block {
            // parse inner rule list until matching '}' (nesting is possible)
            loop {
                self.skip_ws_comments();
                match self.peek() {
                    None | Some(b'}') => {
                        self.pos = (self.pos + 1).min(self.s.len());
                        break;
                    }
                    Some(b'@') => {
                        if let Some(mut r) = self.at_rule_rules() {
                            rules.append(&mut r);
                        }
                    }
                    _ => {
                        if let Some(r) = self.rule() {
                            rules.push(r);
                        } else {
                            let _ = self.bump();
                        }
                    }
                }
            }
        }
        self.cur_layer = prev_layer;
        Some(rules)
    }

    /// Layer name → index, assigned in first-mention order; anonymous
    /// (empty-name) layers always get a fresh index.
    fn layer_index(&mut self, name: &str) -> u32 {
        if name.is_empty() {
            let i = self.layer_count;
            self.layer_count += 1;
            return i;
        }
        if let Some(&i) = self.layer_map.get(name) {
            return i;
        }
        let i = self.layer_count;
        self.layer_count += 1;
        self.layer_map.insert(name.to_string(), i);
        i
    }

    /// Skip an @-rule: either `;`-terminated or a `{...}` block.
    fn skip_at_rule(&mut self) {
        while !self.eof() {
            match self.bump() {
                Some(b';') => return,
                Some(b'{') => {
                    let mut depth = 1;
                    while !self.eof() && depth > 0 {
                        match self.bump() {
                            Some(b'{') => depth += 1,
                            Some(b'}') => depth -= 1,
                            _ => {}
                        }
                    }
                    return;
                }
                None => return,
                _ => {}
            }
        }
    }

    /// Identifier with CSS escapes: `\x` → literal `x`, `\hhhhhh` → codepoint.
    fn ident(&mut self) -> String {
        let mut out = String::new();
        while let Some(c) = self.peek() {
            if c == b'\\' {
                self.pos += 1;
                let st = self.pos;
                while self.pos < self.s.len()
                    && self.s[self.pos].is_ascii_hexdigit()
                    && self.pos - st < 6
                {
                    self.pos += 1;
                }
                if self.pos > st {
                    if let Ok(n) =
                        u32::from_str_radix(&String::from_utf8_lossy(&self.s[st..self.pos]), 16)
                    {
                        if let Some(ch) = char::from_u32(n) {
                            out.push(ch);
                        }
                    }
                    if self.peek() == Some(b' ') {
                        self.pos += 1;
                    }
                } else if let Some(lit) = self.peek() {
                    out.push(lit as char);
                    self.pos += 1;
                }
            } else if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_') || c >= 0x80 {
                // case preserved — class/id/attr-value identifiers are
                // case-sensitive; callers wanting ASCII-CI (tags, props,
                // at-rule names, pseudos) lowercase the result themselves
                out.push(c as char);
                self.pos += 1;
            } else {
                break;
            }
        }
        out
    }

    fn rule(&mut self) -> Option<(u32, Rule)> {
        // prelude: everything until '{' → selector list
        let mut selectors = Vec::new();
        loop {
            self.skip_ws_comments();
            if self.peek() == Some(b'{') {
                self.pos += 1;
                break;
            }
            if self.eof() {
                return None;
            }
            if let Some(sel) = self.selector() {
                selectors.push(sel);
            } else {
                // bad selector — skip to { or ;
                while !self.eof() && !matches!(self.peek(), Some(b'{') | Some(b';')) {
                    self.pos += 1;
                }
                if self.peek() == Some(b';') {
                    self.pos += 1;
                    return None;
                }
            }
            self.skip_ws_comments();
            if self.peek() == Some(b',') {
                self.pos += 1;
            }
        }
        let decls = self.decl_block();
        if selectors.is_empty() {
            return None;
        }
        Some((
            self.cur_layer.unwrap_or(u32::MAX),
            Rule { selectors, decls },
        ))
    }

    fn selector(&mut self) -> Option<Selector> {
        self.complex_selector(false)
    }

    /// Complex selector: compound (combinator compound)*. When `relative`
    /// (inside `:has`), an optional leading `>`/`+`/`~` binds the first
    /// compound to the anchor — stored as the first part's combinator.
    fn complex_selector(&mut self, relative: bool) -> Option<Selector> {
        let mut parts: Vec<(Combinator, Vec<Simple>)> = Vec::new();
        let mut comb = Combinator::None;
        if relative {
            self.skip_ws_comments();
            comb = match self.peek() {
                Some(b'>') => {
                    self.pos += 1;
                    Combinator::Child
                }
                Some(b'+') => {
                    self.pos += 1;
                    Combinator::NextSibling
                }
                Some(b'~') => {
                    self.pos += 1;
                    Combinator::Sibling
                }
                _ => Combinator::Descendant,
            };
        }
        loop {
            // whitespace/comments → descendant combinator when a compound
            // precedes and no explicit combinator is pending
            let before = self.pos;
            self.skip_ws_comments();
            if self.pos > before && !parts.is_empty() && comb == Combinator::None {
                comb = Combinator::Descendant;
            }
            match self.peek() {
                Some(b'>') => {
                    self.pos += 1;
                    comb = Combinator::Child;
                    continue;
                }
                Some(b'+') => {
                    self.pos += 1;
                    comb = Combinator::NextSibling;
                    continue;
                }
                Some(b'~') => {
                    self.pos += 1;
                    comb = Combinator::Sibling;
                    continue;
                }
                _ => {}
            }
            let save = self.pos;
            match self.compound() {
                Some(simples) if !simples.is_empty() => {
                    parts.push((comb, simples));
                    comb = Combinator::None;
                }
                _ => {
                    self.pos = save;
                    // a selector may not end on an explicit combinator
                    if matches!(
                        comb,
                        Combinator::Child | Combinator::NextSibling | Combinator::Sibling
                    ) {
                        return None;
                    }
                    break;
                }
            }
        }
        if parts.is_empty() {
            None
        } else {
            Some(Selector { parts })
        }
    }

    /// One compound selector: a run of `*`, `&`, tag, `.class`, `#id`,
    /// `[attr...]`, `:pseudo` simples. None = hard parse error.
    fn compound(&mut self) -> Option<Vec<Simple>> {
        let mut simples = Vec::new();
        loop {
            match self.peek() {
                Some(b'*') => {
                    self.pos += 1;
                    simples.push(Simple::Universal);
                }
                Some(b'&') => {
                    self.pos += 1;
                    simples.push(Simple::Nest);
                }
                Some(b'.') => {
                    self.pos += 1;
                    let name = self.ident();
                    if name.is_empty() {
                        return None;
                    }
                    simples.push(Simple::Class(name));
                }
                Some(b'#') => {
                    self.pos += 1;
                    let name = self.ident();
                    if name.is_empty() {
                        return None;
                    }
                    simples.push(Simple::Id(name));
                }
                Some(b'[') => match self.attr_sel() {
                    Some(s) => simples.push(s),
                    None => return None,
                },
                Some(b':') => match self.pseudo_sel() {
                    Some(s) => simples.push(s),
                    None => return None,
                },
                Some(c) if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c >= 0x80 => {
                    let name = self.ident().to_lowercase();
                    simples.push(Simple::Tag(name));
                }
                _ => break,
            }
        }
        Some(simples)
    }

    /// `[attr]`, `[attr=v]`, `[attr op v]` with `~= |= ^= $= *=` and `i`/`s`
    /// flags. Malformed forms degrade to AttrExists.
    fn attr_sel(&mut self) -> Option<Simple> {
        self.pos += 1; // '['
        self.skip_ws_comments();
        let name = self.ident().to_lowercase();
        if name.is_empty() {
            return None;
        }
        self.skip_ws_comments();
        let op = match self.peek() {
            Some(b'=') => {
                self.pos += 1;
                AttrOp::Eq
            }
            Some(c @ (b'~' | b'|' | b'^' | b'$' | b'*'))
                if self.s.get(self.pos + 1) == Some(&b'=') =>
            {
                self.pos += 2;
                match c {
                    b'~' => AttrOp::Includes,
                    b'|' => AttrOp::DashMatch,
                    b'^' => AttrOp::Prefix,
                    b'$' => AttrOp::Suffix,
                    _ => AttrOp::Substring,
                }
            }
            _ => {
                self.skip_to(b']');
                return Some(Simple::AttrExists(name));
            }
        };
        self.skip_ws_comments();
        let v = match self.peek() {
            Some(q @ (b'"' | b'\'')) => {
                self.pos += 1;
                let st = self.pos;
                while self.peek().is_some_and(|c| c != q) {
                    self.pos += 1;
                }
                let v = String::from_utf8_lossy(&self.s[st..self.pos]).into_owned();
                let _ = self.bump(); // closing quote, if present
                v
            }
            _ => {
                // bare value: up to ws or ']' (case preserved — attr values
                // are case-sensitive)
                let st = self.pos;
                while self.peek().is_some_and(|c| !Self::is_ws(c) && c != b']') {
                    self.pos += 1;
                }
                String::from_utf8_lossy(&self.s[st..self.pos])
                    .trim()
                    .to_owned()
            }
        };
        self.skip_ws_comments();
        // optional `i`/`s` flag
        let mut ci = false;
        if self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
            ci = self.ident().to_lowercase() == "i";
        }
        self.skip_ws_comments();
        self.skip_to(b']');
        Some(match op {
            AttrOp::Eq if !ci => Simple::AttrEquals(name, v),
            _ => Simple::AttrOp(name, op, v, ci),
        })
    }

    /// Skip up to and including the next `term` byte (or EOF).
    fn skip_to(&mut self, term: u8) {
        while self.peek().is_some_and(|c| c != term) {
            self.pos += 1;
        }
        let _ = self.bump();
    }

    /// `:ident` / `::ident` pseudo. Known `::pseudo-elements` parse as
    /// [`Simple::PseudoElem`]; unknown/dynamic-state pseudos as
    /// [`Simple::Never`].
    fn pseudo_sel(&mut self) -> Option<Simple> {
        self.pos += 1; // ':'
        let dbl = if self.peek() == Some(b':') {
            self.pos += 1;
            true
        } else {
            false
        };
        let name = self.ident().to_lowercase();
        if name.is_empty() {
            return None;
        }
        let sm = if dbl {
            match name.as_str() {
                "before" => Simple::PseudoElem(PseudoElem::Before),
                "after" => Simple::PseudoElem(PseudoElem::After),
                "marker" => Simple::PseudoElem(PseudoElem::Marker),
                "first-letter" => Simple::PseudoElem(PseudoElem::FirstLetter),
                "first-line" => Simple::PseudoElem(PseudoElem::FirstLine),
                "selection" => Simple::PseudoElem(PseudoElem::Selection),
                "placeholder" => Simple::PseudoElem(PseudoElem::Placeholder),
                // ::backdrop, ::-webkit-*, ::part(x)... valid but we
                // generate no pseudo boxes — never matches.
                _ => {
                    self.skip_paren_arg();
                    Simple::Never
                }
            }
        } else {
            match name.as_str() {
                // CSS2 allowed single-colon spellings for these four
                "before" => Simple::PseudoElem(PseudoElem::Before),
                "after" => Simple::PseudoElem(PseudoElem::After),
                "first-letter" => Simple::PseudoElem(PseudoElem::FirstLetter),
                "first-line" => Simple::PseudoElem(PseudoElem::FirstLine),
                "first-child" => Simple::FirstChild,
                "last-child" => Simple::LastChild,
                "root" => Simple::Root,
                // :host/:host-context select a shadow host — we build no
                // shadow trees, so a hoisted <style>'s :host rules must
                // never leak onto the document (real bug: MDN's shadow
                // :host{display:inline-flex} restyled <html>).
                "host" | "host-context" => {
                    self.skip_paren_arg();
                    Simple::Never
                }
                "first-of-type" => Simple::FirstOfType,
                "last-of-type" => Simple::LastOfType,
                "only-child" => Simple::OnlyChild,
                "only-of-type" => Simple::OnlyOfType,
                "empty" => Simple::Empty,
                "checked" => Simple::Checked,
                "disabled" => Simple::Disabled,
                "enabled" => Simple::Enabled,
                "is" => self
                    .paren_selectors(false)
                    .map(Simple::Is)
                    .unwrap_or(Simple::Never),
                "where" => self
                    .paren_selectors(false)
                    .map(Simple::Where)
                    .unwrap_or(Simple::Never),
                "not" => self
                    .paren_selectors(false)
                    .map(Simple::Not)
                    .unwrap_or(Simple::Never),
                "has" => self
                    .paren_selectors(true)
                    .map(Simple::Has)
                    .unwrap_or(Simple::Never),
                "nth-child" | "nth-last-child" | "nth-of-type" | "nth-last-of-type" => {
                    let arg = self.paren_text();
                    match parse_nth(&arg) {
                        Some((a, b)) => match name.as_str() {
                            "nth-child" => Simple::NthChild(a, b),
                            "nth-last-child" => Simple::NthLastChild(a, b),
                            "nth-of-type" => Simple::NthOfType(a, b),
                            _ => Simple::NthLastOfType(a, b),
                        },
                        None => Simple::Never,
                    }
                }
                "hover" => Simple::Hover,
                "focus" => Simple::Focus,
                "focus-within" => Simple::FocusWithin,
                "focus-visible" => Simple::FocusVisible,
                "active" => Simple::Active,
                "link" | "any-link" => Simple::Link,
                "visited" => Simple::Visited,
                "target" => Simple::Target,
                _ => {
                    // remaining functional/dynamic pseudos (:lang(...),
                    // :modal, ...) — valid, but we have no state to match.
                    self.skip_paren_arg();
                    Simple::Never
                }
            }
        };
        Some(sm)
    }

    /// `( sel, sel, ... )` — selector list inside a functional pseudo.
    /// `relative` allows a leading combinator (`:has` arguments).
    fn paren_selectors(&mut self, relative: bool) -> Option<Vec<Selector>> {
        if self.peek() != Some(b'(') {
            return None;
        }
        self.pos += 1;
        let mut list = Vec::new();
        loop {
            self.skip_ws_comments();
            match self.peek() {
                Some(b')') => {
                    self.pos += 1;
                    break;
                }
                None => break,
                _ => {}
            }
            if let Some(sel) = self.complex_selector(relative) {
                list.push(sel);
            } else {
                // unparseable arg — skip to ',' or ')' at this depth
                let mut depth = 0i32;
                while let Some(c) = self.peek() {
                    match c {
                        b'(' => depth += 1,
                        b')' if depth == 0 => break,
                        b')' => depth -= 1,
                        b',' if depth == 0 => break,
                        _ => {}
                    }
                    self.pos += 1;
                }
            }
            self.skip_ws_comments();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b')') => {
                    self.pos += 1;
                    break;
                }
                None => break,
                _ => {
                    // unexpected trailing token — skip one byte to make
                    // progress, then try again
                    self.pos += 1;
                }
            }
        }
        if list.is_empty() {
            None
        } else {
            Some(list)
        }
    }

    /// Raw text inside `( ... )` (balanced), for `:nth-*(An+B)` args.
    fn paren_text(&mut self) -> String {
        if self.peek() != Some(b'(') {
            return String::new();
        }
        self.pos += 1;
        let st = self.pos;
        let mut depth = 1i32;
        while !self.eof() && depth > 0 {
            match self.peek() {
                Some(b'(') => depth += 1,
                Some(b')') => depth -= 1,
                _ => {}
            }
            if depth > 0 {
                self.pos += 1;
            }
        }
        let end = self.pos;
        let _ = self.bump(); // closing ')'
        String::from_utf8_lossy(&self.s[st..end]).into_owned()
    }

    /// Skip a `(...)` arg on an unrecognized functional pseudo.
    fn skip_paren_arg(&mut self) {
        if self.peek() != Some(b'(') {
            return;
        }
        let mut depth = 0i32;
        while !self.eof() {
            match self.bump() {
                Some(b'(') => depth += 1,
                Some(b')') => {
                    depth -= 1;
                    if depth == 0 {
                        return;
                    }
                }
                None => return,
                _ => {}
            }
        }
    }

    fn decl_block(&mut self) -> Vec<Decl> {
        let mut decls = Vec::new();
        loop {
            self.skip_ws_comments();
            match self.peek() {
                Some(b'}') | None => {
                    let _ = self.bump(); // consume '}' only if present
                    break;
                }
                Some(b';') => {
                    self.pos += 1;
                }
                _ => {
                    let prop = self.ident().to_lowercase();
                    if prop.is_empty() {
                        // garbage — skip to ; or }
                        while !self.eof() && !matches!(self.peek(), Some(b';') | Some(b'}')) {
                            self.pos += 1;
                        }
                        continue;
                    }
                    self.skip_ws_comments();
                    if self.peek() == Some(b':') {
                        self.pos += 1;
                    } else {
                        while !self.eof() && !matches!(self.peek(), Some(b';') | Some(b'}')) {
                            self.pos += 1;
                        }
                        continue;
                    }
                    self.skip_ws_comments();
                    // value: to ; or } — handle parens/strings naively
                    let st = self.pos;
                    let mut depth = 0i32;
                    while !self.eof() {
                        match self.peek() {
                            Some(b'(') => depth += 1,
                            Some(b')') => depth -= 1,
                            Some(b';') | Some(b'}') if depth <= 0 => break,
                            _ => {}
                        }
                        self.pos += 1;
                    }
                    let mut value = String::from_utf8_lossy(&self.s[st..self.pos])
                        .trim()
                        .to_string();
                    let important = if value.to_lowercase().ends_with("!important") {
                        value = value[..value.len() - 10].trim().to_string();
                        true
                    } else {
                        false
                    };
                    if !value.is_empty() {
                        decls.push(Decl {
                            prop,
                            value,
                            important,
                        });
                    }
                }
            }
        }
        decls
    }
}

/// Specificity (ids, classes, tags) — used to order equal-priority decls.
/// `:is`/`:not`/`:has` contribute their most specific argument; `:where` 0.
fn specificity(sel: &Selector) -> (u32, u32, u32) {
    let (mut id, mut cls, mut tag) = (0, 0, 0);
    for (_, simples) in &sel.parts {
        for s in simples {
            match s {
                Simple::Id(_) => id += 1,
                Simple::Class(_)
                | Simple::AttrExists(_)
                | Simple::AttrEquals(..)
                | Simple::AttrOp(..)
                | Simple::FirstChild
                | Simple::LastChild
                | Simple::FirstOfType
                | Simple::LastOfType
                | Simple::OnlyChild
                | Simple::OnlyOfType
                | Simple::NthChild(..)
                | Simple::NthLastChild(..)
                | Simple::NthOfType(..)
                | Simple::NthLastOfType(..)
                | Simple::Empty
                | Simple::Checked
                | Simple::Disabled
                | Simple::Enabled
                | Simple::Root
                | Simple::Hover
                | Simple::Focus
                | Simple::FocusWithin
                | Simple::FocusVisible
                | Simple::Active
                | Simple::Link
                | Simple::Visited
                | Simple::Target
                | Simple::Never
                | Simple::Unsupported => cls += 1,
                Simple::Tag(_) | Simple::PseudoElem(_) => tag += 1,
                Simple::Is(l) | Simple::Not(l) | Simple::Has(l) => {
                    if let Some((i, c, t)) = l.iter().map(specificity).max() {
                        id += i;
                        cls += c;
                        tag += t;
                    }
                }
                Simple::Universal | Simple::Nest | Simple::Where(_) => {}
            }
        }
    }
    (id, cls, tag)
}

/// `An+B` microsyntax → (a, b). Accepts `odd`/`even`, `n`, `±An`, `±An±B`,
/// `±B`; an `of <selector>` clause is parsed off but ignored (approximation
/// — we match against the full sibling position).
fn parse_nth(arg: &str) -> Option<(i32, i32)> {
    let a = arg.trim().to_ascii_lowercase();
    let a = a.split(" of ").next().unwrap_or("");
    let a: String = a.chars().filter(|c| !c.is_whitespace()).collect();
    match a.as_str() {
        "odd" => Some((2, 1)),
        "even" => Some((2, 0)),
        _ => {
            if let Some(np) = a.find('n') {
                let (a_s, rest) = a.split_at(np);
                let a = match a_s {
                    "" | "+" => 1,
                    "-" => -1,
                    s => s.parse().ok()?,
                };
                let b_s = &rest[1..];
                let b = if b_s.is_empty() { 0 } else { b_s.parse().ok()? };
                Some((a, b))
            } else {
                a.parse::<i32>().ok().map(|b| (0, b))
            }
        }
    }
}

/// Does 1-based `idx` satisfy `An+B`?
fn nth_matches(idx: usize, a: i32, b: i32) -> bool {
    if idx == 0 {
        return false;
    }
    let i = idx as i32;
    if a == 0 {
        return i == b;
    }
    let d = i - b;
    d % a == 0 && d / a >= 0
}

/// (1-based position of `node` among qualifying element siblings, total
/// count). `of_type` restricts to same-tag siblings; `from_end` counts the
/// position back from the last.
fn elem_pos(doc: &Document, node: NodeId, of_type: bool, from_end: bool) -> (usize, usize) {
    let Some(p) = doc.parent(node) else {
        return (1, 1);
    };
    let tag = doc.tag(node);
    let (mut pos, mut count) = (0usize, 0usize);
    for &c in doc.children(p) {
        if doc.tag(c).is_none() || (of_type && doc.tag(c) != tag) {
            continue;
        }
        count += 1;
        if c == node {
            pos = count;
        }
    }
    let pos = if from_end && pos > 0 {
        count + 1 - pos
    } else {
        pos
    };
    (pos, count)
}

fn prev_elem_sibling(doc: &Document, node: NodeId) -> Option<NodeId> {
    let p = doc.parent(node)?;
    let kids = doc.children(p);
    let pos = kids.iter().position(|&c| c == node)?;
    kids[..pos]
        .iter()
        .rev()
        .copied()
        .find(|&c| doc.tag(c).is_some())
}

fn next_elem_sibling(doc: &Document, node: NodeId) -> Option<NodeId> {
    let p = doc.parent(node)?;
    let kids = doc.children(p);
    let pos = kids.iter().position(|&c| c == node)?;
    kids[pos + 1..]
        .iter()
        .copied()
        .find(|&c| doc.tag(c).is_some())
}

fn attr_op_matches(doc: &Document, node: NodeId, a: &str, op: &AttrOp, v: &str, ci: bool) -> bool {
    let Some(av) = doc.attr(node, a) else {
        return false;
    };
    let (av, v) = if ci {
        (av.to_lowercase(), v.to_lowercase())
    } else {
        (av.to_string(), v.to_string())
    };
    match op {
        AttrOp::Eq => av == v,
        AttrOp::Includes => !v.is_empty() && av.split_whitespace().any(|w| w == v.as_str()),
        AttrOp::DashMatch => av == v || av.starts_with(&format!("{v}-")),
        AttrOp::Prefix => !v.is_empty() && av.starts_with(v.as_str()),
        AttrOp::Suffix => !v.is_empty() && av.ends_with(v.as_str()),
        AttrOp::Substring => !v.is_empty() && av.contains(v.as_str()),
    }
}

/// `node` is `anchor` itself or a strict ancestor of it — the chain
/// relation `:hover`/`:focus-within`/`:active` light up.
fn on_chain(doc: &Document, node: NodeId, anchor: NodeId) -> bool {
    let mut cur = Some(anchor);
    while let Some(c) = cur {
        if c == node {
            return true;
        }
        cur = doc.parent(c);
    }
    false
}

fn simple_matches(doc: &Document, node: NodeId, s: &Simple, st: &PseudoState) -> bool {
    match s {
        Simple::Universal | Simple::Nest => true,
        Simple::Tag(t) => doc.tag(node) == Some(t.as_str()),
        Simple::Class(c) => doc
            .attr(node, "class")
            .is_some_and(|v| v.split_whitespace().any(|x| x == c)),
        Simple::Id(i) => doc.attr(node, "id") == Some(i.as_str()),
        Simple::AttrExists(a) => doc.attr(node, a).is_some(),
        Simple::AttrEquals(a, v) => doc.attr(node, a) == Some(v.as_str()),
        Simple::AttrOp(a, op, v, ci) => attr_op_matches(doc, node, a, op, v, *ci),
        Simple::FirstChild => elem_pos(doc, node, false, false).0 == 1,
        Simple::LastChild => elem_pos(doc, node, false, true).0 == 1,
        Simple::FirstOfType => elem_pos(doc, node, true, false).0 == 1,
        Simple::LastOfType => elem_pos(doc, node, true, true).0 == 1,
        Simple::OnlyChild => elem_pos(doc, node, false, false).1 == 1,
        Simple::OnlyOfType => elem_pos(doc, node, true, false).1 == 1,
        Simple::NthChild(a, b) => nth_matches(elem_pos(doc, node, false, false).0, *a, *b),
        Simple::NthLastChild(a, b) => nth_matches(elem_pos(doc, node, false, true).0, *a, *b),
        Simple::NthOfType(a, b) => nth_matches(elem_pos(doc, node, true, false).0, *a, *b),
        Simple::NthLastOfType(a, b) => nth_matches(elem_pos(doc, node, true, true).0, *a, *b),
        Simple::Empty => doc
            .children(node)
            .iter()
            .all(|&c| doc.tag(c).is_none() && doc.text(c).map_or(true, |t| t.trim().is_empty())),
        Simple::Checked => {
            doc.attr(node, "checked").is_some()
                || (doc.tag(node) == Some("option") && doc.attr(node, "selected").is_some())
        }
        Simple::Disabled => doc.attr(node, "disabled").is_some(),
        Simple::Enabled => {
            matches!(
                doc.tag(node),
                Some(
                    "input" | "button" | "select" | "textarea" | "optgroup" | "option" | "fieldset"
                )
            ) && doc.attr(node, "disabled").is_none()
        }
        Simple::Root => doc.tag(node) == Some("html"),
        Simple::Is(l) | Simple::Where(l) => l.iter().any(|s| matches_st(doc, node, s, st)),
        Simple::Not(l) => !l.iter().any(|s| matches_st(doc, node, s, st)),
        Simple::Has(l) => l.iter().any(|rel| has_match(doc, node, rel, st)),
        Simple::Hover => st.hovered.is_some_and(|h| on_chain(doc, node, h)),
        Simple::Focus | Simple::FocusVisible => st.focused == Some(node),
        Simple::FocusWithin => st.focused.is_some_and(|f| on_chain(doc, node, f)),
        Simple::Active => st.active.is_some_and(|a| on_chain(doc, node, a)),
        Simple::Link => {
            matches!(doc.tag(node), Some("a") | Some("area")) && doc.attr(node, "href").is_some()
        }
        Simple::Visited => false,
        Simple::Target => st
            .target_frag
            .as_deref()
            .is_some_and(|f| !f.is_empty() && doc.attr(node, "id") == Some(f)),
        Simple::Never | Simple::Unsupported | Simple::PseudoElem(_) => false,
    }
}

/// `:has(rel)` — does a relative selector match, anchored on `node`?
fn has_match(doc: &Document, node: NodeId, rel: &Selector, st: &PseudoState) -> bool {
    rel_step(doc, node, rel, 0, st)
}

/// Match relative-selector part `i` against a node reachable from `from`
/// via that part's combinator, then continue left-to-right.
fn rel_step(doc: &Document, from: NodeId, rel: &Selector, i: usize, st: &PseudoState) -> bool {
    if i >= rel.parts.len() {
        return true;
    }
    let (comb, simples) = &rel.parts[i];
    let hit = |c: NodeId| compound_matches(doc, c, simples, st) && rel_step(doc, c, rel, i + 1, st);
    match comb {
        Combinator::Child => doc
            .children(from)
            .iter()
            .any(|&c| doc.tag(c).is_some() && hit(c)),
        Combinator::NextSibling => next_elem_sibling(doc, from).is_some_and(hit),
        Combinator::Sibling => {
            let mut s = next_elem_sibling(doc, from);
            loop {
                match s {
                    Some(c) if hit(c) => return true,
                    Some(c) => s = next_elem_sibling(doc, c),
                    None => return false,
                }
            }
        }
        // Descendant, or the leading-None of an implicit relative selector
        _ => doc
            .descendants(from)
            .any(|c| doc.tag(c).is_some() && hit(c)),
    }
}

/// Cheap bail: the rightmost compound's tag/classes/id must hold on `node`
/// for the selector to match — lets computed() skip most rules without
/// running the full matcher.
fn rightmost_ok(doc: &Document, node: NodeId, s: &Selector) -> bool {
    let Some((_, simples)) = s.parts.last() else {
        return false;
    };
    for sm in simples {
        match sm {
            Simple::Tag(t) => {
                if doc.tag(node) != Some(t.as_str()) {
                    return false;
                }
            }
            Simple::Class(cl) => {
                let has = doc
                    .attr(node, "class")
                    .is_some_and(|a| a.split_whitespace().any(|c| c == cl));
                if !has {
                    return false;
                }
            }
            Simple::Id(i) => {
                if doc.attr(node, "id") != Some(i.as_str()) {
                    return false;
                }
            }
            _ => {}
        }
    }
    true
}

fn compound_matches(doc: &Document, node: NodeId, simples: &[Simple], st: &PseudoState) -> bool {
    simples.iter().all(|s| simple_matches(doc, node, s, st))
}

/// Match a selector against an element node (right-to-left), with no
/// interaction state — `:hover`/`:focus`/... never match here; callers
/// holding a [`PseudoState`] should use `matches_st` (or `StyleCtx`).
pub fn matches(doc: &Document, node: NodeId, sel: &Selector) -> bool {
    matches_st(doc, node, sel, &PseudoState::default())
}

/// Match a selector under an explicit interaction state — same walk as
/// [`matches`], but the dynamic-state simples consult `st`.
fn matches_st(doc: &Document, node: NodeId, sel: &Selector, st: &PseudoState) -> bool {
    let n = sel.parts.len();
    if n == 0 {
        return false;
    }
    let (_, last) = &sel.parts[n - 1];
    if !compound_matches(doc, node, last, st) {
        return false;
    }
    // walk leftward: parts[i].0 is the combinator between compounds i-1
    // and i — it tells how to find the parts[i-1] node from `cur`.
    let mut cur = node;
    for i in (1..n).rev() {
        let comb = sel.parts[i].0;
        let simples = &sel.parts[i - 1].1;
        match comb {
            Combinator::Child => {
                let Some(p) = doc.parent(cur) else {
                    return false;
                };
                if doc.tag(p).is_none() || !compound_matches(doc, p, simples, st) {
                    return false;
                }
                cur = p;
            }
            Combinator::NextSibling => {
                let Some(s) = prev_elem_sibling(doc, cur) else {
                    return false;
                };
                if !compound_matches(doc, s, simples, st) {
                    return false;
                }
                cur = s;
            }
            Combinator::Sibling => {
                let mut s = prev_elem_sibling(doc, cur);
                let found = loop {
                    match s {
                        Some(c) if compound_matches(doc, c, simples, st) => break Some(c),
                        Some(c) => s = prev_elem_sibling(doc, c),
                        None => break None,
                    }
                };
                let Some(c) = found else { return false };
                cur = c;
            }
            _ => {
                let mut p = doc.parent(cur);
                loop {
                    match p {
                        Some(a) => {
                            if doc.tag(a).is_some() && compound_matches(doc, a, simples, st) {
                                cur = a;
                                break;
                            }
                            p = doc.parent(a);
                        }
                        None => return false,
                    }
                }
            }
        }
    }
    true
}

/// Per-document style-resolution context: owns the scratch buffer used for
/// candidate lists and memoizes the `:root` element's custom properties, so
/// repeated `computed` calls over a full tree don't reallocate or re-match.
/// Create once per document + sheet set, reuse for every node.
pub struct StyleCtx<'a> {
    doc: &'a Document,
    sheets: Vec<&'a StyleSheet>,
    buf: Vec<u32>,
    /// html element's `--*` decls, resolved on first var() use.
    root_customs: Option<StrMap<String>>,
    /// Resolved outputs of ancestor elements, memoized for the parent
    /// lookups done by inherit/unset/all:inherit and the inherited-prop
    /// pass. Queried nodes themselves are not stored (layout has its own
    /// Rc'd style_cache).
    node_memo: FxHashMap<NodeId, cascade::NodeOut>,
    /// Interaction state `:hover`/`:focus`/... match against.
    state: PseudoState,
}

/// Cascade ordering of declaration keys (important, origin, layer, spec,
/// order): `true` = `new` supersedes `old`. Normal decls: higher origin,
/// then higher layer index (unlayered = u32::MAX wins), then spec/order.
/// `!important` inverts origin and layer precedence (Cascade 5): earlier
/// origin and earlier layer are stronger.
fn decl_wins(
    new: (bool, u8, u32, (u32, u32, u32), usize),
    old: (bool, u8, u32, (u32, u32, u32), usize),
) -> bool {
    if new.0 != old.0 {
        return new.0;
    }
    if !new.0 {
        (new.1, new.2, new.3, new.4) >= (old.1, old.2, old.3, old.4)
    } else {
        if new.1 != old.1 {
            return new.1 < old.1;
        }
        if new.2 != old.2 {
            return new.2 < old.2;
        }
        (new.3, new.4) >= (old.3, old.4)
    }
}

/// Cascade `sheets` (UA sheet(s) first, then author, in order) for `node`.
/// Returns property → value with inheritance applied for text props.
/// For resolving more than a handful of nodes, prefer `StyleCtx::computed`
/// (this builds a fresh context — and re-resolves :root customs — per call).
pub fn computed(doc: &Document, node: NodeId, sheets: &[&StyleSheet]) -> StrMap<String> {
    StyleCtx::new(doc, sheets).computed(node)
}

impl<'a> StyleCtx<'a> {
    pub fn new(doc: &'a Document, sheets: &[&'a StyleSheet]) -> Self {
        StyleCtx {
            doc,
            sheets: sheets.to_vec(),
            buf: Vec::new(),
            root_customs: None,
            node_memo: FxHashMap::default(),
            state: PseudoState::default(),
        }
    }

    /// Install the interaction state (`:hover`, `:focus`, `:active`,
    /// `:target`) the next `computed` calls match against.
    pub fn set_state(&mut self, state: PseudoState) {
        self.state = state;
    }

    /// The `<html>` element's own custom-property declarations, computed
    /// once and shared read-only thereafter (an empty map if there's no
    /// html element — same as the previous per-node `find_deep` miss).
    fn root_customs(&mut self) -> &StrMap<String> {
        if self.root_customs.is_none() {
            self.root_customs = Some(
                self.doc
                    .find_deep(self.doc.root, "html")
                    .map(|h| element_customs(self.doc, h, &self.sheets))
                    .unwrap_or_default(),
            );
        }
        self.root_customs.as_ref().unwrap()
    }

    /// Cascaded + resolved computed style for `node`: winning declaration
    /// per property, with `var()`/`env()`, `all`, and the CSS-wide keywords
    /// resolved, custom properties inherited down the tree (with
    /// `@property` semantics), and the standard inherited-property set
    /// filled from the parent's computed values. Ancestor results are
    /// memoized in `node_memo` for the parent lookups.
    pub fn computed(&mut self, node: NodeId) -> StrMap<String> {
        if let Some(o) = self.node_memo.get(&node) {
            return o.props.clone();
        }
        self.node_style(node).props
    }
}

/// Logical → physical property expansion (LTR horizontal writing mode):
/// run `f` once per longhand `prop` expands to (identity for most props).
fn each_physical(prop: &str, mut f: impl FnMut(&str)) {
    match prop {
        "padding-inline" => {
            f("padding-left");
            f("padding-right");
        }
        "padding-inline-start" => f("padding-left"),
        "padding-inline-end" => f("padding-right"),
        "padding-block" => {
            f("padding-top");
            f("padding-bottom");
        }
        "padding-block-start" => f("padding-top"),
        "padding-block-end" => f("padding-bottom"),
        "margin-inline" => {
            f("margin-left");
            f("margin-right");
        }
        "margin-inline-start" => f("margin-left"),
        "margin-inline-end" => f("margin-right"),
        "margin-block" => {
            f("margin-top");
            f("margin-bottom");
        }
        "margin-block-start" => f("margin-top"),
        "margin-block-end" => f("margin-bottom"),
        "border-inline" => {
            f("border-left");
            f("border-right");
        }
        "border-block" => {
            f("border-top");
            f("border-bottom");
        }
        "border-inline-color" => f("border-color"),
        "border-block-color" => f("border-color"),
        "border-inline-width" => f("border-width"),
        "border-block-width" => f("border-width"),
        "inset-inline" => {
            f("left");
            f("right");
        }
        "inset-block" => {
            f("top");
            f("bottom");
        }
        "inset-inline-start" => f("left"),
        "inset-inline-end" => f("right"),
        "inset-block-start" => f("top"),
        "inset-block-end" => f("bottom"),
        // logical size aliases (horizontal LTR only — no writing-mode)
        "inline-size" => f("width"),
        "block-size" => f("height"),
        "min-inline-size" => f("min-width"),
        "max-inline-size" => f("max-width"),
        "min-block-size" => f("min-height"),
        "max-block-size" => f("max-height"),
        // logical border-radius corners
        "border-start-start-radius" => f("border-top-left-radius"),
        "border-start-end-radius" => f("border-top-right-radius"),
        "border-end-start-radius" => f("border-bottom-left-radius"),
        "border-end-end-radius" => f("border-bottom-right-radius"),
        p => f(p),
    }
}

/// Whitespace-split a declaration value, keeping `fn(...)` groups whole —
/// `calc(50% - 1em)` and `rgb(0 0 0)` stay single tokens.
fn decl_tokens(v: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut st = 0usize;
    let b = v.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'(' => depth += 1,
            b')' => depth -= 1,
            c if c.is_ascii_whitespace() && depth <= 0 => {
                if st < i {
                    out.push(&v[st..i]);
                }
                st = i + 1;
            }
            _ => {}
        }
    }
    if st < b.len() {
        out.push(&v[st..]);
    }
    out
}

/// Whitespace-separated tokens of a value, keeping `fn(...)` groups whole.
/// A top-level `/` is emitted as its own token (the `background`
/// shorthand's position/size separator).
fn ws_tokens(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut st = 0usize;
    let b = s.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'/' if depth <= 0 => {
                if st < i {
                    out.push(&s[st..i]);
                }
                out.push(&s[i..i + 1]);
                i += 1;
                st = i;
                continue;
            }
            c if (c as char).is_whitespace() && depth <= 0 => {
                if st < i {
                    out.push(&s[st..i]);
                }
                i += 1;
                st = i;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    if st < s.len() {
        out.push(&s[st..]);
    }
    out
}

/// The i-th of a 1–4 value list in the usual corner pattern (tl, tr, br, bl).
fn nth4<'a>(t: &[&'a str], i: usize) -> &'a str {
    match t.len() {
        1 => t[0],
        2 => t[i % 2],
        3 => t[[0, 1, 2, 1][i]],
        _ => t[i.min(3)],
    }
}

/// `border-radius` shorthand → per-corner `border-<corner>-radius` values
/// in `h [v]` form — a `/` splits horizontal from vertical radii, each
/// side taking the usual 1–4 corner pattern (tl, tr, br, bl).
fn radius_corners(value: &str) -> [Option<String>; 4] {
    let (h, v) = match value.find('/') {
        Some(i) => (&value[..i], &value[i + 1..]),
        None => (value, ""),
    };
    let hs = decl_tokens(h);
    let vs = if v.trim().is_empty() {
        hs.clone()
    } else {
        decl_tokens(v)
    };
    let mut out: [Option<String>; 4] = Default::default();
    if hs.is_empty() || vs.is_empty() {
        return out;
    }
    for i in 0..4 {
        let (hc, vc) = (nth4(&hs, i), nth4(&vs, i));
        out[i] = Some(if hc == vc {
            hc.to_string()
        } else {
            format!("{hc} {vc}")
        });
    }
    out
}

/// Does `t` look like a <length> or <percentage> (for shorthand token
/// classification — units aren't validated beyond a numeric prefix)?
fn is_len_or_pct(t: &str) -> bool {
    let t = t.trim().to_ascii_lowercase();
    if t.is_empty() {
        return false;
    }
    if let Some(p) = t.strip_suffix('%') {
        return p.trim().parse::<f32>().is_ok();
    }
    if ["calc(", "min(", "max(", "clamp(", "var("]
        .iter()
        .any(|p| t.starts_with(p))
    {
        return true;
    }
    for suf in [
        "px", "em", "rem", "vw", "vh", "svw", "svh", "dvw", "dvh", "lvw", "lvh", "pt", "pc", "in",
        "cm", "mm", "q", "ex", "ch", "lh", "rlh", "vmin", "vmax",
    ] {
        if let Some(p) = t.strip_suffix(suf) {
            if p.parse::<f32>().is_ok() {
                return true;
            }
        }
    }
    t.parse::<f32>().is_ok()
}

/// `background` shorthand → full longhand expansion (css-backgrounds-3
/// §3.10). Each emitted longhand becomes a normalized comma list aligned
/// to the layer count: a component a layer omits takes that longhand's
/// initial value, and the shorthand resets every longhand (including a
/// `background-color` of `transparent` when the shorthand has none).
fn expand_background(value: &str, f: &mut impl FnMut(&str, &str)) {
    #[derive(Default)]
    struct BgLayer<'a> {
        image: Option<&'a str>,
        pos: Vec<&'a str>,
        size: Vec<&'a str>,
        rep: Vec<&'a str>,
        att: Vec<&'a str>,
        boxes: Vec<&'a str>,
        color: Option<&'a str>,
    }
    let mut layers: Vec<BgLayer> = Vec::new();
    for lay in split_top(value, ",") {
        let mut l = BgLayer::default();
        // after a `/`, the next <bg-size> tokens (max 2: auto, cover,
        // contain, <length-percentage>) belong to background-size; the
        // first non-size token ends size mode and classifies normally
        let mut in_size = false;
        for tok in ws_tokens(lay) {
            if tok == "/" {
                in_size = true;
                continue;
            }
            let tl = tok.to_ascii_lowercase();
            if in_size {
                let is_sz =
                    matches!(tl.as_str(), "auto" | "cover" | "contain") || is_len_or_pct(&tl);
                if is_sz && l.size.len() < 2 {
                    l.size.push(tok);
                    continue;
                }
                in_size = false;
            }
            if tl.starts_with("url(")
                || tl == "none"
                || tl.contains("gradient(")
                || tl.starts_with("image-set(")
                || tl.starts_with("-webkit-image-set(")
                || tl.starts_with("cross-fade(")
                || tl.starts_with("element(")
            {
                l.image = Some(tok);
            } else if matches!(
                tl.as_str(),
                "repeat" | "repeat-x" | "repeat-y" | "no-repeat" | "space" | "round"
            ) {
                l.rep.push(tok);
            } else if matches!(tl.as_str(), "fixed" | "scroll" | "local") {
                l.att.push(tok);
            } else if matches!(
                tl.as_str(),
                "border-box" | "padding-box" | "content-box" | "text"
            ) {
                l.boxes.push(tok);
            } else if matches!(tl.as_str(), "left" | "right" | "top" | "bottom" | "center")
                || is_len_or_pct(&tl)
            {
                l.pos.push(tok);
            } else {
                l.color = Some(tok);
            }
        }
        layers.push(l);
    }
    if layers.is_empty() {
        return;
    }
    // The shorthand prop itself stays for readers that scan the raw value.
    f("background", value);
    let mut images = Vec::new();
    let mut poss = Vec::new();
    let mut poss_x = Vec::new();
    let mut poss_y = Vec::new();
    let mut sizes = Vec::new();
    let mut reps = Vec::new();
    let mut origins = Vec::new();
    let mut clips = Vec::new();
    let mut atts = Vec::new();
    let mut color: Option<&str> = None;
    for l in &layers {
        images.push(l.image.unwrap_or("none").to_string());
        poss.push(if l.pos.is_empty() {
            "0% 0%".to_string()
        } else {
            l.pos.join(" ")
        });
        // split the layer's position tokens into per-axis values for the
        // -x/-y longhands: an edge keyword binds a following length
        let (mut xs, mut ys): (Vec<&str>, Vec<&str>) = (Vec::new(), Vec::new());
        let mut i = 0;
        while i < l.pos.len() {
            let tl = l.pos[i].to_ascii_lowercase();
            match tl.as_str() {
                "left" | "right" => {
                    xs.push(l.pos[i]);
                    if let Some(t2) = l.pos.get(i + 1) {
                        if is_len_or_pct(t2) {
                            xs.push(t2);
                            i += 1;
                        }
                    }
                }
                "top" | "bottom" => {
                    ys.push(l.pos[i]);
                    if let Some(t2) = l.pos.get(i + 1) {
                        if is_len_or_pct(t2) {
                            ys.push(t2);
                            i += 1;
                        }
                    }
                }
                _ => {
                    if xs.is_empty() {
                        xs.push(l.pos[i]);
                    } else {
                        ys.push(l.pos[i]);
                    }
                }
            }
            i += 1;
        }
        poss_x.push(if xs.is_empty() {
            "0%".to_string()
        } else {
            xs.iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        });
        poss_y.push(if ys.is_empty() {
            "center".to_string()
        } else {
            ys.iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        });
        sizes.push(if l.size.is_empty() {
            "auto".to_string()
        } else {
            l.size.join(" ")
        });
        reps.push(if l.rep.is_empty() {
            "repeat".to_string()
        } else {
            l.rep.join(" ")
        });
        atts.push(if l.att.is_empty() {
            "scroll".to_string()
        } else {
            l.att.join(" ")
        });
        origins.push(
            l.boxes
                .first()
                .copied()
                .unwrap_or("padding-box")
                .to_string(),
        );
        clips.push(l.boxes.get(1).copied().unwrap_or("border-box").to_string());
        color = l.color.or(color);
    }
    f("background-image", &images.join(", "));
    f("background-position", &poss.join(", "));
    f("background-position-x", &poss_x.join(", "));
    f("background-position-y", &poss_y.join(", "));
    f("background-size", &sizes.join(", "));
    f("background-repeat", &reps.join(", "));
    f("background-origin", &origins.join(", "));
    f("background-clip", &clips.join(", "));
    f("background-attachment", &atts.join(", "));
    f("background-color", color.unwrap_or("transparent"));
}

/// Expands a declaration into the physical longhands the engine reads, per
/// the cascade: a rule's `margin:0` must beat a *less specific* rule's
/// `margin-left:1.6em` — only per-side expansion makes that possible.
/// `each_physical` maps 1:1 logical → physical; box shorthands
/// (`margin`/`padding`/`inset`/`border-width|style|color`) additionally
/// split their value across the four sides, `border`/`border-<side>` fan
/// their whole token list into the width/style/color longhands (layout
/// picks its component out — the keyword classes are disjoint),
/// `border-radius` becomes the four corner longhands, `outline` its three,
/// `overflow` its two axes. The shorthand prop itself is always kept for
/// direct readers.
fn each_decl(prop: &str, value: &str, mut f: impl FnMut(&str, &str)) {
    if prop == "background" {
        expand_background(value, &mut f);
        return;
    }
    each_physical(prop, |p| expand_decl(p, value, &mut f));
}

/// Physical-property expansion shared by `each_decl` — see its docs.
fn expand_decl(prop: &str, value: &str, f: &mut impl FnMut(&str, &str)) {
    // `-webkit-box` is the legacy flex display (line-clamp idiom) —
    // normalize so every display consumer sees `flex`
    if prop == "display" {
        match value.trim() {
            "-webkit-box" => return f("display", "flex"),
            "-webkit-inline-box" => return f("display", "inline-flex"),
            _ => {}
        }
    }
    if prop == "list-style" {
        // `list-style: <type> <position> <image>` in any order — split into
        // the longhands layout reads (quoted strings can hold spaces;
        // `none` covers both type and image).
        f(prop, value);
        for t in cascade::split_ws_parens(value) {
            match t {
                "inside" | "outside" => f("list-style-position", t),
                "none" => {
                    f("list-style-type", "none");
                    f("list-style-image", "none");
                }
                _ if t.starts_with("url(") => f("list-style-image", t),
                _ => f("list-style-type", t),
            }
        }
        return;
    }
    let sides = match prop {
        "margin" => Some(("margin-top", "margin-right", "margin-bottom", "margin-left")),
        "padding" => Some((
            "padding-top",
            "padding-right",
            "padding-bottom",
            "padding-left",
        )),
        "inset" => Some(("top", "right", "bottom", "left")),
        "border-width" => Some((
            "border-top-width",
            "border-right-width",
            "border-bottom-width",
            "border-left-width",
        )),
        "border-style" => Some((
            "border-top-style",
            "border-right-style",
            "border-bottom-style",
            "border-left-style",
        )),
        "border-color" => Some((
            "border-top-color",
            "border-right-color",
            "border-bottom-color",
            "border-left-color",
        )),
        _ => None,
    };
    if let Some((t, r, b, l)) = sides {
        let v: Vec<&str> = cascade::split_ws_parens(value);
        let vt = v.first().copied().unwrap_or("0");
        let vr = v.get(1).copied().unwrap_or(vt);
        let vb = v.get(2).copied().unwrap_or(vt);
        let vl = v.get(3).copied().unwrap_or(vr);
        f(prop, value);
        f(t, vt);
        f(r, vr);
        f(b, vb);
        f(l, vl);
        return;
    }
    if prop == "flex" {
        // `flex` shorthand → grow/shrink/basis longhands at this decl's
        // cascade position, so a less specific `flex:1` can't beat a more
        // specific `flex-grow`. Keywords: none→0 0 auto, auto→1 1 auto,
        // initial→0 1 auto. Otherwise number tokens fill grow then shrink,
        // a non-number token is the basis; omitted components default to
        // grow=1, shrink=1, basis=0% (spec shorthand defaults).
        let v = value.trim();
        let (g, s, b) = match v {
            "none" => ("0", "0", "auto"),
            "auto" => ("1", "1", "auto"),
            "initial" => ("0", "1", "auto"),
            _ => {
                let (mut g, mut s, mut b) = ("1", "1", "0%");
                let (mut gseen, mut bseen) = (false, false);
                for t in v.split_whitespace() {
                    if t.parse::<f32>().is_ok() {
                        if !gseen {
                            g = t;
                            gseen = true;
                        } else {
                            s = t;
                        }
                    } else {
                        b = t;
                        bseen = true;
                    }
                }
                if !bseen && gseen {
                    b = "0%"; // `flex:<n>` → `n 1 0%`
                }
                (g, s, b)
            }
        };
        f(prop, value);
        f("flex-grow", g);
        f("flex-shrink", s);
        f("flex-basis", b);
        return;
    }
    match prop {
        // `border` resets every longhand of all four sides.
        "border" => {
            f("border", value);
            for side in ["top", "right", "bottom", "left"] {
                f(&format!("border-{side}-width"), value);
                f(&format!("border-{side}-style"), value);
                f(&format!("border-{side}-color"), value);
            }
        }
        "border-top" | "border-right" | "border-bottom" | "border-left" => {
            f(prop, value);
            f(&format!("{prop}-width"), value);
            f(&format!("{prop}-style"), value);
            f(&format!("{prop}-color"), value);
        }
        "border-radius" => {
            f("border-radius", value);
            let c = radius_corners(value);
            const CORNERS: [&str; 4] = [
                "border-top-left-radius",
                "border-top-right-radius",
                "border-bottom-right-radius",
                "border-bottom-left-radius",
            ];
            for i in 0..4 {
                if let Some(v) = &c[i] {
                    f(CORNERS[i], v);
                }
            }
        }
        "outline" => {
            f("outline", value);
            f("outline-width", value);
            f("outline-style", value);
            f("outline-color", value);
        }
        "overflow" => {
            f("overflow", value);
            f("overflow-x", value);
            f("overflow-y", value);
        }
        "gap" => {
            f("gap", value);
            let mut it = value.split_whitespace();
            let rg = it.next().unwrap_or("normal");
            let cg = it.next().unwrap_or(rg);
            f("row-gap", rg);
            f("column-gap", cg);
        }
        "place-items" => {
            f("place-items", value);
            let mut it = value.split_whitespace();
            let av = it.next().unwrap_or("normal");
            let j = it.next().unwrap_or(av);
            f("align-items", av);
            f("justify-items", j);
        }
        "place-content" => {
            f("place-content", value);
            let mut it = value.split_whitespace();
            let av = it.next().unwrap_or("normal");
            let j = it.next().unwrap_or(av);
            f("align-content", av);
            f("justify-content", j);
        }
        "place-self" => {
            f("place-self", value);
            let mut it = value.split_whitespace();
            let av = it.next().unwrap_or("auto");
            let j = it.next().unwrap_or(av);
            f("align-self", av);
            f("justify-self", j);
        }
        "grid-template" => {
            f("grid-template", value);
            let (rows, cols, areas) = grid_template_parts(value);
            f("grid-template-rows", &rows);
            f("grid-template-columns", &cols);
            f("grid-template-areas", &areas);
        }
        "grid" => {
            f("grid", value);
            for (p, v) in expand_grid(value) {
                f(p, &v);
            }
        }
        _ => f(prop, value),
    }
}

/// `grid-template: <rows> [/ <cols>]` → (row track list, col track list,
/// areas). Row segments interleave `'name'` area strings with track sizes;
/// the names are re-quoted into a `grid-template-areas` value. Missing
/// sides report `none` — a shorthand resets them.
fn grid_template_parts(value: &str) -> (String, String, String) {
    let (rows, cols) = value.split_once('/').unwrap_or((value, ""));
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
    let tracks = tracks.trim();
    let rows = if tracks.is_empty() { "none" } else { tracks }.to_string();
    let cols = cols.trim();
    let cols = if cols.is_empty() { "none" } else { cols }.to_string();
    let areas = if names.is_empty() {
        "none".to_string()
    } else {
        names
            .iter()
            .map(|n| format!("'{n}'"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    (rows, cols, areas)
}

/// `grid` shorthand → longhand pairs. Forms: `none`, the `grid-template`
/// form (rows [/ cols], area strings interleaved in rows), and the
/// auto-flow forms — `grid: <rows> / auto-flow <cols>` sets column flow
/// and `grid: auto-flow <rows> / <cols>` sets row flow — `dense` may sit
/// on either side of `auto-flow`.
fn expand_grid(value: &str) -> Vec<(&'static str, String)> {
    let v = value.trim();
    let mut out: Vec<(&'static str, String)> = Vec::new();
    if v.is_empty() || v == "none" {
        out.push(("grid-template-rows", "none".to_string()));
        out.push(("grid-template-columns", "none".to_string()));
        out.push(("grid-template-areas", "none".to_string()));
        return out;
    }
    if !v.contains("auto-flow") {
        let (rows, cols, areas) = grid_template_parts(v);
        out.push(("grid-template-rows", rows));
        out.push(("grid-template-columns", cols));
        out.push(("grid-template-areas", areas));
        return out;
    }
    // auto-flow forms: the side of `/` carrying `auto-flow` becomes the
    // flow axis' auto track list; the other side is the explicit template
    let (left, right) = v.split_once('/').unwrap_or((v, ""));
    for (i, side) in [left, right].into_iter().enumerate() {
        let toks: Vec<&str> = side.split_whitespace().collect();
        if toks.iter().any(|t| *t == "auto-flow") {
            let dense = toks.iter().any(|t| *t == "dense");
            let tracks = toks
                .iter()
                .copied()
                .filter(|t| *t != "auto-flow" && *t != "dense")
                .collect::<Vec<_>>()
                .join(" ");
            let dir = if i == 0 { "row" } else { "column" };
            out.push((
                "grid-auto-flow",
                if dense {
                    format!("{dir} dense")
                } else {
                    dir.to_string()
                },
            ));
            out.push((
                if i == 0 {
                    "grid-auto-rows"
                } else {
                    "grid-auto-columns"
                },
                tracks,
            ));
            // the template on the flow axis is empty
            out.push((
                if i == 0 {
                    "grid-template-rows"
                } else {
                    "grid-template-columns"
                },
                "none".to_string(),
            ));
        } else if i == 0 {
            // row template may interleave 'area' strings with track sizes
            let (rows, _, areas) = grid_template_parts(&format!("{side} /"));
            out.push(("grid-template-rows", rows));
            out.push(("grid-template-areas", areas));
        } else {
            out.push((
                "grid-template-columns",
                if side.trim().is_empty() {
                    "none".to_string()
                } else {
                    side.trim().to_string()
                },
            ));
        }
    }
    out
}

/// Custom-property declarations (--x) matching this element.
fn element_customs(doc: &Document, node: NodeId, sheets: &[&StyleSheet]) -> StrMap<String> {
    let mut out = StrMap::default();
    let mut buf = Vec::new();
    for sh in sheets {
        sh.candidates_into(doc, node, &mut buf);
        for &ri in &buf {
            let r = &sh.rules[ri as usize];
            if !r
                .selectors
                .iter()
                .any(|s| rightmost_ok(doc, node, s) && matches(doc, node, s))
            {
                continue;
            }
            for d in &r.decls {
                if d.prop.starts_with("--") {
                    out.insert(d.prop.clone(), d.value.clone());
                }
            }
        }
    }
    out
}

/// Parse an element `style` attribute into declarations: `prop:value` pairs
/// split on top-level `;` (parens protect `;` inside functions), with the
/// `!important` suffix stripped into the flag.
fn inline_decls(style: &str) -> Vec<Decl> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<Decl>| {
        let part = cur.trim().to_string();
        cur.clear();
        if let Some((p, v)) = part.split_once(':') {
            let mut value = v.trim().to_string();
            let important = if value.to_lowercase().ends_with("!important") {
                value = value[..value.len() - 10].trim().to_string();
                true
            } else {
                false
            };
            let prop = p.trim().to_lowercase();
            if !prop.is_empty() && !value.is_empty() {
                out.push(Decl {
                    prop,
                    value,
                    important,
                });
            }
        }
    };
    for ch in style.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            ';' if depth <= 0 => {
                flush(&mut cur, &mut out);
                continue;
            }
            _ => {}
        }
        cur.push(ch);
    }
    flush(&mut cur, &mut out);
    out
}

/// Substitute `var(--name)`/`var(--name, fallback)`/`env()` recursively —
/// delegates to the cycle-safe resolver; on guaranteed-invalid the raw
/// text is left in place (downstream parsers treat it as unresolvable).
fn resolve_vars(value: &str, customs: &StrMap<String>) -> String {
    cascade::VarResolver::new(customs)
        .subst(value)
        .unwrap_or_else(|| value.to_string())
}

/// Collect <style> element texts from the document → parsed sheets.
pub fn document_sheets(doc: &Document) -> Vec<StyleSheet> {
    document_sheets_vw(doc, 1024.0)
}

/// Like `document_sheets` but evaluates @media against `viewport_w` px.
pub fn document_sheets_vw(doc: &Document, viewport_w: f32) -> Vec<StyleSheet> {
    let mut sheets = Vec::new();
    for id in doc.descendants(doc.root) {
        if doc.tag(id) == Some("style") {
            let text: String = doc
                .children(id)
                .iter()
                .filter_map(|&c| doc.text(c))
                .collect();
            sheets.push(parse_vw(&text, viewport_w));
        }
    }
    sheets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rules() {
        let s = parse("h1 { color: red; font-size: 32px } p, .x { margin: 1em } /* c */");
        assert_eq!(s.rules.len(), 2);
        assert_eq!(s.rules[0].decls[0].prop, "color");
        assert_eq!(s.rules[0].decls[0].value, "red");
    }

    #[test]
    fn important_flag() {
        let s = parse("p { color: blue !important }");
        assert!(s.rules[0].decls[0].important);
        assert_eq!(s.rules[0].decls[0].value, "blue");
    }

    #[test]
    fn selector_matching() {
        let doc = rowser_html::parse(
            "<body><div id=a class='x y'><p><b>deep</b></p><p class=x>two</p></div></body>",
        );
        let b = doc.find_deep(doc.root, "b").unwrap();
        let sheet = parse("div p b { color: red } div > p { x: 1 } .y .x { z: 2 }");
        assert!(matches(&doc, b, &sheet.rules[0].selectors[0]));
        let p2 = doc.find_deep(doc.root, "p").unwrap();
        assert!(matches(&doc, p2, &sheet.rules[1].selectors[0]));
        let css = parse("div.x#a { k: v }");
        let div = doc.find_deep(doc.root, "div").unwrap();
        assert!(matches(&doc, div, &css.rules[0].selectors[0]));
    }

    #[test]
    fn cascade_specificity() {
        let doc = rowser_html::parse("<body><p id=a class=x>hi</p></body>");
        let p = doc.find_deep(doc.root, "p").unwrap();
        let ua = parse("p { color: gray }");
        let author = parse("p { color: blue } .x { color: green } #a { color: red }");
        let c = computed(&doc, p, &[&ua, &author]);
        assert_eq!(c.get("color").unwrap(), "red");
    }

    #[test]
    fn inline_style_attr_wins() {
        let doc = rowser_html::parse("<body><p style=\"color:green\">hi</p></body>");
        let p = doc.find_deep(doc.root, "p").unwrap();
        let author = parse("p { color: red }");
        let c = computed(&doc, p, &[&author]);
        assert_eq!(c.get("color").unwrap(), "green");
        // ...but author !important still beats a normal inline decl
        let important = parse("p { color: blue !important }");
        let c = computed(&doc, p, &[&important]);
        assert_eq!(c.get("color").unwrap(), "blue");
    }

    #[test]
    fn cascade_important() {
        let doc = rowser_html::parse("<body><p id=a>hi</p></body>");
        let p = doc.find_deep(doc.root, "p").unwrap();
        let ua = parse("p { color: red !important }");
        let author = parse("#a { color: blue }");
        let c = computed(&doc, p, &[&ua, &author]);
        assert_eq!(c.get("color").unwrap(), "red");
    }
}

/// `prefers-color-scheme` resolution: dark when `ROWSER_COLOR_SCHEME=dark`
/// (or `ROWSER_DARK=1/true`), light otherwise.
fn dark_mode() -> bool {
    std::env::var("ROWSER_COLOR_SCHEME").map_or(false, |v| v.eq_ignore_ascii_case("dark"))
        || std::env::var("ROWSER_DARK").map_or(false, |v| v == "1" || v == "true")
}

/// Split `q` on `sep` at paren depth 0.
fn split_top<'a>(q: &'a str, sep: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let (mut depth, mut st) = (0i32, 0usize);
    let b = q.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ => {}
        }
        if depth == 0 && b[i..].starts_with(sep.as_bytes()) {
            out.push(&q[st..i]);
            i += sep.len();
            st = i;
            continue;
        }
        i += 1;
    }
    out.push(&q[st..]);
    out
}

/// Assumed viewport height for `height` media features (no ctx plumbed).
const VH: f32 = 768.0;

/// CSS length → px for media evaluation (rem/em = 16px root font).
fn css_len_px(t: &str) -> Option<f32> {
    let t = t.trim();
    for (suf, mul) in [("rem", 16.0f32), ("em", 16.0), ("px", 1.0)] {
        if let Some(num) = t.strip_suffix(suf) {
            return num.trim().parse::<f32>().ok().map(|v| v * mul);
        }
    }
    t.parse::<f32>().ok()
}

/// One media-feature paren's inner text → bool. Handles `name: value`
/// conditions, bare names, and range syntax `(400px <= width <= 800px)`.
fn media_feature(f: &str, vw: f32, dark: bool) -> bool {
    let f = f.trim();
    if f.contains('<') || f.contains('>') {
        return media_range(f, vw);
    }
    if let Some((name, val)) = f.split_once(':') {
        let (name, val) = (name.trim(), val.trim());
        let px = || css_len_px(val);
        return match name {
            "min-width" => px().is_some_and(|v| vw >= v),
            "max-width" => px().is_some_and(|v| vw <= v),
            "width" => px().is_some_and(|v| (vw - v).abs() < 0.5),
            "min-height" => px().is_some_and(|v| VH >= v),
            "max-height" => px().is_some_and(|v| VH <= v),
            "height" => px().is_some_and(|v| (VH - v).abs() < 0.5),
            "prefers-color-scheme" => (val == "dark") == dark,
            "prefers-reduced-motion" => val == "no-preference",
            "prefers-reduced-transparency" | "inverted-colors" | "forced-colors" => {
                val.starts_with("no")
            }
            "orientation" => {
                if val == "portrait" {
                    VH >= vw
                } else {
                    vw >= VH
                }
            }
            "grid" | "monochrome" => val == "0",
            "scripting" => val == "enabled" || val == "initial-only",
            "hover" | "any-hover" => val == "hover",
            "pointer" | "any-pointer" => val == "fine",
            // lenient: unknown features resolve true so their rules apply
            _ => cascade::media_extra(name, val, vw),
        };
    }
    // bare feature name: truthiness — most are "capability exists" → true
    !matches!(
        f,
        "monochrome" | "grid" | "print" | "forced-colors" | "inverted-colors" | "color-index"
    )
}

/// `(width >= 400px)` / `(400px <= width <= 800px)` range comparisons;
/// comparison ops may be unspaced (`width>=400px`).
fn media_range(f: &str, vw: f32) -> bool {
    // space out <, >, = so unspaced ops tokenize
    let mut spaced = String::new();
    for ch in f.trim().chars() {
        if matches!(ch, '<' | '>' | '=') {
            spaced.push(' ');
            spaced.push(ch);
            spaced.push(' ');
        } else {
            spaced.push(ch);
        }
    }
    // merge adjacent op chars (`<` + `=` → `<=`)
    let mut toks: Vec<String> = Vec::new();
    for t in spaced.split_whitespace() {
        let is_op = matches!(t, "<" | ">" | "=");
        let prev_op = toks
            .last()
            .is_some_and(|l: &String| matches!(l.as_str(), "<" | ">" | "="));
        if is_op && prev_op {
            toks.last_mut().unwrap().push_str(t);
        } else {
            toks.push(t.to_string());
        }
    }
    let Some(wpos) = toks.iter().position(|t| {
        matches!(
            t.as_str(),
            "width"
                | "height"
                | "min-width"
                | "max-width"
                | "min-height"
                | "max-height"
                | "aspect-ratio"
                | "min-aspect-ratio"
                | "max-aspect-ratio"
                | "resolution"
                | "min-resolution"
                | "max-resolution"
                | "color"
                | "min-color"
                | "max-color"
                | "color-index"
                | "min-color-index"
                | "max-color-index"
                | "monochrome"
                | "min-monochrome"
                | "max-monochrome"
        )
    }) else {
        return true;
    };
    let feat = toks[wpos].as_str();
    let (dim, px): (f32, fn(&String) -> Option<f32>) = if feat.contains("aspect-ratio") {
        (vw / VH, |t| cascade::parse_ratio(t))
    } else if feat.contains("resolution") {
        (1.0, |t| cascade::parse_dppx(t))
    } else if feat.contains("color-index") || feat.contains("monochrome") {
        (0.0, |t| t.trim().parse::<f32>().ok())
    } else if feat.contains("color") {
        (8.0, |t| t.trim().parse::<f32>().ok())
    } else if feat.contains("height") {
        (VH, |t| css_len_px(t))
    } else {
        (vw, |t| css_len_px(t))
    };
    let cmp = |a: f32, op: &str, b: f32| match op {
        "<" => a < b,
        "<=" => a <= b,
        ">" => a > b,
        ">=" => a >= b,
        "=" | "==" => (a - b).abs() < 0.5,
        _ => true,
    };
    let mut ok = true;
    // `value op name`
    if wpos >= 2 {
        if let Some(v) = px(&toks[wpos - 2]) {
            ok &= cmp(v, &toks[wpos - 1], dim);
        }
    }
    // `name op value`
    if wpos + 2 < toks.len() {
        if let Some(v) = px(&toks[wpos + 2]) {
            ok &= cmp(dim, &toks[wpos + 1], v);
        }
    }
    ok
}

/// One media query's condition (`a and b` → all).
fn media_and(q: &str, vw: f32, dark: bool) -> bool {
    split_top(q, " and ")
        .iter()
        .all(|p| media_atom(p, vw, dark))
}

/// One media query's condition (`a or b` → any).
fn media_or(q: &str, vw: f32, dark: bool) -> bool {
    split_top(q, " or ").iter().any(|p| media_and(p, vw, dark))
}

/// A media-query atom: parenthesized feature (possibly nested boolean) or
/// a bare media type (`screen`, `print`, ...).
fn media_atom(a: &str, vw: f32, dark: bool) -> bool {
    let a = a.trim();
    if let Some(inner0) = a.strip_prefix('(') {
        let inner = inner0.strip_suffix(')').unwrap_or(inner0).trim();
        // nested boolean algebra inside parens
        if split_top(inner, " or ").len() > 1 {
            return media_or(inner, vw, dark);
        }
        if split_top(inner, " and ").len() > 1 {
            return media_and(inner, vw, dark);
        }
        if let Some(rest) = inner.strip_prefix("not ") {
            return !media_atom(rest, vw, dark);
        }
        return media_feature(inner, vw, dark);
    }
    if let Some(rest) = a.strip_prefix("not ") {
        return !media_atom(rest, vw, dark);
    }
    // media type token
    match a {
        "print" | "speech" | "aural" | "braille" | "embossed" | "handheld" | "projection"
        | "tty" | "tv" => false,
        // all/screen/unknown types — lenient true
        _ => true,
    }
}

/// Full media query list: `not`/`only` prefixes, `,` = or.
fn media_matches(q: &str, vw: f32, dark: bool) -> bool {
    let q = q.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    split_top(&q, ",").iter().any(|one| {
        let mut one = one.trim();
        let mut negate = false;
        loop {
            if let Some(r) = one.strip_prefix("not ") {
                negate = !negate;
                one = r.trim();
            } else if let Some(r) = one.strip_prefix("only ") {
                one = r.trim();
            } else {
                break;
            }
        }
        let ok = media_or(one, vw, dark);
        if negate {
            !ok
        } else {
            ok
        }
    })
}

/// @supports evaluator: the boolean structure (`and`/`or`/`not`, nested
/// parens) is evaluated properly. A `(prop: value)` leaf counts as
/// supported when layout knows the prop, it's vendor-prefixed (`-webkit-`,
/// `-moz-` — we pose as a modern engine; decls we don't implement are
/// inert anyway), or it's a custom property. Plain unknown props are
/// unsupported — which is what lets `not (some-prop)` guards pass.
fn supports_matches(q: &str) -> bool {
    supports_or(&q.trim().to_lowercase())
}

fn supports_or(q: &str) -> bool {
    split_top(q, " or ").iter().any(|s| supports_and(s))
}

fn supports_and(q: &str) -> bool {
    split_top(q, " and ").iter().all(|s| supports_atom(s))
}

fn supports_atom(a: &str) -> bool {
    let a = a.trim();
    if let Some(rest) = a.strip_prefix("not ") {
        return !supports_atom(rest);
    }
    if let Some(rest) = a.strip_prefix('(') {
        // unwrap one balanced paren layer; inner re-enters boolean eval
        let mut depth = 0i32;
        let mut inner = rest;
        for (i, c) in rest.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    if depth == 0 {
                        inner = &rest[..i];
                        break;
                    }
                    depth -= 1;
                }
                _ => {}
            }
        }
        return supports_or(inner);
    }
    supports_leaf(a)
}

/// `(prop: value)` leaf test — see [`supports_matches`] for the policy.
fn supports_leaf(inner: &str) -> bool {
    const KNOWN: &[&str] = &[
        "display",
        "color",
        "background",
        "background-color",
        "font-size",
        "font-weight",
        "font-style",
        "font-family",
        "text-decoration",
        "text-align",
        "white-space",
        "vertical-align",
        "margin",
        "margin-top",
        "margin-right",
        "margin-bottom",
        "margin-left",
        "padding",
        "padding-top",
        "padding-right",
        "padding-bottom",
        "padding-left",
        "border",
        "width",
        "height",
        "line-height",
        "float",
        "gap",
        "border-collapse",
        "grid-template-columns",
        "grid-template-rows",
        "grid-column",
        "grid-row",
        "grid-column-start",
        "grid-column-end",
        "grid-row-start",
        "grid-row-end",
        "grid-auto-flow",
        "grid-auto-rows",
        "grid-auto-columns",
        "row-gap",
        "column-gap",
        "place-items",
        "place-content",
        "place-self",
        "align-items",
        "justify-items",
        "align-content",
        "justify-content",
        "align-self",
        "justify-self",
        "aspect-ratio",
        "object-fit",
        "object-position",
        "background-image",
        "background-position",
        "background-position-x",
        "background-position-y",
        "background-size",
        "background-repeat",
        "background-origin",
        "background-clip",
        "background-attachment",
        "grid",
        "grid-template",
        "grid-template-areas",
        "grid-area",
        "grid-template-columns",
        "grid-template-rows",
        "grid-column",
        "grid-row",
        "grid-column-start",
        "grid-column-end",
        "grid-row-start",
        "grid-row-end",
        "grid-auto-flow",
        "grid-auto-rows",
        "grid-auto-columns",
        "row-gap",
        "column-gap",
        "place-items",
        "place-content",
        "place-self",
        "align-items",
        "justify-items",
        "align-content",
        "justify-content",
        "align-self",
        "justify-self",
        "aspect-ratio",
    ];
    match inner.trim().split_once(':') {
        Some((prop, _)) => {
            let p = prop.trim();
            !p.is_empty() && (p.starts_with('-') || KNOWN.contains(&p))
        }
        // no `prop: value` shape (e.g. selector()/font-tech() leaf) — lenient
        None => true,
    }
}

#[cfg(test)]
mod sel4 {
    use super::*;
    use rowser_dom::{Document, NodeId};

    fn sel(css: &str) -> Selector {
        let mut sh = parse(&format!("{css} {{ k: v }}"));
        sh.rules.remove(0).selectors.remove(0)
    }

    fn all(doc: &Document, tag: &str) -> Vec<NodeId> {
        doc.descendants(doc.root)
            .filter(|&n| doc.tag(n) == Some(tag))
            .collect()
    }

    #[test]
    fn is_where_not() {
        let doc = rowser_html::parse(
            "<body><div class=x><p id=one></p><p id=two class=y></p><span></span></div></body>",
        );
        let ps = all(&doc, "p");
        let (one, two) = (ps[0], ps[1]);
        let span = doc.find_deep(doc.root, "span").unwrap();
        assert!(matches(&doc, one, &sel(":is(p,b)")));
        assert!(!matches(&doc, span, &sel(":is(p,b)")));
        assert!(matches(&doc, two, &sel(":is(.y, #nope)")));
        assert!(matches(&doc, one, &sel(":where(p)")));
        assert!(matches(&doc, span, &sel(":where(span)")));
        assert!(matches(&doc, one, &sel("p:not(.y)")));
        assert!(!matches(&doc, two, &sel("p:not(.y)")));
        // :not with a selector LIST (Selectors 4)
        assert!(matches(&doc, one, &sel(":not(.y, #two)")));
        assert!(!matches(&doc, two, &sel(":not(.y, #two)")));
        // nested functional pseudo: :is inside :not
        assert!(matches(&doc, one, &sel("p:not(:is(.y,.z))")));
        // complex selector inside :is — combinators work
        assert!(matches(&doc, two, &sel(":is(.x > p)")));
        // specificity: :is contributes max-arg (even the non-matching one),
        // :where contributes 0
        let sheet = parse(":is(#zz, .y) { a: is } .y { a: cls } :where(#two) { a: whr }");
        let c = computed(&doc, two, &[&sheet]);
        assert_eq!(c.get("a").unwrap(), "is"); // :is(#zz) → id spec wins
        let sheet = parse(":where(#two) { a: whr } .y { a: cls }");
        let c = computed(&doc, two, &[&sheet]);
        assert_eq!(c.get("a").unwrap(), "cls"); // :where zero → .y wins
    }

    #[test]
    fn has_relational() {
        let doc = rowser_html::parse(
            "<body><div id=a><p class=x></p></div><div id=b><p></p></div>\
             <div id=c><i></i><p class=x></p></div><span id=s></span><span class=x></span></body>",
        );
        let divs = all(&doc, "div");
        let (a, b, c) = (divs[0], divs[1], divs[2]);
        // :has(> .x): direct child .x — a and c qualify, b doesn't
        assert!(matches(&doc, a, &sel("div:has(> .x)")));
        assert!(!matches(&doc, b, &sel("div:has(> .x)")));
        assert!(matches(&doc, c, &sel("div:has(> .x)")));
        // :has(.x): any descendant
        assert!(matches(&doc, a, &sel("div:has(.x)")));
        assert!(!matches(&doc, b, &sel("div:has(.x)")));
        // :has(> p.x) — compound after combinator
        assert!(matches(&doc, a, &sel("div:has(> p.x)")));
        // :has(+ .x) — next sibling has .x (span#s's next sibling is span.x)
        let s = doc.find_deep(doc.root, "span").unwrap();
        assert!(matches(&doc, s, &sel("span:has(+ .x)")));
        assert!(!matches(&doc, a, &sel("div:has(+ .x)")));
        // :has(~ span.x) — subsequent sibling (divs' later siblings are spans)
        assert!(matches(&doc, c, &sel("div:has(~ span.x)")));
        assert!(!matches(&doc, a, &sel("div:has(~ .zz)")));
    }

    #[test]
    fn attr_operators() {
        let doc = rowser_html::parse(
            "<body><a class='XY zz' href='HTTPS://X.com' data-lang=en-US></a></body>",
        );
        let a = doc.find_deep(doc.root, "a").unwrap();
        assert!(matches(&doc, a, &sel("[class]")));
        assert!(matches(&doc, a, &sel("[class*=x i]")));
        assert!(!matches(&doc, a, &sel("[class*=x]")));
        assert!(matches(&doc, a, &sel("[class~=zz]")));
        assert!(!matches(&doc, a, &sel("[class~=y]")));
        assert!(matches(&doc, a, &sel("[data-lang|=en]")));
        assert!(!matches(&doc, a, &sel("[data-lang|=US]")));
        assert!(matches(&doc, a, &sel("[href^=https i]")));
        assert!(matches(&doc, a, &sel("[href$=COM i]")));
        assert!(matches(&doc, a, &sel("[href*=://x. i]")));
        assert!(matches(&doc, a, &sel("[class=\"XY zz\"]")));
        assert!(matches(&doc, a, &sel("[class=\"xy zz\" i]")));
    }

    #[test]
    fn sibling_combinators() {
        let doc = rowser_html::parse(
            "<body><ul><li id=l1></li><li id=l2></li><b></b><li id=l3></li></ul></body>",
        );
        let lis = all(&doc, "li");
        let (l1, l2, l3) = (lis[0], lis[1], lis[2]);
        assert!(matches(&doc, l2, &sel("li + li")));
        assert!(!matches(&doc, l1, &sel("li + li")));
        assert!(!matches(&doc, l3, &sel("li + li"))); // prev sibling is <b>
        assert!(matches(&doc, l3, &sel("b + li")));
        assert!(matches(&doc, l2, &sel("li ~ li")));
        assert!(matches(&doc, l3, &sel("li ~ li"))); // li l1/l2 precede
        assert!(!matches(&doc, l1, &sel("li ~ li")));
        assert!(matches(&doc, l3, &sel("ul > b + li")));
    }

    #[test]
    fn structural_pseudos() {
        let doc = rowser_html::parse(
            "<body><ul><li></li><li></li><li></li><li></li></ul>\
             <p></p><p></p><input type=checkbox checked><input type=text>\
             <div><em>only</em></div></body>",
        );
        let lis = all(&doc, "li");
        assert!(matches(&doc, lis[0], &sel("li:nth-child(2n+1)")));
        assert!(!matches(&doc, lis[1], &sel("li:nth-child(2n+1)")));
        assert!(matches(&doc, lis[2], &sel("li:nth-child(odd)")));
        assert!(matches(&doc, lis[3], &sel("li:nth-child(even)")));
        assert!(matches(&doc, lis[3], &sel("li:nth-last-child(1)")));
        assert!(matches(&doc, lis[0], &sel("li:nth-last-child(4)")));
        assert!(matches(&doc, lis[0], &sel("li:first-child")));
        assert!(matches(&doc, lis[3], &sel("li:last-child")));
        let ps = all(&doc, "p");
        assert!(matches(&doc, ps[0], &sel("p:first-of-type")));
        assert!(matches(&doc, ps[1], &sel("p:last-of-type")));
        assert!(!matches(&doc, ps[0], &sel("p:last-of-type")));
        assert!(matches(&doc, ps[1], &sel("p:nth-of-type(2)")));
        assert!(matches(&doc, ps[0], &sel("p:nth-last-of-type(2)")));
        let em = doc.find_deep(doc.root, "em").unwrap();
        assert!(matches(&doc, em, &sel("em:only-child")));
        assert!(matches(&doc, em, &sel("em:only-of-type")));
        let inputs = all(&doc, "input");
        assert!(matches(&doc, inputs[0], &sel("input:checked")));
        assert!(!matches(&doc, inputs[1], &sel("input:checked")));
        assert!(matches(&doc, inputs[1], &sel("input:enabled")));
        assert!(!matches(&doc, inputs[1], &sel("input:disabled")));
        let lis2 = all(&doc, "li");
        assert!(matches(&doc, lis2[0], &sel("li:empty")));
        assert!(!matches(&doc, em, &sel("em:empty")));
    }

    #[test]
    fn dark_variant_and_host() {
        // Tailwind dark: → `&:where(.dark,.dark *)` — & matches any element
        let doc = rowser_html::parse("<html class=dark><body><p></p></body></html>");
        let p = doc.find_deep(doc.root, "p").unwrap();
        let html = doc.find_deep(doc.root, "html").unwrap();
        let s = sel("&:where(.dark,.dark *)");
        assert!(matches(&doc, p, &s));
        assert!(matches(&doc, html, &s));
        let s2 = sel(":where(.dark,.dark *)");
        assert!(matches(&doc, p, &s2));
        // no shadow DOM: :host never matches; :root still selects <html>
        assert!(!matches(&doc, html, &sel(":host")));
        assert!(!matches(&doc, html, &sel(":host(.dark)")));
        assert!(matches(&doc, html, &sel(":root")));
        // light doc: dark variant must NOT match
        let light = rowser_html::parse("<html><body><p></p></body></html>");
        let lp = light.find_deep(light.root, "p").unwrap();
        assert!(!matches(&light, lp, &s2));
    }

    #[test]
    fn never_matching_pseudos() {
        let doc = rowser_html::parse("<body><p></p></body>");
        let p = doc.find_deep(doc.root, "p").unwrap();
        // dynamic-state + pseudo-elements parse but never match
        for s in [
            "p:hover",
            "p:focus",
            "a:visited",
            "p::before",
            "p::after",
            "p::first-line",
            "p::-webkit-scrollbar",
            "p:focus-visible",
            "p:lang(en)",
            "p:not(::placeholder)", // inner Never → not(never) = true!
        ] {
            let r = parse(&format!("{s} {{ k: v }}"));
            assert_eq!(r.rules.len(), 1, "rule should still parse: {s}");
        }
        assert!(!matches(&doc, p, &sel("p:hover")));
        assert!(!matches(&doc, p, &sel("p::before")));
        // ...but other selectors in the same rule still match
        let sheet = parse("*,:before,:after { k: v }");
        assert!(matches(&doc, p, &sheet.rules[0].selectors[0]));
        let c = computed(&doc, p, &[&sheet]);
        assert_eq!(c.get("k").unwrap(), "v");
        // :not(never-matching) = matches
        assert!(matches(&doc, p, &sel("p:not(::placeholder)")));
    }

    #[test]
    fn pseudo_state_hover() {
        let doc = rowser_html::parse("<body><ul><li><a href=\"/x\">l</a></li></ul></body>");
        let (a, li, ul) = (
            doc.find_deep(doc.root, "a").unwrap(),
            doc.find_deep(doc.root, "li").unwrap(),
            doc.find_deep(doc.root, "ul").unwrap(),
        );
        let st = PseudoState {
            hovered: Some(a),
            ..PseudoState::default()
        };
        // the hovered element and every ancestor match :hover
        assert!(matches_st(&doc, a, &sel("a:hover"), &st));
        assert!(matches_st(&doc, li, &sel("li:hover"), &st));
        assert!(matches_st(&doc, li, &sel("ul:hover li"), &st));
        assert!(!matches_st(&doc, ul, &sel("ul:focus"), &st));
        // a different hover target rewrites the chain
        let off = PseudoState {
            hovered: Some(ul),
            ..PseudoState::default()
        };
        assert!(matches_st(&doc, ul, &sel("ul:hover"), &off));
        assert!(!matches_st(&doc, a, &sel("a:hover"), &off));
        assert!(!matches_st(
            &doc,
            a,
            &sel("a:hover"),
            &PseudoState::default()
        ));
    }

    #[test]
    fn pseudo_state_link_visited_target() {
        let doc =
            rowser_html::parse("<body><a href=\"/x\" id=t>l</a><a>no</a><p id=t2>p</p></body>");
        let st = PseudoState::default();
        let links = all(&doc, "a");
        assert!(matches_st(&doc, links[0], &sel("a:link"), &st));
        assert!(!matches_st(&doc, links[1], &sel("a:link"), &st));
        // no history → :visited never matches
        assert!(!matches_st(&doc, links[0], &sel("a:visited"), &st));
        let p = doc.find_deep(doc.root, "p").unwrap();
        let tgt = PseudoState {
            target_frag: Some("t2".into()),
            ..PseudoState::default()
        };
        assert!(matches_st(&doc, p, &sel(":target"), &tgt));
        assert!(!matches_st(&doc, links[0], &sel(":target"), &tgt));
    }

    #[test]
    fn pseudo_state_focus_active() {
        let doc = rowser_html::parse("<body><form><button>b</button></form></body>");
        let (form, btn) = (
            doc.find_deep(doc.root, "form").unwrap(),
            doc.find_deep(doc.root, "button").unwrap(),
        );
        let st = PseudoState {
            focused: Some(btn),
            active: Some(btn),
            ..PseudoState::default()
        };
        assert!(matches_st(&doc, btn, &sel("button:focus"), &st));
        assert!(matches_st(&doc, btn, &sel(":focus-visible"), &st));
        assert!(matches_st(&doc, form, &sel("form:focus-within"), &st));
        assert!(matches_st(&doc, form, &sel("form:active"), &st));
        assert!(!matches_st(&doc, form, &sel("form:focus"), &st));
    }

    #[test]
    fn pseudo_state_styles() {
        // a:hover{color:red} applies only while the <a> is hovered
        let doc = rowser_html::parse("<body><a href=\"/x\">x</a></body>");
        let a = doc.find_deep(doc.root, "a").unwrap();
        let sheet = parse("a { color: blue } a:hover { color: red }");
        let mut ctx = StyleCtx::new(&doc, &[&sheet]);
        assert_eq!(ctx.computed(a).get("color").unwrap(), "blue");
        ctx.set_state(PseudoState {
            hovered: Some(a),
            ..PseudoState::default()
        });
        assert_eq!(ctx.computed(a).get("color").unwrap(), "red");
    }

    #[test]
    fn media_queries() {
        // light mode default (no env set in tests)
        let n = |css: &str| parse(css).rules.len();
        assert_eq!(n("@media (prefers-color-scheme: light) { p { a: 1 } }"), 1);
        assert_eq!(n("@media (prefers-color-scheme: dark) { p { a: 1 } }"), 0);
        assert_eq!(n("@media screen and (min-width: 40rem) { p { a: 1 } }"), 1);
        assert_eq!(n("@media screen and (min-width: 80rem) { p { a: 1 } }"), 0);
        // range syntax: (width >= 64rem) == 1024px at default vw
        assert_eq!(n("@media (width >= 64rem) { p { a: 1 } }"), 1);
        assert_eq!(n("@media (width >= 65rem) { p { a: 1 } }"), 0);
        assert_eq!(n("@media (400px <= width <= 2000px) { p { a: 1 } }"), 1);
        // not / or / comma lists / print
        assert_eq!(n("@media not print { p { a: 1 } }"), 1);
        assert_eq!(n("@media print { p { a: 1 } }"), 0);
        assert_eq!(n("@media print, screen { p { a: 1 } }"), 1);
        assert_eq!(
            n("@media (min-width: 9999px) or (max-width: 9999px) { p { a: 1 } }"),
            1
        );
        assert_eq!(
            n("@media screen and (min-width: 1px) and (max-width: 2px) { p { a: 1 } }"),
            0
        );
        // vw-sensitive
        assert_eq!(parse_vw("@media (min-width: 2in)", 100.0).rules.len(), 0);
    }

    #[test]
    fn layer_ordering() {
        let doc = rowser_html::parse("<body><p></p></body>");
        let p = doc.find_deep(doc.root, "p").unwrap();
        // unlayered beats layered even when earlier
        let sh = parse("p { color: green } @layer a { p { color: red } }");
        assert_eq!(computed(&doc, p, &[&sh]).get("color").unwrap(), "green");
        // layer order by first mention wins over source order
        let sh = parse("@layer a, b; @layer b { p { color: blue } } @layer a { p { color: red } }");
        assert_eq!(computed(&doc, p, &[&sh]).get("color").unwrap(), "blue");
        // later layer beats earlier in source order too
        let sh = parse("@layer a { p { color: red } } @layer b { p { color: blue } }");
        assert_eq!(computed(&doc, p, &[&sh]).get("color").unwrap(), "blue");
        // layered !important: earlier layer wins (inversion)
        let sh = parse(
            "@layer a { p { color: red !important } } @layer b { p { color: blue !important } }",
        );
        assert_eq!(computed(&doc, p, &[&sh]).get("color").unwrap(), "red");
    }

    #[test]
    fn goon_sheet() {
        // Real Tailwind v4 sheet — skips gracefully if absent.
        let Ok(css) = std::fs::read_to_string("/tmp/goon.css") else {
            eprintln!("goon_sheet: /tmp/goon.css missing, skipping");
            return;
        };
        let sh = parse(&css);
        // Before this work, all :is/:where/:has/attr-op rules died
        // (parse produced ~700 rules; now ~1000+). Print + sanity floor.
        let counts = |f: fn(&Simple) -> bool| -> usize {
            sh.rules
                .iter()
                .flat_map(|r| &r.selectors)
                .flat_map(|s| s.parts.iter().flat_map(|(_, ss)| ss.iter()))
                .filter(|s| f(*s))
                .count()
        };
        eprintln!("GOON RULES: {}", sh.rules.len());
        eprintln!("  is: {}", counts(|s| matches!(s, Simple::Is(_))));
        eprintln!("  where: {}", counts(|s| matches!(s, Simple::Where(_))));
        eprintln!("  has: {}", counts(|s| matches!(s, Simple::Has(_))));
        eprintln!("  never: {}", counts(|s| matches!(s, Simple::Never)));
        eprintln!("  attrop: {}", counts(|s| matches!(s, Simple::AttrOp(..))));
        eprintln!(
            "  unsupported: {}",
            counts(|s| matches!(s, Simple::Unsupported))
        );
        let functional = counts(|s| {
            matches!(
                s,
                Simple::Is(_) | Simple::Where(_) | Simple::Has(_) | Simple::AttrOp(..)
            )
        });
        eprintln!("  functional total: {functional}");
        // the sheet is known to carry :is/:where/:has/attr-op selectors —
        // before this work they were Unsupported (dead). Now they parse.
        assert!(
            sh.rules.len() > 200,
            "Tailwind sheet should yield many rules"
        );
        assert!(functional > 0, "functional pseudos should survive parse");
        // The preflight `*,:before,:after,::backdrop` rule must match via `*`
        let doc = rowser_html::parse("<html><body><p></p></body></html>");
        let p = doc.find_deep(doc.root, "p").unwrap();
        let c = computed(&doc, p, &[&sh]);
        eprintln!("  p computed props: {}", c.len());
    }

    #[test]
    fn border_shorthands_expand() {
        let doc = rowser_html::parse("<body><p>hi</p></body>");
        let p = doc.find_deep(doc.root, "p").unwrap();
        let s = parse("p{border:2px dashed red;border-top-color:blue}");
        let c = computed(&doc, p, &[&s]);
        // side shorthands carry the full token list for component extraction
        assert_eq!(c.get("border-left-width").unwrap(), "2px dashed red");
        assert_eq!(c.get("border-left-style").unwrap(), "2px dashed red");
        assert_eq!(c.get("border-top-color").unwrap(), "blue");
    }

    #[test]
    fn border_width_style_split() {
        let doc = rowser_html::parse("<body><p>hi</p></body>");
        let p = doc.find_deep(doc.root, "p").unwrap();
        let s = parse("p{border-width:1px 2px;border-style:solid dotted}");
        let c = computed(&doc, p, &[&s]);
        assert_eq!(c.get("border-top-width").unwrap(), "1px");
        assert_eq!(c.get("border-right-width").unwrap(), "2px");
        assert_eq!(c.get("border-bottom-style").unwrap(), "solid");
        assert_eq!(c.get("border-right-style").unwrap(), "dotted");
    }

    #[test]
    fn border_radius_expands_corners() {
        let doc = rowser_html::parse("<body><p>hi</p></body>");
        let p = doc.find_deep(doc.root, "p").unwrap();
        let s = parse("p{border-radius:10px 20px/5px}");
        let c = computed(&doc, p, &[&s]);
        assert_eq!(c.get("border-top-left-radius").unwrap(), "10px 5px");
        assert_eq!(c.get("border-top-right-radius").unwrap(), "20px 5px");
        assert_eq!(c.get("border-bottom-right-radius").unwrap(), "10px 5px");
        assert_eq!(c.get("border-bottom-left-radius").unwrap(), "20px 5px");
    }

    #[test]
    fn outline_overflow_expand() {
        let doc = rowser_html::parse("<body><p>hi</p></body>");
        let p = doc.find_deep(doc.root, "p").unwrap();
        let s = parse("p{outline:2px solid red;overflow:hidden}");
        let c = computed(&doc, p, &[&s]);
        assert_eq!(c.get("outline-width").unwrap(), "2px solid red");
        assert_eq!(c.get("overflow-x").unwrap(), "hidden");
    }

    #[test]
    fn background_shorthand_expands_longhands() {
        let doc = rowser_html::parse("<body><p id=a></p></body>");
        let p = doc.find_deep(doc.root, "p").unwrap();
        let sh = parse(
            "p{background:url(a.png) center/cover no-repeat fixed #123456, url(b.png) repeat-x}",
        );
        let c = computed(&doc, p, &[&sh]);
        assert_eq!(c.get("background-image").unwrap(), "url(a.png), url(b.png)");
        assert_eq!(c.get("background-color").unwrap(), "#123456");
        assert_eq!(c.get("background-size").unwrap(), "cover, auto");
        assert_eq!(c.get("background-repeat").unwrap(), "no-repeat, repeat-x");
        assert_eq!(c.get("background-position").unwrap(), "center, 0% 0%");
        assert_eq!(c.get("background-attachment").unwrap(), "fixed, scroll");
        assert_eq!(
            c.get("background-origin").unwrap(),
            "padding-box, padding-box"
        );
        assert_eq!(c.get("background-clip").unwrap(), "border-box, border-box");
    }

    #[test]
    fn background_shorthand_boxes_and_reset() {
        let doc = rowser_html::parse("<body><p id=a></p></body>");
        let p = doc.find_deep(doc.root, "p").unwrap();
        let sh = parse("p{background:url(x) content-box border-box round, url(y) local}");
        let c = computed(&doc, p, &[&sh]);
        assert_eq!(
            c.get("background-origin").unwrap(),
            "content-box, padding-box"
        );
        assert_eq!(c.get("background-clip").unwrap(), "border-box, border-box");
        assert_eq!(c.get("background-repeat").unwrap(), "round, repeat");
        assert_eq!(c.get("background-attachment").unwrap(), "scroll, local");
        // shorthand without a color resets background-color
        assert_eq!(c.get("background-color").unwrap(), "transparent");
    }

    #[test]
    fn flex_shorthand_expands_to_longhands() {
        // `flex` → flex-grow/flex-shrink/flex-basis at the decl's own
        // cascade position; keywords map to their spec expansions
        let doc = rowser_html::parse(
            "<body><div class=a></div><span class=b></span><p class=c></p><em class=d></em></body>",
        );
        let sh = parse(".a{flex:2} .b{flex:none} .c{flex:1 0 30px} .d{flex:auto}");
        let a = computed(&doc, doc.find_deep(doc.root, "div").unwrap(), &[&sh]);
        assert_eq!(a.get("flex-grow").map(String::as_str), Some("2"));
        assert_eq!(a.get("flex-shrink").map(String::as_str), Some("1"));
        assert_eq!(a.get("flex-basis").map(String::as_str), Some("0%"));
        let b = computed(&doc, doc.find_deep(doc.root, "span").unwrap(), &[&sh]);
        assert_eq!(b.get("flex-grow").map(String::as_str), Some("0"));
        assert_eq!(b.get("flex-shrink").map(String::as_str), Some("0"));
        assert_eq!(b.get("flex-basis").map(String::as_str), Some("auto"));
        let c = computed(&doc, doc.find_deep(doc.root, "p").unwrap(), &[&sh]);
        assert_eq!(c.get("flex-grow").map(String::as_str), Some("1"));
        assert_eq!(c.get("flex-shrink").map(String::as_str), Some("0"));
        assert_eq!(c.get("flex-basis").map(String::as_str), Some("30px"));
        let d = computed(&doc, doc.find_deep(doc.root, "em").unwrap(), &[&sh]);
        assert_eq!(d.get("flex-grow").map(String::as_str), Some("1"));
        assert_eq!(d.get("flex-shrink").map(String::as_str), Some("1"));
        assert_eq!(d.get("flex-basis").map(String::as_str), Some("auto"));
        // a more specific longhand still beats the shorthand's expansion
        let doc = rowser_html::parse("<body><div class='a x'></div></body>");
        let sh = parse(".a{flex:1} .a.x{flex-grow:3}");
        let e = computed(&doc, doc.find_deep(doc.root, "div").unwrap(), &[&sh]);
        assert_eq!(e.get("flex-grow").map(String::as_str), Some("3"));
    }
}
