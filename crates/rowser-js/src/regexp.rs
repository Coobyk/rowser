//! A small backtracking regular-expression engine, built from scratch on
//! std only.
//!
//! The pattern is parsed into an AST (`Node`) and matched by recursive
//! backtracking with continuation-passing over `&[u16]` — JS strings are
//! UTF-16: non-unicode patterns match code units, `u`/`v` patterns match
//! whole code points (a surrogate pair is a single char). A global step
//! cap keeps catastrophic backtracking from hanging the engine — when the
//! cap is exceeded the match attempt fails rather than hanging.

use std::collections::HashSet;
use std::rc::Rc;

/// Max backtracking steps per match attempt.
const MAX_STEPS: u64 = 1_000_000;
/// Max recursion depth in the matcher (stack safety on pathological input).
const MAX_DEPTH: u32 = 4_000;

#[derive(Debug)]
pub enum Node {
    Empty,
    /// a single character: a code point in unicode patterns (astral values
    /// consume a surrogate pair — surrogate code points arise from `\uXXXX`
    /// escapes and only match lone units), a UTF-16 code unit otherwise
    Ch(u32),
    /// `.` — any char except line terminators (unless `s` flag)
    Any,
    Class {
        neg: bool,
        items: Vec<ClassItem>,
    },
    Seq(Vec<Node>),
    Alt(Vec<Node>),
    Rep {
        inner: Box<Node>,
        min: u32,
        max: u32,
        lazy: bool,
    },
    /// capture group `idx` (1-based)
    Group {
        idx: usize,
        inner: Box<Node>,
    },
    /// `\1`..`\9` backreference
    Backref(u32),
    /// `\k<name>` — resolved to Backref after parse; carries the normalized
    /// name plus the raw source text (Annex-B literal fallback needs it)
    BackrefName(String, String),
    Anchor(Anchor),
    /// `(?=..)` `(?!..)` `(?<=..)` `(?<!..)`
    Look {
        inner: Box<Node>,
        neg: bool,
        behind: bool,
    },
}

#[derive(Debug, Clone)]
pub enum ClassItem {
    /// single char — code point in unicode patterns, code unit otherwise
    Ch(u32),
    Range(u32, u32),
    Pre(Pre),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pre {
    Digit,
    NotDigit,
    Word,
    NotWord,
    Space,
    NotSpace,
}

#[derive(Debug, Clone, Copy)]
pub enum Anchor {
    Start,
    End,
    WordB,
    NotWordB,
}

#[derive(Debug)]
pub struct Regex {
    pub source: String,
    /// flags in spec order (subset of `dgimsuvy` present)
    pub flags: String,
    pub root: Node,
    pub n_groups: usize,
    /// named groups in definition order: (name, 1-based group idx)
    pub names: Vec<(String, usize)>,
    pub global: bool,
    pub ignore_case: bool,
    pub multiline: bool,
    pub dot_all: bool,
    pub sticky: bool,
    pub has_indices: bool,
    pub unicode: bool,
    pub unicode_sets: bool,
}

#[derive(Debug, Clone)]
pub struct RxMatch {
    pub start: usize,
    pub end: usize,
    /// caps[0] = whole match; caps[g] = capture group g (1-based) — indices
    /// are UTF-16 unit offsets into the haystack
    pub caps: Vec<Option<(usize, usize)>>,
}

impl Regex {
    pub fn compile(source: &str, flags: &str) -> Result<Rc<Regex>, String> {
        let mut seen = String::new();
        for f in flags.chars() {
            if !"dgimsuvy".contains(f) {
                return Err(format!("invalid flag '{f}'"));
            }
            if seen.contains(f) {
                return Err(format!("duplicate flag '{f}'"));
            }
            seen.push(f);
        }
        if seen.contains('u') && seen.contains('v') {
            return Err("flags 'u' and 'v' are mutually exclusive".into());
        }
        let icase = seen.contains('i');
        let (total_groups, has_named) = count_capture_groups(source);
        let mut p = ReParser {
            s: source.chars().collect(),
            i: 0,
            n_groups: 0,
            names: Vec::new(),
            total_groups,
            has_named,
            unicode: seen.contains('u') || seen.contains('v'),
            pending: None,
            pending_class: None,
            name_scope: Vec::new(),
        };
        let root = p.alt()?;
        if p.i < p.s.len() {
            return Err(format!("unexpected '{}'", p.s[p.i]));
        }
        let names = p.names.clone();
        let root = resolve_names(root, &names, p.unicode)?;
        let canonical: String = "dgimsuvy".chars().filter(|c| seen.contains(*c)).collect();
        Ok(Rc::new(Regex {
            source: source.to_string(),
            flags: canonical,
            root,
            n_groups: p.n_groups,
            names,
            global: seen.contains('g'),
            ignore_case: icase,
            multiline: seen.contains('m'),
            dot_all: seen.contains('s'),
            sticky: seen.contains('y'),
            has_indices: seen.contains('d'),
            unicode: seen.contains('u'),
            unicode_sets: seen.contains('v'),
        }))
    }

    /// unicode-matching: atoms consume whole code points (`u` or `v` flag)
    fn umatch(&self) -> bool {
        self.unicode || self.unicode_sets
    }

    /// index of the code point containing unit `p` — for unicode matching a
    /// lastIndex inside a surrogate pair maps to the pair's character
    /// (spec RegExpBuiltinExec's `inputIndex` computation)
    fn boundary(&self, s: &[u16], p: usize) -> usize {
        if self.umatch() && p > 0 && p < s.len() && is_hi(s[p - 1]) && is_lo(s[p]) {
            p - 1
        } else {
            p
        }
    }

    /// spec AdvanceStringIndex — +2 over a surrogate pair in unicode mode
    fn advance(&self, s: &[u16], p: usize) -> usize {
        if self.umatch() && p + 1 < s.len() && is_hi(s[p]) && is_lo(s[p + 1]) {
            p + 2
        } else {
            p + 1
        }
    }

    /// Leftmost match starting at or after `start` (honors `y`/sticky).
    pub fn exec(&self, s: &[u16], start: usize) -> Option<RxMatch> {
        if start > s.len() {
            return None;
        }
        if self.sticky {
            return self.try_at(s, start);
        }
        let mut p = start;
        loop {
            if let Some(m) = self.try_at(s, p) {
                return Some(m);
            }
            if p >= s.len() {
                return None;
            }
            p = self.advance(s, p);
        }
    }

    /// Leftmost match at or after `from`, ignoring stickiness — used by
    /// string ops that drive their own position.
    pub fn find(&self, s: &[u16], from: usize) -> Option<RxMatch> {
        if from > s.len() {
            return None;
        }
        let mut p = from;
        loop {
            if let Some(m) = self.try_at(s, p) {
                return Some(m);
            }
            if p >= s.len() {
                return None;
            }
            p = self.advance(s, p);
        }
    }

