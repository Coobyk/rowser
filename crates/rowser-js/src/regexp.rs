//! A small backtracking regular-expression engine, built from scratch on
//! std only.
//!
//! The pattern is parsed into an AST (`Node`) and matched by recursive
//! backtracking with continuation-passing over `&[char]` (JS works on
//! UTF-16 code units; we approximate with code points). A global step cap
//! keeps catastrophic backtracking from hanging the engine — when the cap
//! is exceeded the match attempt fails rather than hanging.

use std::rc::Rc;

/// Max backtracking steps per match attempt.
const MAX_STEPS: u64 = 1_000_000;
/// Max recursion depth in the matcher (stack safety on pathological input).
const MAX_DEPTH: u32 = 4_000;

#[derive(Debug)]
pub enum Node {
    Empty,
    Ch(char),
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
    /// `\k<name>` — resolved to Backref after parse
    BackrefName(String),
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
    Ch(char),
    Range(char, char),
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
    /// flags in spec order (subset of `dgimsuy` present)
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
}

#[derive(Debug, Clone)]
pub struct RxMatch {
    pub start: usize,
    pub end: usize,
    /// caps[0] = whole match; caps[g] = capture group g (1-based)
    pub caps: Vec<Option<(usize, usize)>>,
}

impl Regex {
    pub fn compile(source: &str, flags: &str) -> Result<Rc<Regex>, String> {
        let mut seen = String::new();
        for f in flags.chars() {
            if !"dgimsuy".contains(f) {
                return Err(format!("invalid flag '{f}'"));
            }
            if seen.contains(f) {
                return Err(format!("duplicate flag '{f}'"));
            }
            seen.push(f);
        }
        let icase = seen.contains('i');
        let mut p = ReParser {
            s: source.chars().collect(),
            i: 0,
            n_groups: 0,
            names: Vec::new(),
            total_groups: count_capture_groups(source),
        };
        let root = p.alt()?;
        if p.i < p.s.len() {
            return Err(format!("unexpected '{}'", p.s[p.i]));
        }
        let names = p.names.clone();
        let root = resolve_names(root, &names)?;
        let canonical: String = "dgimsuy".chars().filter(|c| seen.contains(*c)).collect();
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
        }))
    }

    /// Leftmost match starting at or after `start` (honors `y`/sticky).
    pub fn exec(&self, s: &[char], start: usize) -> Option<RxMatch> {
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
            p += 1;
        }
    }

    /// Leftmost match at or after `from`, ignoring stickiness — used by
    /// string ops that drive their own position.
    pub fn find(&self, s: &[char], from: usize) -> Option<RxMatch> {
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
            p += 1;
        }
    }

    /// Anchored match attempt at exactly `pos`.
    pub fn try_at(&self, s: &[char], pos: usize) -> Option<RxMatch> {
        let mut c = Ctx {
            s,
            caps: vec![None; self.n_groups + 1],
            steps: 0,
            depth: 0,
            aborted: false,
            icase: self.ignore_case,
            multiline: self.multiline,
            dotall: self.dot_all,
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

struct ReParser {
    s: Vec<char>,
    i: usize,
    n_groups: usize,
    names: Vec<(String, usize)>,
    /// total capture groups in the whole pattern (prescan) — needed to tell
    /// `\1` backreferences from legacy octal escapes
    total_groups: usize,
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

    fn alt(&mut self) -> Result<Node, String> {
        let mut arms = vec![self.seq()?];
        while self.eat('|') {
            arms.push(self.seq()?);
        }
        Ok(if arms.len() == 1 {
            arms.pop().unwrap()
        } else {
            Node::Alt(arms)
        })
    }

    fn seq(&mut self) -> Result<Node, String> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            items.push(self.term()?);
        }
        match items.len() {
            0 => Ok(Node::Empty),
            1 => Ok(items.pop().unwrap()),
            _ => Ok(Node::Seq(items)),
        }
    }

    /// term = atom + optional quantifier
    fn term(&mut self) -> Result<Node, String> {
        let atom = self.atom()?;
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
            Some('{') => match self.try_brace_quant() {
                Some(mm) => mm,
                None => return Ok(atom),
            },
            _ => return Ok(atom),
        };
        let lazy = self.eat('?');
        // tolerate a stray possessive `+` (not JS syntax, harmless to accept)
        self.eat('+');
        Ok(Node::Rep {
            inner: Box::new(atom),
            min,
            max,
            lazy,
        })
    }

    /// `{n}` `{n,}` `{n,m}` — returns None (no consumption) if not valid form.
    fn try_brace_quant(&mut self) -> Option<(u32, u32)> {
        let save = self.i;
        self.i += 1; // {
        let lo = self.digits()?;
        let (min, max) = if self.eat(',') {
            if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                let hi = self.digits()?;
                if hi < lo {
                    self.i = save;
                    return None;
                }
                (lo, hi)
            } else {
                (lo, u32::MAX)
            }
        } else {
            (lo, lo)
        };
        if self.peek() == Some('}') {
            self.i += 1;
            Some((min, max))
        } else {
            self.i = save;
            None
        }
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
            // stray quantifier chars are treated as literals (Annex-B leniency)
            c => Ok(Node::Ch(c)),
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
                            let mut name = String::new();
                            loop {
                                match self.next() {
                                    Some('>') => break,
                                    Some(c) => name.push(c),
                                    None => return Err("unterminated group name".into()),
                                }
                            }
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
                    // unknown (?x — be lenient: treat `(?` as a capture of `?`
                    self.i -= 1;
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
            'n' => Node::Ch('\n'),
            't' => Node::Ch('\t'),
            'r' => Node::Ch('\r'),
            'f' => Node::Ch('\u{c}'),
            'v' => Node::Ch('\u{b}'),
            '0' => {
                // \0 or legacy octal
                Node::Ch(self.octal_after(0))
            }
            'x' => Node::Ch(self.hex(2).unwrap_or('x')),
            'u' => Node::Ch(self.uesc()),
            'c' => {
                // control letter \cA → \x01
                match self.peek() {
                    Some(l) if l.is_ascii_alphabetic() => {
                        self.i += 1;
                        Node::Ch((l as u8 % 32) as char)
                    }
                    _ => Node::Ch('c'),
                }
            }
            'k' => {
                if self.eat('<') {
                    let mut name = String::new();
                    loop {
                        match self.next() {
                            Some('>') => break,
                            Some(c) => name.push(c),
                            None => return Err("unterminated \\k<name>".into()),
                        }
                    }
                    Node::BackrefName(name)
                } else {
                    Node::Ch('k')
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
                } else {
                    // legacy octal / literal digits — read back the extra
                    // digits and take up to 3 octal digits, or identity for 8/9
                    let first = digits.chars().next().unwrap();
                    if first == '8' || first == '9' {
                        self.i -= digits.len() - 1;
                        Node::Ch(first)
                    } else {
                        // octal: first 1-3 octal digits
                        let od: String = digits
                            .chars()
                            .take_while(|d| ('0'..='7').contains(d))
                            .take(3)
                            .collect();
                        self.i -= digits.len() - od.len();
                        let v = u32::from_str_radix(&od, 8).unwrap_or(0);
                        Node::Ch(char::from_u32(v).unwrap_or('\0'))
                    }
                }
            }
            other => Node::Ch(other), // identity escape: \. \/ \\ \- \* \$ ...
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
            'b' => ClassItem::Ch('\u{8}'), // backspace inside []
            'n' => ClassItem::Ch('\n'),
            't' => ClassItem::Ch('\t'),
            'r' => ClassItem::Ch('\r'),
            'f' => ClassItem::Ch('\u{c}'),
            'v' => ClassItem::Ch('\u{b}'),
            '0' => ClassItem::Ch(self.octal_after(0)),
            'x' => ClassItem::Ch(self.hex(2).unwrap_or('x')),
            'u' => ClassItem::Ch(self.uesc()),
            'c' => match self.peek() {
                Some(l) if l.is_ascii_alphabetic() => {
                    self.i += 1;
                    ClassItem::Ch((l as u8 % 32) as char)
                }
                _ => ClassItem::Ch('c'),
            },
            '1'..='9' => {
                // legacy octal inside class
                let mut v = c.to_digit(10).unwrap();
                for _ in 0..2 {
                    match self.peek().and_then(|d| d.to_digit(8)) {
                        Some(d) => {
                            v = v * 8 + d;
                            self.i += 1;
                        }
                        None => break,
                    }
                }
                ClassItem::Ch(char::from_u32(v).unwrap_or('\0'))
            }
            other => ClassItem::Ch(other),
        })
    }

    /// additional octal digits after a leading `\0`
    fn octal_after(&mut self, v: u32) -> char {
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
        char::from_u32(v).unwrap_or('\0')
    }

    fn hex(&mut self, n: usize) -> Option<char> {
        let mut v: u32 = 0;
        for _ in 0..n {
            let d = self.peek()?.to_digit(16)?;
            v = v * 16 + d;
            self.i += 1;
        }
        char::from_u32(v)
    }

    /// `\uNNNN` or `\u{NNNNN}`
    fn uesc(&mut self) -> char {
        if self.eat('{') {
            let mut v: u32 = 0;
            while let Some(d) = self.peek().and_then(|c| c.to_digit(16)) {
                v = v.saturating_mul(16).saturating_add(d);
                self.i += 1;
            }
            self.eat('}');
            char::from_u32(v).unwrap_or('\u{fffd}')
        } else {
            self.hex(4).unwrap_or('u')
        }
    }

    fn class_atom(&mut self) -> Result<ClassItem, String> {
        let c = self.next().ok_or("unterminated character class")?;
        if c == '\\' {
            self.class_escape()
        } else {
            Ok(ClassItem::Ch(c))
        }
    }

    fn class(&mut self) -> Result<Node, String> {
        let neg = self.eat('^');
        let mut items = Vec::new();
        loop {
            match self.peek() {
                None => return Err("unterminated character class".into()),
                Some(']') => {
                    self.i += 1;
                    break;
                }
                _ => {}
            }
            let a = self.class_atom()?;
            // range? `a-z` — only when both sides are single chars and the
            // `-` isn't trailing (before `]`)
            if self.peek() == Some('-') && self.peek2().is_some() && self.peek2() != Some(']') {
                self.i += 1; // '-'
                let b = self.class_atom()?;
                match (a, b) {
                    (ClassItem::Ch(lo), ClassItem::Ch(hi)) => {
                        items.push(ClassItem::Range(lo, hi));
                    }
                    (a, b) => {
                        // `[\d-z]` — not a real range; keep '-' literal
                        items.push(a);
                        items.push(ClassItem::Ch('-'));
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
/// skipping escapes and `[...]` classes. Used to disambiguate `\1` as a
/// backreference vs a legacy octal escape.
fn count_capture_groups(src: &str) -> usize {
    let s: Vec<char> = src.chars().collect();
    let mut n = 0;
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
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    n
}

/// Resolve `\k<name>` backreferences to numeric group indices.
fn resolve_names(n: Node, names: &[(String, usize)]) -> Result<Node, String> {
    Ok(match n {
        Node::BackrefName(name) => match names.iter().find(|(nm, _)| *nm == name) {
            Some((_, idx)) => Node::Backref(*idx as u32),
            None => return Err(format!("unknown group name '{name}'")),
        },
        Node::Seq(items) => Node::Seq(
            items
                .into_iter()
                .map(|x| resolve_names(x, names))
                .collect::<Result<_, _>>()?,
        ),
        Node::Alt(arms) => Node::Alt(
            arms.into_iter()
                .map(|x| resolve_names(x, names))
                .collect::<Result<_, _>>()?,
        ),
        Node::Rep {
            inner,
            min,
            max,
            lazy,
        } => Node::Rep {
            inner: Box::new(resolve_names(*inner, names)?),
            min,
            max,
            lazy,
        },
        Node::Group { idx, inner } => Node::Group {
            idx,
            inner: Box::new(resolve_names(*inner, names)?),
        },
        Node::Look { inner, neg, behind } => Node::Look {
            inner: Box::new(resolve_names(*inner, names)?),
            neg,
            behind,
        },
        other => other,
    })
}

// ===========================================================================
// matcher
// ===========================================================================

struct Ctx<'h> {
    s: &'h [char],
    caps: Vec<Option<(usize, usize)>>,
    steps: u64,
    depth: u32,
    aborted: bool,
    icase: bool,
    multiline: bool,
    dotall: bool,
    /// set by the top-level continuation when the whole pattern matched
    end: usize,
}

type K<'a, 'h> = &'a mut dyn FnMut(&mut Ctx<'h>, usize) -> bool;

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
        Node::Ch(w) => {
            if pos < c.s.len() && ch_eq(c.s[pos], *w, c.icase) {
                k(c, pos + 1)
            } else {
                false
            }
        }
        Node::Any => {
            if pos < c.s.len() && (c.dotall || !is_lineterm(c.s[pos])) {
                k(c, pos + 1)
            } else {
                false
            }
        }
        Node::Class { neg, items } => {
            if pos < c.s.len() && (class_match(items, c.s[pos], c.icase) != *neg) {
                k(c, pos + 1)
            } else {
                false
            }
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
        Node::Backref(g) => {
            let r = match c.caps.get(*g as usize).copied().flatten() {
                Some((a, b)) => {
                    let l = b - a;
                    pos + l <= c.s.len()
                        && (0..l).all(|i| ch_eq(c.s[pos + i], c.s[a + i], c.icase))
                        && k(c, pos + l)
                }
                // unmatched group → backref matches empty
                None => k(c, pos),
            };
            r
        }
        Node::BackrefName(_) => false, // resolved at compile
        Node::Anchor(a) => {
            let ok = match a {
                Anchor::Start => pos == 0 || (c.multiline && is_lineterm(c.s[pos - 1])),
                Anchor::End => {
                    pos == c.s.len() || (c.multiline && pos < c.s.len() && is_lineterm(c.s[pos]))
                }
                Anchor::WordB => at_wordb(c.s, pos),
                Anchor::NotWordB => !at_wordb(c.s, pos),
            };
            ok && k(c, pos)
        }
        Node::Look { inner, neg, behind } => {
            let saved = c.caps.clone();
            let ok = if *behind {
                // lookbehind: some start ≤ pos must match ending exactly at pos
                let mut found = false;
                let mut st = pos + 1;
                while st > 0 {
                    st -= 1;
                    if mat(c, inner, st, &mut |_c2, e| e == pos) {
                        found = true;
                        break;
                    }
                    if c.aborted {
                        break;
                    }
                }
                found
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

fn one_match(c: &Ctx, n: &Node, pos: usize) -> bool {
    match n {
        Node::Ch(w) => ch_eq(c.s[pos], *w, c.icase),
        Node::Any => c.dotall || !is_lineterm(c.s[pos]),
        Node::Class { neg, items } => class_match(items, c.s[pos], c.icase) != *neg,
        Node::Seq(items) if items.len() == 1 => one_match(c, &items[0], pos),
        Node::Alt(arms) => arms.iter().any(|a| one_match(c, a, pos)),
        _ => false,
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
    let r = if lazy {
        (count >= min && k(c, pos))
            || (count < max && rep_step(c, inner, pos, count, min, max, lazy, k))
    } else {
        (count < max && rep_step(c, inner, pos, count, min, max, lazy, k))
            || (count >= min && k(c, pos))
    };
    c.depth -= 1;
    r
}

/// Match one more iteration of a (non-simple) repetition.
#[allow(clippy::too_many_arguments)]
fn rep_step<'h>(
    c: &mut Ctx<'h>,
    inner: &Node,
    pos: usize,
    count: u32,
    min: u32,
    max: u32,
    lazy: bool,
    k: K<'_, 'h>,
) -> bool {
    mat(c, inner, pos, &mut |c2, e| {
        if e == pos {
            // zero-length iteration — further reps can't progress; the rest
            // of the required minimum is satisfied by empty matches
            k(c2, e)
        } else {
            rep(c2, inner, e, count + 1, min, max, lazy, &mut *k)
        }
    })
}

/// Greedy/lazy repetition of a single-char consumer — iterative, no recursion.
fn rep_simple<'h>(
    c: &mut Ctx<'h>,
    inner: &Node,
    pos: usize,
    min: u32,
    max: u32,
    lazy: bool,
    k: K<'_, 'h>,
) -> bool {
    let mut run = 0usize;
    let limit = (max as usize).min(c.s.len() - pos);
    while run < limit && one_match(c, inner, pos + run) {
        run += 1;
    }
    let need = min as usize;
    if need > run {
        return false;
    }
    if lazy {
        for a in need..=run {
            if k(c, pos + a) {
                return true;
            }
            if c.aborted {
                return false;
            }
        }
        false
    } else {
        for a in (need..=run).rev() {
            if k(c, pos + a) {
                return true;
            }
            if c.aborted {
                return false;
            }
        }
        false
    }
}

// -- char predicates ---------------------------------------------------------

fn is_lineterm(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn at_wordb(s: &[char], pos: usize) -> bool {
    let before = pos > 0 && is_word(s[pos - 1]);
    let after = pos < s.len() && is_word(s[pos]);
    before != after
}

fn is_re_space(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

fn fold(c: char) -> char {
    c.to_uppercase().next().unwrap_or(c)
}

fn ch_eq(a: char, b: char, icase: bool) -> bool {
    a == b || (icase && fold(a) == fold(b))
}

fn pre_match(p: Pre, c: char) -> bool {
    match p {
        Pre::Digit => c.is_ascii_digit(),
        Pre::NotDigit => !c.is_ascii_digit(),
        Pre::Word => is_word(c),
        Pre::NotWord => !is_word(c),
        Pre::Space => is_re_space(c),
        Pre::NotSpace => !is_re_space(c),
    }
}

fn class_match(items: &[ClassItem], c: char, icase: bool) -> bool {
    items.iter().any(|it| match it {
        ClassItem::Ch(w) => ch_eq(c, *w, icase),
        ClassItem::Range(lo, hi) => {
            (*lo..=*hi).contains(&c) || (icase && (fold(*lo)..=fold(*hi)).contains(&fold(c)))
        }
        ClassItem::Pre(p) => pre_match(*p, c),
    })
}
