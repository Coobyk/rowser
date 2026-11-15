//! Recursive-descent parser producing the AST.

use crate::ast::*;
use crate::lexer::{lex, Kw, Tok, Token};
use crate::Error;
use std::rc::Rc;

const MAX_PARSE_DEPTH: usize = 300;

pub fn parse(src: &str) -> Result<Vec<Stmt>, Error> {
    let toks = lex(src)?;
    let mut p = Parser::new(toks);
    let stmts = p.program()?;
    validate(&stmts)?;
    Ok(stmts)
}

/// Parse eval source: `strict` = strictCaller (a "use strict" directive in
/// the source ORs on top), `in_fn`/`in_method` seed new.target/super
/// availability from the calling context.
pub fn parse_eval(
    src: &str,
    strict: bool,
    in_fn: bool,
    in_method: bool,
) -> Result<Vec<Stmt>, Error> {
    let toks = lex(src)?;
    let mut p = Parser::new(toks);
    p.strict = strict;
    if in_fn {
        // new.target only — `return` is a SyntaxError in ALL eval code,
        // even inside a function (early error on StatementList)
        p.nonarrow_fn_depth = 1;
    }
    if in_method {
        p.method_depth = 1;
    }
    let stmts = p.program()?;
    validate_strict_ctx(&stmts, strict, in_fn, in_method)?;
    Ok(stmts)
}

/// Does a statement list begin with a "use strict" directive prologue?
pub fn stmts_strict(stmts: &[Stmt]) -> bool {
    for s in stmts {
        match s {
            Stmt::Expr(Expr::Str(t)) if t == "use strict" => return true,
            Stmt::Expr(Expr::Str(_)) => {}
            _ => return false,
        }
    }
    false
}

/// Parse a single expression from source (used for template interpolations).
fn parse_expr_src(src: &str) -> Result<Expr, Error> {
    let toks = lex(src)?;
    let mut p = Parser::new(toks);
    let e = p.expression()?;
    if !matches!(p.peek().tok, Tok::Eof) {
        return Err(p.err("trailing tokens in expression"));
    }
    Ok(e)
}

struct Parser {
    toks: Vec<Token>,
    pos: usize,
    /// when set, `in` is NOT treated as a binary operator — used while
    /// parsing for-head init expressions so `for (x in y)` and the common
    /// minified `for (a in b&&c, d)` forms parse correctly
    no_in: bool,
    /// depth >0 while parsing the binding list of a let/const (lexical)
    /// declaration — `let` can't be bound there even in sloppy mode
    lex_decl: u32,
    /// strict-mode context (directive prologue, class body, inherited)
    strict: bool,
    /// >0 while inside `(` — the contents may reinterpret as a
    /// destructuring pattern, suppressing ObjectLiteral early errors
    paren_depth: u32,
    /// inside a function body — `return` validity
    fn_depth: usize,
    /// non-arrow function nesting — `new.target`, `return`, and
    /// parameter/super checks use this (arrows inherit, so they don't bump)
    nonarrow_fn_depth: usize,
    /// inside a generator body — `yield` is the yield-expression
    gen_depth: usize,
    /// `yield` is a reserved identifier here ([Yield] static context):
    /// function *declarations* inherit it, expressions/methods reset
    /// it, arrows transparently share the enclosing context
    yield_decl: usize,
    /// inside a class constructor — `super()` call validity
    ctor_depth: usize,
    /// inside any method/ctor — `super.prop` validity
    method_depth: usize,
    /// inside FormalParameters — YieldExpression is an early error here
    param_depth: usize,
    /// recursion guard for deeply nested source
    depth: usize,
}

impl Parser {
    fn new(toks: Vec<Token>) -> Self {
        Parser {
            toks,
            pos: 0,
            no_in: false,
            lex_decl: 0,
            strict: false,
            paren_depth: 0,
            fn_depth: 0,
            nonarrow_fn_depth: 0,
            gen_depth: 0,
            yield_decl: 0,
            ctor_depth: 0,
            method_depth: 0,
            param_depth: 0,
            depth: 0,
        }
    }

    // ---- token helpers ----------------------------------------------------
    fn peek(&self) -> &Token {
        &self.toks[self.pos.min(self.toks.len() - 1)]
    }
    fn peek2(&self) -> &Token {
        &self.toks[(self.pos + 1).min(self.toks.len() - 1)]
    }
    fn next(&mut self) -> &Token {
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        &self.toks[self.pos - 1]
    }
    fn err(&self, msg: impl Into<String>) -> Error {
        let t = self.peek();
        Error::new(msg, t.line, t.col)
    }
    fn at_eof(&self) -> bool {
        matches!(self.peek().tok, Tok::Eof)
    }
    fn at_punct(&self, p: &str) -> bool {
        matches!(&self.peek().tok, Tok::Punct(q) if *q == p)
    }
    fn at_kw(&self, k: Kw) -> bool {
        matches!(&self.peek().tok, Tok::Kw(q) if *q == k)
    }
    fn eat_punct(&mut self, p: &str) -> bool {
        if self.at_punct(p) {
            self.next();
            true
        } else {
            false
        }
    }
    fn eat_kw(&mut self, k: Kw) -> bool {
        if self.at_kw(k) {
            self.next();
            true
        } else {
            false
        }
    }
    fn expect_punct(&mut self, p: &str) -> Result<(), Error> {
        if self.eat_punct(p) {
            Ok(())
        } else {
            Err(self.err(format!("expected `{p}`, found {}", self.describe())))
        }
    }
    fn expect_kw(&mut self, k: Kw) -> Result<(), Error> {
        if self.eat_kw(k) {
            Ok(())
        } else {
            Err(self.err(format!(
                "expected `{}`, found {}",
                k.name(),
                self.describe()
            )))
        }
    }
    fn describe(&self) -> String {
        match &self.peek().tok {
            Tok::Eof => "end of input".into(),
            Tok::Ident(s) => format!("`{s}`"),
            Tok::Kw(k) => format!("`{}`", k.name()),
            Tok::Punct(p) => format!("`{p}`"),
            Tok::Num(n) => format!("`{n}`"),
            Tok::BigInt(n) => format!("`{n}n`"),
            Tok::Str(s) => format!("`\"{s}\"`"),
            Tok::Template(_) => "template literal".into(),
            Tok::Regex { pattern, flags } => format!("`/{pattern}/{flags}`"),
        }
    }
    /// Automatic semicolon insertion: consume `;`, or allow newline/`}`/EOF.
    fn semi(&mut self) -> Result<(), Error> {
        if self.eat_punct(";") {
            return Ok(());
        }
        if self.at_eof() || self.at_punct("}") || self.peek().newline_before {
            return Ok(());
        }
        Err(self.err(format!("expected `;`, found {}", self.describe())))
    }

    /// Identifier position that also accepts contextual keywords used as names.
    fn ident_name(&mut self) -> Result<String, Error> {
        match self.peek().tok.clone() {
            Tok::Ident(s) => {
                self.next();
                Ok(s)
            }
            // contextual keywords usable as identifiers/property names
            Tok::Kw(k) if is_contextual_ident_kw(&k) => {
                self.next();
                Ok(k.name().to_string())
            }
            _ => Err(self.err(format!("expected identifier, found {}", self.describe()))),
        }
    }

    /// Property-name position: identifiers, keywords, strings, numbers, [computed].
    fn prop_key(&mut self, computed_ok: bool) -> Result<PropKey, Error> {
        match self.peek().tok.clone() {
            Tok::Ident(s) => {
                self.next();
                Ok(PropKey::Ident(s))
            }
            Tok::Kw(k) => {
                self.next();
                Ok(PropKey::Ident(k.name().to_string()))
            }
            Tok::Str(s) => {
                self.check_octal()?;
                self.next();
                Ok(PropKey::Str(s))
            }
            Tok::Num(n) => {
                self.check_octal()?;
                self.next();
                Ok(PropKey::Num(n))
            }
            Tok::Punct("[") if computed_ok => {
                self.next();
                let e = self.expression()?;
                self.expect_punct("]")?;
                Ok(PropKey::Computed(e))
            }
            _ => Err(self.err(format!("expected property name, found {}", self.describe()))),
        }
    }

    // ---- program & statements --------------------------------------------
    fn program(&mut self) -> Result<Vec<Stmt>, Error> {
        let mut stmts = Vec::new();
        let mut prologue = true;
        while !self.at_eof() {
            let dir = self.directive_candidate();
            let s = self.statement()?;
            self.check_directive(&s, dir, &mut prologue);
            stmts.push(s);
        }
        Ok(stmts)
    }

    /// Is the next token a String literal that could be a directive?
    /// (escapes or parens disqualify it)
    fn directive_candidate(&self) -> bool {
        matches!(&self.peek().tok, Tok::Str(_)) && !self.peek().escaped
    }

    /// Directive-prologue bookkeeping: `Stmt` continues the prologue iff it
    /// is a bare string expression; an unescaped "use strict" turns strict on.
    fn check_directive(&mut self, s: &Stmt, tok_was_str: bool, prologue: &mut bool) {
        if !*prologue {
            return;
        }
        match s {
            Stmt::Expr(Expr::Str(t)) => {
                if tok_was_str && t == "use strict" {
                    self.strict = true;
                }
            }
            _ => *prologue = false,
        }
    }

    /// Function body `{ stmts }`: handles the directive prologue and
    /// saves/restores the context flags that reset at function boundaries.
    /// `arrow`: arrows inherit super/new.target context (but never `yield`).
    fn fn_body(&mut self, arrow: bool) -> Result<Vec<Stmt>, Error> {
        let saved = (
            self.strict,
            self.fn_depth,
            self.gen_depth,
            self.ctor_depth,
            self.method_depth,
        );
        self.fn_depth += 1;
        if !arrow {
            self.nonarrow_fn_depth += 1;
        }
        // arrows are transparent to `yield` — a `yield` inside an arrow
        // inside a generator belongs to that generator
        // strictness of this body = inherited strict or own directive —
        // peek at the first body token now so params/strict errors see it
        if self.directive_candidate() {
            if let Tok::Str(t) = &self.peek().tok {
                if t == "use strict" {
                    self.strict = true;
                }
            }
        }
        let mut stmts = Vec::new();
        let mut prologue = true;
        while !self.at_punct("}") {
            if self.at_eof() {
                return Err(self.err("unterminated block"));
            }
            let dir = self.directive_candidate();
            let s = self.statement()?;
            self.check_directive(&s, dir, &mut prologue);
            stmts.push(s);
        }
        self.expect_punct("}")?;
        (
            self.strict,
            self.fn_depth,
            self.gen_depth,
            self.ctor_depth,
            self.method_depth,
        ) = saved;
        if !arrow {
            self.nonarrow_fn_depth -= 1;
        }
        Ok(stmts)
    }

