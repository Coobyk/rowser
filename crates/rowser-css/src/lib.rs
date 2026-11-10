//! rowser-css: CSS parsing and matching, from scratch.
//!
//! Pragmatic subset of CSS Syntax Level 3: rule lists, qualified rules with
//! selector lists + declaration blocks, `!important`, comments, @rules
//! (skipped). Selectors: `*`, tag, `.class`, `#id`, `[attr]`, `[attr=v]`,
//! descendant and `>` child combinators, `:first-child`/`:last-child`.
//!
//! The cascade: user-agent sheet < author sheets, then !important author >
//! !important UA; ties broken by specificity then source order.

use rowser_dom::{Document, NodeId};
use rustc_hash::FxHashMap;

/// Property maps keyed by CSS property name — Fx-hashed (string keys are
/// small and lookups/inserts dominate the cascade).
pub type StrMap<V> = FxHashMap<String, V>;

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
#[derive(Debug, Clone)]
pub struct Selector {
    /// (compound, combinator-before-it). First entry's combinator is None.
    pub parts: Vec<(Combinator, Vec<Simple>)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Combinator {
    Descendant,
    Child,
    None,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Simple {
    Universal,
    Tag(String),
    Class(String),
    Id(String),
    AttrExists(String),
    AttrEquals(String, String),
    FirstChild,
    LastChild,
    /// `:root` — the html element.
    Root,
    /// Unsupported pseudo — selector fails quietly.
    Unsupported,
}

pub struct StyleSheet {
    pub rules: Vec<Rule>,
    /// Rightmost-compound index built at parse time: rules bucketed by a
    /// tag/class/id their rightmost compound requires. `plain` holds rules
    /// whose rightmost compound needs no such key (`*`, `[attr]`, pseudos).
    /// The buckets are a superset of real matches — full matching still runs.
    by_tag: FxHashMap<String, Vec<u32>>,
    by_class: FxHashMap<String, Vec<u32>>,
    by_id: FxHashMap<String, Vec<u32>>,
    plain: Vec<u32>,
}

pub fn parse(css: &str) -> StyleSheet {
    parse_vw(css, 1024.0)
}

pub fn parse_vw(css: &str, vw: f32) -> StyleSheet {
    let mut p = CssParser { s: css.as_bytes(), pos: 0, vw };
    let mut rules = Vec::new();
    p.skip_ws_comments();
    while !p.eof() {
        if p.peek() == Some(b'@') {
            if let Some(mut inner) = p.at_rule_rules() {
                rules.append(&mut inner);
            }
        } else if let Some(r) = p.rule() {
            rules.push(r);
        }
        p.skip_ws_comments();
    }
    let mut sheet = StyleSheet {
        rules,
        by_tag: FxHashMap::default(),
        by_class: FxHashMap::default(),
        by_id: FxHashMap::default(),
        plain: Vec::new(),
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
                        self.by_class.entry(k.to_string()).or_default().push(i as u32);
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
    /// @media/@supports/@layer are unwrapped — inner rules returned when
    /// the condition passes. Others are skipped. Returns rules to append.
    fn at_rule_rules(&mut self) -> Option<Vec<Rule>> {
        self.pos += 1; // '@'
        let name = self.ident();
        self.skip_ws_comments();
        let st = self.pos;
        while !self.eof() && self.peek() != Some(b'{') && self.peek() != Some(b';') {
            self.pos += 1;
        }
        let prelude = String::from_utf8_lossy(&self.s[st..self.pos]).trim().to_lowercase();
        let has_block = self.peek() == Some(b'{');
        if has_block {
            self.pos += 1;
        }
        let include = match name.as_str() {
            "media" => media_matches(&prelude, self.vw),
            "supports" => supports_matches(&prelude),
            "layer" => true,
            _ => false,
        };
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
                            if include {
                                rules.append(&mut r);
                            }
                        }
                    }
                    _ => {
                        if let Some(r) = self.rule() {
                            if include {
                                rules.push(r);
                            }
                        } else {
                            let _ = self.bump();
                        }
                    }
                }
            }
        }
        if include {
            Some(rules)
        } else {
            Some(Vec::new())
        }
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
                out.push((c as char).to_ascii_lowercase());
                self.pos += 1;
            } else {
                break;
            }
        }
        out
    }

    fn rule(&mut self) -> Option<Rule> {
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
        Some(Rule { selectors, decls })
    }

    fn selector(&mut self) -> Option<Selector> {
        let mut parts: Vec<(Combinator, Vec<Simple>)> = Vec::new();
        let mut comb = Combinator::None;
        loop {
            // skip whitespace — becomes Descendant combinator if another
            // compound follows
            let before = self.pos;
            while self.peek().is_some_and(Self::is_ws) {
                self.pos += 1;
            }
            let had_ws = self.pos > before;
            match self.peek() {
                Some(b'>') => {
                    self.pos += 1;
                    comb = Combinator::Child;
                    continue;
                }
                Some(b'+') | Some(b'~') => return None, // sibling combinators unsupported
                _ => {}
            }
            if had_ws && !parts.is_empty() {
                comb = Combinator::Descendant;
            }
            let mut simples = Vec::new();
            loop {
                match self.peek() {
                    Some(b'*') => {
                        self.pos += 1;
                        simples.push(Simple::Universal);
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
                    Some(b'[') => {
                        self.pos += 1;
                        self.skip_ws_comments();
                        let name = self.ident();
                        if name.is_empty() {
                            return None;
                        }
                        self.skip_ws_comments();
                        if self.peek() == Some(b'=') {
                            self.pos += 1;
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
                                _ => self.ident(),
                            };
                            self.skip_ws_comments();
                            if self.peek() == Some(b']') {
                                self.pos += 1;
                            }
                            simples.push(Simple::AttrEquals(name, v));
                        } else {
                            if self.peek() == Some(b']') {
                                self.pos += 1;
                            }
                            simples.push(Simple::AttrExists(name));
                        }
                    }
                    Some(b':') => {
                        self.pos += 1;
                        if self.peek() == Some(b':') {
                            self.pos += 1;
                        }
                        let name = self.ident();
                        simples.push(match name.as_str() {
                            "first-child" => Simple::FirstChild,
                            "last-child" => Simple::LastChild,
                            "root" => Simple::Root,
                            _ => {
                                // skip pseudo-args like :nth-child(2n)
                                if self.peek() == Some(b'(') {
                                    while self.peek().is_some_and(|c| c != b')') {
                                        self.pos += 1;
                                    }
                                    let _ = self.bump(); // closing ')', if present
                                }
                                Simple::Unsupported
                            }
                        });
                    }
                    Some(c) if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c >= 0x80 => {
                        let name = self.ident();
                        simples.push(Simple::Tag(name));
                    }
                    _ => break,
                }
            }
            if simples.is_empty() {
                break;
            }
            parts.push((comb, simples));
            comb = Combinator::None;
            self.skip_ws_comments();
            match self.peek() {
                Some(b'>') => continue,
                Some(b',') | Some(b'{') | None => break,
                _ => {
                    if self.pos > before {
                        continue; // whitespace → descendant
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
                    let prop = self.ident();
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
                    let mut value =
                        String::from_utf8_lossy(&self.s[st..self.pos]).trim().to_string();
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
fn specificity(sel: &Selector) -> (u32, u32, u32) {
    let (mut id, mut cls, mut tag) = (0, 0, 0);
    for (_, simples) in &sel.parts {
        for s in simples {
            match s {
                Simple::Id(_) => id += 1,
                Simple::Class(_) | Simple::AttrExists(_) | Simple::AttrEquals(..) | Simple::FirstChild | Simple::LastChild => cls += 1,
                Simple::Tag(_) => tag += 1,
                _ => {}
            }
        }
    }
    (id, cls, tag)
}

fn simple_matches(doc: &Document, node: NodeId, s: &Simple) -> bool {
    match s {
        Simple::Universal => true,
        Simple::Tag(t) => doc.tag(node) == Some(t.as_str()),
        Simple::Class(c) => doc
            .attr(node, "class")
            .is_some_and(|v| v.split_whitespace().any(|x| x == c)),
        Simple::Id(i) => doc.attr(node, "id") == Some(i.as_str()),
        Simple::AttrExists(a) => doc.attr(node, a).is_some(),
        Simple::AttrEquals(a, v) => doc.attr(node, a) == Some(v.as_str()),
        Simple::FirstChild => doc
            .parent(node)
            .is_some_and(|p| doc.children(p).first().copied() == Some(node)),
        Simple::LastChild => doc
            .parent(node)
            .is_some_and(|p| doc.children(p).last().copied() == Some(node)),
        Simple::Root => doc.tag(node) == Some("html"),
        Simple::Unsupported => false,
    }
}

/// Cheap bail: the rightmost compound's tag/classes/id must hold on `node`
/// for the selector to match — lets computed() skip most rules without
/// running the full matcher.
fn rightmost_ok(doc: &Document, node: NodeId, s: &Selector) -> bool {
    let Some((_, simples)) = s.parts.last() else { return false };
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

fn compound_matches(doc: &Document, node: NodeId, simples: &[Simple]) -> bool {
    simples.iter().all(|s| simple_matches(doc, node, s))
}

/// Match a selector against an element node (right-to-left).
pub fn matches(doc: &Document, node: NodeId, sel: &Selector) -> bool {
    let n = sel.parts.len();
    if n == 0 {
        return false;
    }
    let (_, last) = &sel.parts[n - 1];
    if !compound_matches(doc, node, last) {
        return false;
    }
    // walk leftward through ancestors
    let mut cur = node;
    for i in (0..n - 1).rev() {
        let (comb, simples) = &sel.parts[i];
        match comb {
            Combinator::Child => {
                let Some(p) = doc.parent(cur) else { return false };
                if doc.tag(p).is_none() || !compound_matches(doc, p, simples) {
                    return false;
                }
                cur = p;
            }
            _ => {
                let mut p = doc.parent(cur);
                loop {
                    match p {
                        Some(a) => {
                            if doc.tag(a).is_some() && compound_matches(doc, a, simples) {
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
        }
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

    pub fn computed(&mut self, node: NodeId) -> StrMap<String> {
        let doc = self.doc;
        let mut out: StrMap<(String, u8, (u32, u32, u32), usize)> = StrMap::default();
        // `--*` decls from rules matching this node (element_customs folded
        // into the main match pass — one selector match per rule, not two).
        let mut customs: StrMap<String> = StrMap::default();
        let mut order = 0usize;
        for (origin, sheet) in self.sheets.iter().enumerate() {
            // origin 0 = UA (weakest), later = author
            sheet.candidates_into(doc, node, &mut self.buf);
            for &ri in &self.buf {
                let rule = &sheet.rules[ri as usize];
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
                    // priority: important > normal; origin weight breaks ties
                    let pri: u8 = if d.important { 2 + origin as u8 } else { origin as u8 };
                    if d.prop.starts_with("--") {
                        customs.insert(d.prop.clone(), d.value.clone());
                    }
                    each_physical(&d.prop, |prop| match out.get_mut(prop) {
                        Some(e) => {
                            if (pri, spec, order) >= (e.1, e.2, e.3) {
                                *e = (d.value.clone(), pri, spec, order);
                            }
                        }
                        None => {
                            out.insert(prop.to_string(), (d.value.clone(), pri, spec, order));
                        }
                    });
                }
                order += 1;
            }
        }
        let mut props: StrMap<String> =
            out.into_iter().map(|(k, (v, _, _, _))| (k, v)).collect();
        // Custom properties: resolve var() only when a value actually uses
        // it — collecting the customs map is free (folded above), and the
        // :root globals are cached on the ctx rather than re-matched per node.
        if props.values().any(|v| v.contains("var(")) {
            for (k, v) in self.root_customs().iter() {
                customs.entry(k.clone()).or_insert_with(|| v.clone());
            }
            for v in props.values_mut() {
                *v = resolve_vars(v, &customs);
            }
        }
        props
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
        p => f(p),
    }
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

/// Substitute `var(--name)` and `var(--name, fallback)` recursively.
fn resolve_vars(value: &str, customs: &StrMap<String>) -> String {
    let mut out = value.to_string();
    for _ in 0..8 {
        let Some(start) = out.find("var(") else { break };
        let mut depth = 1i32;
        let mut end = start + 4;
        while end < out.len() && depth > 0 {
            match out.as_bytes()[end] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            end += 1;
        }
        if depth != 0 {
            break;
        }
        let inner = out[start + 4..end - 1].trim();
        let (name, fallback) = inner.split_once(',').map(|(a, b)| (a.trim(), b.trim())).unwrap_or((inner, ""));
        let rep = customs
            .get(name)
            .map(String::as_str)
            .unwrap_or(fallback);
        out = format!("{}{}{}", &out[..start], rep, &out[end..]);
    }
    out
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
    fn cascade_important() {
        let doc = rowser_html::parse("<body><p id=a>hi</p></body>");
        let p = doc.find_deep(doc.root, "p").unwrap();
        let ua = parse("p { color: red !important }");
        let author = parse("#a { color: blue }");
        let c = computed(&doc, p, &[&ua, &author]);
        assert_eq!(c.get("color").unwrap(), "red");
    }
}


fn media_matches(q: &str, vw: f32) -> bool {
    let q = q.replace("screen", "");
    let width_of = |q: &str, key: &str| -> Option<f32> {
        q.split(key)
            .nth(1)?
            .split(')')
            .next()?
            .trim()
            .trim_end_matches("px")
            .parse::<f32>()
            .ok()
    };
    if q.contains("min-width") {
        if let Some(px) = width_of(&q, "min-width:") {
            return vw >= px;
        }
    }
    if q.contains("max-width") {
        if let Some(px) = width_of(&q, "max-width:") {
            return vw <= px;
        }
    }
    if q.contains("prefers-color-scheme") {
        return q.contains("dark");
    }
    if q.contains("prefers-reduced-motion") || q.contains("print") {
        return false;
    }
    true
}

/// Very small @supports evaluator: true only if every "(prop: v)" property
/// in the query is one layout understands (or it's a `not` of a known-prop).
fn supports_matches(q: &str) -> bool {
    const KNOWN: &[&str] = &[
        "display", "color", "background", "background-color", "font-size", "font-weight",
        "font-style", "font-family", "text-decoration", "text-align", "white-space",
        "vertical-align", "margin", "margin-top", "margin-right", "margin-bottom",
        "margin-left", "padding", "padding-top", "padding-right", "padding-bottom",
        "padding-left", "border", "width", "height", "line-height", "float", "gap",
        "border-collapse",
    ];
    let mut ok = true;
    for part in q.split('(').skip(1) {
        let Some(inner) = part.split(')').next() else { continue };
        if let Some((prop, _)) = inner.split_once(':') {
            let p = prop.trim();
            if !p.starts_with("--") && !KNOWN.contains(&p) {
                ok = false;
            }
        }
    }
    ok
}

#[cfg(test)]
mod dbg_tests {
    use super::*;
    #[test]
    fn dbg_coobyk() {
        let css = std::fs::read_to_string("/tmp/coobyk.css").unwrap();
        let sh = parse(&css);
        eprintln!("RULES: {}", sh.rules.len());
        eprintln!("display:flex decls: {}", sh.rules.iter().filter(|r| r.decls.iter().any(|d| d.prop=="display" && d.value.contains("flex"))).count());
        eprintln!("--spacing decls: {}", sh.rules.iter().filter(|r| r.decls.iter().any(|d| d.prop=="--spacing")).count());
        eprintln!("gap decls: {}", sh.rules.iter().filter(|r| r.decls.iter().any(|d| d.prop=="gap")).count());
    }
}

#[cfg(test)]
mod dbg2 {
    use super::*;
    use rowser_dom::{Node, NodeKind};
    fn el(d: &mut rowser_dom::Document, p: NodeId, tag: &str, cls: &str) -> NodeId {
        let mut attrs = std::collections::HashMap::new();
        attrs.insert("class".to_string(), cls.to_string());
        d.nodes.push(Node { kind: NodeKind::Element { tag: tag.into(), attrs }, parent: None, children: vec![], sibling_index: 0 });
        let id = d.nodes.len() - 1;
        d.append(p, id);
        id
    }
    #[test]
    fn dbg_vars() {
        let css = ":root,:host{--spacing:.25rem;--bg:#fff}.x{padding-left:calc(var(--spacing) * 2)}";
        let sh = parse(css);
        eprintln!("rules: {}", sh.rules.len());
        let mut d = rowser_dom::Document::new();
        let root = d.root;
        let html = el(&mut d, root, "html", "");
        let a = el(&mut d, html, "a", "x");
        let c = computed(&d, a, &[&sh]);
        eprintln!("computed: {:?}", c);
        eprintln!("html customs: {:?}", element_customs(&d, html, &[&sh]));
    }
}