    /// Anchored match attempt at exactly `pos`.
    pub fn try_at(&self, s: &[u16], pos: usize) -> Option<RxMatch> {
        let pos = self.boundary(s, pos);
        if pos > s.len() {
            return None;
        }
        let mut c = Ctx {
            s,
            caps: vec![None; self.n_groups + 1],
            steps: 0,
            depth: 0,
            aborted: false,
            icase: self.ignore_case,
            multiline: self.multiline,
            dotall: self.dot_all,
            umatch: self.umatch(),
            end: usize::MAX,
        };
        let found = mat(&mut c, &self.root, pos, &mut |c2, e| {
            c2.end = e;
            true
        });
        if !found || c.end == usize::MAX {
            return None;
        }
        let mut caps = c.caps;
        caps[0] = Some((pos, c.end));
        Some(RxMatch {
            start: pos,
            end: c.end,
            caps,
        })
    }
}

// ===========================================================================
// pattern parser
// ===========================================================================

/// split an astral code point into its UTF-16 surrogate halves
fn split_pair(v: u32) -> (u32, u32) {
    (
        0xD800 + ((v - 0x10000) >> 10),
        0xDC00 + ((v - 0x10000) & 0x3FF),
    )
}

/// a literal char node — astral chars become a unit sequence outside
/// unicode patterns
fn lit_node(c: char, unicode: bool) -> Node {
    let v = c as u32;
    if v > 0xFFFF && !unicode {
        let (hi, lo) = split_pair(v);
        Node::Seq(vec![Node::Ch(hi), Node::Ch(lo)])
    } else {
        Node::Ch(v)
    }
}

struct ReParser {
    s: Vec<char>,
    i: usize,
    n_groups: usize,
    names: Vec<(String, usize)>,
    /// total capture groups in the whole pattern (prescan) — needed to tell
    /// `\1` backreferences from legacy octal escapes
    total_groups: usize,
    /// the pattern contains at least one `(?<name>...)` — `\k<name>` is
    /// only a named backreference then (Annex-B literal fallback else)
    has_named: bool,
    /// `u`/`v` flag — Annex-B extensions (quantifiable assertions, lone
    /// `]`/`}`/`{`, octals) are disabled and stray syntax is a hard error
    unicode: bool,
    /// pending UTF-16 unit from an astral literal split in a non-unicode
    /// pattern — the low half is the *next* atom (a quantifier binds to it)
    pending: Option<u32>,
    /// same, inside a character class
    pending_class: Option<u32>,
    /// per-open-disjunction group-name scopes: `(union, arm)` — `arm` is
    /// the names defined inside the alternative currently being parsed
    /// (a second use in the SAME alternative is a SyntaxError), `union`
    /// accumulates every arm's names and propagates to the enclosing
    /// alternative when the disjunction ends — this is the spec's
    /// "duplicate names are allowed only in different `|` arms" rule
    name_scope: Vec<(HashSet<String>, HashSet<String>)>,
}

impl ReParser {
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }
    fn peek2(&self) -> Option<char> {
        self.s.get(self.i + 1).copied()
    }
    fn next(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.i += 1;
        Some(c)
    }
    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, c: char) -> Result<(), String> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(format!("expected '{c}'"))
        }
    }

    /// a literal pattern char → an atom. Astral literals in non-unicode
    /// patterns are two UTF-16 units: the high half is returned and the
    /// low half is queued in `pending` as the following atom (so a
    /// quantifier binds to the low unit, matching unit-level parsing).
    fn lit_char(&mut self, c: char) -> Node {
        let v = c as u32;
        if v > 0xFFFF && !self.unicode {
            let (hi, lo) = split_pair(v);
            self.pending = Some(lo);
            Node::Ch(hi)
        } else {
            Node::Ch(v)
        }
    }

    /// same, as a character-class item (`pending_class` queues the low half)
    fn lit_class(&mut self, c: char) -> ClassItem {
        let v = c as u32;
        if v > 0xFFFF && !self.unicode {
            let (hi, lo) = split_pair(v);
            self.pending_class = Some(lo);
            ClassItem::Ch(hi)
        } else {
            ClassItem::Ch(v)
        }
    }

    fn alt(&mut self) -> Result<Node, String> {
        self.name_scope.push((HashSet::new(), HashSet::new()));
        let mut arms = vec![self.seq()?];
        while self.eat('|') {
            // a new alternative: names defined so far stay reachable only
            // within the previous arm — a fresh arm may reuse them
            self.name_scope.last_mut().unwrap().1.clear();
            arms.push(self.seq()?);
        }
        let (mut union, arm) = self.name_scope.pop().unwrap();
        union.extend(arm);
        // all names defined inside this disjunction "occur" in the
        // enclosing alternative — a collision there means two same-named
        // groups are reachable on one match path → SyntaxError
        if let Some((_u, p_arm)) = self.name_scope.last_mut() {
            for n in union {
                if !p_arm.insert(n.clone()) {
                    return Err(format!(
                        "duplicate group name '{n}' in the same alternative"
                    ));
                }
            }
        }
        Ok(if arms.len() == 1 {
            arms.pop().unwrap()
        } else {
            Node::Alt(arms)
        })
    }

    fn seq(&mut self) -> Result<Node, String> {
        let mut items = Vec::new();
        loop {
            let atom = if let Some(u) = self.pending.take() {
                Node::Ch(u)
            } else {
                match self.peek() {
                    Some('|') | Some(')') | None => break,
                    _ => {
                        let a = self.atom()?;
                        if self.pending.is_some() {
                            // astral literal split — the high half is a
                            // finished atom; the pending low half takes
                            // the quantifier slot
                            items.push(a);
                            continue;
                        }
                        a
                    }
                }
            };
            items.push(self.quant(atom)?);
        }
        match items.len() {
            0 => Ok(Node::Empty),
            1 => Ok(items.pop().unwrap()),
            _ => Ok(Node::Seq(items)),
        }
    }

    /// optional quantifier following an atom
    fn quant(&mut self, atom: Node) -> Result<Node, String> {
        let (min, max) = match self.peek() {
            Some('*') => {
                self.i += 1;
                (0, u32::MAX)
            }
            Some('+') => {
                self.i += 1;
                (1, u32::MAX)
            }
            Some('?') => {
                self.i += 1;
                (0, 1)
            }
            Some('{') => match self.try_brace_quant()? {
                Some(mm) => mm,
                None => return Ok(atom),
            },
            _ => return Ok(atom),
        };
        // Assertions can't be quantified in the base grammar. Annex B adds
        // one exception for non-unicode patterns: lookaheads may carry a
        // quantifier. Lookbehinds and anchors are never quantifiable.
        match &atom {
            Node::Anchor(_) => {
                return Err("quantifier on assertion".into());
            }
            Node::Look { behind, .. } => {
                if *behind || self.unicode {
                    return Err("quantifier on assertion".into());
                }
            }
            _ => {}
        }
        let lazy = self.eat('?');
        Ok(Node::Rep {
            inner: Box::new(atom),
            min,
            max,
            lazy,
        })
    }

    /// `{n}` `{n,}` `{n,m}` — Ok(None) (no consumption) if not a valid
    /// form; Err if the quantifier is well-formed but out of order
    fn try_brace_quant(&mut self) -> Result<Option<(u32, u32)>, String> {
        let save = self.i;
        self.i += 1; // {
        let Some(lo) = self.digits() else {
            self.i = save;
            return Ok(None);
        };
        let (min, max) = if self.eat(',') {
            if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                let hi = self.digits().unwrap();
                (lo, hi)
            } else {
                (lo, u32::MAX)
            }
        } else {
            (lo, lo)
        };
        if self.peek() == Some('}') {
            self.i += 1;
            // out-of-order only errors when the whole `{n,m}` shape
            // matched — `a{2,1` without the brace stays a literal
            if max < min {
                return Err("numbers out of order in {} quantifier".into());
            }
            Ok(Some((min, max)))
        } else {
            self.i = save;
            Ok(None)
        }
    }

    /// does the text starting at `at` (which must be `{`) parse as a
    /// braced quantifier shape `{d}`, `{d,}` or `{d,d}`? lookahead only
    fn brace_quant_shape_at(&self, at: usize) -> bool {
        let mut i = at + 1;
        if !self.s.get(i).is_some_and(|c| c.is_ascii_digit()) {
            return false;
        }
        while self.s.get(i).is_some_and(|c| c.is_ascii_digit()) {
            i += 1;
        }
        if self.s.get(i) == Some(&',') {
            i += 1;
            while self.s.get(i).is_some_and(|c| c.is_ascii_digit()) {
                i += 1;
            }
        }
        self.s.get(i) == Some(&'}')
    }

    fn digits(&mut self) -> Option<u32> {
        let mut v: u64 = 0;
        let mut any = false;
        while let Some(d) = self.peek().and_then(|c| c.to_digit(10)) {
            any = true;
            v = (v * 10 + d as u64).min(u32::MAX as u64);
            self.i += 1;
        }
        any.then_some(v as u32)
    }

    fn atom(&mut self) -> Result<Node, String> {
        let c = self.next().ok_or("unexpected end of pattern")?;
        match c {
            '(' => self.group(),
            '[' => self.class(),
            '.' => Ok(Node::Any),
            '^' => Ok(Node::Anchor(Anchor::Start)),
            '$' => Ok(Node::Anchor(Anchor::End)),
            '\\' => self.escape(),
            ')' | '|' => Err(format!("unexpected '{c}'")),
            // `*`, `+`, `?` in atom position are stray quantifiers — a hard
            // error in every mode ("nothing to repeat")
            '*' | '+' | '?' => Err("nothing to repeat".into()),
            // lone `]`, `}`, `{` are literals only under Annex-B leniency;
            // with `u`/`v` they are a SyntaxError
            ']' | '}' | '{' if self.unicode => Err(format!("unexpected '{c}'")),
            // `{` in atom position: if it parses as a braced quantifier it
            // is a SyntaxError (InvalidBracedQuantifier) — else a literal
            '{' if self.brace_quant_shape_at(self.i - 1) => Err("quantifier without atom".into()),
            // stray `]`/`}`/`{` are treated as literals (Annex-B leniency)
            c => Ok(self.lit_char(c)),
        }
    }

    /// after `(`
    fn group(&mut self) -> Result<Node, String> {
        if self.peek() == Some('?') {
            self.i += 1;
            match self.peek() {
                Some(':') => {
                    self.i += 1;
                    let n = self.alt()?;
                    self.expect(')')?;
                    Ok(n)
                }
                Some('=') => {
                    self.i += 1;
                    let n = self.alt()?;
                    self.expect(')')?;
                    Ok(Node::Look {
                        inner: Box::new(n),
                        neg: false,
                        behind: false,
                    })
                }
                Some('!') => {
                    self.i += 1;
                    let n = self.alt()?;
                    self.expect(')')?;
                    Ok(Node::Look {
                        inner: Box::new(n),
                        neg: true,
                        behind: false,
                    })
                }
                Some('<') => {
                    self.i += 1;
                    match self.peek() {
                        Some('=') => {
                            self.i += 1;
                            let n = self.alt()?;
                            self.expect(')')?;
                            Ok(Node::Look {
                                inner: Box::new(n),
                                neg: false,
                                behind: true,
                            })
                        }
                        Some('!') => {
                            self.i += 1;
                            let n = self.alt()?;
                            self.expect(')')?;
                            Ok(Node::Look {
                                inner: Box::new(n),
                                neg: true,
                                behind: true,
                            })
                        }
                        _ => {
                            // named group (?<name>...)
                            let (name, _raw) = self.parse_group_name()?;
                            // a name may be reused across different `|`
                            // alternatives but never twice in the same one
                            let (union, arm) = self.name_scope.last_mut().expect("inside alt()");
                            if !arm.insert(name.clone()) {
                                return Err(format!(
                                    "duplicate group name '{name}' in the same alternative"
                                ));
                            }
                            union.insert(name.clone());
                            self.n_groups += 1;
                            let idx = self.n_groups;
                            self.names.push((name, idx));
                            let n = self.alt()?;
                            self.expect(')')?;
                            Ok(Node::Group {
                                idx,
                                inner: Box::new(n),
                            })
                        }
                    }
                }
                _ => {
                    // `(?x` — only `?:` `?=` `?!` `?<name>` `?<=` `?<!` exist
                    Err(format!(
                        "invalid group specifier '(?{}'",
                        self.peek().unwrap_or(' ')
                    ))
                }
            }
        } else {
            self.n_groups += 1;
            let idx = self.n_groups;
            let n = self.alt()?;
            self.expect(')')?;
            Ok(Node::Group {
                idx,
                inner: Box::new(n),
            })
        }
    }

    /// GroupName after `(?<` or `\k<` — RegExpIdentifierName code points:
    /// literal chars or `\uXXXX` / `\u{cp}` escapes (both allowed in either
    /// mode here); a `\u` lead-surrogate escape followed by a `\u`
    /// trail-surrogate escape combines into a single code point. Validates
    /// ID_Start / ID_Continue and returns (normalized name, raw source).
    fn parse_group_name(&mut self) -> Result<(String, String), String> {
        let raw_start = self.i;
        let mut cps: Vec<u32> = Vec::new();
        loop {
            let cp = match self.next() {
                None => return Err("unterminated group name".into()),
                Some('>') => break,
                Some('\\') => match self.next() {
                    Some('u') => self.name_escape()?,
                    _ => return Err("invalid escape in group name".into()),
                },
                Some(c) => c as u32,
            };
            let ok = if cps.is_empty() {
                is_id_start_cp(cp)
            } else {
                is_id_continue_cp(cp)
            };
            if !ok {
                return Err(format!("invalid group name character U+{cp:X}"));
            }
            cps.push(cp);
        }
        if cps.is_empty() {
            return Err("empty group name".into());
        }
        let raw: String = self.s[raw_start..self.i - 1].iter().collect();
        let name: String = cps.iter().filter_map(|&cp| char::from_u32(cp)).collect();
        Ok((name, raw))
    }

    /// `\u` escape inside a group name — `\uXXXX` or `\u{cp}`, with
    /// surrogate-pair combining (`\uD800\uDC00` → U+10000)
    fn name_escape(&mut self) -> Result<u32, String> {
        let v = if self.eat('{') {
            let mut v: u32 = 0;
            let mut any = false;
            while let Some(d) = self.peek().and_then(|c| c.to_digit(16)) {
                any = true;
                v = v.saturating_mul(16).saturating_add(d);
                self.i += 1;
            }
            if !self.eat('}') || !any || v > 0x10FFFF {
                return Err("invalid \\u escape in group name".into());
            }
            v
        } else {
            self.hex(4).ok_or("invalid \\u escape in group name")?
        };
        if (0xD800..0xDC00).contains(&v) && self.peek() == Some('\\') && self.peek2() == Some('u') {
            let save = self.i;
            self.i += 2;
            if let Some(t) = self.hex(4) {
                if (0xDC00..0xE000).contains(&t) {
                    return Ok(0x10000 + ((v - 0xD800) << 10) + (t - 0xDC00));
                }
            }
            self.i = save;
        }
        Ok(v)
    }

    /// `\x` outside a char class
    fn escape(&mut self) -> Result<Node, String> {
        let c = self.next().ok_or("trailing backslash")?;
        Ok(match c {
            'd' => pre_node(Pre::Digit),
            'D' => pre_node(Pre::NotDigit),
            'w' => pre_node(Pre::Word),
            'W' => pre_node(Pre::NotWord),
            's' => pre_node(Pre::Space),
            'S' => pre_node(Pre::NotSpace),
            'b' => Node::Anchor(Anchor::WordB),
            'B' => Node::Anchor(Anchor::NotWordB),
            'n' => Node::Ch(0x0A),
            't' => Node::Ch(0x09),
            'r' => Node::Ch(0x0D),
            'f' => Node::Ch(0x0C),
            'v' => Node::Ch(0x0B),
            '0' => {
                if self.unicode && self.peek().is_some_and(|d| d.is_ascii_digit()) {
                    return Err("octal escapes are not allowed in unicode mode".into());
                }
                // \0 or legacy octal
                Node::Ch(self.octal_after(0))
            }
            'x' => {
                let save = self.i;
                match self.hex(2) {
                    Some(v) => Node::Ch(v),
                    None if self.unicode => {
                        return Err("invalid \\x escape".into());
                    }
                    None => {
                        self.i = save;
                        Node::Ch('x' as u32)
                    }
                }
            }
            'u' => {
                let save = self.i;
                match self.uesc_opt() {
                    Some(v) => Node::Ch(v),
                    None if self.unicode => {
                        return Err("invalid \\u escape".into());
                    }
                    None => {
                        self.i = save;
                        Node::Ch('u' as u32)
                    }
                }
            }
            'c' => {
                // control letter \cA → \x01
                match self.peek() {
                    Some(l) if l.is_ascii_alphabetic() => {
                        self.i += 1;
                        Node::Ch((l as u32) % 32)
                    }
                    _ if self.unicode => {
                        return Err("invalid \\c escape".into());
                    }
                    _ => Node::Ch('c' as u32),
                }
            }
            'k' => {
                if self.eat('<') {
                    if !self.unicode && !self.has_named {
                        // Annex-B: `\k` is literal when the pattern has no
                        // named groups at all — the `<` starts a fresh
                        // literal atom
                        self.i -= 1;
                        Node::Ch('k' as u32)
                    } else {
                        let (name, raw) = self.parse_group_name()?;
                        Node::BackrefName(name, raw)
                    }
                } else if self.unicode {
                    return Err("invalid \\k escape".into());
                } else {
                    Node::Ch('k' as u32)
                }
            }
            '1'..='9' => {
                let mut digits = String::new();
                digits.push(c);
                while let Some(d) = self.peek() {
                    if d.is_ascii_digit() {
                        digits.push(d);
                        self.i += 1;
                    } else {
                        break;
                    }
                }
                let n: u32 = digits.parse().unwrap_or(0);
                if (n as usize) <= self.total_groups {
                    Node::Backref(n)
                } else if self.unicode {
                    // no such capture group → SyntaxError in unicode mode
                    return Err(format!("invalid backreference \\{digits}"));
                } else {
                    // legacy octal / literal digits — read back the extra
                    // digits and take the Annex-B octal prefix: `0`-`3` get
                    // up to 3 digits, `4`-`7` only 2 (`FourToSeven OctalDigit`)
                    let first = digits.chars().next().unwrap();
                    if first == '8' || first == '9' {
                        self.i -= digits.len() - 1;
                        Node::Ch(first as u32)
                    } else {
                        let cap = if first <= '3' { 3 } else { 2 };
                        let od: String = digits
                            .chars()
                            .take_while(|d| ('0'..='7').contains(d))
                            .take(cap)
                            .collect();
                        self.i -= digits.len() - od.len();
                        let v = u32::from_str_radix(&od, 8).unwrap_or(0);
                        Node::Ch(v)
                    }
                }
            }
            other => {
                // identity escapes in unicode mode are limited to
                // SyntaxCharacter + `/` — anything else is a SyntaxError
                if self.unicode && !"^$\\.*+?()[]{}|/".contains(other) {
                    return Err(format!("invalid escape '\\{other}'"));
                }
                self.lit_char(other)
            }
        })
    }

    /// `\x` inside a char class → a class item
    fn class_escape(&mut self) -> Result<ClassItem, String> {
        let c = self.next().ok_or("trailing backslash")?;
        Ok(match c {
            'd' => ClassItem::Pre(Pre::Digit),
            'D' => ClassItem::Pre(Pre::NotDigit),
            'w' => ClassItem::Pre(Pre::Word),
            'W' => ClassItem::Pre(Pre::NotWord),
            's' => ClassItem::Pre(Pre::Space),
            'S' => ClassItem::Pre(Pre::NotSpace),
            'b' => ClassItem::Ch(0x08), // backspace inside []
            'n' => ClassItem::Ch(0x0A),
            't' => ClassItem::Ch(0x09),
            'r' => ClassItem::Ch(0x0D),
            'f' => ClassItem::Ch(0x0C),
            'v' => ClassItem::Ch(0x0B),
            '0' => {
                if self.unicode && self.peek().is_some_and(|d| d.is_ascii_digit()) {
                    return Err("octal escapes are not allowed in unicode mode".into());
                }
                ClassItem::Ch(self.octal_after(0))
            }
            'x' => {
                let save = self.i;
                match self.hex(2) {
                    Some(v) => ClassItem::Ch(v),
                    None if self.unicode => {
                        return Err("invalid \\x escape".into());
                    }
                    None => {
                        self.i = save;
                        ClassItem::Ch('x' as u32)
                    }
                }
            }
            'u' => {
                let save = self.i;
                match self.uesc_opt() {
                    Some(v) => ClassItem::Ch(v),
                    None if self.unicode => {
                        return Err("invalid \\u escape".into());
                    }
                    None => {
                        self.i = save;
                        ClassItem::Ch('u' as u32)
                    }
                }
            }
            'c' => match self.peek() {
                Some(l) if l.is_ascii_alphabetic() => {
                    self.i += 1;
                    ClassItem::Ch((l as u32) % 32)
                }
                _ if self.unicode => {
                    return Err("invalid \\c escape".into());
                }
                // Annex B: inside a class, `\c_` / `\c<digit>` are control
                // chars formed from the second char's codepoint % 32
                Some(l) if l == '_' || l.is_ascii_digit() => {
                    self.i += 1;
                    ClassItem::Ch((l as u32) % 32)
                }
                _ => ClassItem::Ch('c' as u32),
            },
            '1'..='9' => {
                if self.unicode {
                    return Err(format!("invalid class escape \\{c}"));
                }
                if c >= '8' {
                    // `[\8]` `[\9]` — NonOctalDecimalEscape → literal digit
                    return Ok(ClassItem::Ch(c as u32));
                }
                // legacy octal inside class — `0`-`3` take up to 3 digits,
                // `4`-`7` only 2 (spec LegacyOctalEscapeSequence)
                let cap = if c <= '3' { 3 } else { 2 };
                let mut v = c.to_digit(10).unwrap();
                for _ in 1..cap {
                    match self.peek().and_then(|d| d.to_digit(8)) {
                        Some(d) => {
                            v = v * 8 + d;
                            self.i += 1;
                        }
                        None => break,
                    }
                }
                ClassItem::Ch(v)
            }
            'k' if self.unicode => {
                // \k is only an atom escape; inside a class it's an
                // invalid escape in unicode mode
                return Err("invalid class escape \\k".into());
            }
            other => {
                if self.unicode && !"^$\\.*+?()[]{}|/-".contains(other) {
                    return Err(format!("invalid class escape '\\{other}'"));
                }
                self.lit_class(other)
            }
        })
    }

    /// additional octal digits after a leading `\0`
    fn octal_after(&mut self, v: u32) -> u32 {
        let mut v = v;
        for _ in 0..2 {
            match self.peek().and_then(|d| d.to_digit(8)) {
                Some(d) => {
                    v = v * 8 + d;
                    self.i += 1;
                }
                None => break,
            }
        }
        v
    }

    /// exactly `n` hex digits → the code unit value (≤ 0xFFFF)
    fn hex(&mut self, n: usize) -> Option<u32> {
        let mut v: u32 = 0;
        for _ in 0..n {
            let d = self.peek()?.to_digit(16)?;
            v = v * 16 + d;
            self.i += 1;
        }
        Some(v)
    }

    /// `\uNNNN`, `\u{NNNNN}` (unicode patterns only), and in unicode
    /// patterns `\uD800`-style lead escapes combine with a following
    /// `\uDC00`-style trail escape into one code point. Returns None for
    /// malformed or out-of-range escapes (callers decide leniency).
    fn uesc_opt(&mut self) -> Option<u32> {
        if self.unicode && self.eat('{') {
            let mut v: u32 = 0;
            let mut any = false;
            while let Some(d) = self.peek().and_then(|c| c.to_digit(16)) {
                any = true;
                v = v.saturating_mul(16).saturating_add(d);
                self.i += 1;
            }
            if !self.eat('}') || !any {
                return None;
            }
            if v > 0x10FFFF {
                return None;
            }
            Some(v)
        } else {
            let v = self.hex(4)?;
            // \uLead \uTrail → a single code point (unicode patterns only)
            if self.unicode
                && (0xD800..0xDC00).contains(&v)
                && self.peek() == Some('\\')
                && self.peek2() == Some('u')
            {
                let save = self.i;
                self.i += 2;
                if let Some(t) = self.hex(4) {
                    if (0xDC00..0xE000).contains(&t) {
                        return Some(0x10000 + ((v - 0xD800) << 10) + (t - 0xDC00));
                    }
                }
                self.i = save;
            }
            Some(v)
        }
    }

    fn class_atom(&mut self) -> Result<ClassItem, String> {
        let c = self.next().ok_or("unterminated character class")?;
        if c == '\\' {
            self.class_escape()
        } else {
            Ok(self.lit_class(c))
        }
    }

    fn class(&mut self) -> Result<Node, String> {
        let neg = self.eat('^');
        let mut items = Vec::new();
        loop {
            let a = if let Some(u) = self.pending_class.take() {
                ClassItem::Ch(u)
            } else {
                match self.peek() {
                    None => return Err("unterminated character class".into()),
                    Some(']') => {
                        self.i += 1;
                        break;
                    }
                    _ => {
                        let it = self.class_atom()?;
                        if self.pending_class.is_some() {
                            // astral literal split — the `-` (if any)
                            // binds to the pending low half
                            items.push(it);
                            continue;
                        }
                        it
                    }
                }
            };
            // range? `a-z` — only when both sides are single chars and the
            // `-` isn't trailing (before `]`)
            if self.peek() == Some('-') && self.peek2().is_some() && self.peek2() != Some(']') {
                self.i += 1; // '-'
                let b = self.class_atom()?;
                match (a, b) {
                    (ClassItem::Ch(lo), ClassItem::Ch(hi)) => {
                        if hi < lo {
                            return Err("range out of order in character class".into());
                        }
                        items.push(ClassItem::Range(lo, hi));
                    }
                    (a, b) => {
                        if self.unicode {
                            // `[\d-z]` — invalid range with the `u` flag
                            return Err("invalid character class range".into());
                        }
                        // `[\d-z]` — not a real range; keep '-' literal
                        items.push(a);
                        items.push(ClassItem::Ch('-' as u32));
                        items.push(b);
                    }
                }
            } else {
                items.push(a);
            }
        }
        Ok(Node::Class { neg, items })
    }
}

