//! AST for the rowser-js interpreter.

use std::rc::Rc;

#[derive(Debug, Clone)]
pub enum Expr {
    Num(f64),
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
    Array(Vec<ArrElem>),
    Object(Vec<ObjProp>),
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

#[derive(Debug, Clone)]
pub enum ArrElem {
    Expr(Expr),
    Spread(Expr),
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
}

#[derive(Debug)]
pub struct ClassMember {
    pub key: PropKey,
    pub kind: MemberKind,
    pub func: Rc<FnDef>,
    pub is_static: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MemberKind {
    Method,
    Getter,
    Setter,
    Ctor,
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
