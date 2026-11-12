//! rowser-html: HTML parsing, from scratch.
//!
//! A WHATWG-shaped pragmatic subset: full tokenizer for tags, attributes,
//! comments, doctype, and character references (named + numeric); tree
//! construction covering the mainline insertion modes (before-html,
//! before-head, in-head, after-head, in-body) with implied-end-tag handling
//! for `p`, `li`, `dd`/`dt`, and headings.
//!
//! Deliberately deferred: the adoption agency algorithm (misnested inline
//! formatting may be dropped), foster parenting (table text lands in the
//! table), foreign content, CDATA, `<template>`, framesets.

use rowser_dom::{Document, NodeId};

pub fn parse(html: &str) -> Document {
    Parser::new(html).run()
}

// ---------- tokenizer ----------

#[derive(Debug, Clone, PartialEq)]
enum Token {
    StartTag {
        name: String,
        attrs: Vec<(String, String)>,
        self_closing: bool,
    },
    EndTag {
        name: String,
    },
    Text(String),
    Comment(String),
    Eof,
}

struct Tokenizer<'a> {
    input: &'a [u8],
    pos: usize,
    /// Raw-text element we're inside ("script"/"style"/"title"/"textarea"),
    /// if any — content until the matching end tag becomes one Text token.
    raw: Option<String>,
}