fn pre_node(p: Pre) -> Node {
    Node::Class {
        neg: false,
        items: vec![ClassItem::Pre(p)],
    }
}

/// Count capture groups `(`, `(` not followed by `?`, and `(?<name>` —
/// skipping escapes and `[...]` classes. Returns (count, has_named) —
/// used to disambiguate `\1` as a backreference vs a legacy octal escape
/// and `\k<name>` as a named backref vs Annex-B literal text.
fn count_capture_groups(src: &str) -> (usize, bool) {
    let s: Vec<char> = src.chars().collect();
    let mut n = 0;
    let mut named = false;
    let mut i = 0;
    let mut in_class = false;
    while i < s.len() {
        match s[i] {
            '\\' => i += 2,
            '[' => {
                in_class = true;
                i += 1;
            }
            ']' => {
                in_class = false;
                i += 1;
            }
            '(' if !in_class => {
                // captures: `(` not followed by `?`, and `(?<name>` —
                // excluding `(?<=` / `(?<!` lookbehinds
                let g = match s.get(i + 1) {
                    Some('?') => match s.get(i + 2) {
                        Some('<') => !matches!(s.get(i + 3), Some('=') | Some('!')),
                        _ => false,
                    },
                    _ => true,
                };
                if g {
                    n += 1;
                    if s.get(i + 1) == Some(&'?') {
                        named = true;
                    }
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    (n, named)
}

/// Resolve `\k<name>` backreferences to numeric group indices. Annex-B
/// leniency: in non-unicode patterns with NO named groups at all, `\k<name>`
/// is just literal text (`k`, `<`, name chars, `>`) — those never reach
/// here because the parser emits literal atoms for them.
fn resolve_names(n: Node, names: &[(String, usize)], unicode: bool) -> Result<Node, String> {
    Ok(match n {
        Node::BackrefName(name, raw) => match names.iter().find(|(nm, _)| *nm == name) {
            Some((_, idx)) => Node::Backref(*idx as u32),
            None => {
                if !unicode && names.is_empty() {
                    let mut items = vec![lit_node('k', unicode), lit_node('<', unicode)];
                    items.extend(raw.chars().map(|c| lit_node(c, unicode)));
                    items.push(lit_node('>', unicode));
                    Node::Seq(items)
                } else {
                    return Err(format!("unknown group name '{name}'"));
                }
            }
        },
        Node::Seq(items) => Node::Seq(
            items
                .into_iter()
                .map(|x| resolve_names(x, names, unicode))
                .collect::<Result<_, _>>()?,
        ),
        Node::Alt(arms) => Node::Alt(
            arms.into_iter()
                .map(|x| resolve_names(x, names, unicode))
                .collect::<Result<_, _>>()?,
        ),
        Node::Rep {
            inner,
            min,
            max,
            lazy,
        } => Node::Rep {
            inner: Box::new(resolve_names(*inner, names, unicode)?),
            min,
            max,
            lazy,
        },
        Node::Group { idx, inner } => Node::Group {
            idx,
            inner: Box::new(resolve_names(*inner, names, unicode)?),
        },
        Node::Look { inner, neg, behind } => Node::Look {
            inner: Box::new(resolve_names(*inner, names, unicode)?),
            neg,
            behind,
        },
        other => other,
    })
}

/// Unicode ID_Start: `char::is_alphabetic` approximates the Letters/Nl
/// categories; Other_ID_Start chars are added explicitly.
fn is_id_start_cp(cp: u32) -> bool {
    if cp == '$' as u32 || cp == '_' as u32 {
        return true;
    }
    let Some(c) = char::from_u32(cp) else {
        return false;
    };
    c.is_alphabetic()
        || matches!(
            c,
            '\u{1885}' | '\u{1886}' | '\u{2118}' | '\u{212e}' | '\u{309b}' | '\u{309c}'
        )
}

/// Unicode ID_Continue: ID_Start + digits + combining marks + connector
/// punctuation + ZWNJ/ZWJ (approximated via the common mark ranges).
fn is_id_continue_cp(cp: u32) -> bool {
    if cp == 0x200C || cp == 0x200D {
        return true;
    }
    if is_id_start_cp(cp) {
        return true;
    }
    let Some(c) = char::from_u32(cp) else {
        return false;
    };
    c.is_numeric() || is_combining_mark(c)
}

/// Mn/Mc/Pc — combining marks and connector punctuation (broad ranges
/// covering the common scripts; enough for identifier validation).
fn is_combining_mark(c: char) -> bool {
    matches!(c,
        '\u{0300}'..='\u{036f}' | '\u{0483}'..='\u{0489}' | '\u{0591}'..='\u{05bd}'
        | '\u{05bf}'..='\u{05c2}' | '\u{05c4}'..='\u{05c5}' | '\u{05c7}'
        | '\u{0610}'..='\u{061a}' | '\u{064b}'..='\u{065f}' | '\u{0670}'
        | '\u{06d6}'..='\u{06e4}' | '\u{06e7}'..='\u{06e8}' | '\u{06ea}'..='\u{06ed}'
        | '\u{0711}' | '\u{0730}'..='\u{074a}' | '\u{07a6}'..='\u{07b0}'
        | '\u{07eb}'..='\u{07f3}' | '\u{0816}'..='\u{0819}' | '\u{081b}'..='\u{0823}'
        | '\u{0825}'..='\u{0827}' | '\u{0829}'..='\u{082d}' | '\u{0859}'..='\u{085b}'
        | '\u{08d3}'..='\u{0902}' | '\u{093a}' | '\u{093c}' | '\u{0941}'..='\u{0948}'
        | '\u{094d}' | '\u{0951}'..='\u{0957}' | '\u{0962}'..='\u{0963}'
        | '\u{0981}' | '\u{09bc}' | '\u{09c1}'..='\u{09c4}' | '\u{09cd}'
        | '\u{09e2}'..='\u{09e3}' | '\u{0a01}'..='\u{0a02}' | '\u{0a3c}'
        | '\u{0a41}'..='\u{0a42}' | '\u{0a47}'..='\u{0a48}' | '\u{0a4b}'..='\u{0a4d}'
        | '\u{0a51}' | '\u{0a70}'..='\u{0a71}' | '\u{0a75}' | '\u{0a81}'..='\u{0a82}'
        | '\u{0abc}' | '\u{0ac1}'..='\u{0ac5}' | '\u{0ac7}'..='\u{0ac8}' | '\u{0acd}'
        | '\u{0ae2}'..='\u{0ae3}' | '\u{0afa}'..='\u{0aff}' | '\u{0b01}' | '\u{0b3c}'
        | '\u{0b3f}' | '\u{0b41}'..='\u{0b44}' | '\u{0b4d}' | '\u{0b56}'
        | '\u{0b62}'..='\u{0b63}' | '\u{0b82}' | '\u{0bc0}' | '\u{0bcd}'
        | '\u{0c00}' | '\u{0c04}' | '\u{0c3e}'..='\u{0c40}' | '\u{0c46}'..='\u{0c48}'
        | '\u{0c4a}'..='\u{0c4d}' | '\u{0c55}'..='\u{0c56}' | '\u{0c62}'..='\u{0c63}'
        | '\u{0c81}' | '\u{0cbc}' | '\u{0cbf}' | '\u{0cc6}' | '\u{0ccc}'..='\u{0ccd}'
        | '\u{0ce2}'..='\u{0ce3}' | '\u{0d00}'..='\u{0d01}' | '\u{0d3b}'..='\u{0d3c}'
        | '\u{0d41}'..='\u{0d44}' | '\u{0d4d}' | '\u{0d62}'..='\u{0d63}'
        | '\u{0d81}' | '\u{0dca}' | '\u{0dd2}'..='\u{0dd4}' | '\u{0dd6}'
        | '\u{0e31}' | '\u{0e34}'..='\u{0e3a}' | '\u{0e47}'..='\u{0e4e}'
        | '\u{0eb1}' | '\u{0eb4}'..='\u{0ebc}' | '\u{0ec8}'..='\u{0ecd}'
        | '\u{0f18}'..='\u{0f19}' | '\u{0f35}' | '\u{0f37}' | '\u{0f39}'
        | '\u{0f71}'..='\u{0f7e}' | '\u{0f80}'..='\u{0f84}' | '\u{0f86}'..='\u{0f87}'
        | '\u{0f8d}'..='\u{0f97}' | '\u{0f99}'..='\u{0fbc}' | '\u{0fc6}'
        | '\u{102d}'..='\u{1030}' | '\u{1032}'..='\u{1037}' | '\u{1039}'..='\u{103a}'
        | '\u{103d}'..='\u{103e}' | '\u{1058}'..='\u{1059}' | '\u{105e}'..='\u{1060}'
        | '\u{1071}'..='\u{1074}' | '\u{1082}' | '\u{1085}'..='\u{1086}' | '\u{108d}'
        | '\u{109d}' | '\u{135d}'..='\u{135f}' | '\u{1712}'..='\u{1714}'
        | '\u{1732}'..='\u{1734}' | '\u{1752}'..='\u{1753}' | '\u{1772}'..='\u{1773}'
        | '\u{17b4}'..='\u{17b5}' | '\u{17b7}'..='\u{17bd}' | '\u{17c6}'
        | '\u{17c9}'..='\u{17d3}' | '\u{17dd}' | '\u{180b}'..='\u{180e}'
        | '\u{18a9}' | '\u{1920}'..='\u{1922}' | '\u{1927}'..='\u{1928}' | '\u{1932}'
        | '\u{1939}'..='\u{193b}' | '\u{1a17}'..='\u{1a18}' | '\u{1a1b}' | '\u{1a56}'
        | '\u{1a58}'..='\u{1a5e}' | '\u{1a60}' | '\u{1a62}' | '\u{1a65}'..='\u{1a6c}'
        | '\u{1a73}'..='\u{1a7c}' | '\u{1a7f}' | '\u{1ab0}'..='\u{1ace}'
        | '\u{1b00}'..='\u{1b03}' | '\u{1b34}' | '\u{1b36}'..='\u{1b3a}' | '\u{1b3c}'
        | '\u{1b42}' | '\u{1b6b}'..='\u{1b73}' | '\u{1b80}'..='\u{1b81}'
        | '\u{1ba2}'..='\u{1ba5}' | '\u{1ba8}'..='\u{1ba9}' | '\u{1bab}'..='\u{1bad}'
        | '\u{1be6}' | '\u{1be8}'..='\u{1be9}' | '\u{1bed}' | '\u{1bef}'..='\u{1bf1}'
        | '\u{1c2c}'..='\u{1c33}' | '\u{1c36}'..='\u{1c37}' | '\u{1cd0}'..='\u{1cd2}'
        | '\u{1cd4}'..='\u{1ce0}' | '\u{1ce2}'..='\u{1ce8}' | '\u{1ced}' | '\u{1cf4}'
        | '\u{1cf8}'..='\u{1cf9}' | '\u{1dc0}'..='\u{1dff}' | '\u{203f}'..='\u{2040}'
        | '\u{2054}' | '\u{20d0}'..='\u{20f0}' | '\u{2cef}'..='\u{2cf1}' | '\u{2d7f}'
        | '\u{2de0}'..='\u{2dff}' | '\u{302a}'..='\u{302f}' | '\u{3099}'..='\u{309a}'
        | '\u{a66f}'..='\u{a672}' | '\u{a674}'..='\u{a67d}' | '\u{a69e}'..='\u{a69f}'
        | '\u{a6f0}'..='\u{a6f1}' | '\u{a802}' | '\u{a806}' | '\u{a80b}'
        | '\u{a825}'..='\u{a826}' | '\u{a82c}' | '\u{a8c4}'..='\u{a8c5}'
        | '\u{a8e0}'..='\u{a8f1}' | '\u{a8ff}' | '\u{a926}'..='\u{a92d}'
        | '\u{a947}'..='\u{a951}' | '\u{a980}'..='\u{a982}' | '\u{a9b3}'
        | '\u{a9b6}'..='\u{a9b9}' | '\u{a9bc}'..='\u{a9bd}' | '\u{a9e5}'
        | '\u{aa29}'..='\u{aa2e}' | '\u{aa31}'..='\u{aa32}' | '\u{aa35}'..='\u{aa36}'
        | '\u{aa43}' | '\u{aa4c}' | '\u{aa7c}' | '\u{aab0}' | '\u{aab2}'..='\u{aab4}'
        | '\u{aab7}'..='\u{aab8}' | '\u{aabe}'..='\u{aabf}' | '\u{aac1}'
        | '\u{aaec}'..='\u{aaed}' | '\u{aaf6}' | '\u{abe5}' | '\u{abe8}' | '\u{abed}'
        | '\u{fb1e}' | '\u{fe00}'..='\u{fe0f}' | '\u{fe20}'..='\u{fe2f}'
        | '\u{fe33}'..='\u{fe34}' | '\u{fe4d}'..='\u{fe4f}' | '\u{ff3f}'
        | '\u{101fd}' | '\u{102e0}' | '\u{10376}'..='\u{1037a}'
        | '\u{10a01}'..='\u{10a03}' | '\u{10a05}'..='\u{10a06}' | '\u{10a0c}'..='\u{10a0f}'
        | '\u{10a38}'..='\u{10a3a}' | '\u{10a3f}' | '\u{10ae5}'..='\u{10ae6}'
        | '\u{10d24}'..='\u{10d27}' | '\u{10eab}'..='\u{10eac}' | '\u{10f46}'..='\u{10f50}'
        | '\u{10f82}'..='\u{10f85}' | '\u{11001}' | '\u{11038}'..='\u{11046}'
        | '\u{11070}' | '\u{11073}'..='\u{11074}' | '\u{1107f}'..='\u{11081}'
        | '\u{110b3}'..='\u{110b6}' | '\u{110b9}'..='\u{110ba}' | '\u{11100}'..='\u{11102}'
        | '\u{11127}'..='\u{1112b}' | '\u{1112d}'..='\u{11134}' | '\u{11173}'
        | '\u{11180}'..='\u{11181}' | '\u{111b6}'..='\u{111be}' | '\u{111c9}'..='\u{111cc}'
        | '\u{111cf}' | '\u{1122f}'..='\u{11231}' | '\u{11234}' | '\u{11236}'..='\u{11237}'
        | '\u{1123e}' | '\u{112df}' | '\u{112e3}'..='\u{112ea}' | '\u{11300}'..='\u{11301}'
        | '\u{1133b}'..='\u{1133c}' | '\u{11340}' | '\u{11366}'..='\u{1136c}'
        | '\u{11370}'..='\u{11374}' | '\u{11438}'..='\u{1143f}' | '\u{11442}'..='\u{11444}'
        | '\u{11446}' | '\u{1145e}' | '\u{114b3}'..='\u{114b8}' | '\u{114ba}'
        | '\u{114bf}'..='\u{114c0}' | '\u{114c2}'..='\u{114c3}' | '\u{115b2}'..='\u{115b5}'
        | '\u{115bc}'..='\u{115bd}' | '\u{115bf}'..='\u{115c0}' | '\u{115dc}'..='\u{115dd}'
        | '\u{11633}'..='\u{1163a}' | '\u{1163d}' | '\u{1163f}'..='\u{11640}' | '\u{116ab}'
        | '\u{116ad}' | '\u{116b0}'..='\u{116b5}' | '\u{116b7}' | '\u{1171d}'..='\u{1171f}'
        | '\u{11722}'..='\u{11725}' | '\u{11727}'..='\u{1172b}' | '\u{1182f}'..='\u{11837}'
        | '\u{11839}'..='\u{1183a}' | '\u{1193b}'..='\u{1193c}' | '\u{1193e}' | '\u{11943}'
        | '\u{119d4}'..='\u{119d7}' | '\u{119da}'..='\u{119db}' | '\u{119e0}'
        | '\u{11a01}'..='\u{11a0a}' | '\u{11a33}'..='\u{11a38}' | '\u{11a3b}'..='\u{11a3e}'
        | '\u{11a47}' | '\u{11a51}'..='\u{11a56}' | '\u{11a59}'..='\u{11a5b}'
        | '\u{11a8a}'..='\u{11a96}' | '\u{11a98}'..='\u{11a99}' | '\u{11c30}'..='\u{11c36}'
        | '\u{11c38}'..='\u{11c3d}' | '\u{11c3f}' | '\u{11c92}'..='\u{11ca7}'
        | '\u{11caa}'..='\u{11cb0}' | '\u{11cb2}'..='\u{11cb3}' | '\u{11cb5}'..='\u{11cb6}'
        | '\u{11d31}'..='\u{11d36}' | '\u{11d3a}' | '\u{11d3c}'..='\u{11d3d}'
        | '\u{11d3f}'..='\u{11d45}' | '\u{11d47}' | '\u{11d90}'..='\u{11d91}' | '\u{11d95}'
        | '\u{11d97}' | '\u{11ef3}'..='\u{11ef4}' | '\u{16af0}'..='\u{16af4}'
        | '\u{16b30}'..='\u{16b36}' | '\u{16f4f}' | '\u{16f8f}'..='\u{16f92}' | '\u{16fe4}'
        | '\u{1bc9d}'..='\u{1bc9e}' | '\u{1cf00}'..='\u{1cf2d}' | '\u{1cf30}'..='\u{1cf46}'
        | '\u{1d165}' | '\u{1d167}'..='\u{1d169}' | '\u{1d173}'..='\u{1d17b}'
        | '\u{1d17e}'..='\u{1d182}' | '\u{1d185}'..='\u{1d18b}' | '\u{1d1aa}'..='\u{1d1ad}'
        | '\u{1d242}'..='\u{1d244}' | '\u{1da00}'..='\u{1da36}' | '\u{1da3b}'..='\u{1da6c}'
        | '\u{1da75}' | '\u{1da84}' | '\u{1da9b}'..='\u{1da9f}' | '\u{1daa1}'..='\u{1daaf}'
        | '\u{1e000}'..='\u{1e006}' | '\u{1e008}'..='\u{1e018}' | '\u{1e01b}'..='\u{1e021}'
        | '\u{1e023}'..='\u{1e024}' | '\u{1e026}'..='\u{1e02a}' | '\u{1e130}'..='\u{1e136}'
        | '\u{1e2ae}' | '\u{1e2ec}'..='\u{1e2ef}' | '\u{1e8d0}'..='\u{1e8d6}'
        | '\u{1e944}'..='\u{1e94a}' | '\u{e0100}'..='\u{e01ef}'
    )
}

// ===========================================================================
// matcher
// ===========================================================================

fn is_hi(u: u16) -> bool {
    (0xD800..0xDC00).contains(&u)
}
fn is_lo(u: u16) -> bool {
    (0xDC00..0xE000).contains(&u)
}
fn join_pair(hi: u32, lo: u32) -> u32 {
    0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
}

struct Ctx<'h> {
    s: &'h [u16],
    caps: Vec<Option<(usize, usize)>>,
    steps: u64,
    depth: u32,
    aborted: bool,
    icase: bool,
    multiline: bool,
    dotall: bool,
    /// match at code-point granularity — surrogate pairs are single chars
    umatch: bool,
    /// set by the top-level continuation when the whole pattern matched
    end: usize,
}

type K<'a, 'h> = &'a mut dyn FnMut(&mut Ctx<'h>, usize) -> bool;

/// decode the "character" starting at `pos` — in unicode mode a surrogate
/// pair is one code point consuming 2 units. Returns (code point, len).
fn cp_at(c: &Ctx, pos: usize) -> Option<(u32, usize)> {
    let u = *c.s.get(pos)? as u32;
    if c.umatch && (0xD800..0xDC00).contains(&u) {
        if let Some(&lo) = c.s.get(pos + 1) {
            if (0xDC00..0xE000).contains(&(lo as u32)) {
                return Some((join_pair(u, lo as u32), 2));
            }
        }
    }
    Some((u, 1))
}

/// decode the "character" ending just before `pos` — for lookbehind's
/// backward matching
fn cp_before(c: &Ctx, pos: usize) -> Option<(u32, usize)> {
    let u = *c.s.get(pos.checked_sub(1)?)? as u32;
    if c.umatch && (0xDC00..0xE000).contains(&u) && pos >= 2 {
        let hi = c.s[pos - 2] as u32;
        if (0xD800..0xDC00).contains(&hi) {
            return Some((join_pair(hi, u), 2));
        }
    }
    Some((u, 1))
}

fn mat<'h>(c: &mut Ctx<'h>, n: &Node, pos: usize, k: K<'_, 'h>) -> bool {
    if c.aborted {
        return false;
    }
    c.steps += 1;
    if c.steps > MAX_STEPS {
        c.aborted = true;
        return false;
    }
    match n {
        Node::Empty => k(c, pos),
        Node::Ch(_) | Node::Any | Node::Class { .. } => {
            let l = match_len_fwd(c, n, pos);
            l > 0 && k(c, pos + l)
        }
        Node::Seq(items) => seq(c, items, pos, k),
        Node::Alt(arms) => {
            for a in arms {
                if mat(c, a, pos, &mut *k) {
                    return true;
                }
            }
            false
        }
        Node::Rep {
            inner,
            min,
            max,
            lazy,
        } => rep(c, inner, pos, 0, *min, *max, *lazy, k),
        Node::Group { idx, inner } => {
            let g = *idx;
            let saved = c.caps[g];
            c.caps[g] = Some((pos, pos));
            let r = mat(c, inner, pos, &mut |c2, e| {
                c2.caps[g] = Some((pos, e));
                k(c2, e)
            });
            if !r {
                c.caps[g] = saved;
            }
            r
        }
        Node::Backref(g) => match c.caps.get(*g as usize).copied().flatten() {
            Some((a, b)) => {
                let l = b - a;
                pos + l <= c.s.len() && units_eq(c, pos, a, l) && k(c, pos + l)
            }
            // unmatched group → backref matches empty
            None => k(c, pos),
        },
        Node::BackrefName(..) => false, // resolved at compile
        Node::Anchor(a) => {
            let ok = match a {
                Anchor::Start => pos == 0 || (c.multiline && is_lineterm_cp(c.s[pos - 1] as u32)),
                Anchor::End => {
                    pos == c.s.len()
                        || (c.multiline && pos < c.s.len() && is_lineterm_cp(c.s[pos] as u32))
                }
                Anchor::WordB => at_wordb(c, pos),
                Anchor::NotWordB => !at_wordb(c, pos),
            };
            ok && k(c, pos)
        }
        Node::Look { inner, neg, behind } => {
            let saved = c.caps.clone();
            let ok = if *behind {
                // lookbehind matches right-to-left ending at pos — the
                // captures land left-to-right via the backward matcher
                mat_bwd(c, inner, pos, &mut |_c2, _e| true)
            } else {
                mat(c, inner, pos, &mut |_c2, _e| true)
            };
            let proceed = ok != *neg;
            if !ok || *neg {
                c.caps = saved;
            }
            proceed && k(c, pos)
        }
    }
}

/// backward matcher — used for `(?<=)`/`(?<!)`: `pos` is the right edge
/// and the continuation receives the new (smaller) position. Captures
/// are still recorded left-to-right.
fn mat_bwd<'h>(c: &mut Ctx<'h>, n: &Node, pos: usize, k: K<'_, 'h>) -> bool {
    if c.aborted {
        return false;
    }
    c.steps += 1;
    if c.steps > MAX_STEPS {
        c.aborted = true;
        return false;
    }
    match n {
        Node::Empty => k(c, pos),
        Node::Ch(_) | Node::Any | Node::Class { .. } => {
            let l = match_len_bwd(c, n, pos);
            l > 0 && k(c, pos - l)
        }
        Node::Seq(items) => seq_bwd(c, items, pos, k),
        Node::Alt(arms) => {
            for a in arms {
                if mat_bwd(c, a, pos, &mut *k) {
                    return true;
                }
            }
            false
        }
        Node::Rep {
            inner,
            min,
            max,
            lazy,
        } => rep_bwd(c, inner, pos, 0, *min, *max, *lazy, k),
        Node::Group { idx, inner } => {
            let g = *idx;
            let saved = c.caps[g];
            c.caps[g] = Some((pos, pos));
            let r = mat_bwd(c, inner, pos, &mut |c2, p| {
                c2.caps[g] = Some((p, pos));
                k(c2, p)
            });
            if !r {
                c.caps[g] = saved;
            }
            r
        }
        Node::Backref(g) => match c.caps.get(*g as usize).copied().flatten() {
            Some((a, b)) => {
                let l = b - a;
                pos >= l && units_eq(c, pos - l, a, l) && k(c, pos - l)
            }
            None => k(c, pos),
        },
        Node::BackrefName(..) => false,
        Node::Anchor(a) => {
            let ok = match a {
                Anchor::Start => pos == 0 || (c.multiline && is_lineterm_cp(c.s[pos - 1] as u32)),
                Anchor::End => {
                    pos == c.s.len()
                        || (c.multiline && pos < c.s.len() && is_lineterm_cp(c.s[pos] as u32))
                }
                Anchor::WordB => at_wordb(c, pos),
                Anchor::NotWordB => !at_wordb(c, pos),
            };
            ok && k(c, pos)
        }
        Node::Look { inner, neg, behind } => {
            let saved = c.caps.clone();
            let ok = if *behind {
                mat_bwd(c, inner, pos, &mut |_c2, _e| true)
            } else {
                mat(c, inner, pos, &mut |_c2, _e| true)
            };
            let proceed = ok != *neg;
            if !ok || *neg {
                c.caps = saved;
            }
            proceed && k(c, pos)
        }
    }
}