    fn statement(&mut self) -> Result<Stmt, Error> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            self.depth -= 1;
            return Err(self.err("maximum parse depth exceeded"));
        }
        let r = self.statement_inner();
        self.depth -= 1;
        r
    }

    fn statement_inner(&mut self) -> Result<Stmt, Error> {
        // an escaped reserved word is an IdentifierName — never the
        // keyword itself (`br\u0061k;` is not a break statement)
        if self.peek().escaped && matches!(self.peek().tok, Tok::Kw(_)) {
            return Err(self.err("escaped reserved word"));
        }
        match self.peek().tok.clone() {
            Tok::Punct(";") => {
                self.next();
                Ok(Stmt::Empty)
            }
            Tok::Punct("{") => {
                self.next();
                let b = self.block_body()?;
                Ok(Stmt::Block(b))
            }
            Tok::Kw(Kw::Var) | Tok::Kw(Kw::Const) => {
                let (kind, decls) = self.var_decl()?;
                self.semi()?;
                Ok(Stmt::VarDecl { kind, decls })
            }
            Tok::Kw(Kw::Let) => {
                // `let` can be an identifier: `let = 5`, `let.x`, `let(x)` or
                // a declaration `let x ...`. Disambiguate by lookahead.
                if matches!(
                    self.peek2().tok,
                    Tok::Ident(_) | Tok::Punct("[") | Tok::Punct("{")
                ) || matches!(&self.peek2().tok, Tok::Kw(k) if is_contextual_ident_kw(k))
                {
                    let (kind, decls) = self.var_decl()?;
                    self.semi()?;
                    Ok(Stmt::VarDecl { kind, decls })
                } else {
                    let e = self.expression()?;
                    self.semi()?;
                    Ok(Stmt::Expr(e))
                }
            }
            Tok::Kw(Kw::Function) => {
                self.next();
                let g = self.eat_punct("*");
                let f = self.fn_tail(true, g)?;
                Ok(Stmt::FnDecl(Rc::new(f)))
            }
            Tok::Kw(Kw::Async) if matches!(self.peek2().tok, Tok::Kw(Kw::Function)) => {
                self.next();
                self.next();
                let g = self.eat_punct("*");
                let mut f = self.fn_tail(true, g)?;
                f.is_async = true;
                Ok(Stmt::FnDecl(Rc::new(f)))
            }
            Tok::Kw(Kw::Class) => {
                self.next();
                let c = self.class_tail(true)?;
                Ok(Stmt::ClassDecl(Rc::new(c)))
            }
            Tok::Kw(Kw::If) => self.if_stmt(),
            Tok::Kw(Kw::While) => {
                self.next();
                self.expect_punct("(")?;
                let c = self.expression()?;
                self.expect_punct(")")?;
                let body = self.loop_body()?;
                Ok(Stmt::While { c, body })
            }
            Tok::Kw(Kw::Do) => {
                self.next();
                let body = self.loop_body()?;
                self.expect_kw(Kw::While)?;
                self.expect_punct("(")?;
                let c = self.expression()?;
                self.expect_punct(")")?;
                self.eat_punct(";");
                Ok(Stmt::DoWhile { body, c })
            }
            Tok::Kw(Kw::For) => self.for_stmt(),
            Tok::Kw(Kw::Switch) => self.switch_stmt(),
            Tok::Kw(Kw::Try) => self.try_stmt(),
            Tok::Kw(Kw::With) => {
                if self.strict {
                    return Err(self.err("`with` is not allowed in strict mode"));
                }
                self.next();
                self.expect_punct("(")?;
                let obj = self.expression()?;
                self.expect_punct(")")?;
                let body = self.stmt_body(false)?;
                // early error: IsLabelledFunction(body)
                let mut inner: &Stmt = &body;
                loop {
                    match inner {
                        Stmt::Labeled(_, i) => inner = i,
                        Stmt::FnDecl(_) => {
                            return Err(self.err(
                                "labelled function declaration not allowed as with body",
                            ));
                        }
                        _ => break,
                    }
                }
                Ok(Stmt::With { obj, body })
            }
            Tok::Kw(Kw::Throw) => {
                self.next();
                if self.peek().newline_before {
                    return Err(self.err("line break after `throw`"));
                }
                let e = self.expression()?;
                self.semi()?;
                Ok(Stmt::Throw(e))
            }
            Tok::Kw(Kw::Return) => {
                // arrow bodies are FunctionBody — `return` is legal
                // inside any function including arrows (fn_depth)
                if self.fn_depth == 0 {
                    return Err(self.err("`return` outside function"));
                }
                self.next();
                let e = if self.at_punct(";")
                    || self.at_punct("}")
                    || self.at_eof()
                    || self.peek().newline_before
                {
                    None
                } else {
                    Some(self.expression()?)
                };
                self.semi()?;
                Ok(Stmt::Return(e))
            }
            Tok::Kw(Kw::Break) => {
                self.next();
                let l = self.label_operand()?;
                self.semi()?;
                Ok(Stmt::Break(l))
            }
            Tok::Kw(Kw::Continue) => {
                self.next();
                let l = self.label_operand()?;
                self.semi()?;
                Ok(Stmt::Continue(l))
            }
            Tok::Kw(Kw::Debugger) => {
                self.next();
                self.semi()?;
                Ok(Stmt::Empty)
            }
            Tok::Kw(Kw::Import)
                if !matches!(self.peek2().tok, Tok::Punct("(") | Tok::Punct(".")) =>
            {
                let rec = self.import_stmt()?;
                Ok(Stmt::Import(rec))
            }
            Tok::Kw(Kw::Export) => {
                let rec = self.export_stmt()?;
                Ok(Stmt::Export(rec))
            }
            Tok::Ident(name) if matches!(self.peek2().tok, Tok::Punct(":")) => {
                self.next();
                self.next();
                let inner = self.stmt_body(true)?;
                Ok(Stmt::Labeled(name, inner))
            }
            _ => {
                let e = self.expression()?;
                self.semi()?;
                Ok(Stmt::Expr(e))
            }
        }
    }

    fn label_operand(&mut self) -> Result<Option<String>, Error> {
        if self.peek().newline_before {
            return Ok(None);
        }
        if let Tok::Ident(s) = self.peek().tok.clone() {
            self.next();
            Ok(Some(s))
        } else {
            Ok(None)
        }
    }

    fn block_body(&mut self) -> Result<Vec<Stmt>, Error> {
        let mut stmts = Vec::new();
        while !self.at_punct("}") {
            if self.at_eof() {
                return Err(self.err("unterminated block"));
            }
            stmts.push(self.statement()?);
        }
        self.expect_punct("}")?;
        Ok(stmts)
    }

    /// Parse `var|let|const decl = expr, ...` (the leading keyword is consumed).
    fn var_decl(&mut self) -> Result<(VarKind, Vec<(Pattern, Option<Expr>)>), Error> {
        let kind = match self.peek().tok {
            Tok::Kw(Kw::Var) => VarKind::Var,
            Tok::Kw(Kw::Let) => VarKind::Let,
            Tok::Kw(Kw::Const) => VarKind::Const,
            _ => return Err(self.err("expected var/let/const")),
        };
        self.next();
        if kind != VarKind::Var {
            self.lex_decl += 1;
        }
        let mut decls = Vec::new();
        let r = (|| {
            loop {
                let pat = self.binding_pattern()?;
                let init = if self.eat_punct("=") {
                    Some(self.assign_expr()?)
                } else {
                    None
                };
                if init.is_none() && !matches!(pat, Pattern::Ident(_)) {
                    return Err(self.err("destructuring requires an initializer"));
                }
                if init.is_none() && kind == VarKind::Const {
                    return Err(self.err("const declaration requires an initializer"));
                }
                decls.push((pat, init));
                if !self.eat_punct(",") {
                    break;
                }
            }
            Ok(())
        })();
        if kind != VarKind::Var {
            self.lex_decl -= 1;
        }
        r?;
        Ok((kind, decls))
    }

    fn binding_pattern(&mut self) -> Result<Pattern, Error> {
        match self.peek().tok.clone() {
            Tok::Ident(s) => {
                self.check_binding_name(&s)?;
                self.next();
                Ok(Pattern::Ident(s))
            }
            Tok::Kw(k) if is_contextual_ident_kw(&k) => {
                let s = k.name().to_string();
                self.check_binding_name(&s)?;
                self.next();
                Ok(Pattern::Ident(s))
            }
            Tok::Punct("[") => {
                self.next();
                let mut elems = Vec::new();
                while !self.at_punct("]") {
                    if self.at_punct(",") {
                        self.next();
                        elems.push(ArrayPatElem::Hole);
                        continue;
                    }
                    if self.eat_punct("...") {
                        elems.push(ArrayPatElem::Rest(self.binding_pattern()?));
                        break;
                    }
                    let p = self.binding_pattern()?;
                    let e = if self.eat_punct("=") {
                        ArrayPatElem::Default(p, self.assign_expr()?)
                    } else {
                        ArrayPatElem::Elem(p)
                    };
                    elems.push(e);
                    if !self.eat_punct(",") {
                        break;
                    }
                }
                self.expect_punct("]")?;
                Ok(Pattern::Array(elems))
            }
            Tok::Punct("{") => {
                self.next();
                let mut elems = Vec::new();
                while !self.at_punct("}") {
                    if self.eat_punct("...") {
                        // rest: store as a special elem keyed "" with val pattern
                        let p = self.binding_pattern()?;
                        elems.push(ObjPatElem {
                            key: PropKey::Str("\u{0}rest".into()),
                            val: p,
                            default: None,
                        });
                        break;
                    }
                    // shorthand `{name}` binds — keyword keys only legal
                    // for contextual names; `{break}` is a SyntaxError
                    let key_was_reserved = matches!(&self.peek().tok, Tok::Kw(k) if !is_contextual_ident_kw(k));
                    let key = self.prop_key(true)?;
                    // `key` or `key: pattern` or `key = default` or `key: pat = d`
                    let (val, default) = if self.eat_punct(":") {
                        let p = self.binding_pattern()?;
                        let d = if self.eat_punct("=") {
                            Some(self.assign_expr()?)
                        } else {
                            None
                        };
                        (p, d)
                    } else {
                        let name = match &key {
                            PropKey::Ident(s) | PropKey::Str(s) => s.clone(),
                            PropKey::Num(n) => return Err(self.err(format!("bad pattern key {n}"))),
                            PropKey::Computed(_) => {
                                return Err(self.err("computed key needs `: pattern`"))
                            }
                        };
                        if key_was_reserved {
                            return Err(self.err(format!(
                                "reserved word `{name}` is not a valid binding"
                            )));
                        }
                        let d = if self.eat_punct("=") {
                            Some(self.assign_expr()?)
                        } else {
                            None
                        };
                        self.check_binding_name(&name)?;
                        (Pattern::Ident(name), d)
                    };
                    elems.push(ObjPatElem { key, val, default });
                    if !self.eat_punct(",") {
                        break;
                    }
                }
                self.expect_punct("}")?;
                Ok(Pattern::Object(elems))
            }
            _ => Err(self.err(format!("expected binding, found {}", self.describe()))),
        }
    }

    fn if_stmt(&mut self) -> Result<Stmt, Error> {
        self.expect_kw(Kw::If)?;
        self.expect_punct("(")?;
        let c = self.expression()?;
        self.expect_punct(")")?;
        let t = self.stmt_body(true)?;
        let e = if self.eat_kw(Kw::Else) {
            let inner = self.stmt_body(true)?;
            Some(inner)
        } else {
            None
        };
        Ok(Stmt::If { c, t, e })
    }

    /// a `for`/`do`/`while` body — FunctionDeclaration is never legal in
    /// these statement positions (Annex-B only permits if/else/label)
    /// Parse a `Statement` (single-statement body position — if/else,
    /// loops, with, labelled items). Lexical declarations are
    /// StatementListItems, never legal here; `function` is only legal via
    /// Annex B.3.2/B.3.3 contexts (`fn_ok` = sloppy if/else or label).
    fn stmt_body(&mut self, fn_ok: bool) -> Result<Box<Stmt>, Error> {
        // `let` in statement position is an ExpressionStatement under the
        // `let [` lookahead restriction — never a lexical declaration.
        // `let\nx`/`let\n{}` split via ASI into `let;` + a statement.
        let body = if matches!(self.peek().tok, Tok::Kw(Kw::Let)) {
            if matches!(self.peek2().tok, Tok::Punct("[")) {
                return Err(
                    self.err("`let [` is not allowed in statement position")
                );
            }
            let e = self.expression()?;
            self.semi()?;
            Stmt::Expr(e)
        } else {
            self.statement()?
        };
        match &body {
            Stmt::VarDecl { kind, .. }
                if matches!(kind, VarKind::Let | VarKind::Const) =>
            {
                return Err(self.err(
                    "lexical declaration not allowed in statement position",
                ));
            }
            Stmt::ClassDecl(_) => {
                return Err(self.err(
                    "class declaration not allowed in statement position",
                ));
            }
            Stmt::FnDecl(_) if !(fn_ok && !self.strict) => {
                return Err(self.err(
                    "function declaration not allowed in statement position",
                ));
            }
            // `for (...) label: function f(){}` — a fn decl reachable
            // through labels is still in a single-statement position:
            // Annex B labelled fns are only legal at function/script/block
            // top level
            Stmt::Labeled(_, inner) if !fn_ok => {
                let mut cur: &Stmt = inner;
                loop {
                    match cur {
                        Stmt::Labeled(_, i) => cur = i,
                        Stmt::FnDecl(_) => {
                            return Err(self.err(
                                "labelled function declaration not allowed                                  in statement position",
                            ));
                        }
                        _ => break,
                    }
                }
            }
            _ => {}
        }
        Ok(Box::new(body))
    }

    fn loop_body(&mut self) -> Result<Box<Stmt>, Error> {
        self.stmt_body(false)
    }

    fn for_stmt(&mut self) -> Result<Stmt, Error> {
        self.expect_kw(Kw::For)?;
        self.eat_kw(Kw::Await); // `for await (x of y)` — sync iteration
        self.expect_punct("(")?;

        // for-in / for-of heads
        let init_is_decl = matches!(self.peek().tok, Tok::Kw(Kw::Var) | Tok::Kw(Kw::Const))
            || (matches!(self.peek().tok, Tok::Kw(Kw::Let))
                && matches!(
                    self.peek2().tok,
                    Tok::Ident(_) | Tok::Punct("[") | Tok::Punct("{")
                ))
            || matches!(&self.peek().tok, Tok::Kw(Kw::Let) if matches!(&self.peek2().tok, Tok::Kw(k) if is_contextual_ident_kw(k)));

        if init_is_decl {
            let kind = match self.peek().tok {
                Tok::Kw(Kw::Var) => VarKind::Var,
                Tok::Kw(Kw::Const) => VarKind::Const,
                _ => VarKind::Let,
            };
            self.next();
            if kind != VarKind::Var {
                self.lex_decl += 1;
            }
            let pat = self.binding_pattern();
            if kind != VarKind::Var {
                self.lex_decl -= 1;
            }
            let pat = pat?;
            if self.at_kw(Kw::Of) {
                self.next();
                // ForOfHead: `of` binds an AssignmentExpression — a
                // top-level `,` is a syntax error
                let right = self.assign_expr()?;
                self.expect_punct(")")?;
                let body = self.loop_body()?;
                return Ok(Stmt::ForOf {
                    left: ForTarget::Var(kind, pat),
                    right,
                    body,
                });
            }
            if self.at_kw(Kw::In) {
                self.next();
                let right = self.expression()?;
                self.expect_punct(")")?;
                let body = self.loop_body()?;
                return Ok(Stmt::ForIn {
                    left: ForTarget::Var(kind, pat),
                    right,
                    body,
                });
            }
            // regular for with decl — continue decl list
            let mut decls = Vec::new();
            let init = if self.eat_punct("=") {
                Some(self.assign_expr()?)
            } else {
                None
            };
            decls.push((pat, init));
            while self.eat_punct(",") {
                let p = self.binding_pattern()?;
                let i = if self.eat_punct("=") {
                    Some(self.assign_expr()?)
                } else {
                    None
                };
                decls.push((p, i));
            }
            self.expect_punct(";")?;
            let test = if self.at_punct(";") {
                None
            } else {
                Some(self.expression()?)
            };
            self.expect_punct(";")?;
            let update = if self.at_punct(")") {
                None
            } else {
                Some(self.expression()?)
            };
            self.expect_punct(")")?;
            let body = self.loop_body()?;
            return Ok(Stmt::For {
                init: Some(ForInit::Var(kind, decls)),
                test,
                update,
                body,
            });
        }

        // expression or `expr in/of` head
        if self.eat_punct(";") {
            let test = if self.at_punct(";") {
                None
            } else {
                Some(self.expression()?)
            };
            self.expect_punct(";")?;
            let update = if self.at_punct(")") {
                None
            } else {
                Some(self.expression()?)
            };
            self.expect_punct(")")?;
            let body = self.loop_body()?;
            return Ok(Stmt::For {
                init: None,
                test,
                update,
                body,
            });
        }

        // parse the head expression with `in` disabled so `for (x in y)`
        // and `for (a in b&&c, d)` see the keyword as the for-in marker
        self.no_in = true;
        // `for (async of` is reserved — `async` LHS directly followed by
        // `of` is an early error (escaped `as\u0079nc` lexes to Ident and
        // stays legal)
        if matches!(self.peek().tok, Tok::Kw(Kw::Async))
            && !self.peek().escaped
            && matches!(self.peek2().tok, Tok::Kw(Kw::Of))
        {
            return Err(self.err("`for (async of` is a reserved form"));
        }
        let first = self.expression()?;
        self.no_in = false;
        if self.at_kw(Kw::Of) {
            self.next();
            let right = self.assign_expr()?;
            self.expect_punct(")")?;
            let body = self.loop_body()?;
            return Ok(Stmt::ForOf {
                left: ForTarget::Expr(first),
                right,
                body,
            });
        }
        if self.at_kw(Kw::In) {
            self.next();
            let right = self.expression()?;
            self.expect_punct(")")?;
            let body = self.loop_body()?;
            return Ok(Stmt::ForIn {
                left: ForTarget::Expr(first),
                right,
                body,
            });
        }
        self.expect_punct(";")?;
        let test = if self.at_punct(";") {
            None
        } else {
            Some(self.expression()?)
        };
        self.expect_punct(";")?;
        let update = if self.at_punct(")") {
            None
        } else {
            Some(self.expression()?)
        };
        self.expect_punct(")")?;
        let body = self.loop_body()?;
        Ok(Stmt::For {
            init: Some(ForInit::Expr(first)),
            test,
            update,
            body,
        })
    }

    fn switch_stmt(&mut self) -> Result<Stmt, Error> {
        self.expect_kw(Kw::Switch)?;
        self.expect_punct("(")?;
        let disc = self.expression()?;
        self.expect_punct(")")?;
        self.expect_punct("{")?;
        let mut cases = Vec::new();
        while !self.at_punct("}") {
            if self.eat_kw(Kw::Case) {
                let test = self.expression()?;
                self.expect_punct(":")?;
                let mut body = Vec::new();
                while !self.at_punct("}") && !self.at_kw(Kw::Case) && !self.at_kw(Kw::Default) {
                    body.push(self.statement()?);
                }
                cases.push(SwitchCase {
                    test: Some(test),
                    body,
                });
            } else if self.eat_kw(Kw::Default) {
                self.expect_punct(":")?;
                let mut body = Vec::new();
                while !self.at_punct("}") && !self.at_kw(Kw::Case) && !self.at_kw(Kw::Default) {
                    body.push(self.statement()?);
                }
                cases.push(SwitchCase { test: None, body });
            } else {
                return Err(self.err("expected `case` or `default`"));
            }
        }
        self.expect_punct("}")?;
        Ok(Stmt::Switch { disc, cases })
    }

    fn try_stmt(&mut self) -> Result<Stmt, Error> {
        self.expect_kw(Kw::Try)?;
        self.expect_punct("{")?;
        let block = self.block_body()?;
        let (param, handler) = if self.eat_kw(Kw::Catch) {
            let p = if self.eat_punct("(") {
                let pat = self.binding_pattern()?;
                self.expect_punct(")")?;
                Some(pat)
            } else {
                None
            };
            self.expect_punct("{")?;
            (p, Some(self.block_body()?))
        } else {
            (None, None)
        };
        let finally = if self.eat_kw(Kw::Finally) {
            self.expect_punct("{")?;
            Some(self.block_body()?)
        } else {
            None
        };
        if handler.is_none() && finally.is_none() {
            return Err(self.err("try requires catch or finally"));
        }
        Ok(Stmt::Try {
            block,
            param,
            handler,
            finally,
        })
    }

    fn import_stmt(&mut self) -> Result<ImportRec, Error> {
        self.expect_kw(Kw::Import)?;
        let mut specifiers = String::new();
        let source;
        match self.peek().tok.clone() {
            Tok::Str(s) => {
                self.next();
                source = Some(s);
            }
            Tok::Punct("{") => {
                self.next();
                let mut names = Vec::new();
                while !self.at_punct("}") {
                    let a = self.ident_name()?;
                    let b = if self.eat_kw(Kw::As) {
                        Some(self.ident_name()?)
                    } else {
                        None
                    };
                    names.push(match b {
                        Some(b) => format!("{a} as {b}"),
                        None => a,
                    });
                    if !self.eat_punct(",") {
                        break;
                    }
                }
                self.expect_punct("}")?;
                specifiers = names.join(", ");
                self.expect_kw(Kw::From)?;
                if let Tok::Str(s) = self.peek().tok.clone() {
                    self.next();
                    source = Some(s);
                } else {
                    return Err(self.err("expected module specifier string"));
                }
            }
            Tok::Punct("*") => {
                self.next();
                self.expect_kw(Kw::As)?;
                specifiers = format!("* as {}", self.ident_name()?);
                self.expect_kw(Kw::From)?;
                if let Tok::Str(s) = self.peek().tok.clone() {
                    self.next();
                    source = Some(s);
                } else {
                    return Err(self.err("expected module specifier string"));
                }
            }
            Tok::Ident(n) => {
                self.next();
                specifiers = n;
                if self.eat_punct(",") {
                    if self.eat_punct("{") {
                        while !self.at_punct("}") {
                            let a = self.ident_name()?;
                            let b = if self.eat_kw(Kw::As) {
                                Some(self.ident_name()?)
                            } else {
                                None
                            };
                            specifiers.push_str(&match b {
                                Some(b) => format!(", {a} as {b}"),
                                None => format!(", {a}"),
                            });
                            if !self.eat_punct(",") {
                                break;
                            }
                        }
                        self.expect_punct("}")?;
                    } else if self.eat_punct("*") {
                        self.expect_kw(Kw::As)?;
                        specifiers.push_str(&format!(", * as {}", self.ident_name()?));
                    }
                }
                self.expect_kw(Kw::From)?;
                if let Tok::Str(s) = self.peek().tok.clone() {
                    self.next();
                    source = Some(s);
                } else {
                    return Err(self.err("expected module specifier string"));
                }
            }
            _ => return Err(self.err("unsupported import form")),
        }
        self.semi()?;
        Ok(ImportRec { source, specifiers })
    }

    fn export_stmt(&mut self) -> Result<ExportRec, Error> {
        self.expect_kw(Kw::Export)?;
        let rec = if self.eat_kw(Kw::Default) {
            // `export default function f(){}` / expr
            if self.at_kw(Kw::Function) {
                self.next();
                let g = self.eat_punct("*");
                let f = self.fn_tail(false, g)?;
                ExportRec::Default(Expr::Fn(Rc::new(f)))
            } else if self.at_kw(Kw::Class) {
                self.next();
                let c = self.class_tail(false)?;
                ExportRec::Default(Expr::Class(Rc::new(c)))
            } else {
                let e = self.assign_expr()?;
                self.semi()?;
                ExportRec::Default(e)
            }
        } else if self.at_punct("{") {
            self.next();
            let mut names = Vec::new();
            while !self.at_punct("}") {
                let a = self.ident_name()?;
                let b = if self.eat_kw(Kw::As) {
                    match self.peek().tok.clone() {
                        Tok::Str(s) => {
                            self.next();
                            Some(s)
                        }
                        _ => Some(self.ident_name()?),
                    }
                } else {
                    None
                };
                names.push((a, b));
                if !self.eat_punct(",") {
                    break;
                }
            }
            self.expect_punct("}")?;
            let from = if self.eat_kw(Kw::From) {
                match self.peek().tok.clone() {
                    Tok::Str(s) => {
                        self.next();
                        Some(s)
                    }
                    _ => return Err(self.err("expected module specifier string")),
                }
            } else {
                None
            };
            self.semi()?;
            ExportRec::Names(names, from)
        } else if self.eat_punct("*") {
            // `export * as ns from "x"` or `export * from "x"`
            if self.eat_kw(Kw::As) {
                let _ = self.ident_name()?;
            }
            self.expect_kw(Kw::From)?;
            match self.peek().tok.clone() {
                Tok::Str(s) => {
                    self.next();
                    self.semi()?;
                    ExportRec::All(s)
                }
                _ => return Err(self.err("expected module specifier string")),
            }
        } else {
            let inner = self.statement()?;
            ExportRec::Decl(Box::new(inner))
        };
        Ok(rec)
    }

    // ---- functions & classes ----------------------------------------------
    /// `fn` after the `function` keyword: `name (params) { body }`.
    /// `name_required`: decls need names, expressions don't.
    /// `gen`: a `*` generator marker was already consumed.
    fn fn_tail(&mut self, name_required: bool, gen: bool) -> Result<FnDef, Error> {
        let name = {
            // the name's [Yield] context: declarations inherit the
            // enclosing generator; the nested fn always uses its own
            // a function's *name* is not yield-restricted by the [Yield]
            // context — `function* yield(){}` is legal in sloppy code
            let saved_gd = (self.gen_depth, self.yield_decl);
            self.gen_depth = gen as usize;
            self.yield_decl = 0;
            let n = match self.peek().tok.clone() {
                Tok::Ident(s) => {
                    self.check_binding_name(&s)?;
                    self.next();
                    Some(s)
                }
                Tok::Kw(k) if is_contextual_ident_kw(&k) => {
                    let s = k.name().to_string();
                    self.check_binding_name(&s)?;
                    self.next();
                    Some(s)
                }
                _ if !name_required => None,
                _ => return Err(self.err("expected function name")),
            };
            (self.gen_depth, self.yield_decl) = saved_gd;
            n
        };
        let old_ctx = (
            self.gen_depth,
            self.yield_decl,
            self.strict,
            self.ctor_depth,
            self.method_depth,
        );
        // a nested function always gets its own [Yield] context from its
        // own `*` marker — only arrows inherit the enclosing one
        self.gen_depth = gen as usize;
        self.yield_decl = gen as usize;
        self.ctor_depth = 0;
        self.method_depth = 0;
        let res = (|| {
            self.expect_punct("(")?;
            let params = self.params()?;
            self.expect_punct(")")?;
            self.expect_punct("{")?;
            // a body's directive retroactively governs its params
            if self.directive_candidate() {
                if let Tok::Str(t) = &self.peek().tok {
                    if t == "use strict" {
                        self.strict = true;
                        // early error: an explicit directive can't apply to
                        // a non-simple parameter list
                        let simple = params
                            .iter()
                            .all(|p| matches!(p, Param::Ident(_)));
                        if !simple {
                            return Err(self.err(
                                "\"use strict\" not allowed with non-simple parameters",
                            ));
                        }
                    }
                }
            }
            self.check_params(&params, false)?;
            let body = self.fn_body(false)?;
            Ok((params, body))
        })();
        (
            self.gen_depth,
            self.yield_decl,
            self.strict,
            self.ctor_depth,
            self.method_depth,
        ) = old_ctx;
        let (params, body) = res?;
        Ok(FnDef {
            name,
            params,
            body,
            is_arrow: false,
            arrow_expr: None,
            is_async: false,
            is_gen: gen,
                    strict: self.strict,
        })
    }

    /// Method body: runs `fn_body` in method context (`super.x` legal,
    /// `super()` only when `is_ctor`) and restores the enclosing context.
    fn method_body(&mut self, is_ctor: bool, gen: bool) -> Result<Vec<Stmt>, Error> {
        let saved = (
            self.ctor_depth,
            self.method_depth,
            self.gen_depth,
            self.yield_decl,
        );
        self.ctor_depth = is_ctor as usize;
        self.method_depth = 1;
        self.gen_depth = gen as usize;
        self.yield_decl = gen as usize;
        let r = self.fn_body(false);
        (
            self.ctor_depth,
            self.method_depth,
            self.gen_depth,
            self.yield_decl,
        ) = saved;
        r
    }

    /// strict-mode + non-simple-parameter-list duplicate check.
    /// `dup_always`: arrows never allow duplicate params (no sloppy form).
    fn check_params(&mut self, params: &[Param], dup_always: bool) -> Result<(), Error> {
        let simple = params.iter().all(|p| matches!(p, Param::Ident(_)));
        for p in params {
            let mut names = Vec::new();
            pattern_names(p, &mut names);
            for n in names {
                self.check_binding_name(n)?;
            }
        }
        if self.strict || !simple || dup_always {
            let mut seen = std::collections::HashSet::new();
            for p in params {
                let mut names = Vec::new();
                pattern_names(p, &mut names);
                for n in names {
                    if !seen.insert(n.to_string()) {
                        return Err(self.err(format!("duplicate parameter `{n}`")));
                    }
                }
            }
        }
        Ok(())
    }

    /// strict-mode early error on legacy octal literals/escapes.
    fn check_octal(&self) -> Result<(), Error> {
        if self.strict && self.peek().octal {
            return Err(self.err("octal literal not allowed in strict mode"));
        }
        Ok(())
    }

    /// strict-mode binding-name restrictions (eval/arguments + strict-only
    /// reserved words).
    fn check_binding_name(&self, name: &str) -> Result<(), Error> {
        // `yield` is reserved in any [Yield] context even in sloppy mode
        if name == "yield" && self.yield_decl > 0 {
            return Err(self.err("`yield` is not a valid binding in a generator"));
        }
        // `let` is never a valid lexical-decl binding name (sloppy `var let`
        // stays legal — gated on the declaration kind, not strictness)
        if name == "let" && self.lex_decl > 0 {
            return Err(
                self.err("`let` is not a valid binding in a lexical declaration")
            );
        }
        if !self.strict {
            return Ok(());
        }
        if matches!(
            name,
            "eval"
                | "arguments"
                | "implements"
                | "interface"
                | "package"
                | "private"
                | "protected"
                | "public"
                | "yield"
                | "let"
                | "static"
        ) {
            return Err(self.err(format!("`{name}` is not a valid binding in strict mode")));
        }
        Ok(())
    }

    fn params(&mut self) -> Result<Vec<Param>, Error> {
        self.param_depth += 1;
        let r = self.params_inner();
        self.param_depth -= 1;
        r
    }

    fn params_inner(&mut self) -> Result<Vec<Param>, Error> {
        let mut params = Vec::new();
        while !self.at_punct(")") {
            if self.eat_punct("...") {
                params.push(Param::Rest(Box::new(self.binding_pattern()?)));
                break;
            }
            let pat = self.binding_pattern()?;
            let p = if self.eat_punct("=") {
                Param::Default(Box::new(pat), self.assign_expr()?)
            } else {
                match pat {
                    Pattern::Ident(s) => Param::Ident(s),
                    other => Param::Pattern(other),
                }
            };
            params.push(p);
            if !self.eat_punct(",") {
                break;
            }
        }
        Ok(params)
    }

    /// class tail after `class` keyword.
    fn class_tail(&mut self, name_required: bool) -> Result<ClassDef, Error> {
        let name = match self.peek().tok.clone() {
            Tok::Ident(s) => {
                self.next();
                Some(s)
            }
            _ if !name_required => None,
            _ if self.at_kw(Kw::Extends) => None,
            _ => return Err(self.err("expected class name")),
        };
        let superclass = if self.eat_kw(Kw::Extends) {
            Some(self.left_hand_side()?)
        } else {
            None
        };
        self.expect_punct("{")?;
        // class bodies are always strict
        let saved = (self.strict, self.ctor_depth, self.method_depth);
        self.strict = true;
        let mut members = Vec::new();
        while !self.at_punct("}") {
            if self.eat_punct(";") {
                continue;
            }
            if self.at_eof() {
                return Err(self.err("unterminated class body"));
            }
            let mut is_static = false;
            let mut is_async = false;
            // modifiers — `static` and `async` can also be member names
            if self.at_kw(Kw::Static)
                && !matches!(self.peek2().tok, Tok::Punct("(") | Tok::Punct("="))
            {
                self.next();
                is_static = true;
            }
            // static initialization block `static { ... }` — parse + skip
            if is_static && self.at_punct("{") {
                self.next();
                let _ = self.block_body()?;
                continue;
            }
            if self.at_kw(Kw::Async)
                && !matches!(
                    self.peek2().tok,
                    Tok::Punct("(") | Tok::Punct("=") | Tok::Punct(":")
                )
                && !self.peek2().newline_before
            {
                self.next();
                is_async = true;
            }
            // getter/setter (contextual)
            let mut kind = MemberKind::Method;
            let is_getset_candidate =
                matches!(self.peek().tok, Tok::Kw(Kw::Get) | Tok::Kw(Kw::Set));
            if is_getset_candidate
                && !matches!(
                    self.peek2().tok,
                    Tok::Punct("(") | Tok::Punct("=") | Tok::Punct(";") | Tok::Punct("}")
                )
            {
                kind = if self.at_kw(Kw::Get) {
                    MemberKind::Getter
                } else {
                    MemberKind::Setter
                };
                self.next();
            }
            let member_gen = self.eat_punct("*"); // generator marker
            let key = self.prop_key(true)?;
            // field declaration `x = 1;` or bare `x;` — unsupported init → skip value
            if self.at_punct("=") || self.at_punct(";") {
                self.eat_punct("=");
                if !self.at_punct(";") && !self.at_punct("}") {
                    let _ = self.assign_expr()?; // parse + discard field init for now
                }
                self.eat_punct(";");
                continue;
            }
            let is_ctor = matches!(&key, PropKey::Ident(s) if s == "constructor") && !is_static;
            self.expect_punct("(")?;
            let params = self.params()?;
            self.expect_punct(")")?;
            match kind {
                MemberKind::Getter if !params.is_empty() => {
                    return Err(self.err("getter must not have parameters"));
                }
                MemberKind::Setter if params.len() != 1 => {
                    return Err(self.err("setter must have exactly one parameter"));
                }
                _ => {}
            }
            self.check_params(&params, false)?;
            self.expect_punct("{")?;
            let body = self.method_body(is_ctor, member_gen)?;
            let fname = match &key {
                PropKey::Ident(s) | PropKey::Str(s) => Some(s.clone()),
                _ => None,
            };
            members.push(ClassMember {
                key,
                kind: if is_ctor { MemberKind::Ctor } else { kind },
                func: Rc::new(FnDef {
                    name: fname,
                    params,
                    body,
                    is_arrow: false,
                    arrow_expr: None,
                    is_async,
                    is_gen: member_gen,
                    strict: self.strict,
                }),
                is_static,
            });
        }
        self.expect_punct("}")?;
        (self.strict, self.ctor_depth, self.method_depth) = saved;
        Ok(ClassDef {
            name,
            superclass,
            members,
        })
    }

    // ---- expressions --------------------------------------------------------
    pub fn expression(&mut self) -> Result<Expr, Error> {
        let first = self.assign_expr()?;
        if !self.at_punct(",") {
            return Ok(first);
        }
        let mut seq = vec![first];
        while self.eat_punct(",") {
            seq.push(self.assign_expr()?);
        }
        Ok(Expr::Seq(seq))
    }

    fn assign_expr(&mut self) -> Result<Expr, Error> {
        // arrow functions — try speculative parse
        if let Some(e) = self.try_arrow()? {
            return Ok(e);
        }
        if self.at_kw(Kw::Yield) && self.gen_depth > 0 {
            // `yield` — encoded as a call to the `$yield` intrinsic.
            // YieldExpression is an early error inside FormalParameters,
            // and [no LineTerminator] separates `yield`/`yield*` from the
            // operand: `yield\n x` parses as `yield; x`.
            if self.param_depth > 0 {
                return Err(self.err(
                    "YieldExpression is not allowed in FormalParameters",
                ));
            }
            self.next();
            // [no LT] separates `yield` from `*` and (when absent) the
            // operand; after `yield *` the operand may cross lines
            let star = !self.peek().newline_before && self.eat_punct("*");
            if (star || !self.peek().newline_before)
                && !self.at_punct(")")
                && !self.at_punct("]")
                && !self.at_punct("}")
                && !self.at_punct(",")
                && !self.at_punct(";")
                && !self.at_punct(":")
                && !self.at_eof()
            {
                let inner = self.assign_expr()?;
                let tag = if star { "$yieldstar" } else { "$yield" };
                return Ok(Expr::Call {
                    callee: Box::new(Expr::Ident(tag.into())),
                    args: vec![Arg::Expr(inner)],
                });
            }
            return Ok(Expr::Call {
                callee: Box::new(Expr::Ident("$yield".into())),
                args: vec![],
            });
        }
        let lhs = self.cond_expr()?;
        let op = match self.peek().tok {
            Tok::Punct("=") => AssignOp::Assign,
            Tok::Punct("+=") => AssignOp::Add,
            Tok::Punct("-=") => AssignOp::Sub,
            Tok::Punct("*=") => AssignOp::Mul,
            Tok::Punct("/=") => AssignOp::Div,
            Tok::Punct("%=") => AssignOp::Mod,
            Tok::Punct("**=") => AssignOp::Pow,
            Tok::Punct("&=") => AssignOp::BitAnd,
            Tok::Punct("|=") => AssignOp::BitOr,
            Tok::Punct("^=") => AssignOp::BitXor,
            Tok::Punct("<<=") => AssignOp::Shl,
            Tok::Punct(">>=") => AssignOp::Shr,
            Tok::Punct(">>>=") => AssignOp::Ushr,
            Tok::Punct("&&=") => AssignOp::LogAnd,
            Tok::Punct("||=") => AssignOp::LogOr,
            Tok::Punct("??=") => AssignOp::Nullish,
            _ => return Ok(lhs),
        };
        self.next();
        if !is_assignable(&lhs, self.strict) {
            return Err(self.err("invalid assignment target"));
        }
        if self.strict {
            if let Expr::Ident(n) = &lhs {
                if n == "eval" || n == "arguments" {
                    return Err(self.err("cannot assign to eval/arguments in strict mode"));
                }
            }
        }
        let value = self.assign_expr()?;
        Ok(Expr::Assign {
            op,
            target: Box::new(lhs),
            value: Box::new(value),
        })
    }

    /// Speculative arrow-function parse. Returns Some(expr) on success,
    /// restoring position on failure.
    fn try_arrow(&mut self) -> Result<Option<Expr>, Error> {
        let start = self.pos;
        let is_async = self.at_kw(Kw::Async) && !self.peek2().newline_before;
        if is_async {
            self.next();
        }
        // case 1: `ident => ...`
        if let Tok::Ident(name) = self.peek().tok.clone() {
            if matches!(self.peek2().tok, Tok::Punct("=>")) {
                self.next();
                self.next();
                return Ok(Some(self.arrow_body(
                    vec![Param::Ident(name)],
                    None,
                    is_async,
                )?));
            }
            self.pos = start;
            return Ok(None);
        }
        // case 2: `( params ) => ...`
        if self.at_punct("(") {
            self.next();
            if let Ok(params) = self.params() {
                if self.at_punct(")") {
                    self.next();
                    if self.eat_punct("=>") {
                        return Ok(Some(self.arrow_body(params, None, is_async)?));
                    }
                }
            }
            self.pos = start;
            return Ok(None);
        }
        self.pos = start;
        Ok(None)
    }

    fn arrow_body(
        &mut self,
        params: Vec<Param>,
        name: Option<String>,
        is_async: bool,
    ) -> Result<Expr, Error> {
        let saved_strict = self.strict;
        let res = self.arrow_body_inner(params, name, is_async);
        self.strict = saved_strict;
        res
    }

    fn arrow_body_inner(
        &mut self,
        params: Vec<Param>,
        name: Option<String>,
        is_async: bool,
    ) -> Result<Expr, Error> {
        if self.at_punct("{") {
            self.next();
            // a block body's directive retroactively governs the params
            if self.directive_candidate() {
                if let Tok::Str(t) = &self.peek().tok {
                    if t == "use strict" {
                        self.strict = true;
                    }
                }
            }
            self.check_params(&params, true)?;
            let body = self.fn_body(true)?;
            Ok(Expr::Fn(Rc::new(FnDef {
                name,
                params,
                body,
                is_arrow: true,
                arrow_expr: None,
                is_async,
                is_gen: false,
                    strict: self.strict,
            })))
        } else {
            self.check_params(&params, true)?;
            // expression body — `yield`/`super`-context rules for arrows
            // apply inside it too
            let saved = self.fn_depth;
            self.fn_depth += 1;
            let res = self.assign_expr();
            self.fn_depth = saved;
            let e = res?;
            Ok(Expr::Fn(Rc::new(FnDef {
                name,
                params,
                body: Vec::new(),
                is_gen: false,
                    strict: self.strict,
                is_arrow: true,
                arrow_expr: Some(e),
                is_async,
            })))
        }
    }

    fn cond_expr(&mut self) -> Result<Expr, Error> {
        let c = self.binary_expr(0)?;
        if self.eat_punct("?") {
            let t = self.assign_expr()?;
            self.expect_punct(":")?;
            let f = self.assign_expr()?;
            Ok(Expr::Cond {
                c: Box::new(c),
                t: Box::new(t),
                f: Box::new(f),
            })
        } else {
            Ok(c)
        }
    }

    /// precedence-climbing binary expression parser.
    /// `min_prec` 0 = lowest (??), higher numbers bind tighter.
    fn binary_expr(&mut self, min_prec: u8) -> Result<Expr, Error> {
        let mut l = self.unary_expr()?;
        loop {
            let (prec, kind) = match self.peek_binary_op() {
                Some(pk) => pk,
                None => break,
            };
            if prec < min_prec {
                break;
            }
            self.next();
            // `**` is right-associative; everything else left.
            let next_min = if kind == BinKind::Pow { prec } else { prec + 1 };
            let r = self.binary_expr(next_min)?;
            l = match kind {
                BinKind::Log(op) => Expr::Logical {
                    op,
                    l: Box::new(l),
                    r: Box::new(r),
                },
                _ => Expr::Binary {
                    op: kind.binop().unwrap(),
                    l: Box::new(l),
                    r: Box::new(r),
                },
            };
        }
        Ok(l)
    }

    fn peek_binary_op(&self) -> Option<(u8, BinKind)> {
        use BinOp::*;
        use LogOp::*;
        let (prec, kind) = match &self.peek().tok {
            Tok::Punct("??") => (1, BinKind::Log(Nullish)),
            Tok::Punct("||") => (2, BinKind::Log(Or)),
            Tok::Punct("&&") => (3, BinKind::Log(And)),
            Tok::Punct("|") => (4, BinKind::B(BitOr)),
            Tok::Punct("^") => (5, BinKind::B(BitXor)),
            Tok::Punct("&") => (6, BinKind::B(BitAnd)),
            Tok::Punct("==") => (7, BinKind::B(Eq)),
            Tok::Punct("!=") => (7, BinKind::B(Ne)),
            Tok::Punct("===") => (7, BinKind::B(StrictEq)),
            Tok::Punct("!==") => (7, BinKind::B(StrictNe)),
            Tok::Punct("<") => (8, BinKind::B(Lt)),
            Tok::Punct("<=") => (8, BinKind::B(Le)),
            Tok::Punct(">") => (8, BinKind::B(Gt)),
            Tok::Punct(">=") => (8, BinKind::B(Ge)),
            Tok::Kw(Kw::Instanceof) => (8, BinKind::B(Instanceof)),
            Tok::Kw(Kw::In) if !self.no_in => (8, BinKind::B(In)),
            Tok::Punct("<<") => (9, BinKind::B(Shl)),
            Tok::Punct(">>") => (9, BinKind::B(Shr)),
            Tok::Punct(">>>") => (9, BinKind::B(Ushr)),
            Tok::Punct("+") => (10, BinKind::B(Add)),
            Tok::Punct("-") => (10, BinKind::B(Sub)),
            Tok::Punct("*") => (11, BinKind::B(Mul)),
            Tok::Punct("/") => (11, BinKind::B(Div)),
            Tok::Punct("%") => (11, BinKind::B(Mod)),
            Tok::Punct("**") => (12, BinKind::Pow),
            _ => return None,
        };
        Some((prec, kind))
    }

    fn unary_expr(&mut self) -> Result<Expr, Error> {
        let op = match self.peek().tok {
            Tok::Punct("-") => Some(UnaryOp::Neg),
            Tok::Punct("+") => Some(UnaryOp::Pos),
            Tok::Punct("!") => Some(UnaryOp::Not),
            Tok::Punct("~") => Some(UnaryOp::BitNot),
            Tok::Kw(Kw::Typeof) => Some(UnaryOp::Typeof),
            Tok::Kw(Kw::Void) => Some(UnaryOp::Void),
            Tok::Kw(Kw::Delete) => Some(UnaryOp::Delete),
            Tok::Kw(Kw::Await) => Some(UnaryOp::Await),
            _ => None,
        };
        if let Some(op) = op {
            self.next();
            let e = self.unary_expr()?;
            if op == UnaryOp::Delete
                && self.strict
                && matches!(&e, Expr::Ident(_))
            {
                return Err(self.err("cannot delete an unqualified identifier in strict mode"));
            }
            return Ok(Expr::Unary {
                op,
                expr: Box::new(e),
            });
        }
        if self.at_punct("++") || self.at_punct("--") {
            let inc = self.at_punct("++");
            self.next();
            let t = self.unary_expr()?;
            if !is_assignable(&t, self.strict) {
                return Err(self.err("invalid ++/-- target"));
            }
            return Ok(Expr::Update {
                inc,
                prefix: true,
                target: Box::new(t),
            });
        }
        let e = self.left_hand_side()?;
        // postfix ++/-- (no newline before)
        if (self.at_punct("++") || self.at_punct("--")) && !self.peek().newline_before {
            let inc = self.at_punct("++");
            self.next();
            if !is_assignable(&e, self.strict) {
                return Err(self.err("invalid ++/-- target"));
            }
            return Ok(Expr::Update {
                inc,
                prefix: false,
                target: Box::new(e),
            });
        }
        Ok(e)
    }

    /// call/member/new chain.
    fn left_hand_side(&mut self) -> Result<Expr, Error> {
        let mut e = if self.at_kw(Kw::New) {
            self.next();
            // `new.target` — only `target` is a valid meta-property, and
            // only inside a function body
            if self.eat_punct(".") {
                let name = self.ident_name()?;
                if name != "target" {
                    return Err(self.err(format!("unsupported meta property new.{name}")));
                }
                if self.nonarrow_fn_depth == 0 {
                    return Err(self.err("new.target outside function"));
                }
                Expr::Ident("$new_target".into())
            } else {
                let callee = self.member_primary()?;
                let args = if self.at_punct("(") {
                    self.call_args()?
                } else {
                    Vec::new()
                };
                Expr::New {
                    callee: Box::new(callee),
                    args,
                }
            }
        } else {
            self.primary()?
        };

        // once `?.` appears, the rest of the chain short-circuits too
        let mut opt_chain = false;
        loop {
            match self.peek().tok.clone() {
                Tok::Punct(".") => {
                    self.next();
                    let name = self.prop_key(false)?;
                    let prop = match name {
                        PropKey::Ident(s) | PropKey::Str(s) => Expr::Str(s),
                        PropKey::Num(n) => Expr::Str(n.to_string()),
                        PropKey::Computed(e) => e,
                    };
                    e = Expr::Member {
                        obj: Box::new(e),
                        prop: Box::new(prop),
                        computed: true,
                        optional: opt_chain,
                    };
                }
                Tok::Punct("?.") => {
                    self.next();
                    opt_chain = true;
                    if self.at_punct("(") {
                        // optional call `a?.()`
                        let args = self.call_args()?;
                        e = Expr::Call {
                            callee: Box::new(Expr::Member {
                                obj: Box::new(e),
                                prop: Box::new(Expr::Str("\u{0}call".into())),
                                computed: true,
                                optional: true,
                            }),
                            args,
                        };
                    } else {
                        let prop = if self.eat_punct("[") {
                            let p = self.expression()?;
                            self.expect_punct("]")?;
                            p
                        } else {
                            match self.prop_key(false)? {
                                PropKey::Ident(s) | PropKey::Str(s) => Expr::Str(s),
                                PropKey::Num(n) => Expr::Str(n.to_string()),
                                PropKey::Computed(c) => c,
                            }
                        };
                        e = Expr::Member {
                            obj: Box::new(e),
                            prop: Box::new(prop),
                            computed: true,
                            optional: true,
                        };
                    }
                }
                Tok::Punct("[") => {
                    self.next();
                    let p = self.expression()?;
                    self.expect_punct("]")?;
                    e = Expr::Member {
                        obj: Box::new(e),
                        prop: Box::new(p),
                        computed: true,
                        optional: opt_chain,
                    };
                }
                Tok::Punct("(") => {
                    if matches!(e, Expr::Super) && self.ctor_depth == 0 {
                        return Err(self.err("super() outside constructor"));
                    }
                    let args = self.call_args()?;
                    e = Expr::Call {
                        callee: Box::new(e),
                        args,
                    };
                }
                Tok::Template(parts) => {
                    // tagged template — treat as call with cooked parts + args
                    self.next();
                    let mut args = Vec::new();
                    let mut strings = Vec::new();
                    let mut exprs = Vec::new();
                    for p in parts {
                        match p {
                            crate::lexer::TemplPart::Str(s) => strings.push(Expr::Str(s)),
                            crate::lexer::TemplPart::Expr(raw) => exprs.push(parse_expr_src(&raw)?),
                        }
                    }
                    args.push(Arg::Expr(Expr::Array(
                        strings.into_iter().map(ArrElem::Expr).collect(),
                    )));
                    for ex in exprs {
                        args.push(Arg::Expr(ex));
                    }
                    e = Expr::Call {
                        callee: Box::new(e),
                        args,
                    };
                }
                _ => break,
            }
        }
        Ok(e)
    }

    /// primary + `.member`/`[idx]` — for `new` callee position (no call args).
    fn member_primary(&mut self) -> Result<Expr, Error> {
        let mut e = if self.at_kw(Kw::New) {
            self.next();
            let callee = self.member_primary()?;
            let args = if self.at_punct("(") {
                self.call_args()?
            } else {
                Vec::new()
            };
            Expr::New {
                callee: Box::new(callee),
                args,
            }
        } else {
            self.primary()?
        };
        loop {
            if self.eat_punct(".") {
                let name = self.prop_key(false)?;
                let prop = match name {
                    PropKey::Ident(s) | PropKey::Str(s) => Expr::Str(s),
                    PropKey::Num(n) => Expr::Str(n.to_string()),
                    PropKey::Computed(e) => e,
                };
                e = Expr::Member {
                    obj: Box::new(e),
                    prop: Box::new(prop),
                    computed: true,
                    optional: false,
                };
            } else if self.at_punct("[") {
                self.next();
                let p = self.expression()?;
                self.expect_punct("]")?;
                e = Expr::Member {
                    obj: Box::new(e),
                    prop: Box::new(p),
                    computed: true,
                    optional: false,
                };
            } else {
                break;
            }
        }
        Ok(e)
    }

    fn call_args(&mut self) -> Result<Vec<Arg>, Error> {
        self.expect_punct("(")?;
        let mut args = Vec::new();
        while !self.at_punct(")") {
            if self.eat_punct("...") {
                args.push(Arg::Spread(self.assign_expr()?));
            } else {
                args.push(Arg::Expr(self.assign_expr()?));
            }
            if !self.eat_punct(",") {
                break;
            }
        }
        self.expect_punct(")")?;
        Ok(args)
    }

    fn primary(&mut self) -> Result<Expr, Error> {
        if self.peek().escaped && matches!(self.peek().tok, Tok::Kw(_)) {
            return Err(self.err("escaped reserved word"));
        }
        match self.peek().tok.clone() {
            Tok::Num(n) => {
                self.check_octal()?;
                self.next();
                Ok(Expr::Num(n))
            }
            Tok::BigInt(n) => {
                self.next();
                Ok(Expr::BigInt(n))
            }
            Tok::Str(s) => {
                self.check_octal()?;
                self.next();
                Ok(Expr::Str(s))
            }
            Tok::Template(parts) => {
                self.check_octal()?;
                self.next();
                let mut out = Vec::new();
                for p in parts {
                    match p {
                        crate::lexer::TemplPart::Str(s) => out.push(TemplPart::Str(s)),
                        crate::lexer::TemplPart::Expr(raw) => {
                            out.push(TemplPart::Expr(parse_expr_src(&raw)?))
                        }
                    }
                }
                Ok(Expr::Template(out))
            }
            Tok::Kw(Kw::True) => {
                self.next();
                Ok(Expr::Bool(true))
            }
            Tok::Kw(Kw::False) => {
                self.next();
                Ok(Expr::Bool(false))
            }
            Tok::Kw(Kw::Null) => {
                self.next();
                Ok(Expr::Null)
            }
            Tok::Regex { pattern, flags } => {
                self.next();
                if let Err(e) = crate::regexp::Regex::compile(&pattern, &flags) {
                    return Err(self.err(format!("invalid regular expression: {e}")));
                }
                Ok(Expr::Regex { pattern, flags })
            }
            Tok::Kw(Kw::This) => {
                self.next();
                Ok(Expr::This)
            }
            Tok::Kw(Kw::Super) => {
                if self.method_depth == 0 {
                    return Err(self.err("super outside method"));
                }
                self.next();
                Ok(Expr::Super)
            }
            Tok::Kw(Kw::Function) => {
                self.next();
                let g = self.eat_punct("*");
                let f = self.fn_tail(false, g)?;
                Ok(Expr::Fn(Rc::new(f)))
            }
            Tok::Kw(Kw::Async) if matches!(self.peek2().tok, Tok::Kw(Kw::Function)) => {
                // `async function [name](...) {}` in expression position
                self.next();
                self.next();
                let g = self.eat_punct("*");
                let mut f = self.fn_tail(false, g)?;
                f.is_async = true;
                Ok(Expr::Fn(Rc::new(f)))
            }
            Tok::Kw(Kw::Import) => {
                // dynamic import("x") / import.meta
                self.next();
                if self.at_punct("(") {
                    let args = self.call_args()?;
                    Ok(Expr::Call {
                        callee: Box::new(Expr::Ident("$dyn_import".into())),
                        args,
                    })
                } else {
                    self.expect_punct(".")?;
                    let _ = self.ident_name()?;
                    Ok(Expr::Ident("$import_meta".into()))
                }
            }
            Tok::Kw(Kw::Class) => {
                self.next();
                let c = self.class_tail(false)?;
                Ok(Expr::Class(Rc::new(c)))
            }
            Tok::Ident(name) => {
                self.next();
                Ok(Expr::Ident(name))
            }
            // contextual keywords usable as identifiers in expression position
            Tok::Kw(k)
                if matches!(
                    k,
                    Kw::Let
                        | Kw::Get
                        | Kw::Set
                        | Kw::Static
                        | Kw::Async
                        | Kw::Of
                        | Kw::Await
                        | Kw::Yield
                        | Kw::From
                        | Kw::As
                ) =>
            {
                // [Yield] early error: `yield` is never an
                // IdentifierReference inside a [Yield] production
                if (k == Kw::Yield && (self.yield_decl > 0 || self.strict))
                    || (self.strict && matches!(k, Kw::Let | Kw::Static))
                {
                    return Err(self.err(format!(
                        "`{}` is not a valid identifier here",
                        k.name()
                    )));
                }
                self.next();
                Ok(Expr::Ident(k.name().to_string()))
            }
            Tok::Punct("(") => {
                self.next();
                // inside `(` the `{`/`[` productions may reinterpret as
                // destructuring patterns — ObjectLiteral-only early
                // errors (duplicate __proto__) don't apply to a cover
                self.paren_depth += 1;
                // `in` inside brackets is an ordinary operator — the
                // for-head's no_in only scopes the top level
                let no_in = std::mem::replace(&mut self.no_in, false);
                let e = self.expression();
                self.no_in = no_in;
                self.paren_depth -= 1;
                let e = e?;
                self.expect_punct(")")?;
                Ok(Expr::Paren(Box::new(e)))
            }
            Tok::Punct("[") => {
                self.next();
                let no_in = std::mem::replace(&mut self.no_in, false);
                let mut elems = Vec::new();
                while !self.at_punct("]") {
                    if self.at_punct(",") {
                        self.next();
                        elems.push(ArrElem::Hole);
                        continue;
                    }
                    if self.eat_punct("...") {
                        let ex = self.assign_expr()?;
                        // `...x,]` — trailing comma marks the element so
                        // pattern conversion can reject it
                        let trail = self.at_punct(",")
                            && matches!(self.peek2().tok, Tok::Punct("]"));
                        elems.push(if trail {
                            ArrElem::SpreadTrail(ex)
                        } else {
                            ArrElem::Spread(ex)
                        });
                    } else {
                        elems.push(ArrElem::Expr(self.assign_expr()?));
                    }
                    if !self.eat_punct(",") {
                        break;
                    }
                }
                self.expect_punct("]")?;
                self.no_in = no_in;
                Ok(Expr::Array(elems))
            }
            Tok::Punct("{") => {
                self.next();
                let no_in = std::mem::replace(&mut self.no_in, false);
                let mut props = Vec::new();
                // spec: two `__proto__: v` DATA properties → early error
                // (getters/setters/methods/computed don't count)
                let mut proto_seen = false;
                let mut dup_proto = false;
                while !self.at_punct("}") {
                    if self.eat_punct("...") {
                        let ex = self.assign_expr()?;
                        let trail = self.at_punct(",")
                            && matches!(self.peek2().tok, Tok::Punct("}"));
                        props.push(ObjProp {
                            key: PropKey::Str(String::new()),
                            val: if trail {
                                PropVal::SpreadTrail(ex)
                            } else {
                                PropVal::Spread(ex)
                            },
                        });
                        self.eat_punct(",");
                        continue;
                    }
                    // `get`/`set` accessor, `async`/`static` methods
                    let is_getset = matches!(self.peek().tok, Tok::Kw(Kw::Get) | Tok::Kw(Kw::Set))
                        && !matches!(
                            self.peek2().tok,
                            Tok::Punct(":")
                                | Tok::Punct(",")
                                | Tok::Punct("(")
                                | Tok::Punct("}")
                                | Tok::Punct("=")
                        );
                    if is_getset {
                        let is_get = self.at_kw(Kw::Get);
                        self.next();
                        let key = self.prop_key(true)?;
                        self.expect_punct("(")?;
                        let params = self.params()?;
                        self.expect_punct(")")?;
                        if is_get && !params.is_empty() {
                            return Err(self.err("getter must not have parameters"));
                        }
                        if !is_get && params.len() != 1 {
                            return Err(self.err("setter must have exactly one parameter"));
                        }
                        self.expect_punct("{")?;
                        let body = self.method_body(false, false)?;
                        let fname = match &key {
                            PropKey::Ident(s) | PropKey::Str(s) => Some(s.clone()),
                            _ => None,
                        };
                        let f = Rc::new(FnDef {
                            name: fname,
                            params,
                            body,
                            is_arrow: false,
                            arrow_expr: None,
                            is_async: false,
                            is_gen: false,
                    strict: self.strict,
                        });
                        props.push(ObjProp {
                            key,
                            val: if is_get {
                                PropVal::Get(f)
                            } else {
                                PropVal::Set(f)
                            },
                        });
                        self.eat_punct(",");
                        continue;
                    }
                    if self.at_kw(Kw::Async)
                        && !matches!(
                            self.peek2().tok,
                            Tok::Punct(":") | Tok::Punct(",") | Tok::Punct("(") | Tok::Punct("}")
                        )
                        && !self.peek2().newline_before
                    {
                        self.next();
                        let member_gen = self.eat_punct("*");
                        let key = self.prop_key(true)?;
                        self.expect_punct("(")?;
                        let params = self.params()?;
                        self.expect_punct(")")?;
                        self.expect_punct("{")?;
                        let body = self.method_body(false, member_gen)?;
                        props.push(ObjProp {
                            key: key.clone(),
                            val: PropVal::Method(Rc::new(FnDef {
                                name: prop_key_name(&key),
                                params,
                                body,
                                is_arrow: false,
                                arrow_expr: None,
                                is_async: true,
                                is_gen: member_gen,
                    strict: self.strict,
                            })),
                        });
                        self.eat_punct(",");
                        continue;
                    }
                    if self.at_punct("*") {
                        self.next();
                        let key = self.prop_key(true)?;
                        self.expect_punct("(")?;
                        let params = self.params()?;
                        self.expect_punct(")")?;
                        self.expect_punct("{")?;
                        let body = self.method_body(false, true)?;
                        props.push(ObjProp {
                            key: key.clone(),
                            val: PropVal::Method(Rc::new(FnDef {
                                name: prop_key_name(&key),
                                params,
                                body,
                                is_arrow: false,
                                arrow_expr: None,
                                is_async: false,
                                is_gen: true,
                    strict: self.strict,
                            })),
                        });
                        self.eat_punct(",");
                        continue;
                    }
                    // a shorthand `{name}` binds the name — keyword keys
        // are only legal for always-contextual names (`{break}` must
        // be a SyntaxError, `{yield}` stays legal sloppy)
        let key_was_reserved = matches!(&self.peek().tok, Tok::Kw(k) if !is_contextual_ident_kw(k));
        let key = self.prop_key(true)?;
                    if matches!(&key, PropKey::Ident(s) | PropKey::Str(s) if s == "__proto__")
                        && self.at_punct(":")
                    {
                        if proto_seen {
                            dup_proto = true;
                        }
                        proto_seen = true;
                    }
                    let val = if self.eat_punct(":") {
                        PropVal::Expr(self.assign_expr()?)
                    } else if self.at_punct("(") {
                        self.next();
                        let params = self.params()?;
                        self.expect_punct(")")?;
                        self.expect_punct("{")?;
                        let body = self.method_body(false, false)?;
                        PropVal::Method(Rc::new(FnDef {
                            name: prop_key_name(&key),
                            params,
                            body,
                            is_arrow: false,
                            arrow_expr: None,
                            is_async: false,
                            is_gen: false,
                    strict: self.strict,
                        }))
                    } else {
                        // shorthand / shorthand with default (destructuring pos)
                        match &key {
                            PropKey::Ident(s) | PropKey::Str(s) => {
                                if key_was_reserved {
                                    return Err(self.err(format!(
                                        "reserved word `{s}` is not a valid binding"
                                    )));
                                }
                                self.check_binding_name(s)?;
                                let ident = Expr::Ident(s.clone());
                                if self.at_punct("=") {
                                    // `{a = 1}` in destructuring — keep as
                                    // Cond-free marker via Assign? Just parse
                                    // and store the assign expr; the binder
                                    // converts Expr→Pattern anyway.
                                    PropVal::Expr(ident)
                                } else {
                                    PropVal::Expr(ident)
                                }
                            }
                            PropKey::Num(n) => {
                                return Err(self.err(format!("expected `:` after key {n}")))
                            }
                            PropKey::Computed(_) => {
                                return Err(self.err("expected `:` after computed key"))
                            }
                        }
                    };
                    // cover-init default `{a = 1}` — consume the `= expr` so
                    // object-destructuring assignments parse; interp converts.
                    if self.at_punct("=") {
                        self.next();
                        let d = self.assign_expr()?;
                        if let PropVal::Expr(Expr::Ident(s)) = &val {
                            props.push(ObjProp {
                                key: key.clone(),
                                val: PropVal::Expr(Expr::Assign {
                                    op: AssignOp::Assign,
                                    target: Box::new(Expr::Ident(s.clone())),
                                    value: Box::new(d),
                                }),
                            });
                            self.eat_punct(",");
                            continue;
                        }
                    }
                    props.push(ObjProp { key, val });
                    if !self.eat_punct(",") {
                        break;
                    }
                }
                self.expect_punct("}")?;
                self.no_in = no_in;
                Ok(if dup_proto {
                    Expr::ObjDup(props)
                } else {
                    Expr::Object(props)
                })
            }
            _ => Err(self.err(format!("unexpected {}", self.describe()))),
        }
    }
}

