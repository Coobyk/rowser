//! AST for the rowser-js interpreter.

use std::rc::Rc;

#[derive(Debug, Clone)]
pub enum Expr {
    Num(f64),
    /// bigint literal `123n` — the literal text, parsed at eval
    BigInt(Rc<str>),
    Str(String),
    Bool(bool),
    Null,
    Ident(String),
    This,
    /// `super` used as `super(args)` or `super.prop` / `super.prop(...)`.
    Super,
    /// regex literal `/pattern/flags`
    Regex {
        pattern: String,
        flags: String,
    },
    Template(Vec<TemplPart>),
    /// a tagged-template call-site object — evaluating it yields the
    /// realm-cached frozen template array for this parse node
    /// (GetTemplateObject); only ever appears as the first argument of
    /// a tagged-template `Call`
    TemplateObj(Rc<TemplateSite>),
    Array(Vec<ArrElem>),
    Object(Vec<ObjProp>),
    /// object literal containing a duplicate `__proto__` data prop —
    /// the early error only applies if it is NOT reinterpreted as a
    /// destructuring assignment target, so validation is deferred
    ObjDup(Vec<ObjProp>),
    Fn(Rc<FnDef>),
    Class(Rc<ClassDef>),
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Update {
        /// true = ++, false = --
        inc: bool,
        prefix: bool,
        target: Box<Expr>,
    },
    Binary {
        op: BinOp,
        l: Box<Expr>,
        r: Box<Expr>,
    },
    /// `&&` / `||` / `??` — short-circuit.
    Logical {
        op: LogOp,
        l: Box<Expr>,
        r: Box<Expr>,
    },
    Assign {
        op: AssignOp,
        target: Box<Expr>,
        value: Box<Expr>,
    },
    Cond {
        c: Box<Expr>,
        t: Box<Expr>,
        f: Box<Expr>,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Arg>,
    },
    Member {
        obj: Box<Expr>,
        prop: Box<Expr>,
        computed: bool,
        optional: bool,
    },
    /// `(expr)` — preserves that the source parenthesized it (cover-
    /// parenthesized targets don't get NamedEvaluation)
    Paren(Box<Expr>),
    New {
        callee: Box<Expr>,
        args: Vec<Arg>,
    },
    Seq(Vec<Expr>),
}

#[derive(Debug, Clone)]
pub enum TemplPart {
    Str(String),
    Expr(Expr),
}