fn seq<'h>(c: &mut Ctx<'h>, items: &[Node], pos: usize, k: K<'_, 'h>) -> bool {
    if c.aborted {
        return false;
    }
    match items.split_first() {
        None => k(c, pos),
        Some((h, t)) => mat(c, h, pos, &mut |c2, p| seq(c2, t, p, &mut *k)),
    }
}

/// sequence matched right-to-left — last item first
fn seq_bwd<'h>(c: &mut Ctx<'h>, items: &[Node], pos: usize, k: K<'_, 'h>) -> bool {
    if c.aborted {
        return false;
    }
    match items.split_last() {
        None => k(c, pos),
        Some((h, t)) => mat_bwd(c, h, pos, &mut |c2, p| seq_bwd(c2, t, p, &mut *k)),
    }
}

/// A node that consumes exactly one char and has no side effects — these
/// get a fast iterative repetition path (avoids deep recursion for `.*`
/// over long inputs).
fn is_simple(n: &Node) -> bool {
    match n {
        Node::Ch(_) | Node::Any | Node::Class { .. } => true,
        Node::Seq(items) => items.len() == 1 && is_simple(&items[0]),
        Node::Alt(arms) => arms.iter().all(is_simple),
        _ => false,
    }
}

/// units consumed when single-char node `n` matches starting at `pos`
/// (0 = no match). Only valid for Ch/Any/Class nodes.
fn match_len_fwd(c: &Ctx, n: &Node, pos: usize) -> usize {
    let Some((cp, len)) = cp_at(c, pos) else {
        return 0;
    };
    let ok = match n {
        Node::Ch(w) => ch_eq_cp(cp, *w, c.icase, c.umatch),
        Node::Any => c.dotall || !is_lineterm_cp(cp),
        Node::Class { neg, items } => class_match(items, cp, c.icase, c.umatch) != *neg,
        _ => return 0,
    };
    if ok {
        len
    } else {
        0
    }
}