/// Contextual keywords that are legal as plain identifiers (`as`, `from`,
/// `let`, `of`, `get`, `set`, `static`, `async`, `await`, `yield`).
pub(crate) fn is_contextual_ident_kw(k: &Kw) -> bool {
    matches!(
        k,
        Kw::Let
            | Kw::Get
            | Kw::Set
            | Kw::Static
            | Kw::Async
            | Kw::Of
            | Kw::From
            | Kw::As
            | Kw::Await
            | Kw::Yield
    )
}

fn prop_key_name(k: &PropKey) -> Option<String> {
    match k {
        PropKey::Ident(s) | PropKey::Str(s) => Some(s.clone()),
        PropKey::Num(n) => Some(n.to_string()),
        PropKey::Computed(_) => None,
    }
}

/// Can `e` be an assignment/update target? (destructuring forms included —
/// their element validity is checked at bind time)
/// Static Semantics: AssignmentTargetType — a LeftHandSideExpression
/// that is an array/object literal must additionally satisfy the
/// destructuring pattern grammar (rest last, no methods/get/set,
/// no optional chains).
fn is_assignable(e: &Expr, strict: bool) -> bool {
    match e {
        Expr::Paren(inner) => return is_assignable(inner, strict),
        // `new.target` desugars to a synthetic ident — not a target;
        // strict mode forbids `eval`/`arguments` as assignment targets
        Expr::Ident(n) => {
            n.as_str() != "$new_target"
                && !(strict && matches!(n.as_str(), "eval" | "arguments"))
        }
        Expr::Member { optional, .. } => !optional,
        Expr::Array(elems) => elems.iter().enumerate().all(|(i, el)| match el {
            ArrElem::Hole => true,
            ArrElem::Expr(ex) => is_assignable_elem(ex, strict),
            ArrElem::Spread(ex) => {
                i + 1 == elems.len() && is_assignable(ex, strict)
            }
            ArrElem::SpreadTrail(_) => false,
        }),
        Expr::Object(props) | Expr::ObjDup(props) => {
            props.iter().enumerate().all(|(i, p)| match &p.val {
            PropVal::Expr(ex) => is_assignable_elem(ex, strict),
            PropVal::Spread(ex) => {
                i + 1 == props.len() && is_assignable(ex, strict)
            }
            PropVal::SpreadTrail(_) => false,
                PropVal::Method(_) | PropVal::Get(_) | PropVal::Set(_) => false,
            })
        }
        _ => false,
    }
}

