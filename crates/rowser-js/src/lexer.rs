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
}

const PUNCTS: &[&str] = &[
    ">>>=",
    "===",
    "!==",
    "**=",
    "<<=",
    ">>=",
    "&&=",
    "||=",
    "??=",
    "...",
    ">>>",
    "=>",
    "==",
    "!=",
    "<=",
    ">=",
    "++",
    "--",
    "+=",
    "-=",
    "*=",
    "/=",
    "%=",
    "&&",
    "||",
    "??",
    "?.",
    "**",
    "<<",
    ">>",
    "&=",
    "|=",
    "^=",
    "{",
    "}",
    "(",
    ")",
    "[",
    "]",
    ";",
    ",",
    "<",
    ">",
    "+",
    "-",
    "*",
    "/",
    "%",
    "&",
    "|",
    "^",
    "!",
    "~",
    "?",
    ":",
    ".",
    "=",
];

pub fn lex(src: &str) -> Result<Vec<Token>, Error> {
    let mut lx = Lexer {
        chars: src.chars().collect(),
        i: 0,
        line: 1,
        col: 1,
        newline: true,
    };
    lx.run()
}

struct Lexer {
    chars: Vec<char>,
    i: usize,
    line: u32,
    col: u32,
    newline: bool,
}

impl Lexer {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.i).copied()
    }
    fn peek2(&self) -> Option<char> {
        self.chars.get(self.i + 1).copied()
    }
    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.i += 1;
        if c == '\n' {
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
                });
                return Ok(out);
            };
            let tok = if c == '"' || c == '\'' {
                Tok::Str(self.string(c)?)
            } else if c == '`' {
                Tok::Template(self.template()?)
            } else if c.is_ascii_digit() || (c == '.' && self.peek2().is_some_and(|d| d.is_ascii_digit())) {
                Tok::Num(self.number()?)
            } else if is_ident_start(c) {
                let name = self.ident();
                match keyword(&name) {
                    Some(kw) => Tok::Kw(kw),
                    None => Tok::Ident(name),
                }
            } else {
                self.punct()?
            };
            out.push(Token {
                tok,
                line,
                col,
                newline_before,
            });
        }
    }

    fn skip_trivia(&mut self) -> Result<(), Error> {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() || c == '\u{feff}' => {
                    self.bump();
                }
                Some('/') => match self.peek2() {
                    Some('/') => {
                        while let Some(c) = self.peek() {
                            if c == '\n' {
                                break;
                            }
                            self.bump();
                        }
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

    fn ident(&mut self) -> String {
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if is_ident_part(c) {
                s.push(c);
                self.bump();
            } else if c == '\\' {
                // minimal \u escape in identifiers
                self.bump();
                if self.peek() == Some('u') {
                    self.bump();
                    if let Some(cp) = self.hex_digits(4) {
                        if let Some(ch) = char::from_u32(cp) {
                            s.push(ch);
                        }
                    }
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        s
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
                    while self.peek().is_some_and(|c| ('0'..='7').contains(&c) || c == '_') {
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
                    while self.peek().is_some_and(|c| c == '0' || c == '1' || c == '_') {
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
        // legacy octal 0o handled above; leading-zero decimal like 010 → treat as decimal
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
        s.parse::<f64>().map_err(|_| self.err(format!("bad number literal `{s}`")))
    }

    fn string(&mut self, quote: char) -> Result<String, Error> {
        self.bump(); // quote
        let mut s = String::new();
        loop {
            let Some(c) = self.bump() else {
                return Err(self.err("unterminated string"));
            };
            match c {
                c if c == quote => return Ok(s),
                '\\' => {
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
                        '0' => s.push('\0'),
                        'x' => {
                            if let Some(cp) = self.hex_digits(2) {
                                if let Some(ch) = char::from_u32(cp) {
                                    s.push(ch);
                                }
                            }
                        }
                        'u' => {
                            if self.peek() == Some('{') {
                                self.bump();
                                let mut v: u32 = 0;
                                while let Some(d) = self.peek().and_then(|c| c.to_digit(16)) {
                                    v = v * 16 + d;
                                    self.bump();
                                }
                                if self.peek() == Some('}') {
                                    self.bump();
                                }
                                if let Some(ch) = char::from_u32(v) {
                                    s.push(ch);
                                }
                            } else if let Some(cp) = self.hex_digits(4) {
                                if let Some(ch) = char::from_u32(cp) {
                                    s.push(ch);
                                }
                            }
                        }
                        '\n' => {} // line continuation
                        '\r' => {
                            if self.peek() == Some('\n') {
                                self.bump();
                            }
                        }
                        other => s.push(other),
                    }
                }
                '\n' => return Err(self.err("newline in string literal")),
                c => s.push(c),
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
                        '`' => cur.push('`'),
                        '$' => cur.push('$'),
                        'x' => {
                            if let Some(cp) = self.hex_digits(2) {
                                if let Some(ch) = char::from_u32(cp) {
                                    cur.push(ch);
                                }
                            }
                        }
                        'u' => {
                            if let Some(cp) = self.hex_digits(4) {
                                if let Some(ch) = char::from_u32(cp) {
                                    cur.push(ch);
                                }
                            }
                        }
                        '\n' => {}
                        other => {
                            cur.push('\\');
                            cur.push(other);
                        }
                    }
                }
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

    fn punct(&mut self) -> Result<Tok, Error> {
        let rest: String = self.chars[self.i..].iter().take(4).collect();
        for p in PUNCTS {
            if rest.starts_with(p) {
                // `?.` must not match when followed by a digit (`a?.5:0` is `a ? .5 : 0`)
                if *p == "?." && self.chars.get(self.i + 2).is_some_and(|c| c.is_ascii_digit()) {
                    continue;
                }
                for _ in 0..p.len() {
                    self.bump();
                }
                return Ok(Tok::Punct(p));
            }
        }
        Err(self.err(format!("unexpected character `{}`", self.peek().unwrap_or('?'))))
    }
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c == '$' || c.is_alphabetic() || c as u32 > 127
}
fn is_ident_part(c: char) -> bool {
    is_ident_start(c) || c.is_ascii_digit() || c == '\u{200c}' || c == '\u{200d}'
}