/// same, ending just before `pos` (backward)
fn match_len_bwd(c: &Ctx, n: &Node, pos: usize) -> usize {
    let Some((cp, len)) = cp_before(c, pos) else {
        return 0;
    };
    let ok = match n {
        Node::Ch(w) => ch_eq_cp(cp, *w, c.icase, c.umatch),
        Node::Any => c.dotall || !is_lineterm_cp(cp),
        Node::Class { neg, items } => class_match(items, cp, c.icase, c.umatch) != *neg,
        _ => return 0,
    };
    if ok {
        len
    } else {
        0
    }
}

/// `match_len_fwd` unwrapping the Seq-of-1/Alt shapes `is_simple` allows
fn one_match_fwd(c: &Ctx, n: &Node, pos: usize) -> usize {
    match n {
        Node::Seq(items) if items.len() == 1 => one_match_fwd(c, &items[0], pos),
        Node::Alt(arms) => {
            for a in arms {
                let l = one_match_fwd(c, a, pos);
                if l > 0 {
                    return l;
                }
            }
            0
        }
        _ => match_len_fwd(c, n, pos),
    }
}

fn one_match_bwd(c: &Ctx, n: &Node, pos: usize) -> usize {
    match n {
        Node::Seq(items) if items.len() == 1 => one_match_bwd(c, &items[0], pos),
        Node::Alt(arms) => {
            for a in arms {
                let l = one_match_bwd(c, a, pos);
                if l > 0 {
                    return l;
                }
            }
            0
        }
        _ => match_len_bwd(c, n, pos),
    }
}

