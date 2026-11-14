//! Tokenizer for the rowser-js interpreter.

use crate::Error;

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Num(f64),
    Str(String),
    /// template literal: alternating string parts and raw expression sources
    Template(Vec<TemplPart>),
    Ident(String),
    Kw(Kw),
    Punct(&'static str),
    /// regex literal `/pattern/flags` — produced when a `/` appears where an
    /// expression cannot continue (see `slash_is_regex`)
    Regex {
        pattern: String,
        flags: String,
    },
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TemplPart {
    Str(String),
    /// raw source between `${` and `}` — re-lexed/parsed by the parser
    Expr(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kw {
    Var,
    Let,
    Const,
    Function,
    Return,
    If,
    Else,
    For,
    While,
    Do,
    Break,
    Continue,
    New,
    Typeof,
    Instanceof,
    In,
    Void,
    Delete,
    Switch,
    Case,
    Default,
    Try,
    Catch,
    Finally,
    Throw,
    Class,
    Extends,
    This,
    True,
    False,
    Null,
    Import,
    Export,
    From,
    As,
    Of,
    Get,
    Set,
    Static,
    Async,
    Await,
    Yield,
    Super,
    Debugger,
    /// always-reserved words: `with` (stmt unsupported → always a parse
    /// error, which strict-mode negative tests count as a pass) and
    /// `enum` (reserved in all modes)
    With,
    Enum,
}

impl Kw {
    pub fn name(self) -> &'static str {
        use Kw::*;
        match self {
            Var => "var",
            Let => "let",
            Const => "const",
            Function => "function",
            Return => "return",
            If => "if",
            Else => "else",
            For => "for",
            While => "while",
            Do => "do",
            Break => "break",
            Continue => "continue",
            New => "new",
            Typeof => "typeof",
            Instanceof => "instanceof",
            In => "in",
            Void => "void",
            Delete => "delete",
            Switch => "switch",
            Case => "case",
            Default => "default",
            Try => "try",
            Catch => "catch",
            Finally => "finally",
            Throw => "throw",
            Class => "class",
            Extends => "extends",
            This => "this",
            True => "true",
            False => "false",
            Null => "null",
            Import => "import",
            Export => "export",
            From => "from",
            As => "as",
            Of => "of",
            Get => "get",
            Set => "set",
            Static => "static",
            Async => "async",
            Await => "await",
            Yield => "yield",
            Super => "super",
            Debugger => "debugger",
            With => "with",
            Enum => "enum",
        }
    }
}

fn keyword(s: &str) -> Option<Kw> {
    use Kw::*;
    Some(match s {
        "var" => Var,
        "let" => Let,
        "const" => Const,
        "function" => Function,
        "return" => Return,
        "if" => If,
        "else" => Else,
        "for" => For,
        "while" => While,
        "do" => Do,
        "break" => Break,
        "continue" => Continue,
        "new" => New,
        "typeof" => Typeof,
        "instanceof" => Instanceof,
        "in" => In,
        "void" => Void,
        "delete" => Delete,
        "switch" => Switch,
        "case" => Case,
        "default" => Default,
        "try" => Try,
        "catch" => Catch,
        "finally" => Finally,
        "throw" => Throw,
        "class" => Class,
        "extends" => Extends,
        "this" => This,
        "true" => True,
        "false" => False,
        "null" => Null,
        "import" => Import,
        "export" => Export,
        "from" => From,
        "as" => As,
        "of" => Of,
        "get" => Get,
        "set" => Set,
        "static" => Static,
        "async" => Async,
        "await" => Await,
        "yield" => Yield,
        "super" => Super,
        "debugger" => Debugger,
        "with" => With,
        "enum" => Enum,
        _ => return None,
    })
}

#[derive(Debug, Clone)]
pub struct Token {
    pub tok: Tok,
    pub line: u32,
    pub col: u32,
    /// true if a newline separated this token from the previous one (for ASI)
    pub newline_before: bool,
    /// legacy-octal number (`0755`) or octal/`\8`-style string escape —
    /// strict-mode early error
    pub octal: bool,
    /// token contains a `\` escape — an escaped `"use strict"` directive
    /// does NOT enable strict mode per spec
    pub escaped: bool,
}

const PUNCTS: &[&str] = &[
    ">>>=", "===", "!==", "**=", "<<=", ">>=", "&&=", "||=", "??=", "...", ">>>", "=>", "==", "!=",
    "<=", ">=", "++", "--", "+=", "-=", "*=", "/=", "%=", "&&", "||", "??", "?.", "**", "<<", ">>",
    "&=", "|=", "^=", "{", "}", "(", ")", "[", "]", ";", ",", "<", ">", "+", "-", "*", "/", "%",
    "&", "|", "^", "!", "~", "?", ":", ".", "=",
];

pub fn lex(src: &str) -> Result<Vec<Token>, Error> {
    let mut lx = Lexer {
        chars: src.chars().collect(),
        i: 0,
        line: 1,
        col: 1,
        newline: true,
        tok_octal: false,
        tok_escaped: false,
    };
    lx.run()
}

struct Lexer {
    chars: Vec<char>,
    i: usize,
    line: u32,
    col: u32,
    newline: bool,
    /// set by number()/string()/template() when the token is a legacy
    /// octal literal or carries an octal/\8-style escape (strict error)
    tok_octal: bool,
    /// set when the token contains any `\` escape sequence
    tok_escaped: bool,
}

impl Lexer {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.i).copied()
    }
    fn peek2(&self) -> Option<char> {
        self.chars.get(self.i + 1).copied()
    }
    fn peek3(&self) -> Option<char> {
        self.chars.get(self.i + 2).copied()
    }
    /// LineTerminator: \n, \r (folding \r\n), \u2028, \u2029
    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.i += 1;
        if c == '\r' || c == '\n' || c == '\u{2028}' || c == '\u{2029}' {
            if c == '\r' && self.peek() == Some('\n') {
                self.i += 1;
            }
            self.line += 1;
            self.col = 1;
            self.newline = true;
        } else {
            self.col += 1;
        }
        Some(c)
    }
    fn err(&self, msg: impl Into<String>) -> Error {
        Error::new(msg, self.line, self.col)
    }

    fn run(&mut self) -> Result<Vec<Token>, Error> {
        let mut out = Vec::new();
        loop {
            self.skip_trivia()?;
            let (line, col, newline_before) = (self.line, self.col, self.newline);
            self.newline = false;
            let Some(c) = self.peek() else {
                out.push(Token {
                    tok: Tok::Eof,
                    line,
                    col,
                    newline_before,
                    octal: false,
                    escaped: false,
                });
                return Ok(out);
            };
            let tok = if c == '"' || c == '\'' {
                Tok::Str(self.string(c)?)
            } else if c == '`' {
                Tok::Template(self.template()?)
            } else if c.is_ascii_digit()
                || (c == '.' && self.peek2().is_some_and(|d| d.is_ascii_digit()))
            {
                Tok::Num(self.number()?)
            } else if is_ident_start(c) || c == '\\' {
                let name = self.ident()?;
                match keyword(&name) {
                    Some(kw) => Tok::Kw(kw),
                    None => Tok::Ident(name),
                }
            } else if c == '#' {
                // `#name` — private class member / brand check (`#x in obj`)
                self.bump();
                let name = self.ident()?;
                Tok::Ident(format!("#{name}"))
            } else if c == '/' && slash_is_regex(out.last()) {
                // regex-literal context; if the scan fails (e.g. it was
                // actually division), fall back to punct tokenization
                match self.try_regex() {
                    Some(t) => t,
                    None => self.punct()?,
                }
            } else {
                self.punct()?
            };
            let (octal, escaped) = (self.tok_octal, self.tok_escaped);
            self.tok_octal = false;
            self.tok_escaped = false;
            out.push(Token {
                tok,
                line,
                col,
                newline_before,
                octal,
                escaped,
            });
        }
    }

    fn skip_trivia(&mut self) -> Result<(), Error> {
        loop {
            match self.peek() {
                // JS whitespace excludes vertical tab (\x0B); BOM is ws
                Some(c) if (c.is_whitespace() || c == '\u{feff}') && c != '\u{b}' => {
                    self.bump();
                }
                // hashbang `#!` only valid as the first two chars of a script
                Some('#') if self.i == 0 && self.peek2() == Some('!') => {
                    self.skip_line_comment();
                }
                // Annex B `<!--` opens a line comment anywhere
                Some('<')
                    if self.peek2() == Some('!')
                        && self.peek3() == Some('-')
                        && self.chars.get(self.i + 3) == Some(&'-') =>
                {
                    self.skip_line_comment();
                }
                // Annex B `-->` opens a line comment only at line start
                Some('-')
                    if self.newline
                        && self.peek2() == Some('-')
                        && self.peek3() == Some('>') =>
                {
                    self.skip_line_comment();
                }
                Some('/') => match self.peek2() {
                    Some('/') => {
                        self.skip_line_comment();
                    }
                    Some('*') => {
                        self.bump();
                        self.bump();
                        let mut closed = false;
                        while let Some(c) = self.bump() {
                            if c == '*' && self.peek() == Some('/') {
                                self.bump();
                                closed = true;
                                break;
                            }
                        }
                        if !closed {
                            return Err(self.err("unterminated block comment"));
                        }
                    }
                    _ => return Ok(()),
                },
                _ => return Ok(()),
            }
        }
    }

    /// Consume until (not including) a LineTerminator or EOF.
    fn skip_line_comment(&mut self) {
        while let Some(c) = self.peek() {
            if c == '\n' || c == '\r' || c == '\u{2028}' || c == '\u{2029}' {
                break;
            }
            self.bump();
        }
    }

    fn ident(&mut self) -> Result<String, Error> {
        let mut s = String::new();
        loop {
            match self.peek() {
                Some(c) if is_ident_part(c) => {
                    s.push(c);
                    self.bump();
                }
                Some('\\') => {
                    self.tok_escaped = true;
                    self.bump();
                    if self.peek() != Some('u') {
                        return Err(self.err("invalid identifier escape"));
                    }
                    self.bump();
                    let cp = if self.peek() == Some('{') {
                        self.bump();
                        let mut v: u32 = 0;
                        let mut any = false;
                        while let Some(d) = self.peek().and_then(|c| c.to_digit(16)) {
                            v = v * 16 + d;
                            any = true;
                            self.bump();
                        }
                        if self.peek() != Some('}') || !any || v > 0x10ffff {
                            return Err(self.err("bad unicode escape in identifier"));
                        }
                        self.bump();
                        v
                    } else {
                        self.hex_digits(4)
                            .ok_or_else(|| self.err("bad unicode escape in identifier"))?
                    };
                    match char::from_u32(cp) {
                        Some(ch) if is_ident_part(ch) => s.push(ch),
                        _ => return Err(self.err("bad unicode escape in identifier")),
                    }
                }
                _ => break,
            }
        }
        if s.is_empty() {
            return Err(self.err("expected identifier"));
        }
        Ok(s)
    }

    fn hex_digits(&mut self, n: usize) -> Option<u32> {
        let mut v: u32 = 0;
        for _ in 0..n {
            let c = self.peek()?;
            let d = c.to_digit(16)?;
            v = v * 16 + d;
            self.bump();
        }
        Some(v)
    }

    fn number(&mut self) -> Result<f64, Error> {
        let mut s = String::new();
        if self.peek() == Some('0') {
            match self.peek2() {
                Some('x') | Some('X') => {
                    self.bump();
                    self.bump();
                    let mut v = String::new();
                    while let Some(c) = self.peek() {
                        if c.is_ascii_hexdigit() || c == '_' {
                            if c != '_' {
                                v.push(c);
                            }
                            self.bump();
                        } else {
                            break;
                        }
                    }
                    return u64::from_str_radix(&v, 16)
                        .map(|n| n as f64)
                        .map_err(|_| self.err("bad hex literal"));
                }
                Some('o') | Some('O') => {
                    self.bump();
                    self.bump();
                    let mut v = String::new();
                    while self
                        .peek()
                        .is_some_and(|c| ('0'..='7').contains(&c) || c == '_')
                    {
                        let c = self.bump().unwrap();
                        if c != '_' {
                            v.push(c);
                        }
                    }
                    return u64::from_str_radix(&v, 8)
                        .map(|n| n as f64)
                        .map_err(|_| self.err("bad octal literal"));
                }
                Some('b') | Some('B') => {
                    self.bump();
                    self.bump();
                    let mut v = String::new();
                    while self
                        .peek()
                        .is_some_and(|c| c == '0' || c == '1' || c == '_')
                    {
                        let c = self.bump().unwrap();
                        if c != '_' {
                            v.push(c);
                        }
                    }
                    return u64::from_str_radix(&v, 2)
                        .map(|n| n as f64)
                        .map_err(|_| self.err("bad binary literal"));
                }
                _ => {}
            }
        }
        // legacy octal: `0` followed by octal digits (may include 8/9 which
        // makes it a NonOctalDecimalIntegerLiteral — still a strict error)
        if self.peek() == Some('0')
            && self.peek2().is_some_and(|c| c.is_ascii_digit())
        {
            let mut oct = true;
            let mut digits = String::new();
            let mut has_underscore = false;
            while let Some(c) = self.peek() {
                match c {
                    '0'..='9' => {
                        if c >= '8' {
                            oct = false;
                        }
                        digits.push(c);
                        self.bump();
                    }
                    '_' => {
                        has_underscore = true;
                        self.bump();
                    }
                    _ => break,
                }
            }
            // `08.5`, `010e2` — plain decimal, continue to the general path
            if matches!(self.peek(), Some('.') | Some('e') | Some('E')) {
                s = digits;
                if oct {
                    self.tok_octal = false;
                }
                let mut seen_dot = false;
                let mut seen_exp = false;
                while let Some(c) = self.peek() {
                    match c {
                        '0'..='9' => s.push(self.bump().unwrap()),
                        '.' if !seen_dot && !seen_exp => {
                            seen_dot = true;
                            s.push(self.bump().unwrap());
                        }
                        'e' | 'E' if !seen_exp => {
                            seen_exp = true;
                            s.push('e');
                            self.bump();
                            if matches!(self.peek(), Some('+') | Some('-')) {
                                s.push(self.bump().unwrap());
                            }
                        }
                        _ => break,
                    }
                }
                return self.number_tail(s);
            }
            if has_underscore {
                return Err(self.err("separators are not allowed in legacy octal"));
            }
            self.tok_octal = true;
            let v = if oct {
                u64::from_str_radix(&digits, 8).map(|n| n as f64).ok()
            } else {
                digits.parse::<f64>().ok()
            };
            return match v {
                Some(n) => self.number_tail_ok(n),
                None => Err(self.err("bad number literal")),
            };
        }
        // leading-zero decimal like `0` alone; general path
        let mut seen_dot = false;
        let mut seen_exp = false;
        while let Some(c) = self.peek() {
            match c {
                '0'..='9' => {
                    s.push(c);
                    self.bump();
                }
                '_' => {
                    self.bump();
                }
                '.' if !seen_dot && !seen_exp => {
                    seen_dot = true;
                    s.push('.');
                    self.bump();
                }
                'e' | 'E' if !seen_exp => {
                    seen_exp = true;
                    s.push('e');
                    self.bump();
                    if matches!(self.peek(), Some('+') | Some('-')) {
                        s.push(self.bump().unwrap());
                    }
                }
                'n' => {
                    // bigint literal — treat as number for now
                    self.bump();
                    break;
                }
                _ => break,
            }
        }
        self.number_tail(s)
    }

    fn number_tail(&mut self, s: String) -> Result<f64, Error> {
        let n = s
            .parse::<f64>()
            .map_err(|_| self.err(format!("bad number literal `{s}`")))?;
        self.number_tail_ok(n)
    }

    /// after any numeric literal: a letter directly abutting it is an error
    fn number_tail_ok(&mut self, n: f64) -> Result<f64, Error> {
        if self.peek().is_some_and(is_ident_start) {
            return Err(self.err("identifier directly after number"));
        }
        Ok(n)
    }

    fn string(&mut self, quote: char) -> Result<String, Error> {
        self.bump(); // quote
        let mut s = String::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(self.err("unterminated string"));
            };
            match c {
                c if c == quote => {
                    self.bump();
                    return Ok(s);
                }
                '\n' | '\r' => return Err(self.err("newline in string literal")),
                '\\' => {
                    self.tok_escaped = true;
                    self.bump();
                    let Some(e) = self.bump() else {
                        return Err(self.err("unterminated escape"));
                    };
                    match e {
                        'n' => s.push('\n'),
                        't' => s.push('\t'),
                        'r' => s.push('\r'),
                        'b' => s.push('\u{8}'),
                        'f' => s.push('\u{c}'),
                        'v' => s.push('\u{b}'),
                        '0'..='7' => {
                            // legacy octal escape \1–\7, \00–\377 (a bare
                            // \0 with no octal digit following is the NUL
                            // escape, not an octal escape)
                            let mut v = e.to_digit(8).unwrap();
                            let mut ndig = 0;
                            for _ in 0..2 {
                                match self.peek().and_then(|d| d.to_digit(8)) {
                                    Some(d) if v < 32 => {
                                        v = v * 8 + d;
                                        ndig += 1;
                                        self.bump();
                                    }
                                    _ => break,
                                }
                            }
                            if e != '0' || ndig > 0 {
                                self.tok_octal = true;
                            }
                            s.push(char::from_u32(v).unwrap_or('\0'));
                        }
                        '8' | '9' => {
                            // \8 \9 — NotEscapeSequence-ish; keep char, flag octal
                            self.tok_octal = true;
                            s.push(e);
                        }
                        'x' => {
                            let Some(cp) = self.hex_digits(2) else {
                                return Err(self.err("bad \\x escape"));
                            };
                            s.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                        }
                        'u' => {
                            let cp = if self.peek() == Some('{') {
                                self.bump();
                                let mut v: u32 = 0;
                                let mut any = false;
                                while let Some(d) = self.peek().and_then(|c| c.to_digit(16))
                                {
                                    v = v.saturating_mul(16).saturating_add(d);
                                    any = true;
                                    self.bump();
                                }
                                if self.peek() != Some('}') || !any || v > 0x10ffff {
                                    return Err(self.err("bad \\u escape"));
                                }
                                self.bump();
                                v
                            } else {
                                self.hex_digits(4)
                                    .ok_or_else(|| self.err("bad \\u escape"))?
                            };
                            let Some(ch) = char::from_u32(cp) else {
                                return Err(self.err("bad \\u escape"));
                            };
                            s.push(ch);
                        }
                        '\n' => {} // line continuation
                        '\r' => {} // \r or \r\n continuation — bump already folded
                        '\u{2028}' | '\u{2029}' => {}
                        other => s.push(other),
                    }
                }
                c => {
                    s.push(c);
                    self.bump();
                }
            }
        }
    }

    /// Scan a template literal `` `a${x}b` `` into string parts and raw expr
    /// sources. The parser re-lexes each expr source.
    fn template(&mut self) -> Result<Vec<TemplPart>, Error> {
        self.bump(); // `
        let mut parts = Vec::new();
        let mut cur = String::new();
        loop {
            let Some(c) = self.bump() else {
                return Err(self.err("unterminated template literal"));
            };
            match c {
                '`' => {
                    if !cur.is_empty() || parts.is_empty() {
                        parts.push(TemplPart::Str(std::mem::take(&mut cur)));
                    }
                    return Ok(parts);
                }
                '$' if self.peek() == Some('{') => {
                    self.bump();
                    parts.push(TemplPart::Str(std::mem::take(&mut cur)));
                    let raw = self.template_expr_source()?;
                    parts.push(TemplPart::Expr(raw));
                }
                '\\' => {
                    let Some(e) = self.bump() else {
                        return Err(self.err("unterminated escape"));
                    };
                    match e {
                        'n' => cur.push('\n'),
                        't' => cur.push('\t'),
                        'r' => cur.push('\r'),
                        'b' => cur.push('\u{8}'),
                        'f' => cur.push('\u{c}'),
                        'v' => cur.push('\u{b}'),
                        '0'..='9' => {
                            // legacy octal / \8\9 in a template — NotEscape;
                            // flag so untagged templates become a parse error
                            self.tok_octal = true;
                            self.tok_escaped = true;
                            cur.push('\\');
                            cur.push(e);
                        }
                        '`' => cur.push('`'),
                        '$' => cur.push('$'),
                        'x' => {
                            match self
                                .hex_digits(2)
                                .and_then(char::from_u32)
                            {
                                Some(ch) => {
                                    self.tok_escaped = true;
                                    cur.push(ch);
                                }
                                None => {
                                    self.tok_escaped = true;
                                    cur.push_str("\\x");
                                }
                            }
                        }
                        'u' => {
                            let cp = if self.peek() == Some('{') {
                                self.bump();
                                let mut v: u32 = 0;
                                let mut any = false;
                                while let Some(d) =
                                    self.peek().and_then(|c| c.to_digit(16))
                                {
                                    v = v.saturating_mul(16).saturating_add(d);
                                    any = true;
                                    self.bump();
                                }
                                if self.peek() == Some('}') && any && v <= 0x10ffff {
                                    self.bump();
                                    char::from_u32(v)
                                } else {
                                    None
                                }
                            } else {
                                self.hex_digits(4).and_then(char::from_u32)
                            };
                            match cp {
                                Some(ch) => {
                                    self.tok_escaped = true;
                                    cur.push(ch);
                                }
                                None => {
                                    self.tok_escaped = true;
                                    cur.push_str("\\u");
                                }
                            }
                        }
                        '\n' => {}
                        '\r' => {}
                        '\u{2028}' | '\u{2029}' => {}
                        other => {
                            self.tok_escaped = true;
                            cur.push('\\');
                            cur.push(other);
                        }
                    }
                }
                // cooked template: raw \r / \r\n normalize to \n (bump folds)
                '\r' => cur.push('\n'),
                c => cur.push(c),
            }
        }
    }

    /// Capture raw source inside `${ ... }` with brace/string balancing.
    fn template_expr_source(&mut self) -> Result<String, Error> {
        let mut depth = 1usize;
        let mut raw = String::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(self.err("unterminated template interpolation"));
            };
            match c {
                '{' => {
                    depth += 1;
                    raw.push(c);
                    self.bump();
                }
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        self.bump();
                        return Ok(raw);
                    }
                    raw.push(c);
                    self.bump();
                }
                '"' | '\'' => {
                    let q = c;
                    raw.push(c);
                    self.bump();
                    while let Some(d) = self.bump() {
                        raw.push(d);
                        if d == '\\' {
                            if let Some(e) = self.bump() {
                                raw.push(e);
                            }
                        } else if d == q {
                            break;
                        }
                    }
                }
                '`' => {
                    // nested template — capture it verbatim
                    raw.push(c);
                    self.bump();
                    loop {
                        let Some(d) = self.bump() else {
                            return Err(self.err("unterminated nested template"));
                        };
                        raw.push(d);
                        match d {
                            '`' => break,
                            '$' if self.peek() == Some('{') => {
                                raw.push(self.bump().unwrap());
                                let inner = self.template_expr_source()?;
                                raw.push_str(&inner);
                                raw.push('}');
                            }
                            '\\' => {
                                if let Some(e) = self.bump() {
                                    raw.push(e);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                c => {
                    raw.push(c);
                    self.bump();
                }
            }
        }
    }

    /// Try to scan `/pattern/flags` starting at the current `/`.
    /// Returns None (and rewinds) if no unescaped, in-line `/` is found —
    /// the caller then treats the `/` as division.
    fn try_regex(&mut self) -> Option<Tok> {
        let (si, sl, sc, sn) = (self.i, self.line, self.col, self.newline);
        self.bump(); // leading /
        let mut pattern = String::new();
        let mut in_class = false;
        loop {
            match self.peek() {
                None => {
                    self.rewind(si, sl, sc, sn);
                    return None;
                }
                Some('\n') | Some('\r') | Some('\u{2028}') | Some('\u{2029}') => {
                    self.rewind(si, sl, sc, sn);
                    return None;
                }
                Some('\\') => {
                    self.bump();
                    match self.peek() {
                        Some('\n') | Some('\r') | Some('\u{2028}') | Some('\u{2029}')
                        | None => {
                            self.rewind(si, sl, sc, sn);
                            return None;
                        }
                        Some(e) => {
                            pattern.push('\\');
                            pattern.push(e);
                            self.bump();
                        }
                    }
                }
                Some('[') => {
                    in_class = true;
                    pattern.push('[');
                    self.bump();
                }
                Some(']') => {
                    in_class = false;
                    pattern.push(']');
                    self.bump();
                }
                Some('/') if !in_class => {
                    self.bump();
                    break;
                }
                Some(c) => {
                    pattern.push(c);
                    self.bump();
                }
            }
        }
        let mut flags = String::new();
        while let Some(c) = self.peek() {
            if is_ident_part(c) {
                flags.push(c);
                self.bump();
            } else {
                break;
            }
        }
        Some(Tok::Regex { pattern, flags })
    }

    fn rewind(&mut self, i: usize, line: u32, col: u32, newline: bool) {
        self.i = i;
        self.line = line;
        self.col = col;
        self.newline = newline;
    }

    fn punct(&mut self) -> Result<Tok, Error> {
        let rest: String = self.chars[self.i..].iter().take(4).collect();
        for p in PUNCTS {
            if rest.starts_with(p) {
                // `?.` must not match when followed by a digit (`a?.5:0` is `a ? .5 : 0`)
                if *p == "?."
                    && self
                        .chars
                        .get(self.i + 2)
                        .is_some_and(|c| c.is_ascii_digit())
                {
                    continue;
                }
                for _ in 0..p.len() {
                    self.bump();
                }
                return Ok(Tok::Punct(p));
            }
        }
        Err(self.err(format!(
            "unexpected character `{}`",
            self.peek().unwrap_or('?')
        )))
    }
}

/// Does a `/` after `prev` begin a regex literal? A `/` starts a regex when
/// the previous significant token CANNOT end an expression: operators,
/// `(` `[` `{` `,` `;` `:`, the `=` family, `=>`, or keywords like
/// `return`/`throw`/`case`/`typeof` etc., or at file start.
fn slash_is_regex(prev: Option<&Token>) -> bool {
    use Kw::*;
    match prev.map(|t| &t.tok) {
        None => true,
        // literals / identifiers / `)` `]` `}` `++` `--` end expressions → division
        Some(Tok::Num(_))
        | Some(Tok::Str(_))
        | Some(Tok::Template(_))
        | Some(Tok::Ident(_))
        | Some(Tok::Regex { .. }) => false,
        Some(Tok::Eof) => false,
        Some(Tok::Punct(p)) => !matches!(*p, ")" | "]" | "}" | "++" | "--"),
        Some(Tok::Kw(k)) => matches!(
            k,
            Return
                | Throw
                | Case
                | Delete
                | Void
                | Typeof
                | Instanceof
                | In
                | New
                | Do
                | Else
                | Yield
                | Await
                | Break
                | Continue
                | Var
                | Const
                | Function
                | If
                | For
                | While
                | Switch
                | Try
                | Catch
                | Finally
                | Class
                | Extends
                | Import
                | Export
                | Debugger
                | With
                | Enum
        ),
        // keywords NOT listed can end an expression (`this`, literals) or act
        // as contextual identifiers (`let`, `of`, `as`, `get`, `set`,
        // `static`, `async`, `from`) → division
    }
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c == '$' || c.is_alphabetic() || c as u32 > 127
}
fn is_ident_part(c: char) -> bool {
    is_ident_start(c) || c.is_ascii_digit() || c == '\u{200c}' || c == '\u{200d}'
}
