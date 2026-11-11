//! Recursive-descent parser producing the AST.

use crate::ast::*;
use crate::lexer::{lex, Kw, Tok, Token};
use crate::Error;
use std::rc::Rc;

pub fn parse(src: &str) -> Result<Vec<Stmt>, Error> {
    let toks = lex(src)?;
    let mut p = Parser { toks, pos: 0 };
    p.program()
}

/// Parse a single expression from source (used for template interpolations).
fn parse_expr_src(src: &str) -> Result<Expr, Error> {
    let toks = lex(src)?;
    let mut p = Parser { toks, pos: 0 };
    let e = p.expression()?;
    if !matches!(p.peek().tok, Tok::Eof) {
        return Err(p.err("trailing tokens in expression"));
    }
    Ok(e)
}

struct Parser {
    toks: Vec<Token>,
    pos: usize,
}

impl Parser {
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
            Err(self.err(format!("expected `{}`, found {}", k.name(), self.describe())))
        }
    }
    fn describe(&self) -> String {
        match &self.peek().tok {
            Tok::Eof => "end of input".into(),
            Tok::Ident(s) => format!("`{s}`"),
            Tok::Kw(k) => format!("`{}`", k.name()),
            Tok::Punct(p) => format!("`{p}`"),
            Tok::Num(n) => format!("`{n}`"),
            Tok::Str(s) => format!("`\"{s}\"`"),
            Tok::Template(_) => "template literal".into(),
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
            Tok::Kw(k)
                if matches!(
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
                ) =>
            {
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
                self.next();
                Ok(PropKey::Str(s))
            }
            Tok::Num(n) => {
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
        while !self.at_eof() {
            stmts.push(self.statement()?);
        }
        Ok(stmts)
    }

    fn statement(&mut self) -> Result<Stmt, Error> {
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
                if matches!(self.peek2().tok, Tok::Ident(_) | Tok::Punct("[") | Tok::Punct("{"))
                    || matches!(&self.peek2().tok, Tok::Kw(k) if matches!(k, Kw::Get|Kw::Set|Kw::Async|Kw::Of|Kw::Await|Kw::Yield))
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
                self.eat_punct("*"); // generator marker — treated as plain fn
                let f = self.fn_tail(true)?;
                Ok(Stmt::FnDecl(Rc::new(f)))
            }
            Tok::Kw(Kw::Async) if matches!(self.peek2().tok, Tok::Kw(Kw::Function)) => {
                self.next();
                self.next();
                self.eat_punct("*");
                let mut f = self.fn_tail(true)?;
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
                let body = Box::new(self.statement()?);
                Ok(Stmt::While { c, body })
            }
            Tok::Kw(Kw::Do) => {
                self.next();
                let body = Box::new(self.statement()?);
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
                let inner = self.statement()?;
                Ok(Stmt::Labeled(name, Box::new(inner)))
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
        let mut decls = Vec::new();
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
        Ok((kind, decls))
    }

    fn binding_pattern(&mut self) -> Result<Pattern, Error> {
        match self.peek().tok.clone() {
            Tok::Ident(s) => {
                self.next();
                Ok(Pattern::Ident(s))
            }
            Tok::Kw(k)
                if matches!(
                    k,
                    Kw::Let | Kw::Get | Kw::Set | Kw::Async | Kw::Of | Kw::Await | Kw::Yield
                ) =>
            {
                self.next();
                Ok(Pattern::Ident(k.name().to_string()))
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
                        let d = if self.eat_punct("=") {
                            Some(self.assign_expr()?)
                        } else {
                            None
                        };
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
        let t = Box::new(self.statement()?);
        let e = if self.eat_kw(Kw::Else) {
            Some(Box::new(self.statement()?))
        } else {
            None
        };
        Ok(Stmt::If { c, t, e })
    }

    fn for_stmt(&mut self) -> Result<Stmt, Error> {
        self.expect_kw(Kw::For)?;
        self.expect_punct("(")?;

        // for-in / for-of heads
        let init_is_decl = matches!(self.peek().tok, Tok::Kw(Kw::Var) | Tok::Kw(Kw::Const))
            || (matches!(self.peek().tok, Tok::Kw(Kw::Let))
                && matches!(self.peek2().tok, Tok::Ident(_) | Tok::Punct("[") | Tok::Punct("{")))
            || matches!(&self.peek().tok, Tok::Kw(Kw::Let) if matches!(&self.peek2().tok, Tok::Kw(k) if matches!(k, Kw::Get|Kw::Set|Kw::Async|Kw::Of|Kw::Await|Kw::Yield)));

        if init_is_decl {
            let kind = match self.peek().tok {
                Tok::Kw(Kw::Var) => VarKind::Var,
                Tok::Kw(Kw::Const) => VarKind::Const,
                _ => VarKind::Let,
            };
            self.next();
            let pat = self.binding_pattern()?;
            if self.at_kw(Kw::Of) {
                self.next();
                let right = self.expression()?;
                self.expect_punct(")")?;
                let body = Box::new(self.statement()?);
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
                let body = Box::new(self.statement()?);
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
            let body = Box::new(self.statement()?);
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
            let body = Box::new(self.statement()?);
            return Ok(Stmt::For {
                init: None,
                test,
                update,
                body,
            });
        }

        let first = self.expression()?;
        if self.at_kw(Kw::Of) {
            self.next();
            let right = self.expression()?;
            self.expect_punct(")")?;
            let body = Box::new(self.statement()?);
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
            let body = Box::new(self.statement()?);
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
        let body = Box::new(self.statement()?);
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
                let f = self.fn_tail(false)?;
                ExportRec::Default(Expr::Fn(Rc::new(f)))
            } else
            if self.at_kw(Kw::Class) {
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
    fn fn_tail(&mut self, name_required: bool) -> Result<FnDef, Error> {
        let name = match self.peek().tok.clone() {
            Tok::Ident(s) => {
                self.next();
                Some(s)
            }
            Tok::Kw(k)
                if matches!(
                    k,
                    Kw::Let | Kw::Get | Kw::Set | Kw::Async | Kw::Of | Kw::Await | Kw::Yield
                ) =>
            {
                self.next();
                Some(k.name().to_string())
            }
            _ if !name_required => None,
            _ => return Err(self.err("expected function name")),
        };
        self.expect_punct("(")?;
        let params = self.params()?;
        self.expect_punct(")")?;
        self.expect_punct("{")?;
        let body = self.block_body()?;
        Ok(FnDef {
            name,
            params,
            body,
            is_arrow: false,
            arrow_expr: None,
            is_async: false,
        })
    }

    fn params(&mut self) -> Result<Vec<Param>, Error> {
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
            if self.at_kw(Kw::Static) && !matches!(self.peek2().tok, Tok::Punct("(") | Tok::Punct("=")) {
                self.next();
                is_static = true;
            }
            if self.at_kw(Kw::Async) && !matches!(self.peek2().tok, Tok::Punct("(") | Tok::Punct("=") | Tok::Punct(":")) && !self.peek2().newline_before {
                self.next();
                is_async = true;
            }
            // getter/setter (contextual)
            let mut kind = MemberKind::Method;
            let is_getset_candidate = matches!(self.peek().tok, Tok::Kw(Kw::Get) | Tok::Kw(Kw::Set));
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
            if self.at_punct("*") {
                // generator — parse as regular method
                self.next();
            }
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
            self.expect_punct("{")?;
            let body = self.block_body()?;
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
                }),
                is_static,
            });
        }
        self.expect_punct("}")?;
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
        if self.at_kw(Kw::Yield) {
            // `yield expr` — treat as plain expr (generators unsupported)
            self.next();
            if !self.at_punct(")") && !self.at_punct("]") && !self.at_punct("}") && !self.at_punct(",") && !self.at_punct(";") && !self.at_eof() {
                return self.assign_expr();
            }
            return Ok(Expr::Ident("undefined".into()));
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
        if self.at_punct("{") {
            self.next();
            let body = self.block_body()?;
            Ok(Expr::Fn(Rc::new(FnDef {
                name,
                params,
                body,
                is_arrow: true,
                arrow_expr: None,
                is_async,
            })))
        } else {
            let e = self.assign_expr()?;
            Ok(Expr::Fn(Rc::new(FnDef {
                name,
                params,
                body: Vec::new(),
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
            Tok::Kw(Kw::In) => (8, BinKind::B(In)),
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
            return Ok(Expr::Unary {
                op,
                expr: Box::new(e),
            });
        }
        if self.at_punct("++") || self.at_punct("--") {
            let inc = self.at_punct("++");
            self.next();
            let t = self.unary_expr()?;
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
            // `new.target` — parse as member of `new`
            if self.eat_punct(".") {
                let _ = self.ident_name()?;
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
                            crate::lexer::TemplPart::Expr(raw) => {
                                exprs.push(parse_expr_src(&raw)?)
                            }
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
        match self.peek().tok.clone() {
            Tok::Num(n) => {
                self.next();
                Ok(Expr::Num(n))
            }
            Tok::Str(s) => {
                self.next();
                Ok(Expr::Str(s))
            }
            Tok::Template(parts) => {
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
            Tok::Kw(Kw::This) => {
                self.next();
                Ok(Expr::This)
            }
            Tok::Kw(Kw::Super) => {
                self.next();
                Ok(Expr::Super)
            }
            Tok::Kw(Kw::Function) => {
                self.next();
                self.eat_punct("*");
                let f = self.fn_tail(false)?;
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
                    Kw::Let | Kw::Get | Kw::Set | Kw::Static | Kw::Async | Kw::Of | Kw::Await | Kw::Yield | Kw::From | Kw::As
                ) =>
            {
                self.next();
                Ok(Expr::Ident(k.name().to_string()))
            }
            Tok::Punct("(") => {
                self.next();
                let e = self.expression()?;
                self.expect_punct(")")?;
                Ok(e)
            }
            Tok::Punct("[") => {
                self.next();
                let mut elems = Vec::new();
                while !self.at_punct("]") {
                    if self.at_punct(",") {
                        self.next();
                        elems.push(ArrElem::Hole);
                        continue;
                    }
                    if self.eat_punct("...") {
                        elems.push(ArrElem::Spread(self.assign_expr()?));
                    } else {
                        elems.push(ArrElem::Expr(self.assign_expr()?));
                    }
                    if !self.eat_punct(",") {
                        break;
                    }
                }
                self.expect_punct("]")?;
                Ok(Expr::Array(elems))
            }
            Tok::Punct("{") => {
                self.next();
                let mut props = Vec::new();
                while !self.at_punct("}") {
                    if self.eat_punct("...") {
                        props.push(ObjProp {
                            key: PropKey::Str(String::new()),
                            val: PropVal::Spread(self.assign_expr()?),
                        });
                        self.eat_punct(",");
                        continue;
                    }
                    // `get`/`set` accessor, `async`/`static` methods
                    let is_getset = matches!(self.peek().tok, Tok::Kw(Kw::Get) | Tok::Kw(Kw::Set))
                        && !matches!(
                            self.peek2().tok,
                            Tok::Punct(":") | Tok::Punct(",") | Tok::Punct("(") | Tok::Punct("}") | Tok::Punct("=")
                        );
                    if is_getset {
                        let is_get = self.at_kw(Kw::Get);
                        self.next();
                        let key = self.prop_key(true)?;
                        self.expect_punct("(")?;
                        let params = self.params()?;
                        self.expect_punct(")")?;
                        self.expect_punct("{")?;
                        let body = self.block_body()?;
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
                        && !matches!(self.peek2().tok, Tok::Punct(":") | Tok::Punct(",") | Tok::Punct("(") | Tok::Punct("}"))
                        && !self.peek2().newline_before
                    {
                        self.next();
                        let key = self.prop_key(true)?;
                        if self.at_punct("*") {
                            self.next();
                        }
                        self.expect_punct("(")?;
                        let params = self.params()?;
                        self.expect_punct(")")?;
                        self.expect_punct("{")?;
                        let body = self.block_body()?;
                        props.push(ObjProp {
                            key: key.clone(),
                            val: PropVal::Method(Rc::new(FnDef {
                                name: prop_key_name(&key),
                                params,
                                body,
                                is_arrow: false,
                                arrow_expr: None,
                                is_async: true,
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
                        let body = self.block_body()?;
                        props.push(ObjProp {
                            key: key.clone(),
                            val: PropVal::Method(Rc::new(FnDef {
                                name: prop_key_name(&key),
                                params,
                                body,
                                is_arrow: false,
                                arrow_expr: None,
                                is_async: false,
                            })),
                        });
                        self.eat_punct(",");
                        continue;
                    }
                    let key = self.prop_key(true)?;
                    let val = if self.eat_punct(":") {
                        PropVal::Expr(self.assign_expr()?)
                    } else if self.at_punct("(") {
                        self.next();
                        let params = self.params()?;
                        self.expect_punct(")")?;
                        self.expect_punct("{")?;
                        let body = self.block_body()?;
                        PropVal::Method(Rc::new(FnDef {
                            name: prop_key_name(&key),
                            params,
                            body,
                            is_arrow: false,
                            arrow_expr: None,
                            is_async: false,
                        }))
                    } else {
                        // shorthand / shorthand with default (destructuring pos)
                        match &key {
                            PropKey::Ident(s) | PropKey::Str(s) => {
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
                Ok(Expr::Object(props))
            }
            _ => Err(self.err(format!("unexpected {}", self.describe()))),
        }
    }
}

fn prop_key_name(k: &PropKey) -> Option<String> {
    match k {
        PropKey::Ident(s) | PropKey::Str(s) => Some(s.clone()),
        PropKey::Num(n) => Some(n.to_string()),
        PropKey::Computed(_) => None,
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