/// all capture-group indices inside an atom — RepeatMatcher clears them
/// before every iteration
fn group_ids(n: &Node, out: &mut Vec<usize>) {
    match n {
        Node::Group { idx, inner } => {
            out.push(*idx);
            group_ids(inner, out);
        }
        Node::Seq(items) | Node::Alt(items) => {
            for x in items {
                group_ids(x, out);
            }
        }
        Node::Rep { inner, .. } | Node::Look { inner, .. } => group_ids(inner, out),
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn rep<'h>(
    c: &mut Ctx<'h>,
    inner: &Node,
    pos: usize,
    count: u32,
    min: u32,
    max: u32,
    lazy: bool,
    k: K<'_, 'h>,
) -> bool {
    if c.aborted {
        return false;
    }
    if is_simple(inner) {
        return rep_simple(c, inner, pos, min, max, lazy, k);
    }
    c.depth += 1;
    if c.depth > MAX_DEPTH {
        c.aborted = true;
        c.depth -= 1;
        return false;
    }
    let mut gids = Vec::new();
    group_ids(inner, &mut gids);
    let r = if lazy {
        (count >= min && k(c, pos))
            || (count < max && rep_step(c, inner, &gids, pos, count, min, max, lazy, k))
    } else {
        (count < max && rep_step(c, inner, &gids, pos, count, min, max, lazy, k))
            || (count >= min && k(c, pos))
    };
    c.depth -= 1;
    r
}

/// Match one more iteration of a (non-simple) repetition. The atom's
/// capture groups are cleared first (spec RepeatMatcher step 4–5) and the
/// capture state is restored when the iteration can't complete.
#[allow(clippy::too_many_arguments)]
fn rep_step<'h>(
    c: &mut Ctx<'h>,
    inner: &Node,
    gids: &[usize],
    pos: usize,
    count: u32,
    min: u32,
    max: u32,
    lazy: bool,
    k: K<'_, 'h>,
) -> bool {
    let saved = c.caps.clone();
    for &g in gids {
        if g < c.caps.len() {
            c.caps[g] = None;
        }
    }
    let ok = mat(c, inner, pos, &mut |c2, e| {
        if e == pos {
            // a zero-length iteration is only allowed while required
            // iterations remain (spec: `min = 0 && y.end = x.end` → fail)
            if count >= min {
                return false;
            }
            rep(c2, inner, e, min, min, max, lazy, &mut *k)
        } else {
            rep(c2, inner, e, count + 1, min, max, lazy, &mut *k)
        }
    });
    if !ok {
        c.caps = saved;
    }
    ok
}