/// One tagged-template literal site — its allocation identity is the
/// spec's [[Site]] parse-node key for the realm [[TemplateMap]] (the
/// interpreter caches the built template object by `Rc` pointer and
/// pins the site in the map so the address can never be reused).
/// `cooked[i]` is `None` where the segment held an invalid escape —
/// the cooked element then reads `undefined`.
#[derive(Debug)]
pub struct TemplateSite {
    pub cooked: Vec<Option<String>>,
    /// raw segment text (TRV — line terminators folded to `\n`)
    pub raw: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum ArrElem {
    Expr(Expr),
    Spread(Expr),
    /// `...expr,` — a trailing comma is legal in a literal but makes
    /// the node unusable as a destructuring pattern
    SpreadTrail(Expr),
    Hole,
}

#[derive(Debug, Clone)]
pub enum Arg {
    Expr(Expr),
    Spread(Expr),
}

#[derive(Debug, Clone)]
pub enum PropKey {
    Ident(String),
    Str(String),
    Num(f64),
    Computed(Expr),
}

#[derive(Debug, Clone)]
pub struct ObjProp {
    pub key: PropKey,
    pub val: PropVal,
}

#[derive(Debug, Clone)]
pub enum PropVal {
    /// `key: expr` or shorthand `key`
    Expr(Expr),
    /// `key() {}` — function def (non-arrow)
    Method(Rc<FnDef>),
    Get(Rc<FnDef>),
    Set(Rc<FnDef>),
    /// `...expr`
    Spread(Expr),
    /// `...expr,` — see ArrElem::SpreadTrail
    SpreadTrail(Expr),
}

#[derive(Debug)]
pub struct FnDef {
    pub name: Option<String>,
    pub params: Vec<Param>,
    pub body: Vec<Stmt>,
    pub is_arrow: bool,
    /// arrow with expression body: `(a) => a + 1`
    pub arrow_expr: Option<Expr>,
    /// parsed `async` — treated as sync for now.
    pub is_async: bool,
    /// `function*` — body runs eagerly at call time, yields collected
    /// into a snapshot iterator (see `Interp::gen_stack`).
    pub is_gen: bool,
    /// strict-mode body: inherited strictness or a "use strict" directive
    /// prologue (class bodies are always strict)
    pub strict: bool,
    /// verbatim source text of this function definition, captured at
    /// parse time — `Function.prototype.toString` returns it untouched.
    /// None for synthesized defs (default ctors, native wrappers).
    pub src: Option<Rc<str>>,
}

#[derive(Debug, Clone)]
pub enum Param {
    Ident(String),
    Rest(Box<Pattern>),
    Default(Box<Pattern>, Expr),
    Pattern(Pattern),
}

#[derive(Debug, Clone)]
pub enum Pattern {
    Ident(String),
    /// `[a, b = 1, , ...rest]`
    Array(Vec<ArrayPatElem>),
    /// `{a, b: x, c = 1, ...rest}`
    Object(Vec<ObjPatElem>),
    /// destructuring-ASSIGNMENT only: a member-expression lvalue
    /// (`[a.b] = arr`, `({x: o.p} = o)`) — evaluated as a reference at
    /// element-assign time. Never produced for declarations.
    Target(Box<Expr>),
}

#[derive(Debug, Clone)]
pub enum ArrayPatElem {
    Hole,
    Elem(Pattern),
    Default(Pattern, Expr),
    Rest(Pattern),
}

#[derive(Debug, Clone)]
pub struct ObjPatElem {
    pub key: PropKey,
    pub val: Pattern,
    pub default: Option<Expr>,
}

#[derive(Debug)]
pub struct ClassDef {
    pub name: Option<String>,
    pub superclass: Option<Expr>,
    pub members: Vec<ClassMember>,
    /// verbatim source text of the whole class definition — the class
    /// object IS its constructor function, whose toString yields the
    /// class source (spec: a class ctor's [[ECMAScriptCode]] is the
    /// class definition)
    pub src: Option<Rc<str>>,
}

#[derive(Debug)]
pub struct ClassMember {
    pub key: PropKey,
    pub kind: MemberKind,
    pub func: Rc<FnDef>,
    pub is_static: bool,
    /// field initializer (kind == Field); static-block bodies ride in
    /// `func.body` (kind == StaticBlock)
    pub init: Option<Expr>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MemberKind {
    Method,
    Getter,
    Setter,
    Ctor,
    /// `x;` / `x = init;` — instance or static field
    Field,
    /// `static { ... }`
    StaticBlock,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnaryOp {
    Neg,
    Pos,
    Not,
    BitNot,
    Typeof,
    Void,
    Delete,
    /// `await` — stub: evaluates operand synchronously.
    Await,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Eq,
    Ne,
    StrictEq,
    StrictNe,
    Lt,
    Le,
    Gt,
    Ge,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    Ushr,
    In,
    Instanceof,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LogOp {
    And,
    Or,
    Nullish,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AssignOp {
    Assign,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    Ushr,
    LogAnd,
    LogOr,
    Nullish,
}

#[derive(Debug)]
pub enum Stmt {
    Expr(Expr),
    VarDecl {
        kind: VarKind,
        decls: Vec<(Pattern, Option<Expr>)>,
    },
    FnDecl(Rc<FnDef>),
    ClassDecl(Rc<ClassDef>),
    If {
        c: Expr,
        t: Box<Stmt>,
        e: Option<Box<Stmt>>,
    },
    Block(Vec<Stmt>),
    Return(Option<Expr>),
    Break(Option<String>),
    Continue(Option<String>),
    While {
        c: Expr,
        body: Box<Stmt>,
    },
    DoWhile {
        body: Box<Stmt>,
        c: Expr,
    },
    For {
        init: Option<ForInit>,
        test: Option<Expr>,
        update: Option<Expr>,
        body: Box<Stmt>,
    },
    ForIn {
        left: ForTarget,
        right: Expr,
        body: Box<Stmt>,
    },
    ForOf {
        left: ForTarget,
        right: Expr,
        body: Box<Stmt>,
        /// `for await (x of y)` — async iteration inside an async ctx
        is_await: bool,
    },
    Switch {
        disc: Expr,
        cases: Vec<SwitchCase>,
    },
    Try {
        block: Vec<Stmt>,
        param: Option<Pattern>,
        handler: Option<Vec<Stmt>>,
        finally: Option<Vec<Stmt>>,
    },
    Throw(Expr),
    Labeled(String, Box<Stmt>),
    /// `with (obj) body` — object environment record (sloppy only)
    With {
        obj: Expr,
        body: Box<Stmt>,
    },
    /// `import ...` — recorded, not resolved.
    Import(ImportRec),
    /// `export` — inner decl executes, exports recorded.
    Export(ExportRec),
    Empty,
}

#[derive(Debug)]
pub struct SwitchCase {
    /// None = default
    pub test: Option<Expr>,
    pub body: Vec<Stmt>,
}

#[derive(Debug)]
pub enum ForInit {
    Var(VarKind, Vec<(Pattern, Option<Expr>)>),
    Expr(Expr),
}

#[derive(Debug)]
pub enum ForTarget {
    Var(VarKind, Pattern),
    Expr(Expr),
}

#[derive(Debug, Clone, Copy)]
pub enum BindMode {
    Decl(VarKind),
    Assign,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VarKind {
    Var,
    Let,
    Const,
}

#[derive(Debug)]
pub struct ImportRec {
    /// e.g. "./mod.js"
    pub source: Option<String>,
    /// free-form description of specifiers
    pub specifiers: String,
}

#[derive(Debug)]
pub enum ExportRec {
    /// `export default expr`
    Default(Expr),
    /// `export { a, b as c }` / `export { a } from "x"`
    Names(Vec<(String, Option<String>)>, Option<String>),
    /// `export const x = ...` / `export function f` / `export class C`
    Decl(Box<Stmt>),
    /// `export * from "x"`
    All(String),
}

// ---- static-semantics walks -----------------------------------------------

/// spec `Contains AwaitExpression` over a parameter — `await` used as a
/// BINDING name is a separate early error and not included here.
/// Does not cross nested function/class boundaries — a nested
/// function's awaits belong to its own parse.
pub fn param_contains_await(p: &Param) -> bool {
    match p {
        Param::Ident(_) => false,
        Param::Rest(p) => pat_contains_await(p),
        Param::Pattern(p) => pat_contains_await(p),
        Param::Default(p, e) => pat_contains_await(p) || expr_contains_await(e),
    }
}

pub fn pat_contains_await(p: &Pattern) -> bool {
    match p {
        Pattern::Ident(_) => false,
        Pattern::Target(e) => expr_contains_await(e),
        Pattern::Array(elems) => elems.iter().any(|el| match el {
            ArrayPatElem::Hole => false,
            ArrayPatElem::Elem(p) | ArrayPatElem::Rest(p) => pat_contains_await(p),
            ArrayPatElem::Default(p, e) => pat_contains_await(p) || expr_contains_await(e),
        }),
        Pattern::Object(elems) => elems.iter().any(|el| {
            matches!(&el.key, PropKey::Computed(e) if expr_contains_await(e))
                || pat_contains_await(&el.val)
                || el.default.as_ref().is_some_and(expr_contains_await)
        }),
    }
}

pub fn expr_contains_await(e: &Expr) -> bool {
    match e {
        Expr::Unary {
            op: UnaryOp::Await,
            ..
        } => true,
        // function/class boundaries — nested awaits are theirs
        Expr::Fn(_) | Expr::Class(_) => false,
        Expr::Template(parts) => parts
            .iter()
            .any(|p| matches!(p, TemplPart::Expr(e) if expr_contains_await(e))),
        Expr::Array(elems) => elems.iter().any(|el| match el {
            ArrElem::Hole => false,
            ArrElem::Expr(e) | ArrElem::Spread(e) | ArrElem::SpreadTrail(e) => {
                expr_contains_await(e)
            }
        }),
        Expr::Object(props) | Expr::ObjDup(props) => props.iter().any(|p| {
            matches!(&p.key, PropKey::Computed(e) if expr_contains_await(e))
                || match &p.val {
                    PropVal::Expr(e) | PropVal::Spread(e) | PropVal::SpreadTrail(e) => {
                        expr_contains_await(e)
                    }
                    // methods/getters/setters are nested functions
                    _ => false,
                }
        }),
        Expr::Unary { expr, .. } => expr_contains_await(expr),
        Expr::Update { target, .. } => expr_contains_await(target),
        Expr::Binary { l, r, .. } | Expr::Logical { l, r, .. } => {
            expr_contains_await(l) || expr_contains_await(r)
        }
        Expr::Assign { target, value, .. } => {
            expr_contains_await(target) || expr_contains_await(value)
        }
        Expr::Cond { c, t, f } => {
            expr_contains_await(c) || expr_contains_await(t) || expr_contains_await(f)
        }
        Expr::Call { callee, args } | Expr::New { callee, args } => {
            expr_contains_await(callee)
                || args.iter().any(|a| match a {
                    Arg::Expr(e) | Arg::Spread(e) => expr_contains_await(e),
                })
        }
        Expr::Member {
            obj,
            prop,
            computed,
            ..
        } => expr_contains_await(obj) || (*computed && expr_contains_await(prop)),
        Expr::Paren(e) => expr_contains_await(e),
        Expr::Seq(es) => es.iter().any(expr_contains_await),
        _ => false,
    }
}