/// an array/object literal element: either an assignable target or
/// `target = default`
fn is_assignable_elem(ex: &Expr, strict: bool) -> bool {
    match ex {
        Expr::Assign {
            op: AssignOp::Assign,
            target,
            ..
        } => is_assignable(target, strict),
        _ => is_assignable(ex, strict),
    }
}

pub(crate) fn pattern_names<'a>(p: &'a Param, out: &mut Vec<&'a str>) {
    match p {
        Param::Ident(n) => out.push(n.as_str()),
        Param::Default(pat, _) | Param::Rest(pat) => collect_pat_names(pat, out),
        Param::Pattern(pat) => collect_pat_names(pat, out),
    }
}

fn collect_pat_names<'a>(p: &'a Pattern, out: &mut Vec<&'a str>) {
    match p {
        Pattern::Target(_) => {}
        Pattern::Ident(n) => out.push(n.as_str()),
        Pattern::Array(elems) => {
            for e in elems {
                match e {
                    ArrayPatElem::Elem(p) | ArrayPatElem::Rest(p) => {
                        collect_pat_names(p, out)
                    }
                    ArrayPatElem::Default(p, _) => collect_pat_names(p, out),
                    ArrayPatElem::Hole => {}
                }
            }
        }
        Pattern::Object(elems) => {
            for e in elems {
                collect_pat_names(&e.val, out);
            }
        }
    }
}