/// backward mirrors of rep/rep_step for lookbehind bodies
#[allow(clippy::too_many_arguments)]
fn rep_bwd<'h>(
    c: &mut Ctx<'h>,
    inner: &Node,
    pos: usize,
    count: u32,
    min: u32,
    max: u32,
    lazy: bool,
    k: K<'_, 'h>,
) -> bool {
    if c.aborted {
        return false;
    }
    if is_simple(inner) {
        return rep_simple_bwd(c, inner, pos, min, max, lazy, k);
    }
    c.depth += 1;
    if c.depth > MAX_DEPTH {
        c.aborted = true;
        c.depth -= 1;
        return false;
    }
    let mut gids = Vec::new();
    group_ids(inner, &mut gids);
    let r = if lazy {
        (count >= min && k(c, pos))
            || (count < max && rep_step_bwd(c, inner, &gids, pos, count, min, max, lazy, k))
    } else {
        (count < max && rep_step_bwd(c, inner, &gids, pos, count, min, max, lazy, k))
            || (count >= min && k(c, pos))
    };
    c.depth -= 1;
    r
}

#[allow(clippy::too_many_arguments)]
fn rep_step_bwd<'h>(
    c: &mut Ctx<'h>,
    inner: &Node,
    gids: &[usize],
    pos: usize,
    count: u32,
    min: u32,
    max: u32,
    lazy: bool,
    k: K<'_, 'h>,
) -> bool {
    let saved = c.caps.clone();
    for &g in gids {
        if g < c.caps.len() {
            c.caps[g] = None;
        }
    }
    let ok = mat_bwd(c, inner, pos, &mut |c2, e| {
        if e == pos {
            if count >= min {
                return false;
            }
            rep_bwd(c2, inner, e, min, min, max, lazy, &mut *k)
        } else {
            rep_bwd(c2, inner, e, count + 1, min, max, lazy, &mut *k)
        }
    });
    if !ok {
        c.caps = saved;
    }
    ok
}

/// Greedy/lazy repetition of a single-char consumer — iterative, no
/// recursion. Items may consume 1 or 2 units (astral chars), so the run
/// is tracked as unit positions.
fn rep_simple<'h>(
    c: &mut Ctx<'h>,
    inner: &Node,
    pos: usize,
    min: u32,
    max: u32,
    lazy: bool,
    k: K<'_, 'h>,
) -> bool {
    let mut pts: Vec<usize> = Vec::with_capacity(16);
    pts.push(pos);
    let mut p = pos;
    while (pts.len() as u32) <= max {
        let l = one_match_fwd(c, inner, p);
        if l == 0 {
            break;
        }
        p += l;
        pts.push(p);
    }
    let run = pts.len() - 1;
    if (run as u32) < min {
        return false;
    }
    if lazy {
        for a in (min as usize)..=run {
            if k(c, pts[a]) {
                return true;
            }
            if c.aborted {
                return false;
            }
        }
    } else {
        for a in (min as usize..=run).rev() {
            if k(c, pts[a]) {
                return true;
            }
            if c.aborted {
                return false;
            }
        }
    }
    false
}

fn rep_simple_bwd<'h>(
    c: &mut Ctx<'h>,
    inner: &Node,
    pos: usize,
    min: u32,
    max: u32,
    lazy: bool,
    k: K<'_, 'h>,
) -> bool {
    let mut pts: Vec<usize> = Vec::with_capacity(16);
    pts.push(pos);
    let mut p = pos;
    while (pts.len() as u32) <= max {
        let l = one_match_bwd(c, inner, p);
        if l == 0 {
            break;
        }
        p -= l;
        pts.push(p);
    }
    let run = pts.len() - 1;
    if (run as u32) < min {
        return false;
    }
    // pts[0] = pos (fewest consumed), pts[run] = deepest — greedy takes
    // the longest match first
    if lazy {
        for a in (min as usize)..=run {
            if k(c, pts[a]) {
                return true;
            }
            if c.aborted {
                return false;
            }
        }
    } else {
        for a in (min as usize..=run).rev() {
            if k(c, pts[a]) {
                return true;
            }
            if c.aborted {
                return false;
            }
        }
    }
    false
}

// -- char predicates ---------------------------------------------------------

fn is_lineterm_cp(cp: u32) -> bool {
    matches!(cp, 0x0A | 0x0D | 0x2028 | 0x2029)
}

/// `\w` — ASCII only, even in unicode patterns
fn is_word_cp(cp: u32) -> bool {
    matches!(cp, 0x30..=0x39 | 0x41..=0x5A | 0x5F | 0x61..=0x7A)
}

/// `\b`/`\B` — spec IsWordChar tests the *canonicalized* char under `i`
fn at_wordb(c: &Ctx, pos: usize) -> bool {
    let w = |u: u16| {
        let cp = u as u32;
        is_word_cp(if c.icase { canon_cp(cp, c.umatch) } else { cp })
    };
    let before = pos > 0 && w(c.s[pos - 1]);
    let after = pos < c.s.len() && w(c.s[pos]);
    before != after
}

/// `\s` — exactly the JS WhiteSpace + LineTerminator set
/// (tab, LF, VT, FF, CR, SP, NBSP, ZWSP-ogham, the Zs block 2000–200A,
/// LS, PS, NNBSP, MMSP, IDSP, and BOM/FEFF — notably NOT U+0085 NEL)
fn is_re_space(cp: u32) -> bool {
    matches!(
        cp,
        0x0009 | 0x000A | 0x000B | 0x000C | 0x000D | 0x0020 | 0x00A0 | 0x1680 | 0x2000
            ..=0x200A | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000 | 0xFEFF
    )
}

/// Canonicalize — spec-correct case folding for the `i` flag.
/// Non-unicode patterns uppercase-fold with two exclusions (multi-char
/// results and non-ASCII→ASCII folds are ignored); `u`/`v` patterns use
/// Unicode simple case folding instead.
fn canon_cp(cp: u32, umatch: bool) -> u32 {
    if umatch {
        scf_cp(cp)
    } else {
        canon_nonu(cp)
    }
}

/// spec Canonicalize for non-unicode patterns: `ch.toUpperCase()` unless
/// the result is more than one code unit (`ß`→`SS` stays `ß`) or `ch`'s
/// code point is ≥ 0x80 while the result's is < 0x80 (`ſ`/`ı`/`K` fold
/// to themselves)
fn canon_nonu(cp: u32) -> u32 {
    let Some(c) = char::from_u32(cp) else {
        return cp;
    };
    let mut it = c.to_uppercase();
    let Some(u) = it.next() else {
        return cp;
    };
    if it.next().is_some() {
        return cp;
    }
    let uv = u as u32;
    if cp >= 128 && uv < 128 {
        return cp;
    }
    uv
}

/// Unicode simple case folding (CaseFolding status C/S) — toLowercase
/// covers nearly everything; the explicit entries are the folds that
/// differ from toLowercase: `ſ`/`ς`/`µ`-style variant letters, the Greek
/// lunate sigmas and symbol forms, the Cyrillic small variants, Cherokee
/// smalls (which fold to capitals), and the Unicode-15.1 additions
/// (`ΐ`→`ΐ`, `ΰ`→`ΰ`, `ﬅ`→`ﬆ`).
fn scf_cp(cp: u32) -> u32 {
    match cp {
        0x00B5 => 0x03BC,          // µ micro sign → μ (CaseFolding; no lowercase)
        0x017F => 0x0073,          // ſ → s
        0x0345 => 0x03B9,          // ͅ combining ypogegrammeni → ι
        0x037B..=0x037D => 0x03C3, // ͻ/ͼ/ͽ lunate sigmas → σ
        0x03C2 => 0x03C3,          // ς → σ
        0x03D0 => 0x03B2,          // ϐ → β
        0x03D1 => 0x03B8,          // ϑ → θ
        0x03D5 => 0x03C6,          // ϕ → φ
        0x03D6 => 0x03C0,          // ϖ → π
        0x03F0 => 0x03BA,          // ϰ → κ
        0x03F1 => 0x03C1,          // ϱ → ρ
        0x03F2 => 0x03F9,          // ϲ → Ϲ
        0x03F3 => 0x037F,          // ϳ → Ϳ
        0x03F5 => 0x03B5,          // ϵ → ε
        // Georgian Nuskhuri → Mkhedruli (the fold target — the case
        // pair is actually Asomtavruli, which folds to Nuskhuri instead)
        0x2D00..=0x2D25 => cp - 0x1C30,
        0x2D27 => 0x10F7, // ⴧ yn → ჷ
        // Cyrillic small variants → base letters
        0x1C80 => 0x0432,
        0x1C81 => 0x0434,
        0x1C82 => 0x043E,
        0x1C83 => 0x0441,
        0x1C84 | 0x1C85 => 0x0442,
        0x1C86 => 0x044A,
        0x1C87 => 0x0463,
        0x1C88 => 0xA64A,
        // Cherokee capitals are simple-fold *fixpoints* — the smalls
        // fold up to them, so toLowercase must not be applied here
        0x13A0..=0x13F5 => cp,
        0x13F8..=0x13FD => cp - 8, // Cherokee smalls ᏸ–ᏽ → 13F0–13F5
        0x1E9B => 0x1E61,          // ẛ → ṡ
        // Greek capitals with prosgegrammeni: only *full* (multi-char)
        // folds exist, so the simple fold is the identity — toLowercase
        // would wrongly give the corresponding small letter
        0x1F88..=0x1F8F => cp,
        0x1F98..=0x1F9F => cp,
        0x1FA8..=0x1FAF => cp,
        0x1FBC | 0x1FCC | 0x1FFC => cp,
        0x1FBE => 0x03B9,               // ι prosgegrammeni → ι
        0x214E => 0x2132,               // ⅎ → Ⅎ
        0xA7F8 => 0x0126,               // ꟸ → Ħ
        0xA7F9 => 0x0153,               // ꟹ → œ
        0xAB70..=0xABBF => cp - 0x97D0, // Cherokee smalls ꭰ–ꮿ → 13A0–13EF
        0x1FD3 => 0x0390,               // ΐ → ΐ
        0x1FE3 => 0x03B0,               // ΰ → ΰ
        0xFB05 => 0xFB06,               // ﬅ → ﬆ
        _ => {
            let Some(c) = char::from_u32(cp) else {
                return cp;
            };
            let mut it = c.to_lowercase();
            match it.next() {
                Some(l) if it.next().is_none() => l as u32,
                _ => cp,
            }
        }
    }
}