impl<'a> Tokenizer<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input: input.as_bytes(),
            pos: 0,
            raw: None,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn starts_with_ci(&self, s: &str) -> bool {
        self.input[self.pos..]
            .iter()
            .take(s.len())
            .copied()
            .map(|b| b.to_ascii_lowercase())
            .eq(s.bytes().map(|b| b.to_ascii_lowercase()))
            && self.input.len() - self.pos >= s.len()
    }

    fn eof(&self) -> bool {
        self.pos >= self.input.len()
    }

    /// Called by the tree constructor when a raw-text element opens.
    fn enter_raw_text(&mut self, tag: &str) {
        self.raw = Some(tag.to_lowercase());
    }

    fn next_token(&mut self) -> Token {
        if self.eof() {
            return Token::Eof;
        }
        if let Some(tag) = self.raw.clone() {
            // Emit raw text until `</tag` (case-insensitive, followed by
            // whitespace, '/', or '>').
            let mut end = self.pos;
            loop {
                if end >= self.input.len() {
                    end = self.input.len();
                    break;
                }
                if self.input[end] == b'<'
                    && self.input[end..].len() > tag.len() + 1
                    && self.input[end + 1] == b'/'
                    && self.input[end + 2..]
                        .iter()
                        .take(tag.len())
                        .copied()
                        .map(|b| b.to_ascii_lowercase())
                        .eq(tag.bytes())
                {
                    let after = self.input.get(end + 2 + tag.len()).copied();
                    if after.is_none_or(|c| {
                        c == b'>'
                            || c == b'/'
                            || c == b' '
                            || c == b'\t'
                            || c == b'\n'
                            || c == 0x0c
                            || c == b'\r'
                    }) {
                        break;
                    }
                }
                end += 1;
            }
            let text = String::from_utf8_lossy(&self.input[self.pos..end]).into_owned();
            self.pos = end;
            self.raw = None;
            if !text.is_empty() {
                return Token::Text(text);
            }
            return self.next_token();
        }
        match self.peek() {
            Some(b'<') => self.tag_token(),
            _ => self.text_token(),
        }
    }

    fn text_token(&mut self) -> Token {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c == b'<' {
                break;
            }
            if c == b'&' {
                if start != self.pos {
                    break;
                }
                if let Some(s) = self.charref() {
                    return Token::Text(s);
                }
                // bare '&' — consume as text
            }
            self.pos += 1;
        }
        Token::Text(String::from_utf8_lossy(&self.input[start..self.pos]).into_owned())
    }

    /// Parse a character reference starting at `&`. Returns the decoded
    /// string and consumes input; returns None (no consume) if malformed.
    fn charref(&mut self) -> Option<String> {
        debug_assert_eq!(self.peek(), Some(b'&'));
        let save = self.pos;
        self.pos += 1;
        match self.peek() {
            Some(b'#') => {
                self.pos += 1;
                let hex = matches!(self.peek(), Some(b'x' | b'X'));
                if hex {
                    self.pos += 1;
                }
                let start = self.pos;
                while self.peek().is_some_and(|c| {
                    if hex {
                        c.is_ascii_hexdigit()
                    } else {
                        c.is_ascii_digit()
                    }
                }) {
                    self.pos += 1;
                }
                if self.pos == start {
                    self.pos = save;
                    return None;
                }
                let digits = &self.input[start..self.pos];
                let cp = if hex {
                    u32::from_str_radix(std::str::from_utf8(digits).ok()?, 16)
                } else {
                    std::str::from_utf8(digits).ok()?.parse()
                }
                .ok()?;
                if self.peek() == Some(b';') {
                    self.pos += 1;
                }
                let cp = match cp {
                    0 => 0xfffd,
                    0x80..=0x9f => WINDOWS_1252[cp as usize - 0x80],
                    c if c > 0x10ffff => 0xfffd,
                    c => c,
                };
                char::from_u32(cp)
                    .map(|c| c.to_string())
                    .or(Some("�".into()))
            }
            _ => {
                let start = self.pos;
                while self.peek().is_some_and(|c| c.is_ascii_alphanumeric()) {
                    self.pos += 1;
                    if self.pos - start > 32 {
                        break;
                    }
                }
                let name = std::str::from_utf8(&self.input[start..self.pos]).ok()?;
                let has_semi = self.peek() == Some(b';');
                if has_semi {
                    // Try full name with semicolon first.
                    if let Some(rep) = named_ref(name) {
                        self.pos += 1;
                        return Some(rep.to_string());
                    }
                }
                // Longest-prefix match for legacy refs without ';'.
                for len in (1..=name.len()).rev() {
                    if let Some(rep) = named_ref(&name[..len]) {
                        self.pos = start + len;
                        return Some(rep.to_string());
                    }
                }
                self.pos = save;
                None
            }
        }
    }

    fn tag_token(&mut self) -> Token {
        self.pos += 1; // consume '<'
        match self.peek() {
            Some(b'/') => {
                self.pos += 1;
                self.end_tag()
            }
            Some(b'!') => {
                self.pos += 1;
                self.markup_decl()
            }
            Some(b'?') => {
                // bogus comment — skip to '>'
                self.pos += 1;
                self.bogus_comment()
            }
            Some(c) if c.is_ascii_alphabetic() => self.start_tag(),
            _ => {
                // literal '<' followed by non-tag char
                Token::Text("<".into())
            }
        }
    }

    fn bogus_comment(&mut self) -> Token {
        let start = self.pos;
        while self.peek().is_some_and(|c| c != b'>') {
            self.pos += 1;
        }
        let text = String::from_utf8_lossy(&self.input[start..self.pos]).into_owned();
        self.pos = (self.pos + 1).min(self.input.len());
        Token::Comment(text)
    }

    fn markup_decl(&mut self) -> Token {
        if self.starts_with_ci("--") {
            self.pos += 2;
            let start = self.pos;
            while self.pos + 2 < self.input.len() && !self.starts_with_ci("-->") {
                self.pos += 1;
            }
            let text = String::from_utf8_lossy(&self.input[start..self.pos]).into_owned();
            self.pos = (self.pos + 3).min(self.input.len());
            Token::Comment(text)
        } else if self.starts_with_ci("doctype") {
            // skip to '>'
            while self.peek().is_some_and(|c| c != b'>') {
                self.pos += 1;
            }
            self.pos = (self.pos + 1).min(self.input.len());
            self.next_token()
        } else if self.peek() == Some(b'[') {
            // CDATA outside foreign content → bogus comment
            self.bogus_comment()
        } else {
            self.bogus_comment()
        }
    }

    fn skip_ws(&mut self) {
        while self
            .peek()
            .is_some_and(|c| c == b' ' || c == b'\t' || c == b'\n' || c == 0x0c || c == b'\r')
        {
            self.pos += 1;
        }
    }

    fn tag_name(&mut self) -> String {
        let start = self.pos;
        while self
            .peek()
            .is_some_and(|c| !matches!(c, b' ' | b'\t' | b'\n' | 0x0c | b'\r' | b'/' | b'>' | b'='))
        {
            self.pos += 1;
        }
        String::from_utf8_lossy(&self.input[start..self.pos]).to_lowercase()
    }

    fn start_tag(&mut self) -> Token {
        let name = self.tag_name();
        let mut attrs: Vec<(String, String)> = Vec::new();
        let mut self_closing = false;
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b'>') => {
                    self.pos += 1;
                    break;
                }
                Some(b'/') => {
                    self.pos += 1;
                    if self.peek() == Some(b'>') {
                        self.pos += 1;
                        self_closing = true;
                        break;
                    }
                }
                None => break,
                _ => {
                    // attribute
                    let attr_name = self.tag_name();
                    if attr_name.is_empty() {
                        self.pos += 1;
                        continue;
                    }
                    self.skip_ws();
                    let mut value = String::new();
                    if self.peek() == Some(b'=') {
                        self.pos += 1;
                        self.skip_ws();
                        value = self.attr_value();
                    }
                    // first occurrence wins (spec)
                    if !attrs.iter().any(|(n, _)| *n == attr_name) {
                        attrs.push((attr_name.to_lowercase(), value));
                    }
                }
            }
        }
        Token::StartTag {
            name,
            attrs,
            self_closing,
        }
    }

    fn attr_value(&mut self) -> String {
        match self.peek() {
            Some(q @ (b'"' | b'\'')) => {
                self.pos += 1;
                let mut out = String::new();
                loop {
                    match self.peek() {
                        Some(c) if c == q => {
                            self.pos += 1;
                            break;
                        }
                        Some(b'&') => {
                            if let Some(s) = self.charref() {
                                out.push_str(&s);
                            } else {
                                out.push('&');
                                self.pos += 1;
                            }
                        }
                        Some(_) => {
                            let start = self.pos;
                            while self.peek().is_some_and(|c| c != q && c != b'&') {
                                self.pos += 1;
                            }
                            out.push_str(&String::from_utf8_lossy(&self.input[start..self.pos]));
                        }
                        None => break,
                    }
                }
                out
            }
            _ => {
                // unquoted
                let mut out = String::new();
                loop {
                    match self.peek() {
                        Some(b'&') => {
                            if let Some(s) = self.charref() {
                                out.push_str(&s);
                            } else {
                                out.push('&');
                                self.pos += 1;
                            }
                        }
                        Some(c)
                            if c == b' '
                                || c == b'\t'
                                || c == b'\n'
                                || c == 0x0c
                                || c == b'\r'
                                || c == b'>' =>
                        {
                            break
                        }
                        Some(_) => {
                            let start = self.pos;
                            while self.peek().is_some_and(|c| {
                                !matches!(c, b'&' | b' ' | b'\t' | b'\n' | 0x0c | b'\r' | b'>')
                            }) {
                                self.pos += 1;
                            }
                            out.push_str(&String::from_utf8_lossy(&self.input[start..self.pos]));
                        }
                        None => break,
                    }
                }
                out
            }
        }
    }

    fn end_tag(&mut self) -> Token {
        match self.peek() {
            Some(c) if c.is_ascii_alphabetic() => {
                let name = self.tag_name();
                // skip attributes/junk to '>'
                while self.peek().is_some_and(|c| c != b'>') {
                    self.pos += 1;
                }
                if self.peek() == Some(b'>') {
                    self.pos += 1;
                }
                Token::EndTag { name }
            }
            Some(b'>') => {
                self.pos += 1;
                self.next_token()
            }
            _ => {
                // bogus
                self.bogus_comment()
            }
        }
    }
}