// ---- post-parse validation (early errors needing scope info) --------------

/// Lexically-declared names of a statement (let/const/class — "hard"
/// lexical bindings; function decls are handled separately), not recursing
/// into nested functions/blocks.
fn lex_names(s: &Stmt, out: &mut Vec<String>) {
    match s {
        Stmt::VarDecl { kind, decls } if *kind != VarKind::Var => {
            for (p, _) in decls {
                let mut ns = Vec::new();
                collect_pat_names(p, &mut ns);
                out.extend(ns.into_iter().map(String::from));
            }
        }
        Stmt::ClassDecl(c) => {
            if let Some(n) = &c.name {
                out.push(n.clone());
            }
        }
        _ => {}
    }
}

/// `var`-declared names of a statement (top-level patterns only).
fn var_names(s: &Stmt, out: &mut Vec<String>) {
    if let Stmt::VarDecl { kind: VarKind::Var, decls } = s {
        for (p, _) in decls {
            let mut ns = Vec::new();
            collect_pat_names(p, &mut ns);
            out.extend(ns.into_iter().map(String::from));
        }
    }
}

struct VCtx {
    /// all block-level bindings per scope (let/const/class + block fns +
    /// catch params) — new lex decls and fns conflict against these
    lexes: Vec<std::collections::HashSet<String>>,
    /// "hard" lexical names per scope (let/const/class only) — `var`
    /// declarations conflict against these; Annex-B fn names don't block var
    hard: Vec<std::collections::HashSet<String>>,
    /// per-function var-name sets (var hoists to nearest fn boundary)
    vars: Vec<std::collections::HashSet<String>>,
    /// label stack: (name, is_iteration)
    labels: Vec<(String, bool)>,
    in_iter: usize,
    in_switch: usize,
    in_fn: usize,
    in_method: usize,
    in_ctor: usize,
    strict: bool,
}