fn ch_eq_cp(a: u32, b: u32, icase: bool, umatch: bool) -> bool {
    a == b || (icase && canon_cp(a, umatch) == canon_cp(b, umatch))
}

/// compare `l` units at `x` against the units at `y` — plain equality
/// unless `i`, and code-point-wise folding for unicode patterns (a case
/// difference can hide inside an astral char)
fn units_eq(c: &Ctx, x: usize, y: usize, l: usize) -> bool {
    if !c.icase {
        return c.s[x..x + l] == c.s[y..y + l];
    }
    if !c.umatch {
        return (0..l).all(|i| ch_eq_cp(c.s[x + i] as u32, c.s[y + i] as u32, true, false));
    }
    let (mut a, mut b) = (x, y);
    let end = x + l;
    while a < end {
        let Some((ca, la)) = cp_at(c, a) else {
            return false;
        };
        let Some((cb, lb)) = cp_at(c, b) else {
            return false;
        };
        if la != lb || !ch_eq_cp(ca, cb, true, true) {
            return false;
        }
        a += la;
        b += lb;
    }
    a == end
}

fn pre_match(p: Pre, cp: u32) -> bool {
    match p {
        Pre::Digit => (0x30..=0x39).contains(&cp),
        Pre::NotDigit => !(0x30..=0x39).contains(&cp),
        Pre::Word => is_word_cp(cp),
        Pre::NotWord => !is_word_cp(cp),
        Pre::Space => is_re_space(cp),
        Pre::NotSpace => !is_re_space(cp),
    }
}

fn class_match(items: &[ClassItem], cp: u32, icase: bool, umatch: bool) -> bool {
    items.iter().any(|it| match it {
        ClassItem::Ch(w) => ch_eq_cp(cp, *w, icase, umatch),
        ClassItem::Range(lo, hi) => {
            (*lo..=*hi).contains(&cp) || (icase && range_icase_match(*lo, *hi, cp, umatch))
        }
        // `\w`/`\d`/`\s` sets are canonicalized too — under `i` the input
        // char folds into the set (u-mode `ſ`→`s` makes it a word char)
        ClassItem::Pre(p) => {
            let q = if icase { canon_cp(cp, umatch) } else { cp };
            pre_match(*p, q)
        }
    })
}

/// `i`-flag class-range test: the spec canonicalizes every char in
/// `[lo,hi]`, so `cp` matches iff some `c` in the range has
/// `canon(c) == canon(cp)` — i.e. a canonical *pre-image* of the folded
/// input lies in the range. (Folding the two endpoints and testing the
/// folded char between them is wrong: `[W-z]` folds to `[W,Z]+[a-z]+`
/// punctuation, dropping `_`/`` ` `` while picking up `A`–`F` through
/// their lowercase pre-images.)
fn range_icase_match(lo: u32, hi: u32, cp: u32, umatch: bool) -> bool {
    let q = canon_cp(cp, umatch);
    // every candidate is re-verified through `canon` — the tables below
    // are best-effort but only true pre-images (canon(v) == q) count
    let hit = |v: u32| lo <= v && v <= hi && canon_cp(v, umatch) == q;
    // `q` itself, when it is a canonical fixpoint (a char can always
    // pre-image itself)
    if hit(q) {
        return true;
    }
    // `q`'s case-variant — `u` folds toward lowercase so the pre-image
    // is the uppercase form; non-`u` folds toward uppercase so it is
    // the lowercase form
    if let Some(v) = case_variant(q, umatch) {
        if hit(v) {
            return true;
        }
    }
    // chars with asymmetric/special folds that land on `q`
    if fold_preimages(q, umatch).iter().any(|&v| hit(v)) {
        return true;
    }
    // affine fold blocks — scripts whose fold relation jumps by a fixed
    // offset rather than forming an upper/lowercase pair:
    // Cherokee smalls fold *up* to capitals (u-mode only — in non-u mode
    // toUppercase gives the same capital so the case-variant path works);
    // Georgian Nuskhuri simple-folds to Mkhedruli and Asomtavruli to
    // Nuskhuri (u-mode); in non-u mode toUppercase maps Nuskhuri up to
    // Asomtavruli instead
    let mut buf = [u32::MAX; 2];
    let vs: &[u32] = if umatch {
        match q {
            0x13A0..=0x13EF => {
                buf[0] = q + 0x97D0;
                &buf[..1]
            }
            0x13F0..=0x13F5 => {
                buf[0] = q + 8;
                &buf[..1]
            }
            0x10D0..=0x10F5 => {
                buf[0] = q + 0x1C30; // Nuskhuri
                buf[1] = q + 0x7C0; // Mtavruli (harmless when unpaired)
                &buf[..2]
            }
            0x2D00..=0x2D25 => {
                buf[0] = q - 0x1C60; // Asomtavruli
                &buf[..1]
            }
            _ => &[],
        }
    } else {
        match q {
            // toUppercase maps Nuskhuri to Asomtavruli (UnicodeData) —
            // the guard keeps only mappings that actually apply
            0x10A0..=0x10C5 => {
                buf[0] = q + 0x1C60;
                &buf[..1]
            }
            0x10D0..=0x10F5 => {
                buf[0] = q + 0x1C30;
                &buf[..1]
            }
            _ => &[],
        }
    };
    vs.iter().any(|&v| hit(v))
}

/// the single-char case-variant of `q` used as a canonical pre-image:
/// `toUppercase` for `u`-mode folding, `toLowercase` for non-`u`
fn case_variant(q: u32, umatch: bool) -> Option<u32> {
    let c = char::from_u32(q)?;
    let v = if umatch {
        let mut it = c.to_uppercase();
        let v = it.next()?;
        if it.next().is_some() {
            return None;
        }
        v
    } else {
        let mut it = c.to_lowercase();
        let v = it.next()?;
        if it.next().is_some() {
            return None;
        }
        v
    };
    Some(v as u32)
}

/// Chars `c` outside `q`'s own case pair that canonicalize to `q` —
/// asymmetric folds the case-variant path cannot reach (e.g. `ſ`→`s`,
/// `ς`→`Σ`, `µ`→`Μ`, `ẛ`→`Ṡ`, the Greek symbol forms).
fn fold_preimages(q: u32, umatch: bool) -> &'static [u32] {
    if umatch {
        // c with scf(c) == q (simple-fold pre-images)
        match q {
            0x006B => &[0x212A],
            0x0073 => &[0x017F],
            0x00DF => &[0x1E9E],
            0x00E5 => &[0x212B],
            0x0126 => &[0xA7F8],
            0x0153 => &[0xA7F9],
            0x01C6 => &[0x01C5],
            0x01C9 => &[0x01C8],
            0x01CC => &[0x01CB],
            0x01F3 => &[0x01F2],
            0x037F => &[0x03F3],
            0x0390 => &[0x1FD3],
            0x03B0 => &[0x1FE3],
            0x03B2 => &[0x03D0],
            0x03B5 => &[0x03F5],
            0x03B8 => &[0x03D1],
            0x03B9 => &[0x0345, 0x1FBE],
            0x03BA => &[0x03F0],
            0x03BC => &[0x00B5],
            0x03C0 => &[0x03D6],
            0x03C1 => &[0x03F1],
            0x03C3 => &[0x03C2, 0x037B, 0x037C, 0x037D],
            0x03C6 => &[0x03D5],
            0x03C9 => &[0x2126],
            0x03F9 => &[0x03F2],
            0x0432 => &[0x1C80],
            0x0434 => &[0x1C81],
            0x043E => &[0x1C82],
            0x0441 => &[0x1C83],
            0x0442 => &[0x1C84, 0x1C85],
            0x044A => &[0x1C86],
            0x0463 => &[0x1C87],
            0x1E61 => &[0x1E9B],
            0x2132 => &[0x214E],
            0xA64A => &[0x1C88],
            0x10F7 => &[0x2D27],
            0xFB06 => &[0xFB05],
            _ => &[],
        }
    } else {
        // c ≥ 0x80 with toUppercase(c) == q (non-u canon pre-images) —
        // non-u canonicalization targets the *uppercase* member, folds
        // to ASCII are excluded by the ≥128 rule in canon_nonu, and the
        // Greek *symbol* letters (03D0/03D1/03D5/03D6/03F0/03F1) are
        // Lu-category: toUppercase maps them to themselves, so they are
        // never pre-images of the capitals (only ϵ/ϲ, which are Ll,
        // qualify)
        match q {
            0x0126 => &[0xA7F8], // ꟸ modifier Ħ
            0x0152 => &[0xA7F9], // ꟹ modifier Œ
            0x01C4 => &[0x01C5],
            0x01C7 => &[0x01C8],
            0x01CA => &[0x01CB],
            0x01F1 => &[0x01F2],
            0x0395 => &[0x03F5],
            0x0399 => &[0x0345, 0x1FBE],
            0x039C => &[0x00B5],
            0x03A3 => &[0x03C2],
            0x03F9 => &[0x03F2],
            0x0412 => &[0x1C80],
            0x0414 => &[0x1C81],
            0x041E => &[0x1C82],
            0x0421 => &[0x1C83],
            0x0422 => &[0x1C84, 0x1C85],
            0x042A => &[0x1C86],
            0x0462 => &[0x1C87],
            0x1E60 => &[0x1E9B],
            0xA64A => &[0x1C88],
            _ => &[],
        }
    }
}