fn named_ref(name: &str) -> Option<&'static str> {
    Some(match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => "\u{a0}",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        "hellip" => "…",
        "mdash" => "—",
        "ndash" => "–",
        "lsquo" => "‘",
        "rsquo" => "’",
        "ldquo" => "“",
        "rdquo" => "”",
        "laquo" => "«",
        "raquo" => "»",
        "times" => "×",
        "divide" => "÷",
        "deg" => "°",
        "plusmn" => "±",
        "middot" => "·",
        "bull" => "•",
        "dagger" => "†",
        "Dagger" => "‡",
        "sect" => "§",
        "para" => "¶",
        "micro" => "µ",
        "euro" => "€",
        "pound" => "£",
        "yen" => "¥",
        "cent" => "¢",
        "curren" => "¤",
        "iexcl" => "¡",
        "iquest" => "¿",
        "szlig" => "ß",
        "agrave" => "à",
        "aacute" => "á",
        "acirc" => "â",
        "atilde" => "ã",
        "auml" => "ä",
        "aring" => "å",
        "aelig" => "æ",
        "ccedil" => "ç",
        "egrave" => "è",
        "eacute" => "é",
        "ecirc" => "ê",
        "euml" => "ë",
        "igrave" => "ì",
        "iacute" => "í",
        "icirc" => "î",
        "iuml" => "ï",
        "ntilde" => "ñ",
        "ograve" => "ò",
        "oacute" => "ó",
        "ocirc" => "ô",
        "otilde" => "õ",
        "ouml" => "ö",
        "oslash" => "ø",
        "ugrave" => "ù",
        "uacute" => "ú",
        "ucirc" => "û",
        "uuml" => "ü",
        "yacute" => "ý",
        "yuml" => "ÿ",
        "Agrave" => "À",
        "Aacute" => "Á",
        "Acirc" => "Â",
        "Atilde" => "Ã",
        "Auml" => "Ä",
        "Aring" => "Å",
        "AElig" => "Æ",
        "Ccedil" => "Ç",
        "Egrave" => "È",
        "Eacute" => "É",
        "Ecirc" => "Ê",
        "Euml" => "Ë",
        "Igrave" => "Ì",
        "Iacute" => "Í",
        "Icirc" => "Î",
        "Iuml" => "Ï",
        "Ntilde" => "Ñ",
        "Ograve" => "Ò",
        "Oacute" => "Ó",
        "Ocirc" => "Ô",
        "Otilde" => "Õ",
        "Ouml" => "Ö",
        "Oslash" => "Ø",
        "Ugrave" => "Ù",
        "Uacute" => "Ú",
        "Ucirc" => "Û",
        "Uuml" => "Ü",
        "Yacute" => "Ý",
        "rarr" => "→",
        "larr" => "←",
        "uarr" => "↑",
        "darr" => "↓",
        "harr" => "↔",
        "infin" => "∞",
        "ne" => "≠",
        "le" => "≤",
        "ge" => "≥",
        "minus" => "−",
        "lowast" => "∗",
        "radic" => "√",
        "sum" => "∑",
        "prod" => "∏",
        "part" => "∂",
        "int" => "∫",
        "asymp" => "≈",
        "prop" => "∝",
        "forall" => "∀",
        "exist" => "∃",
        "isin" => "∈",
        "notin" => "∉",
        "cap" => "∩",
        "cup" => "∪",
        "sub" => "⊂",
        "sup" => "⊃",
        "and" => "∧",
        "or" => "∨",
        "not" => "¬",
        "oplus" => "⊕",
        "otimes" => "⊗",
        "perp" => "⊥",
        "sdot" => "⋅",
        "Alpha" => "Α",
        "Beta" => "Β",
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Omega" => "Ω",
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" => "ε",
        "theta" => "θ",
        "lambda" => "λ",
        "mu" => "μ",
        "pi" => "π",
        "sigma" => "σ",
        "phi" => "φ",
        "omega" => "ω",
        _ => return None,
    })
}