impl VCtx {
    fn err(&self, msg: impl Into<String>) -> Error {
        Error::new(msg, 0, 0)
    }
    /// declare a let/const/class name in the current block scope
    fn decl_lex(&mut self, name: &str) -> Result<(), Error> {
        // lex names conflict with same-block bindings; at the function-body
        // top scope they also conflict with that body's var names (nested
        // blocks may legally shadow a var)
        let top_level = self.lexes.len() == 1;
        if self.lexes.last().is_some_and(|t| t.contains(name))
            || (top_level && self.vars.last().is_some_and(|v| v.contains(name)))
        {
            return Err(self.err(format!("duplicate declaration `{name}`")));
        }
        self.lexes.last_mut().unwrap().insert(name.to_string());
        self.hard.last_mut().unwrap().insert(name.to_string());
        Ok(())
    }
    /// declare a var name: a var propagates up through every enclosing
    /// statement list, so it joins the VarDeclaredNames of EVERY ancestor
    /// block — conflicts with any lexically-declared name (let/const/
    /// class/block-fn) in those lists, e.g. `{function f(){} {var f}}`
    fn decl_var(&mut self, name: &str) -> Result<(), Error> {
        if self.lexes.iter().any(|s| s.contains(name)) {
            return Err(self.err(format!("`{name}` conflicts with a lexical declaration")));
        }
        for v in self.vars.iter_mut() {
            v.insert(name.to_string());
        }
        Ok(())
    }
    /// declare a block-level function name: conflicts with var names in
    /// the SAME statement list (`{var f; function f(){}}`) and lex names;
    /// a same-name block fn is an error only in strict code
    fn decl_fn(&mut self, name: &str, in_block: bool) -> Result<(), Error> {
        if in_block {
            if self.hard.last().is_some_and(|h| h.contains(name))
                || self.vars.last().is_some_and(|v| v.contains(name))
                || (self.strict && self.lexes.last().is_some_and(|t| t.contains(name)))
            {
                return Err(self.err(format!("duplicate declaration `{name}`")));
            }
            self.lexes.last_mut().unwrap().insert(name.to_string());
            // Annex B: block fn in sloppy code also creates a var binding
            // at function scope — it enters the VarDeclaredNames of every
            // enclosing statement list
            if !self.strict {
                for v in self.vars.iter_mut() {
                    v.insert(name.to_string());
                }
            }
            Ok(())
        } else {
            self.decl_var(name)
        }
    }
    fn push_scope(&mut self) {
        self.lexes.push(Default::default());
        self.hard.push(Default::default());
        self.vars.push(Default::default());
    }
    fn pop_scope(&mut self) {
        self.lexes.pop();
        self.hard.pop();
        // var names declared inside the popped block propagate UP into
        // the parent's VarDeclaredNames
        if let Some(inner) = self.vars.pop() {
            if let Some(outer) = self.vars.last_mut() {
                for n in inner {
                    outer.insert(n);
                }
            }
        }
    }
}

