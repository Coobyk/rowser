//! Tokenizer for the rowser-js interpreter.

use crate::Error;

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Num(f64),
    /// bigint literal `123n` — payload is the literal text
    /// ("123" / "0x7f" / "0o17" / "0b101") without the `n` suffix
    BigInt(std::rc::Rc<str>),
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
    /// literal segment. `cooked` is the template value (escapes
    /// applied); `None` means the segment contains an invalid escape —
    /// legal only in a tagged template where the cooked value reads as
    /// `undefined` (an untagged template is a SyntaxError). `raw` is
    /// the verbatim segment text (the TRV — line terminators already
    /// normalized to `\n`).
    Str { cooked: Option<String>, raw: String },
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
    /// char-index of the token's first char in the source (the lexer
    /// works on `chars: Vec<char>` — this is a CHAR index, not bytes)
    pub pos: usize,
    /// token length in chars — `pos + len` is the char-index just past
    /// the token's last char
    pub len: usize,
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
        // `{`/`}` context stack: entry = the `{` is an object literal (in
        // expression position). A `}` closing a BLOCK re-enables regex
        // literals for the following `/` — `{ if (x) { } /re/g }`
        let mut brace_expr: Vec<bool> = Vec::new();
        let mut rbrace_block = false;
        loop {
            self.skip_trivia()?;
            let (line, col, newline_before) = (self.line, self.col, self.newline);
            let tok_start = self.i;
            self.newline = false;
            let Some(c) = self.peek() else {
                out.push(Token {
                    tok: Tok::Eof,
                    pos: tok_start,
                    len: 0,
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
                self.number()?
            } else if is_ident_start(c) || c == '\\' {
                let name = self.ident()?;
                // escaped reserved words lex to their keyword token —
                // IdentifierReference/BindingIdentifier positions only
                // accept Tok::Ident so they're rejected contextually,
                // while IdentifierName positions (member names, prop
                // keys, labels) accept them per spec
                match keyword(&name) {
                    // escaped CONTEXTUAL keywords are ordinary identifiers
                    // (`as\u0079nc`, `l\u0065t`, `yi\u0065ld`) — only true
                    // ReservedWords keep their Kw token to be rejected
                    // contextually by the parser
                    Some(kw)
                        if self.tok_escaped
                            && matches!(
                                kw,
                                Kw::Let
                                    | Kw::Static
                                    | Kw::Async
                                    | Kw::Await
                                    | Kw::Yield
                                    | Kw::Of
                                    | Kw::As
                                    | Kw::From
                                    | Kw::Get
                                    | Kw::Set
                            ) =>
                    {
                        Tok::Ident(name)
                    }
                    Some(kw) => Tok::Kw(kw),
                    None => Tok::Ident(name),
                }
            } else if c == '#' {
                // `#name` — private class member / brand check (`#x in obj`)
                self.bump();
                let name = self.ident()?;
                Tok::Ident(format!("#{name}"))
            } else if c == '/' && (rbrace_block || slash_is_regex(out.last())) {
                // regex-literal context; if the scan fails (e.g. it was
                // actually division), fall back to punct tokenization
                match self.try_regex() {
                    Some(t) => t,
                    None => self.punct()?,
                }
            } else {
                self.punct()?
            };
            match &tok {
                Tok::Punct("{") => {
                    brace_expr.push(brace_is_object(out.last()));
                    rbrace_block = false;
                }
                Tok::Punct("}") => {
                    rbrace_block = !brace_expr.pop().unwrap_or(true);
                }
                _ => rbrace_block = false,
            }
            let (octal, escaped) = (self.tok_octal, self.tok_escaped);
            self.tok_octal = false;
            self.tok_escaped = false;
            // newlines seen INSIDE the token (line continuations in
            // strings, template bodies) don't count for ASI — only real
            // inter-token line breaks set the next token's flag
            self.newline = false;
            out.push(Token {
                tok,
                pos: tok_start,
                len: self.i - tok_start,
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
                // JS WhiteSpace = TAB/VT/FF/SP/NBSP/FEFF + Zs; NEL and
                // FS/GS/RS/US are not whitespace
                Some(c)
                    if (c.is_whitespace() && !matches!(c, '\u{85}' | '\u{1c}'..='\u{1f}'))
                        || c == '\u{feff}' =>
                {
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
                    if self.newline && self.peek2() == Some('-') && self.peek3() == Some('>') =>
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
                        // position 0 needs ID_Start (ZWJ/ZWNJ are
                        // Continue-only — `\u200D_x` is a syntax error)
                        Some(ch)
                            if if s.is_empty() {
                                is_ident_start(ch)
                            } else {
                                is_ident_part(ch)
                            } =>
                        {
                            s.push(ch)
                        }
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

    /// scan digits of one class; `_` is legal only between two digits.
    fn scan_num_digits(&mut self, pred: impl Fn(char) -> bool) -> Result<String, Error> {
        let mut out = String::new();
        let mut last_digit = false;
        loop {
            match self.peek() {
                Some(c) if pred(c) => {
                    out.push(c);
                    self.bump();
                    last_digit = true;
                }
                Some('_') => {
                    if !last_digit {
                        return Err(self.err("numeric separator must be between digits"));
                    }
                    self.bump();
                    match self.peek() {
                        Some(c2) if pred(c2) => {}
                        _ => return Err(self.err("numeric separator must be between digits")),
                    }
                    last_digit = false;
                }
                _ => break,
            }
        }
        Ok(out)
    }

    fn number(&mut self) -> Result<Tok, Error> {
        let mut s = String::new();
        if self.peek() == Some('0') {
            let radix = match self.peek2() {
                Some('x') | Some('X') => Some(16u32),
                Some('o') | Some('O') => Some(8u32),
                Some('b') | Some('B') => Some(2u32),
                _ => None,
            };
            if let Some(r) = radix {
                self.bump();
                self.bump();
                let v = self.scan_num_digits(|c| c.is_digit(r))?;
                if v.is_empty() {
                    return Err(self.err("missing digits in numeric literal"));
                }
                let n = v
                    .chars()
                    .fold(0f64, |a, c| a * r as f64 + c.to_digit(r).unwrap() as f64);
                if self.peek() == Some('n') {
                    self.bump();
                    let p = match r {
                        16 => "0x",
                        8 => "0o",
                        _ => "0b",
                    };
                    let raw = std::rc::Rc::from(format!("{p}{v}").as_str());
                    return self.number_tail_ok(Tok::BigInt(raw));
                }
                return self.number_tail_ok(Tok::Num(n));
            }
        }
        // legacy octal: `0` followed by octal digits (may include 8/9 which
        // makes it a NonOctalDecimalIntegerLiteral — still a strict error)
        if self.peek() == Some('0') && self.peek2().is_some_and(|c| c.is_ascii_digit()) {
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
                        '_' => return Err(self.err("numeric separator must be between digits")),
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
                Some(n) => self.number_tail_ok(Tok::Num(n)),
                None => Err(self.err("bad number literal")),
            };
        }
        // leading-zero decimal like `0` alone; `0_…` is invalid —
        // separators can't follow a leading zero
        if self.peek() == Some('0') && self.peek2() == Some('_') {
            return Err(self.err("numeric separator must be between digits"));
        }
        let mut s = self.scan_num_digits(|c| c.is_ascii_digit())?;
        let mut seen_dot = false;
        let mut seen_exp = false;
        // a `.` right after digits is always part of the literal —
        // `10.` is a complete DecimalLiteral; a following identifier
        // char is then an early error (`10.foo`, `10._`) caught by
        // `number_tail_ok`, while `e`/`E` starts the exponent (`7.e1`)
        if self.peek() == Some('.') {
            seen_dot = true;
            s.push('.');
            self.bump();
            s.push_str(&self.scan_num_digits(|c| c.is_ascii_digit())?);
        }
        if matches!(self.peek(), Some('e') | Some('E')) {
            seen_exp = true;
            s.push('e');
            self.bump();
            if matches!(self.peek(), Some('+') | Some('-')) {
                s.push(self.bump().unwrap());
            }
            let exp = self.scan_num_digits(|c| c.is_ascii_digit())?;
            if exp.is_empty() {
                return Err(self.err("missing exponent digits"));
            }
            s.push_str(&exp);
        }
        if self.peek() == Some('n') {
            // bigint literal — decimal integers only
            self.bump();
            if seen_dot || seen_exp {
                return Err(self.err("bigint literal must be an integer"));
            }
            return self.number_tail_ok(Tok::BigInt(std::rc::Rc::from(s.as_str())));
        }
        let _ = seen_exp;
        self.number_tail(s)
    }

    fn number_tail(&mut self, s: String) -> Result<Tok, Error> {
        let n = s
            .parse::<f64>()
            .map_err(|_| self.err(format!("bad number literal `{s}`")))?;
        self.number_tail_ok(Tok::Num(n))
    }

    /// after any numeric literal: a letter directly abutting it is an error
    fn number_tail_ok(&mut self, n: Tok) -> Result<Tok, Error> {
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
                            // `\0` followed by 8/9 is a
                            // NonOctalDecimalEscapeSequence — sloppy
                            // yields NUL + literal digit, strict errors
                            if e != '0' || ndig > 0 || matches!(self.peek(), Some('8' | '9')) {
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
                                while let Some(d) = self.peek().and_then(|c| c.to_digit(16)) {
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
                            // surrogate escapes pair into an astral char;
                            // lone surrogates are WTF-16 code units we
                            // can't store in a UTF-8 string → U+FFFD
                            let ch = if (0xd800..=0xdbff).contains(&cp) {
                                let (si, scol) = (self.i, self.col);
                                let mut paired = None;
                                if self.peek() == Some('\\') && self.peek2() == Some('u') {
                                    self.bump();
                                    self.bump();
                                    if let Some(lo) = self.hex_digits(4) {
                                        if (0xdc00..=0xdfff).contains(&lo) {
                                            paired = Some(
                                                0x10000 + ((cp - 0xd800) << 10) + (lo - 0xdc00),
                                            );
                                        }
                                    }
                                    if paired.is_none() {
                                        self.i = si;
                                        self.col = scol;
                                    }
                                }
                                paired.and_then(char::from_u32).unwrap_or('\u{fffd}')
                            } else {
                                char::from_u32(cp).unwrap_or('\u{fffd}')
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

    /// append the source chars consumed since `s0` to `raw` — escape
    /// bodies are hex digits / `{`/`}`, never line terminators, so no
    /// folding is needed
    fn push_raw_since(&self, raw: &mut String, s0: usize) {
        for &ch in &self.chars[s0..self.i] {
            raw.push(ch);
        }
    }

    /// Scan a template literal `` `a${x}b` `` into string parts and raw expr
    /// sources. Each literal part keeps BOTH the cooked value (the TV —
    /// escapes applied) and the raw text (the TRV — verbatim, with line
    /// terminators folded to `\n`). An invalid escape (NotEscapeSequence)
    /// poisons the segment: cooked becomes `None`, which only a tagged
    /// template may consume (as `undefined`); untagged use is a parse
    /// error in the parser. The parser re-lexes each expr source.
    fn template(&mut self) -> Result<Vec<TemplPart>, Error> {
        self.bump(); // `
        let mut parts = Vec::new();
        let mut cooked = String::new();
        let mut raw = String::new();
        // false once the segment's TV is poisoned by an invalid escape
        let mut ok = true;
        loop {
            let Some(c) = self.bump() else {
                return Err(self.err("unterminated template literal"));
            };
            match c {
                '`' => {
                    // take unconditionally — a poisoned segment's buffer
                    // must not leak into the next segment's cooked
                    let ck = std::mem::take(&mut cooked);
                    parts.push(TemplPart::Str {
                        cooked: if ok { Some(ck) } else { None },
                        raw: std::mem::take(&mut raw),
                    });
                    return Ok(parts);
                }
                '$' if self.peek() == Some('{') => {
                    self.bump();
                    let ck = std::mem::take(&mut cooked);
                    parts.push(TemplPart::Str {
                        cooked: if ok { Some(ck) } else { None },
                        raw: std::mem::take(&mut raw),
                    });
                    ok = true;
                    let src = self.template_expr_source()?;
                    parts.push(TemplPart::Expr(src));
                }
                '\\' => {
                    raw.push('\\');
                    let Some(e) = self.bump() else {
                        return Err(self.err("unterminated escape"));
                    };
                    match e {
                        'n' => {
                            raw.push('n');
                            cooked.push('\n');
                        }
                        't' => {
                            raw.push('t');
                            cooked.push('\t');
                        }
                        'r' => {
                            raw.push('r');
                            cooked.push('\r');
                        }
                        'b' => {
                            raw.push('b');
                            cooked.push('\u{8}');
                        }
                        'f' => {
                            raw.push('f');
                            cooked.push('\u{c}');
                        }
                        'v' => {
                            raw.push('v');
                            cooked.push('\u{b}');
                        }
                        '0'..='9' => {
                            raw.push(e);
                            // `\0` alone is the NUL escape; `\0`+digit and
                            // \1-\9 are NotEscapeSequence — tagged-ok only
                            if e == '0' && !self.peek().is_some_and(|d| d.is_ascii_digit()) {
                                cooked.push('\0');
                            } else {
                                ok = false;
                            }
                        }
                        'x' => {
                            raw.push('x');
                            let s0 = self.i;
                            match self.hex_digits(2) {
                                Some(cp) => {
                                    self.push_raw_since(&mut raw, s0);
                                    cooked.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                                }
                                None => {
                                    self.push_raw_since(&mut raw, s0);
                                    ok = false;
                                }
                            }
                        }
                        'u' => {
                            raw.push('u');
                            let s0 = self.i;
                            let cp = if self.peek() == Some('{') {
                                self.bump();
                                let mut v: u32 = 0;
                                let mut any = false;
                                while let Some(d) = self.peek().and_then(|c| c.to_digit(16)) {
                                    v = v.saturating_mul(16).saturating_add(d);
                                    any = true;
                                    self.bump();
                                }
                                if self.peek() == Some('}') && any && v <= 0x10ffff {
                                    self.bump();
                                    self.push_raw_since(&mut raw, s0);
                                    Some(char::from_u32(v).unwrap_or('\u{fffd}'))
                                } else {
                                    self.push_raw_since(&mut raw, s0);
                                    None
                                }
                            } else {
                                match self.hex_digits(4) {
                                    Some(cp) => {
                                        // adjacent `\uD800\uDC00`-style
                                        // escapes pair into an astral char
                                        // (string-literal convention; a
                                        // lone surrogate cooks to U+FFFD)
                                        let ch = if (0xd800..=0xdbff).contains(&cp) {
                                            let (si, scol) = (self.i, self.col);
                                            let mut paired = None;
                                            if self.peek() == Some('\\')
                                                && self.peek2() == Some('u')
                                            {
                                                self.bump();
                                                self.bump();
                                                if let Some(lo) = self.hex_digits(4) {
                                                    if (0xdc00..=0xdfff).contains(&lo) {
                                                        paired = Some(
                                                            0x10000
                                                                + ((cp - 0xd800) << 10)
                                                                + (lo - 0xdc00),
                                                        );
                                                    }
                                                }
                                                if paired.is_none() {
                                                    self.i = si;
                                                    self.col = scol;
                                                }
                                            }
                                            paired.and_then(char::from_u32).unwrap_or('\u{fffd}')
                                        } else {
                                            char::from_u32(cp).unwrap_or('\u{fffd}')
                                        };
                                        self.push_raw_since(&mut raw, s0);
                                        Some(ch)
                                    }
                                    None => {
                                        self.push_raw_since(&mut raw, s0);
                                        None
                                    }
                                }
                            };
                            match cp {
                                Some(ch) => cooked.push(ch),
                                None => ok = false,
                            }
                        }
                        // line continuation: TV is empty; TRV is `\` +
                        // the LineTerminatorSequence folded to \n
                        '\n' | '\r' => raw.push('\n'),
                        '\u{2028}' | '\u{2029}' => raw.push(e),
                        other => {
                            // SingleEscapeCharacter (' " \ `) or a
                            // NonEscapeCharacter — the TV is the char
                            // itself, the TRV keeps the backslash
                            raw.push(other);
                            cooked.push(other);
                        }
                    }
                }
                // CR / CRLF fold to a single \n in both TV and TRV
                // (bump() already consumed the pair)
                '\r' => {
                    cooked.push('\n');
                    raw.push('\n');
                }
                c => {
                    cooked.push(c);
                    raw.push(c);
                }
            }
        }
    }

    /// Capture raw source inside `${ ... }` with brace/string balancing.
    fn template_expr_source(&mut self) -> Result<String, Error> {
        /// heuristic for `/` inside a template interpolation: after `(` `,`
        /// `=` operators or keywords it's a regex, after a value it's division
        fn tmpl_regex_ctx(raw: &str) -> bool {
            let trimmed = raw.trim_end();
            let Some(c) = trimmed.chars().last() else {
                return true;
            };
            if "([{,:;=!&|?+-*/%^~<>".contains(c) {
                return true;
            }
            for kw in [
                "return",
                "typeof",
                "instanceof",
                "new",
                "delete",
                "void",
                "throw",
                "yield",
                "await",
                "case",
                "do",
                "else",
                "in",
                "of",
            ] {
                if trimmed.ends_with(kw) {
                    let before = trimmed[..trimmed.len() - kw.len()].chars().last();
                    if before.is_none()
                        || !(before.unwrap().is_alphanumeric() || "_$".contains(before.unwrap()))
                    {
                        return true;
                    }
                }
            }
            false
        }
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
                '/' if tmpl_regex_ctx(&raw) => {
                    // regex literal inside an interpolation — scan verbatim
                    // so quotes/braces in the pattern don't break tracking
                    raw.push('/');
                    self.bump();
                    let mut in_class = false;
                    while let Some(d) = self.peek() {
                        raw.push(d);
                        self.bump();
                        match d {
                            '\\' => {
                                if let Some(e) = self.peek() {
                                    raw.push(e);
                                    self.bump();
                                }
                            }
                            '[' => in_class = true,
                            ']' => in_class = false,
                            '/' if !in_class => break,
                            '\n' | '\r' => break,
                            _ => {}
                        }
                    }
                    while matches!(self.peek(), Some(c) if c.is_alphanumeric() || c == '_') {
                        raw.push(self.bump().unwrap());
                    }
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
                        Some('\n') | Some('\r') | Some('\u{2028}') | Some('\u{2029}') | None => {
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

/// Is a `{` after `prev` an object literal? True when the `{` sits where
/// an expression could start — after `(`/`[`/`{`/`,`/`:`/`?`/`=>`, every
/// binary/unary operator, or an expression-start keyword (`return`,
/// `typeof`, `case`, …). Everywhere else — statement start, `)`, `]`,
/// `}`, `;`, identifiers, literals — `{` opens a block.
fn brace_is_object(prev: Option<&Token>) -> bool {
    use Kw::*;
    match prev.map(|t| &t.tok) {
        None => false,
        Some(Tok::Num(_))
        | Some(Tok::BigInt(_))
        | Some(Tok::Str(_))
        | Some(Tok::Template(_))
        | Some(Tok::Ident(_))
        | Some(Tok::Regex { .. })
        | Some(Tok::Eof) => false,
        Some(Tok::Punct(p)) => !matches!(*p, ")" | "]" | "}" | ";" | "." | "++" | "--"),
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
                | Yield
                | Await
                | Of
                | Extends
        ),
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
        | Some(Tok::BigInt(_))
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
    c == '_' || c == '$' || unicode_ident::is_xid_start(c)
}
fn is_ident_part(c: char) -> bool {
    is_ident_start(c) || unicode_ident::is_xid_continue(c) || c == '\u{200c}' || c == '\u{200d}'
}