/// Windows-1252 remapping for numeric refs 0x80–0x9F.
const WINDOWS_1252: [u32; 32] = [
    0x20ac, 0x0081, 0x201a, 0x0192, 0x201e, 0x2026, 0x2020, 0x2021, 0x02c6, 0x2030, 0x0160, 0x2039,
    0x0152, 0x008d, 0x017d, 0x008f, 0x0090, 0x2018, 0x2019, 0x201c, 0x201d, 0x2022, 0x2013, 0x2014,
    0x02dc, 0x2122, 0x0161, 0x203a, 0x0153, 0x009d, 0x017e, 0x0178,
];

// ---------- tree construction ----------

const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr", "command", "keygen",
];

/// Tags that close an open <p> when their start tag is seen in body.
const P_CLOSERS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "center",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "main",
    "menu",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "ul",
];

const RAW_TEXT: &[&str] = &[
    "script",
    "style",
    "title",
    "textarea",
    "iframe",
    "xmp",
    "noembed",
    "noframes",
    "plaintext",
];
/// Whose raw text is kept as a text child (script/style/title/textarea).
const KEEP_RAW: &[&str] = &["script", "style", "title", "textarea"];

const HEAD_TAGS: &[&str] = &[
    "base", "basefont", "bgsound", "link", "meta", "title", "style", "script", "noscript",
    "template",
];

#[derive(PartialEq)]
enum Mode {
    BeforeHtml,
    BeforeHead,
    InHead,
    AfterHead,
    InBody,
    AfterBody,
}