fn body_strict(stmts: &[Stmt], inherited: bool) -> bool {
    if inherited {
        return true;
    }
    for s in stmts {
        match s {
            Stmt::Expr(Expr::Str(t)) if t == "use strict" => return true,
            Stmt::Expr(Expr::Str(_)) => {}
            _ => return false,
        }
    }
    false
}

fn validate(stmts: &[Stmt]) -> Result<(), Error> {
    validate_strict_ctx(stmts, false, false, false)
}

/// Script/Eval validation — `in_fn`/`in_method` seed the early-error
/// context so `new.target`/`super.prop` pass inside direct eval.
fn validate_strict_ctx(
    stmts: &[Stmt],
    strict: bool,
    in_fn: bool,
    in_method: bool,
) -> Result<(), Error> {
    // Script goal: module declarations are illegal at top level
    for s in stmts {
        match s {
            Stmt::Import(_) | Stmt::Export(_) => {
                return Err(Error {
                    msg: "import/export declarations are only allowed in modules"
                        .into(),
                    line: 0,
                    col: 0,
                })
            }
            _ => {}
        }
    }
    let mut cx = VCtx {
        lexes: vec![Default::default()],
        hard: vec![Default::default()],
        vars: vec![Default::default()],
        labels: Vec::new(),
        in_iter: 0,
        in_switch: 0,
        in_fn: in_fn as usize,
        in_method: in_method as usize,
        in_ctor: 0,
        strict: strict || body_strict(stmts, false),
    };
    vstmts(stmts, &mut cx)
}

/// Validate one block's statements: `new_scope` opens a fresh lex scope
/// (false at program/fn top level where the scope was pre-opened).
fn vstmts(stmts: &[Stmt], cx: &mut VCtx) -> Result<(), Error> {
    vblock(stmts, cx, false)
}

fn vblock(stmts: &[Stmt], cx: &mut VCtx, new_scope: bool) -> Result<(), Error> {
    if new_scope {
        cx.push_scope();
    }
    // pass 1: declare this level's lexical + var + fn names (spec checks all
    // bound names before descending — dup `let x` before use still errors)
    let mut lex = Vec::new();
    let mut vars = Vec::new();
    let mut fns = Vec::new();
    for s in stmts {
        lex_names(s, &mut lex);
        var_names(s, &mut vars);
        if let Stmt::FnDecl(f) = s {
            if let Some(n) = &f.name {
                fns.push(n.clone());
            }
        }
    }
    let mut r = Ok(());
    for n in &lex {
        if let Err(e) = cx.decl_lex(n) {
            r = Err(e);
            break;
        }
    }
    if r.is_ok() {
        for n in &vars {
            if let Err(e) = cx.decl_var(n) {
                r = Err(e);
                break;
            }
        }
    }
    if r.is_ok() {
        for n in &fns {
            if let Err(e) = cx.decl_fn(n, new_scope) {
                r = Err(e);
                break;
            }
        }
    }
    // pass 2: descend
    if r.is_ok() {
        for s in stmts {
            if let Err(e) = vstmt(s, cx) {
                r = Err(e);
                break;
            }
        }
    }
    if new_scope {
        cx.pop_scope();
    }
    r
}