struct Parser<'a> {
    tok: Tokenizer<'a>,
    doc: Document,
    stack: Vec<NodeId>,
    mode: Mode,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            tok: Tokenizer::new(input),
            doc: Document::new(),
            stack: Vec::new(),
            mode: Mode::BeforeHtml,
        }
    }

    fn run(mut self) -> Document {
        loop {
            let token = self.tok.next_token();
            let eof = matches!(token, Token::Eof);
            self.handle(token);
            if eof {
                break;
            }
        }
        self.doc
    }

    fn current(&self) -> NodeId {
        *self.stack.last().unwrap_or(&self.doc.root)
    }

    fn current_tag(&self) -> Option<&str> {
        self.doc.tag(self.current())
    }

    fn in_stack(&self, tag: &str) -> bool {
        self.stack.iter().any(|&n| self.doc.tag(n) == Some(tag))
    }

    /// Pop elements until `tag` is popped (inclusive). No-op if absent.
    fn close(&mut self, tag: &str) {
        if !self.in_stack(tag) {
            return;
        }
        while let Some(n) = self.stack.pop() {
            if self.doc.tag(n) == Some(tag) {
                break;
            }
        }
    }

    /// Pop until any of `tags` is popped (inclusive), but only when one is
    /// open above every `stop` boundary — mirrors the spec's scoped close:
    /// a `<li>` inside a nested list must not close an outer `<li>` (the
    /// `ul`/`ol` between them shields it).
    fn close_in_scope(&mut self, tags: &[&str], stops: &[&str]) {
        for i in (0..self.stack.len()).rev() {
            let t = self.doc.tag(self.stack[i]).unwrap_or("");
            if tags.contains(&t) {
                self.stack.truncate(i);
                return;
            }
            if stops.contains(&t) {
                return;
            }
        }
    }

    /// Close <p> if one is open within button scope (approximated by the stack).
    fn close_p(&mut self) {
        if self.in_stack("p") {
            self.close("p");
        }
    }

    fn insert_element(&mut self, name: &str, attrs: Vec<(String, String)>) -> NodeId {
        let id = self.doc.create_element(name, attrs);
        let parent = self.current();
        self.doc.append(parent, id);
        id
    }

    fn push_element(&mut self, name: &str, attrs: Vec<(String, String)>) -> NodeId {
        let id = self.insert_element(name, attrs);
        self.stack.push(id);
        id
    }

    fn insert_text(&mut self, text: &str) {
        let id = self.doc.create_text(text);
        let parent = self.current();
        self.doc.append(parent, id);
    }

    fn ensure(&mut self, tag: &str) -> NodeId {
        // find or create a direct element in the stack with tag
        if let Some(&n) = self.stack.iter().find(|&&n| self.doc.tag(n) == Some(tag)) {
            return n;
        }
        self.push_element(tag, Vec::new())
    }

    fn handle(&mut self, token: Token) {
        // loop lets a token be reprocessed in a later mode
        let mut token = Some(token);
        while let Some(t) = token.take() {
            token = self.dispatch(t);
        }
    }

    /// Handle one token in the current mode; returns Some(token) to
    /// reprocess it after a mode switch.
    fn dispatch(&mut self, token: Token) -> Option<Token> {
        match self.mode {
            Mode::BeforeHtml => match token {
                Token::Text(t) if t.trim().is_empty() => None,
                Token::StartTag { name, attrs, .. } if name == "html" => {
                    let id = self.doc.create_element("html", attrs);
                    self.doc.append(self.doc.root, id);
                    self.stack.push(id);
                    self.mode = Mode::BeforeHead;
                    None
                }
                Token::Eof => {
                    self.ensure("html");
                    self.mode = Mode::BeforeHead;
                    Some(Token::Eof)
                }
                other => {
                    self.ensure("html");
                    self.mode = Mode::BeforeHead;
                    Some(other)
                }
            },
            Mode::BeforeHead => match token {
                Token::Text(t) if t.trim().is_empty() => None,
                Token::StartTag { name, attrs, .. } if name == "head" => {
                    self.push_element("head", attrs);
                    self.mode = Mode::InHead;
                    None
                }
                Token::Eof => {
                    self.ensure("head");
                    self.mode = Mode::InHead;
                    Some(Token::Eof)
                }
                other => {
                    self.ensure("head");
                    self.mode = Mode::InHead;
                    Some(other)
                }
            },
            Mode::InHead => match token {
                Token::Text(t) => {
                    // Raw-text content (script/style/title) lands inside the
                    // current element; other text pushes us out of the head.
                    if self
                        .current_tag()
                        .is_some_and(|c| KEEP_RAW.contains(&c) || c == "head")
                    {
                        self.insert_text(&t);
                        None
                    } else {
                        self.close("head");
                        self.mode = Mode::AfterHead;
                        Some(Token::Text(t))
                    }
                }
                Token::StartTag {
                    name,
                    attrs,
                    self_closing,
                } if HEAD_TAGS.contains(&name.as_str()) => {
                    let raw = RAW_TEXT.contains(&name.as_str());
                    if VOID.contains(&name.as_str()) || self_closing {
                        self.insert_element(&name, attrs);
                    } else {
                        self.push_element(&name, attrs);
                        if raw {
                            self.tok.enter_raw_text(&name);
                        }
                    }
                    None
                }
                Token::EndTag { name } if HEAD_TAGS.contains(&name.as_str()) || name == "head" => {
                    if self.in_stack(&name) {
                        self.close(&name);
                    }
                    if name == "head" {
                        self.mode = Mode::AfterHead;
                    }
                    None
                }
                Token::Eof => {
                    self.close("head");
                    self.mode = Mode::AfterHead;
                    Some(Token::Eof)
                }
                other => {
                    self.close("head");
                    self.mode = Mode::AfterHead;
                    Some(other)
                }
            },
            Mode::AfterHead => match token {
                Token::Text(t) if t.trim().is_empty() => None,
                Token::StartTag { name, attrs, .. } if name == "body" => {
                    self.push_element("body", attrs);
                    self.mode = Mode::InBody;
                    None
                }
                Token::StartTag { name, attrs, .. } if name == "frameset" => {
                    self.push_element("frameset", attrs);
                    self.mode = Mode::InBody;
                    None
                }
                Token::Eof => {
                    self.ensure("body");
                    self.mode = Mode::InBody;
                    Some(Token::Eof)
                }
                other => {
                    self.ensure("body");
                    self.mode = Mode::InBody;
                    Some(other)
                }
            },
            Mode::InBody => self.in_body(token),
            Mode::AfterBody => match token {
                Token::Eof => None,
                Token::Text(t) if t.trim().is_empty() => {
                    self.insert_text(&t);
                    None
                }
                other => {
                    self.mode = Mode::InBody;
                    Some(other)
                }
            },
        }
    }

    fn in_body(&mut self, token: Token) -> Option<Token> {
        match token {
            Token::Eof => {
                self.stack.clear();
                self.mode = Mode::AfterBody;
                None
            }
            Token::Text(t) => {
                self.insert_text(&t);
                None
            }
            Token::Comment(c) => {
                let id = self.doc.create_comment(&c);
                let parent = self.current();
                self.doc.append(parent, id);
                None
            }
            Token::StartTag {
                name,
                attrs,
                self_closing,
            } => {
                match name.as_str() {
                    "html" | "head" => None, // already have them
                    "body" => None,
                    _ if VOID.contains(&name.as_str()) => {
                        if name == "hr" {
                            self.close_p();
                        }
                        self.insert_element(&name, attrs);
                        None
                    }
                    "p" => {
                        self.close_p();
                        self.push_element(&name, attrs);
                        None
                    }
                    "li" => {
                        // <li> closes a previous <li> only within the same
                        // list — a nested ul/ol shields the outer item
                        self.close_in_scope(&["li"], &["ul", "ol", "menu", "dir"]);
                        self.close_p();
                        self.push_element(&name, attrs);
                        None
                    }
                    "dd" | "dt" => {
                        // same scoping for definition-list items
                        self.close_in_scope(&["dd", "dt"], &["dl", "ul", "ol", "menu", "dir"]);
                        self.close_p();
                        self.push_element(&name, attrs);
                        None
                    }
                    n if n.starts_with('h') && n.len() == 2 && n.as_bytes()[1].is_ascii_digit() => {
                        if self.in_stack("h1")
                            || self.in_stack("h2")
                            || self.in_stack("h3")
                            || self.in_stack("h4")
                            || self.in_stack("h5")
                            || self.in_stack("h6")
                        {
                            self.stack.pop();
                        }
                        self.close_p();
                        self.push_element(&name, attrs);
                        None
                    }
                    _ if P_CLOSERS.contains(&name.as_str()) => {
                        self.close_p();
                        self.push_element(&name, attrs);
                        None
                    }
                    _ if RAW_TEXT.contains(&name.as_str()) => {
                        self.push_element(&name, attrs);
                        if !self_closing {
                            self.tok.enter_raw_text(&name);
                        }
                        None
                    }
                    "pre" | "listing" => {
                        self.close_p();
                        self.push_element(&name, attrs);
                        None
                    }
                    "form" => {
                        if !self.in_stack("form") {
                            self.close_p();
                            self.push_element(&name, attrs);
                        }
                        None
                    }
                    "table" => {
                        self.close_p();
                        self.push_element(&name, attrs);
                        None
                    }
                    _ => {
                        if self_closing {
                            self.insert_element(&name, attrs);
                        } else {
                            self.push_element(&name, attrs);
                        }
                        None
                    }
                }
            }
            Token::EndTag { name } => {
                match name.as_str() {
                    "body" => {
                        self.close("body");
                        self.mode = Mode::AfterBody;
                    }
                    "html" => {
                        self.close("body");
                        self.mode = Mode::AfterBody;
                    }
                    "p" => {
                        if self.in_stack("p") {
                            self.close("p");
                        }
                    }
                    "br" => {
                        // spec treats </br> as <br>
                        self.insert_element("br", Vec::new());
                    }
                    n if VOID.contains(&n) => {}
                    n => {
                        if self.in_stack(n) {
                            self.close(n);
                        }
                        // unmatched end tag: ignore
                    }
                }
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_document() {
        let doc = parse("<p>hello</p>");
        let html = doc.find(doc.root, "html").expect("html");
        let body = doc.find(html, "body").expect("body");
        let p = doc.find(body, "p").expect("p");
        assert_eq!(doc.text_content(p), "hello");
    }

    #[test]
    fn full_document() {
        let doc = parse(
            "<!doctype html><html><head><title>t</title></head><body><h1>Hi</h1><p>a<b>b</b></p></body></html>",
        );
        let title = doc.find_deep(doc.root, "title").unwrap();
        assert_eq!(doc.text_content(title), "t");
        let h1 = doc.find_deep(doc.root, "h1").unwrap();
        assert_eq!(doc.text_content(h1), "Hi");
        let b = doc.find_deep(doc.root, "b").unwrap();
        assert_eq!(doc.text_content(b), "b");
    }

    #[test]
    fn implied_p_close() {
        let doc = parse("<p>one<p>two");
        let body = doc.find_deep(doc.root, "body").unwrap();
        let ps: Vec<_> = doc
            .children(body)
            .iter()
            .filter(|&&c| doc.tag(c) == Some("p"))
            .collect();
        assert_eq!(ps.len(), 2);
        assert_eq!(doc.text_content(*ps[0]), "one");
        assert_eq!(doc.text_content(*ps[1]), "two");
    }

    #[test]
    fn entities() {
        let doc = parse("<p>fish &amp; chips &#8212; &#x263a;</p>");
        let t = doc.text_content(doc.root);
        assert!(t.contains("fish & chips"), "{t}");
        assert!(t.contains('—'), "{t}");
        assert!(t.contains('☺'), "{t}");
    }

    #[test]
    fn raw_script() {
        let doc = parse("<body><script>if (a<b) { x(); }</script><p>ok</p>");
        let script = doc.find_deep(doc.root, "script").unwrap();
        let text_node = doc.children(script)[0];
        assert_eq!(doc.text(text_node), Some("if (a<b) { x(); }"));
        let p = doc.find_deep(doc.root, "p").unwrap();
        assert_eq!(doc.text_content(p), "ok");
    }

    #[test]
    fn list_items() {
        let doc = parse("<ul><li>a<li>b<li>c</ul>");
        let ul = doc.find_deep(doc.root, "ul").unwrap();
        let lis: Vec<_> = doc
            .children(ul)
            .iter()
            .filter(|&&c| doc.tag(c) == Some("li"))
            .collect();
        assert_eq!(lis.len(), 3);
    }

    #[test]
    fn attrs_and_void() {
        let doc = parse("<body><img src='a.png' alt=\"x\"><br><input type=text>");
        let img = doc.find_deep(doc.root, "img").unwrap();
        assert_eq!(doc.attr(img, "src"), Some("a.png"));
        assert!(doc.find_deep(doc.root, "br").is_some());
    }
}