fn vstmt(s: &Stmt, cx: &mut VCtx) -> Result<(), Error> {
    match s {
        Stmt::Block(b) => vblock(b, cx, true),
        Stmt::VarDecl { decls, .. } => {
            for (_, i) in decls {
                if let Some(e) = i {
                    vexpr(e, cx)?;
                }
            }
            Ok(())
        }
        Stmt::FnDecl(f) => vfn(f, cx, None),
        Stmt::ClassDecl(c) => vclass(c, cx),
        Stmt::Expr(e) => vexpr(e, cx),
        Stmt::If { c: cond, t, e } => {
            vexpr(cond, cx)?;
            // Annex B.3.4: `if (x) function f(){}` is sloppy-only
            if cx.strict
                && (matches!(&**t, Stmt::FnDecl(_))
                    || e.as_ref().is_some_and(|e| matches!(&**e, Stmt::FnDecl(_))))
            {
                return Err(cx.err(
                    "function declaration not allowed in statement position in strict mode",
                ));
            }
            vstmt(t, cx)?;
            if let Some(e) = e {
                vstmt(e, cx)?;
            }
            Ok(())
        }
        Stmt::While { c, body } => {
            vexpr(c, cx)?;
            // a bare function declaration is never a valid iteration body
            if matches!(&**body, Stmt::FnDecl(_)) {
                return Err(cx.err("function declaration not allowed in statement position"));
            }
            cx.in_iter += 1;
            let r = vstmt(body, cx);
            cx.in_iter -= 1;
            r
        }
        Stmt::DoWhile { body, c } => {
            if matches!(&**body, Stmt::FnDecl(_)) {
                return Err(cx.err("function declaration not allowed in statement position"));
            }
            cx.in_iter += 1;
            let r = vstmt(body, cx);
            cx.in_iter -= 1;
            r?;
            vexpr(c, cx)
        }
        Stmt::For { init, test, update, body } => {
            cx.push_scope();
            if let Some(i) = init {
                match i {
                    ForInit::Var(kind, decls) => {
                        let mut ns = Vec::new();
                        for (p, _) in decls {
                            let mut nn = Vec::new();
                            collect_pat_names(p, &mut nn);
                            ns.extend(nn.into_iter().map(String::from));
                        }
                        for n in &ns {
                            if *kind == VarKind::Var {
                                cx.decl_var(n)?;
                            } else {
                                cx.decl_lex(n)?;
                            }
                        }
                        for (_, e) in decls {
                            if let Some(e) = e {
                                vexpr(e, cx)?;
                            }
                        }
                    }
                    ForInit::Expr(e) => vexpr(e, cx)?,
                }
            }
            if let Some(t) = test {
                vexpr(t, cx)?;
            }
            if let Some(u) = update {
                vexpr(u, cx)?;
            }
            if matches!(&**body, Stmt::FnDecl(_)) {
                return Err(cx.err("function declaration not allowed in statement position"));
            }
            cx.in_iter += 1;
            let r = vstmt(body, cx);
            cx.in_iter -= 1;
            cx.pop_scope();
            r
        }
        Stmt::ForIn { left, right, body } | Stmt::ForOf { left, right, body } => {
            cx.push_scope();
            match left {
                ForTarget::Var(kind, p) => {
                    let mut ns = Vec::new();
                    collect_pat_names(p, &mut ns);
                    for n in ns {
                        if *kind == VarKind::Var {
                            cx.decl_var(&n)?;
                        } else {
                            cx.decl_lex(&n)?;
                        }
                    }
                }
                ForTarget::Expr(e) => {
                    if !is_assignable(e, cx.strict) {
                        return Err(cx.err("invalid for-in/of target"));
                    }
                    vexpr_pat(e, cx)?;
                }
            }
            vexpr(right, cx)?;
            if matches!(&**body, Stmt::FnDecl(_)) {
                return Err(cx.err("function declaration not allowed in statement position"));
            }
            cx.in_iter += 1;
            let r = vstmt(body, cx);
            cx.in_iter -= 1;
            cx.pop_scope();
            r
        }
        Stmt::Switch { disc, cases } => {
            vexpr(disc, cx)?;
            cx.in_switch += 1;
            cx.push_scope();
            let mut r = Ok(());
            for c in cases {
                if let Err(e) = vblock(&c.body, cx, false) {
                    r = Err(e);
                    break;
                }
            }
            cx.pop_scope();
            cx.in_switch -= 1;
            r
        }
        Stmt::Try { block, param, handler, finally } => {
            vblock(block, cx, true)?;
            if let Some(p) = param {
                cx.push_scope();
                let mut ns = Vec::new();
                collect_pat_names(p, &mut ns);
                let mut seen = std::collections::HashSet::new();
                for n in ns {
                    // catch params conflict with same-block lex decls but
                    // NOT with `var` — put them in `lexes` only
                    if !seen.insert(n.clone()) {
                        return Err(cx.err(format!("duplicate declaration `{n}`")));
                    }
                    cx.lexes.last_mut().unwrap().insert(n.to_string());
                }
            }
            if let Some(h) = handler {
                vblock(h, cx, param.is_none())?;
            }
            if param.is_some() {
                cx.pop_scope();
            }
            if let Some(f) = finally {
                vblock(f, cx, true)?;
            }
            Ok(())
        }
        Stmt::Throw(e) => vexpr(e, cx),
        Stmt::Return(e) => {
            if cx.in_fn == 0 {
                return Err(cx.err("return outside function"));
            }
            if let Some(e) = e {
                vexpr(e, cx)?;
            }
            Ok(())
        }
        Stmt::Break(l) => match l {
            Some(l) => {
                if cx.labels.iter().any(|(n, _)| n == l) {
                    Ok(())
                } else {
                    Err(cx.err(format!("undefined label `{l}`")))
                }
            }
            None if cx.in_iter == 0 && cx.in_switch == 0 => {
                Err(cx.err("break outside loop or switch"))
            }
            None => Ok(()),
        },
        Stmt::Continue(l) => match l {
            Some(l) => {
                if cx.labels.iter().any(|(n, it)| n == l && *it) {
                    Ok(())
                } else {
                    Err(cx.err(format!("undefined iteration label `{l}`")))
                }
            }
            None if cx.in_iter == 0 => Err(cx.err("continue outside loop")),
            None => Ok(()),
        },
        Stmt::Labeled(name, inner) => {
            // a label can't cover a lexical declaration or duplicate a live label
            if matches!(
                &**inner,
                Stmt::VarDecl { kind, .. } if *kind != VarKind::Var
            ) || matches!(&**inner, Stmt::ClassDecl(_))
            {
                return Err(cx.err("labelled lexical declaration"));
            }
            if cx.strict && matches!(&**inner, Stmt::FnDecl(_)) {
                return Err(cx.err(
                    "labelled function declaration not allowed in strict mode",
                ));
            }
            if cx.labels.iter().any(|(n, _)| n == name) {
                return Err(cx.err(format!("duplicate label `{name}`")));
            }
            let is_iter = matches!(
                &**inner,
                Stmt::While { .. }
                    | Stmt::DoWhile { .. }
                    | Stmt::For { .. }
                    | Stmt::ForIn { .. }
                    | Stmt::ForOf { .. }
            );
            cx.labels.push((name.clone(), is_iter));
            let r = vstmt(inner, cx);
            cx.labels.pop();
            r
        }
        Stmt::With { body, .. } => {
            if cx.strict {
                return Err(cx.err("`with` is not allowed in strict mode"));
            }
            vstmt(body, cx)
        }
        Stmt::Empty => Ok(()),
        // nested module declarations are always illegal (module top-level
        // only — and `parse` is the Script goal anyway)
        Stmt::Import(_) | Stmt::Export(_) => {
            Err(cx.err("import/export declarations are only allowed in modules"))
        }
    }
}

/// validate a function: fresh lex/var stacks seeded with param names.
/// Non-simple sloppy param lists get their own scope so the body's `let`
/// can shadow a param (`function f({a}){ let a }` is legal).
fn vfn(f: &FnDef, cx: &mut VCtx, method: Option<bool>) -> Result<(), Error> {
    let saved = (
        std::mem::take(&mut cx.lexes),
        std::mem::take(&mut cx.hard),
        std::mem::take(&mut cx.vars),
        std::mem::take(&mut cx.labels),
        cx.in_iter,
        cx.in_switch,
        cx.in_fn,
        cx.in_method,
        cx.in_ctor,
        cx.strict,
    );
    cx.strict = body_strict(&f.body, cx.strict);
    cx.in_iter = 0;
    cx.in_switch = 0;
    cx.in_fn += 1;
    if !f.is_arrow {
        // ordinary functions drop enclosing super context; methods get it
        // from `method` (Some(is_ctor)); arrows inherit it (None keeps it)
        match method {
            Some(is_ctor) => {
                cx.in_method = 1;
                cx.in_ctor = is_ctor as usize;
            }
            None => {
                cx.in_method = 0;
                cx.in_ctor = 0;
            }
        }
    }
    cx.push_scope();
    let simple = f.params.iter().all(|p| matches!(p, Param::Ident(_)));
    let mut r = Ok(());
    for p in &f.params {
        let mut ns = Vec::new();
        pattern_names(p, &mut ns);
        for n in ns {
            if cx.strict {
                if let Err(e) = cx.decl_lex(n) {
                    r = Err(e);
                    break;
                }
            } else {
                cx.lexes.last_mut().unwrap().insert(n.to_string());
            }
        }
    }
    if r.is_ok() {
        if !simple && !cx.strict {
            // param scope separate from body scope
            cx.push_scope();
            r = vblock(&f.body, cx, false);
            cx.pop_scope();
        } else {
            r = vstmts(&f.body, cx);
        }
    }
    if r.is_ok() {
        if let Some(ae) = &f.arrow_expr {
            r = vexpr(ae, cx);
        }
    }
    (
        cx.lexes,
        cx.hard,
        cx.vars,
        cx.labels,
        cx.in_iter,
        cx.in_switch,
        cx.in_fn,
        cx.in_method,
        cx.in_ctor,
        cx.strict,
    ) = saved;
    r
}

fn vclass(c: &ClassDef, cx: &mut VCtx) -> Result<(), Error> {
    if let Some(s) = &c.superclass {
        vexpr(s, cx)?;
    }
    let saved_strict = cx.strict;
    cx.strict = true; // class bodies are always strict
    let saved = (cx.in_method, cx.in_ctor);
    let mut r = Ok(());
    for m in &c.members {
        if let Err(e) = vfn(&m.func, cx, Some(matches!(m.kind, MemberKind::Ctor))) {
            r = Err(e);
            break;
        }
    }
    (cx.in_method, cx.in_ctor) = saved;
    cx.strict = saved_strict;
    r
}

fn vexpr(e: &Expr, cx: &mut VCtx) -> Result<(), Error> {
    match e {
        Expr::Ident(n) if n == "$new_target" => {
            if cx.in_fn == 0 {
                return Err(cx.err("new.target outside function"));
            }
            Ok(())
        }
        Expr::Super => {
            if cx.in_method == 0 {
                return Err(cx.err("super outside method"));
            }
            Ok(())
        }
        Expr::Call { callee, args } => {
            if matches!(&**callee, Expr::Super) && cx.in_ctor == 0 {
                return Err(cx.err("super() outside constructor"));
            }
            vexpr(callee, cx)?;
            for a in args {
                match a {
                    Arg::Expr(e) | Arg::Spread(e) => vexpr(e, cx)?,
                }
            }
            Ok(())
        }
        Expr::Assign { target, value, .. } => {
            if !is_assignable(target, cx.strict) {
                return Err(cx.err("invalid assignment target"));
            }
            if cx.strict {
                if let Expr::Ident(n) = &**target {
                    if n == "eval" || n == "arguments" {
                        return Err(cx.err("cannot assign to eval/arguments"));
                    }
                }
            }
            vexpr_pat(target, cx)?;
            vexpr(value, cx)
        }
        Expr::Update { target, .. } => {
            if !is_assignable(target, cx.strict) {
                return Err(cx.err("invalid ++/-- target"));
            }
            vexpr(target, cx)
        }
        Expr::Paren(inner) => vexpr(inner, cx),
        Expr::Unary { expr, .. } => vexpr(expr, cx),
        Expr::Binary { l, r, .. } | Expr::Logical { l, r, .. } => {
            vexpr(l, cx)?;
            vexpr(r, cx)
        }
        Expr::Cond { c, t, f } => {
            vexpr(c, cx)?;
            vexpr(t, cx)?;
            vexpr(f, cx)
        }
        Expr::Member { obj, prop, .. } => {
            vexpr(obj, cx)?;
            vexpr(prop, cx)
        }
        Expr::New { callee, args } => {
            vexpr(callee, cx)?;
            for a in args {
                match a {
                    Arg::Expr(e) | Arg::Spread(e) => vexpr(e, cx)?,
                }
            }
            Ok(())
        }
        Expr::Fn(f) => vfn(f, cx, None),
        Expr::Class(c) => vclass(c, cx),
        Expr::Array(elems) => {
            for el in elems {
                match el {
                    ArrElem::Expr(e) | ArrElem::Spread(e) | ArrElem::SpreadTrail(e) => vexpr(e, cx)?,
                    ArrElem::Hole => {}
                }
            }
            Ok(())
        }
        Expr::ObjDup(_) => {
            return Err(cx.err("duplicate __proto__ property"));
        }
        Expr::Object(props) => {
            for p in props {
                if let PropKey::Computed(c) = &p.key {
                    vexpr(c, cx)?;
                }
                match &p.val {
                    PropVal::Expr(e) => vexpr(e, cx)?,
                    PropVal::Spread(e) | PropVal::SpreadTrail(e) => vexpr(e, cx)?,
                    PropVal::Get(f) | PropVal::Set(f) | PropVal::Method(f) => {
                        vfn(f, cx, Some(false))?;
                    }
                }
            }
            Ok(())
        }
        Expr::Template(parts) => {
            for p in parts {
                if let TemplPart::Expr(e) = p {
                    vexpr(e, cx)?;
                }
            }
            Ok(())
        }
        Expr::Seq(exprs) => {
            for e in exprs {
                vexpr(e, cx)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum BinKind {
    B(BinOp),
    Log(LogOp),
    Pow,
}

impl BinKind {
    fn binop(self) -> Option<BinOp> {
        match self {
            BinKind::B(b) => Some(b),
            BinKind::Pow => Some(BinOp::Pow),
            BinKind::Log(_) => None,
        }
    }
}

/// Expression validation in destructuring-assignment target position:
/// `Expr::ObjDup` is legal here (the `__proto__` keys read as ordinary
/// property names), and nested pattern structure recurses; anything
/// else falls back to ordinary expression validation.
fn vexpr_pat(e: &Expr, cx: &mut VCtx) -> Result<(), Error> {
    match e {
        Expr::Paren(inner) => vexpr_pat(inner, cx),
        Expr::ObjDup(props) | Expr::Object(props) => {
            for p in props {
                match &p.val {
                    PropVal::Expr(ex) => vexpr_pat(ex, cx)?,
                    PropVal::Spread(ex) | PropVal::SpreadTrail(ex) => {
                        vexpr_pat(ex, cx)?
                    }
                    PropVal::Method(d) | PropVal::Get(d) | PropVal::Set(d) => {
                        vfn(d, cx, Some(false))?
                    }
                }
            }
            Ok(())
        }
        Expr::Array(elems) => {
            for el in elems {
                match el {
                    ArrElem::Expr(ex) | ArrElem::Spread(ex)
                    | ArrElem::SpreadTrail(ex) => vexpr_pat(ex, cx)?,
                    ArrElem::Hole => {}
                }
            }
            Ok(())
        }
        // `a = default` inside a pattern: the target side stays a
        // pattern; the default is an ordinary expression
        Expr::Assign {
            op: AssignOp::Assign,
            target,
            value,
        } => {
            vexpr_pat(target, cx)?;
            vexpr(value, cx)
        }
        Expr::Member { obj, prop, .. } => {
            vexpr(obj, cx)?;
            vexpr(prop, cx)
        }
        _ => vexpr(e, cx),
    }
}
