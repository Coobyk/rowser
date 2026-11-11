//! Tree-walking evaluator.

use crate::ast::{self, *};
use crate::{parser, Error};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

pub const SYM_ITERATOR: u64 = 1;
pub const SYM_TOPRIMITIVE: u64 = 2;
pub const SYM_TOSTRINGTAG: u64 = 3;
pub const SYM_HASINSTANCE: u64 = 4;
pub const SYM_MATCH: u64 = 5;
pub const SYM_REPLACE: u64 = 6;
pub const SYM_SPLIT: u64 = 7;
pub const SYM_SEARCH: u64 = 8;
pub const SYM_MATCHALL: u64 = 9;
const REST_PAT_KEY: &str = "\u{0}rest";

// ---------------------------------------------------------------------------
// Values & objects
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub enum Value {
    Und,
    Null,
    Bool(bool),
    Num(f64),
    Str(Rc<str>),
    /// (unique id, description)
    Sym(u64, Rc<str>),
    Obj(ObjRef),
}

pub type ObjRef = Rc<RefCell<Obj>>;

#[derive(Clone, PartialEq, Eq, Hash)]
pub enum PropKey {
    S(Rc<str>),
    Sym(u64),
}

impl PropKey {
    pub fn from_str(s: &str) -> PropKey {
        PropKey::S(Rc::from(s))
    }
}

#[derive(Clone)]
pub struct Prop {
    pub val: Value,
    pub writable: bool,
    pub enumerable: bool,
    pub configurable: bool,
    pub get: Option<Value>,
    pub set: Option<Value>,
}

impl Prop {
    pub fn data(v: Value) -> Prop {
        Prop {
            val: v,
            writable: true,
            enumerable: true,
            configurable: true,
            get: None,
            set: None,
        }
    }
    pub fn builtin(v: Value) -> Prop {
        Prop {
            val: v,
            writable: true,
            enumerable: false,
            configurable: true,
            get: None,
            set: None,
        }
    }
    pub fn accessor(get: Option<Value>, set: Option<Value>) -> Prop {
        Prop {
            val: Value::Und,
            writable: false,
            enumerable: false,
            configurable: true,
            get,
            set,
        }
    }
    pub fn is_accessor(&self) -> bool {
        self.get.is_some() || self.set.is_some()
    }
}

/// Insertion-ordered property map (JS preserves key insertion order).
#[derive(Default)]
pub struct PropMap {
    map: HashMap<PropKey, Prop>,
    order: Vec<PropKey>,
}

impl PropMap {
    pub fn new() -> PropMap {
        PropMap::default()
    }
    pub fn get(&self, k: &PropKey) -> Option<&Prop> {
        self.map.get(k)
    }
    pub fn get_mut(&mut self, k: &PropKey) -> Option<&mut Prop> {
        self.map.get_mut(k)
    }
    pub fn insert(&mut self, k: PropKey, p: Prop) {
        if !self.map.contains_key(&k) {
            self.order.push(k.clone());
        }
        self.map.insert(k, p);
    }
    pub fn remove(&mut self, k: &PropKey) -> Option<Prop> {
        let r = self.map.remove(k);
        if r.is_some() {
            self.order.retain(|x| x != k);
        }
        r
    }
    pub fn contains_key(&self, k: &PropKey) -> bool {
        self.map.contains_key(k)
    }
    /// iteration in insertion order
    pub fn iter(&self) -> impl Iterator<Item = (&PropKey, &Prop)> + '_ {
        self.order.iter().filter_map(move |k| self.map.get_key_value(k))
    }
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut Prop> + '_ {
        self.map.values_mut()
    }
    pub fn or_insert_with(&mut self, k: PropKey, f: impl FnOnce() -> Prop) -> &mut Prop {
        if !self.map.contains_key(&k) {
            self.order.push(k.clone());
        }
        self.map.entry(k).or_insert_with(f)
    }
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

pub struct Obj {
    pub proto: Option<ObjRef>,
    pub props: PropMap,
    pub kind: ObjKind,
}

#[derive(Clone)]
pub enum CtorKind {
    /// ordinary function — allocates `this` on `new`
    NotClass,
    /// class ctor w/o extends
    Base,
    /// class ctor w/ extends — `this` allocated by super()
    Derived,
}

#[derive(Clone)]
pub enum Func {
    User {
        def: Rc<FnDef>,
        env: EnvRef,
        /// home object for `super.prop` lookups (methods)
        home: Option<ObjRef>,
        ctor: CtorKind,
    },
    Native {
        name: Rc<str>,
        f: NativeFn,
    },
}

pub type NativeFn = fn(&mut Interp, &Value, &[Value]) -> Result<Value, Value>;

#[derive(Clone)]
pub enum ObjKind {
    Ord,
    Arr(Vec<Value>),
    Func(Func),
    /// bound function: target, bound-this, bound leading args
    Bound {
        target: Value,
        thisv: Value,
        extra: Vec<Value>,
    },
    Err,
    Promise(PromiseState),
    /// materialized iterator for builtin keys()/values()/entries() etc.
    Iter {
        items: Vec<Value>,
        idx: usize,
    },
    /// opaque host data bag (dates etc.)
    Host(Rc<str>),
    Map(Vec<(Value, Value)>),
    Set(Vec<Value>),
    /// compiled regular expression
    Regex(Rc<crate::regexp::Regex>),
}

#[derive(Clone)]
pub enum PromiseState {
    Pending,
    Fulfilled(Box<Value>),
    Rejected(Box<Value>),
}

impl Obj {
    pub fn new(proto: Option<ObjRef>, kind: ObjKind) -> Obj {
        Obj {
            proto,
            props: PropMap::new(),
            kind,
        }
    }
    pub fn is_callable(&self) -> bool {
        matches!(self.kind, ObjKind::Func(_) | ObjKind::Bound { .. })
    }
}

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

pub struct Binding {
    pub val: Value,
    pub mutable: bool,
}

pub struct Scope {
    pub vars: RefCell<HashMap<Rc<str>, Binding>>,
    pub parent: Option<EnvRef>,
    /// function scope: `this`/arguments/new.target/super boundary
    pub is_fn: bool,
    pub thisv: RefCell<Value>,
    pub new_target: RefCell<Value>,
    /// set on method scopes for `super.prop`
    pub home: RefCell<Option<ObjRef>>,
    /// set on ctor scopes for `super()`
    pub ctor_fn: RefCell<Value>,
}

pub type EnvRef = Rc<Scope>;

impl Scope {
    pub fn new(parent: Option<EnvRef>, is_fn: bool) -> EnvRef {
        Rc::new(Scope {
            vars: RefCell::new(HashMap::new()),
            parent,
            is_fn,
            thisv: RefCell::new(Value::Und),
            new_target: RefCell::new(Value::Und),
            home: RefCell::new(None),
            ctor_fn: RefCell::new(Value::Und),
        })
    }
}

// ---------------------------------------------------------------------------
// Completion / results
// ---------------------------------------------------------------------------

pub enum Completion {
    /// a statement that produced a value
    Normal(Value),
    /// a statement that produced no value (decls, empty stmts) — the
    /// previous completion value is kept, per spec
    Empty,
    Break(Option<String>),
    Continue(Option<String>),
    Return(Value),
}

pub type SRes = Result<Completion, Value>;
pub type ERes = Result<Value, Value>;

// ---------------------------------------------------------------------------
// Interpreter
// ---------------------------------------------------------------------------

pub struct Protos {
    pub obj: ObjRef,
    pub func: ObjRef,
    pub array: ObjRef,
    pub string: ObjRef,
    pub number: ObjRef,
    pub boolean: ObjRef,
    pub symbol: ObjRef,
    pub error: ObjRef,
    pub type_error: ObjRef,
    pub range_error: ObjRef,
    pub syntax_error: ObjRef,
    pub ref_error: ObjRef,
    pub promise: ObjRef,
    pub iterator: ObjRef,
    pub regexp: ObjRef,
    pub global: ObjRef,
}

pub struct Interp {
    pub global: EnvRef,
    pub p: Protos,
    /// collected console output
    pub output: Vec<String>,
    /// recorded import/export specifiers
    pub modules: Vec<String>,
    pub labels: Vec<String>,
    pub depth: usize,
    pub sym_counter: u64,
    pub rng: u64,
    /// Embedder-owned state (e.g. the DOM bridge's `DomState`). Native
    /// functions receive `&mut Interp`, so host bindings stash their world
    /// here and downcast. Not touched by the interpreter itself.
    pub ext: Option<Box<dyn std::any::Any>>,
}

/// JS call-depth limit. Each JS call consumes substantial Rust stack in
/// debug builds; embedders should run the interpreter on a thread with a
/// large stack (e.g. 32+ MiB) if deep recursion is expected.
const MAX_DEPTH: usize = 400;

impl Interp {
    pub fn new() -> Interp {
        let mk = |proto: Option<ObjRef>| {
            Rc::new(RefCell::new(Obj::new(proto, ObjKind::Ord)))
        };
        let obj = mk(None);
        let func = mk(Some(obj.clone()));
        Interp {
            global: Scope::new(None, true),
            p: Protos {
                array: mk(Some(obj.clone())),
                string: mk(Some(obj.clone())),
                number: mk(Some(obj.clone())),
                boolean: mk(Some(obj.clone())),
                symbol: mk(Some(obj.clone())),
                error: mk(Some(obj.clone())),
                type_error: mk(Some(obj.clone())),
                range_error: mk(Some(obj.clone())),
                syntax_error: mk(Some(obj.clone())),
                ref_error: mk(Some(obj.clone())),
                promise: mk(Some(obj.clone())),
                iterator: mk(Some(obj.clone())),
                regexp: mk(Some(obj.clone())),
                global: mk(Some(obj.clone())),
                obj,
                func,
            },
            output: Vec::new(),
            modules: Vec::new(),
            labels: Vec::new(),
            depth: 0,
            sym_counter: 16,
            rng: {
                let n = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0x9e3779b97f4a7c15);
                n ^ 0x9e3779b97f4a7c15
            },
            ext: None,
        }
    }

    /// xorshift64 for Math.random (not crypto-grade, spec-fine).
    pub fn next_random(&mut self) -> f64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        (x >> 11) as f64 / 9007199254740992.0
    }

    // -- error helpers ------------------------------------------------------
    pub fn err_obj(&mut self, proto: &ObjRef, name: &str, msg: &str) -> Value {
        let o = Rc::new(RefCell::new(Obj::new(Some(proto.clone()), ObjKind::Err)));
        o.borrow_mut().props.insert(
            PropKey::from_str("message"),
            Prop::data(Value::Str(Rc::from(msg))),
        );
        o.borrow_mut().props.insert(
            PropKey::from_str("name"),
            Prop::data(Value::Str(Rc::from(name))),
        );
        o.borrow_mut().props.insert(
            PropKey::from_str("stack"),
            Prop::data(Value::Str(Rc::from(format!("{name}: {msg}")))),
        );
        Value::Obj(o)
    }
    pub fn type_err(&mut self, msg: impl Into<String>) -> Value {
        let p = self.p.type_error.clone();
        self.err_obj(&p, "TypeError", &msg.into())
    }
    pub fn ref_err(&mut self, msg: impl Into<String>) -> Value {
        let p = self.p.ref_error.clone();
        self.err_obj(&p, "ReferenceError", &msg.into())
    }
    pub fn range_err(&mut self, msg: impl Into<String>) -> Value {
        let p = self.p.range_error.clone();
        self.err_obj(&p, "RangeError", &msg.into())
    }
    pub fn syntax_err(&mut self, msg: impl Into<String>) -> Value {
        let p = self.p.syntax_error.clone();
        self.err_obj(&p, "SyntaxError", &msg.into())
    }
    pub fn plain_err(&mut self, msg: impl Into<String>) -> Value {
        let p = self.p.error.clone();
        self.err_obj(&p, "Error", &msg.into())
    }

    // -- env ops ------------------------------------------------------------
    pub fn env_lookup(&self, env: &EnvRef, name: &str) -> Option<Value> {
        let mut cur = Some(env.clone());
        while let Some(e) = cur {
            if let Some(b) = e.vars.borrow().get(name) {
                return Some(b.val.clone());
            }
            cur = e.parent.clone();
        }
        // fall back to global object properties (globalThis.x = …)
        if let Some(v) = self.obj_get_raw(&self.p.global.clone(), &PropKey::from_str(name)) {
            return Some(v);
        }
        None
    }

    pub fn env_assign(&mut self, env: &EnvRef, name: &str, v: Value) -> Result<(), Value> {
        let mut cur = Some(env.clone());
        while let Some(e) = cur {
            if let Some(b) = e.vars.borrow_mut().get_mut(name) {
                if !b.mutable {
                    return Err(self.type_err(format!("Assignment to constant variable '{name}'")));
                }
                b.val = v;
                return Ok(());
            }
            cur = e.parent.clone();
        }
        // sloppy: create global; mirror onto global object
        self.global
            .vars
            .borrow_mut()
            .insert(Rc::from(name), Binding { val: v.clone(), mutable: true });
        self.p.global.borrow_mut().props.insert(
            PropKey::from_str(name),
            Prop::data(v),
        );
        Ok(())
    }

    /// declare in the given scope (let/const/block fns)
    pub fn declare(&mut self, env: &EnvRef, name: &str, v: Value, mutable: bool) {
        env.vars
            .borrow_mut()
            .insert(Rc::from(name), Binding { val: v.clone(), mutable });
        if Rc::ptr_eq(env, &self.global) {
            self.p.global.borrow_mut().props.insert(
                PropKey::from_str(name),
                Prop::data(v),
            );
        }
    }

    /// `var` — walks to nearest function scope.
    pub fn declare_var(&mut self, env: &EnvRef, name: &str, v: Value) {
        let mut cur = env.clone();
        loop {
            if cur.is_fn || cur.parent.is_none() {
                break;
            }
            let next = cur.parent.clone().unwrap();
            cur = next;
        }
        let exists = cur.vars.borrow().contains_key(name);
        if !exists {
            cur.vars.borrow_mut().insert(
                Rc::from(name),
                Binding {
                    val: v.clone(),
                    mutable: true,
                },
            );
        } else if !matches!(v, Value::Und) {
            cur.vars.borrow_mut().get_mut(name).unwrap().val = v.clone();
        }
        if Rc::ptr_eq(&cur, &self.global) {
            self.p.global.borrow_mut().props.insert(
                PropKey::from_str(name),
                Prop::data(v),
            );
        }
    }

    fn find_fn_scope(&self, env: &EnvRef) -> EnvRef {
        let mut cur = env.clone();
        loop {
            if cur.is_fn || cur.parent.is_none() {
                return cur;
            }
            let next = cur.parent.clone().unwrap();
            cur = next;
        }
    }

    fn find_home(&self, env: &EnvRef) -> Option<(EnvRef, ObjRef)> {
        let mut cur = Some(env.clone());
        while let Some(e) = cur {
            if let Some(h) = e.home.borrow().clone() {
                return Some((e.clone(), h));
            }
            if e.is_fn {
                return None;
            }
            cur = e.parent.clone();
        }
        None
    }

    // -- object helpers -------------------------------------------------------
    pub fn new_obj(&self) -> ObjRef {
        Rc::new(RefCell::new(Obj::new(Some(self.p.obj.clone()), ObjKind::Ord)))
    }
    pub fn new_arr(&self, items: Vec<Value>) -> ObjRef {
        Rc::new(RefCell::new(Obj::new(
            Some(self.p.array.clone()),
            ObjKind::Arr(items),
        )))
    }
    /// Compile `pattern`/`flags` and build a RegExp object.
    pub fn make_regexp(&mut self, pattern: &str, flags: &str) -> ERes {
        match crate::regexp::Regex::compile(pattern, flags) {
            Ok(re) => {
                let o = Rc::new(RefCell::new(Obj::new(
                    Some(self.p.regexp.clone()),
                    ObjKind::Regex(re),
                )));
                o.borrow_mut().props.insert(
                    PropKey::from_str("lastIndex"),
                    Prop::builtin(Value::Num(0.0)),
                );
                Ok(Value::Obj(o))
            }
            Err(e) => Err(
                self.syntax_err(format!("Invalid regular expression: /{pattern}/: {e}"))
            ),
        }
    }
    pub fn set_data(&self, o: &ObjRef, key: &str, v: Value) {
        o.borrow_mut()
            .props
            .insert(PropKey::from_str(key), Prop::data(v));
    }
    pub fn set_builtin(&self, o: &ObjRef, key: &str, v: Value) {
        o.borrow_mut()
            .props
            .insert(PropKey::from_str(key), Prop::builtin(v));
    }
    pub fn native(&self, name: &str, f: NativeFn) -> Value {
        let o = Rc::new(RefCell::new(Obj::new(
            Some(self.p.func.clone()),
            ObjKind::Func(Func::Native {
                name: Rc::from(name),
                f,
            }),
        )));
        self.set_builtin(&o, "length", Value::Num(0.0));
        self.set_builtin(&o, "name", Value::Str(Rc::from(name)));
        Value::Obj(o)
    }

    /// raw own-prop lookup (no proto walk, no getters)
    pub fn obj_get_raw(&self, o: &ObjRef, key: &PropKey) -> Option<Value> {
        if let ObjKind::Arr(items) = &o.borrow().kind {
            if let PropKey::S(s) = key {
                if s.as_ref() == "length" {
                    return Some(Value::Num(items.len() as f64));
                }
                if let Some(i) = arr_index(s) {
                    return Some(items.get(i).cloned().unwrap_or(Value::Und));
                }
            }
        }
        o.borrow().props.get(key).map(|p| p.val.clone())
    }

    pub fn is_callable(&self, v: &Value) -> bool {
        matches!(v, Value::Obj(o) if o.borrow().is_callable())
    }

    // -- property access -------------------------------------------------------
    pub fn get_prop(&mut self, base: &Value, key: &PropKey) -> ERes {
        match base {
            Value::Und | Value::Null => {
                let k = self.key_str(key);
                Err(self.type_err(format!(
                    "Cannot read properties of {} (reading '{k}')",
                    if matches!(base, Value::Und) { "undefined" } else { "null" }
                )))
            }
            Value::Obj(o) => self.obj_get(o, key, base),
            Value::Str(s) => {
                if let PropKey::S(k) = key {
                    if k.as_ref() == "length" {
                        return Ok(Value::Num(s.chars().count() as f64));
                    }
                    if let Some(i) = arr_index(k) {
                        return Ok(s
                            .chars()
                            .nth(i)
                            .map(|c| Value::Str(Rc::from(c.to_string().as_str())))
                            .unwrap_or(Value::Und));
                    }
                }
                let p = self.p.string.clone();
                self.obj_get(&p, key, base)
            }
            Value::Num(_) => {
                let p = self.p.number.clone();
                self.obj_get(&p, key, base)
            }
            Value::Bool(_) => {
                let p = self.p.boolean.clone();
                self.obj_get(&p, key, base)
            }
            Value::Sym(..) => {
                let p = self.p.symbol.clone();
                self.obj_get(&p, key, base)
            }
        }
    }

    /// proto-chain walk; `receiver` is the `this` for getters.
    fn obj_get(&mut self, o: &ObjRef, key: &PropKey, receiver: &Value) -> ERes {
        let mut cur = Some(o.clone());
        while let Some(c) = cur {
            {
                let b = c.borrow();
                if let ObjKind::Arr(items) = &b.kind {
                    if let PropKey::S(s) = key {
                        if s.as_ref() == "length" {
                            return Ok(Value::Num(items.len() as f64));
                        }
                        if let Some(i) = arr_index(s) {
                            return Ok(items.get(i).cloned().unwrap_or(Value::Und));
                        }
                    }
                }
                if let Some(p) = b.props.get(key) {
                    if p.is_accessor() {
                        match &p.get {
                            Some(g) => {
                                let g = g.clone();
                                drop(b);
                                return self.call(&g, receiver.clone(), Vec::new());
                            }
                            None => return Ok(Value::Und),
                        }
                    }
                    return Ok(p.val.clone());
                }
            }
            let next = c.borrow().proto.clone();
            cur = next;
        }
        Ok(Value::Und)
    }

    pub fn set_prop(&mut self, base: &Value, key: PropKey, v: Value) -> Result<(), Value> {
        match base {
            Value::Und | Value::Null => {
                let k = self.key_str(&key);
                Err(self.type_err(format!(
                    "Cannot set properties of {} (setting '{k}')",
                    if matches!(base, Value::Und) { "undefined" } else { "null" }
                )))
            }
            Value::Obj(o) => self.obj_set(o, key, v, base),
            _ => {
                // primitive — check proto chain for setter, else silently ignore
                let proto = match base {
                    Value::Str(_) => Some(self.p.string.clone()),
                    Value::Num(_) => Some(self.p.number.clone()),
                    Value::Bool(_) => Some(self.p.boolean.clone()),
                    Value::Sym(..) => Some(self.p.symbol.clone()),
                    _ => None,
                };
                let mut cur = proto;
                while let Some(c) = cur {
                    if let Some(p) = c.borrow().props.get(&key) {
                        if let Some(s) = &p.set {
                            let s = s.clone();
                            return self.call(&s, base.clone(), vec![v]).map(|_| ());
                        }
                        return Ok(()); // non-setter found → sloppy no-op
                    }
                    cur = c.borrow().proto.clone();
                }
                Ok(())
            }
        }
    }

    fn obj_set(&mut self, o: &ObjRef, key: PropKey, v: Value, receiver: &Value) -> Result<(), Value> {
        // walk chain looking for existing prop (setter or data)
        let mut cur = Some(o.clone());
        while let Some(c) = cur {
            let (found_accessor, found_data, is_own) = {
                let b = c.borrow();
                match b.props.get(&key) {
                    Some(p) if p.is_accessor() => (Some(p.set.clone()), None, false),
                    Some(p) => (None, Some(p.writable), Rc::ptr_eq(&c, o)),
                    None => (None, None, false),
                }
            };
            if let Some(setter) = found_accessor {
                return match setter {
                    Some(s) => self.call(&s, receiver.clone(), vec![v]).map(|_| ()),
                    None => Ok(()),
                };
            }
            if let Some(writable) = found_data {
                if is_own {
                    // update own prop
                    let mut b = c.borrow_mut();
                    if let ObjKind::Arr(items) = &mut b.kind {
                        let _ = items; // handled below via index path before props
                    }
                    if !writable {
                        return Ok(()); // non-writable: sloppy no-op
                    }
                    b.props.get_mut(&key).unwrap().val = v;
                    return Ok(());
                }
                // prop lives on a proto → shadow with own data prop
                break;
            }
            let next = c.borrow().proto.clone();
            cur = next;
        }
        // define own prop
        let mut b = o.borrow_mut();
        if let ObjKind::Arr(items) = &mut b.kind {
            if let PropKey::S(s) = &key {
                if let Some(i) = arr_index(s) {
                    if i >= items.len() {
                        items.resize(i + 1, Value::Und);
                    }
                    items[i] = v;
                    return Ok(());
                }
                if s.as_ref() == "length" {
                    if let Value::Num(n) = v {
                        let n = n.max(0.0) as usize;
                        items.resize(n, Value::Und);
                        return Ok(());
                    }
                }
            }
        }
        b.props.insert(key, Prop::data(v));
        Ok(())
    }

    pub fn has_prop(&mut self, base: &Value, key: &PropKey) -> bool {
        let Value::Obj(o) = base else {
            return false;
        };
        let mut cur = Some(o.clone());
        while let Some(c) = cur {
            {
                let b = c.borrow();
                if let ObjKind::Arr(items) = &b.kind {
                    if let PropKey::S(s) = key {
                        if s.as_ref() == "length" {
                            return true;
                        }
                        if let Some(i) = arr_index(s) {
                            if i < items.len() {
                                return true;
                            }
                        }
                    }
                }
                if b.props.contains_key(key) {
                    return true;
                }
            }
            cur = c.borrow().proto.clone();
        }
        false
    }

    pub fn delete_prop(&mut self, base: &Value, key: &PropKey) -> bool {
        let Value::Obj(o) = base else {
            return true;
        };
        let mut b = o.borrow_mut();
        if let ObjKind::Arr(items) = &mut b.kind {
            if let PropKey::S(s) = key {
                if let Some(i) = arr_index(s) {
                    if i < items.len() {
                        items[i] = Value::Und;
                        return true;
                    }
                }
            }
        }
        b.props.remove(key);
        true
    }

    /// enumerable own + inherited string keys (for..in), deduped
    pub fn enum_keys(&self, base: &Value) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        if let Value::Str(s) = base {
            for i in 0..s.chars().count() {
                out.push(i.to_string());
            }
            return out;
        }
        let Value::Obj(o) = base else {
            return out;
        };
        let mut cur = Some(o.clone());
        while let Some(c) = cur {
            {
                let b = c.borrow();
                if let ObjKind::Arr(items) = &b.kind {
                    for i in 0..items.len() {
                        let k = i.to_string();
                        if seen.insert(k.clone()) {
                            out.push(k);
                        }
                    }
                }
                for (k, p) in b.props.iter() {
                    if !p.enumerable {
                        continue;
                    }
                    if let PropKey::S(s) = k {
                        if seen.insert(s.to_string()) {
                            out.push(s.to_string());
                        }
                    }
                }
            }
            cur = c.borrow().proto.clone();
        }
        out
    }

    /// enumerable own string keys
    pub fn own_keys(&self, base: &Value) -> Vec<String> {
        let mut out = Vec::new();
        let Value::Obj(o) = base else {
            return out;
        };
        let b = o.borrow();
        if let ObjKind::Arr(items) = &b.kind {
            for i in 0..items.len() {
                out.push(i.to_string());
            }
        }
        for (k, p) in b.props.iter() {
            if !p.enumerable {
                continue;
            }
            if let PropKey::S(s) = k {
                out.push(s.to_string());
            }
        }
        out
    }

    pub fn key_str(&self, k: &PropKey) -> String {
        match k {
            PropKey::S(s) => s.to_string(),
            PropKey::Sym(id) => format!("Symbol(#{id})"),
        }
    }

    // -- coercions ---------------------------------------------------------------
    pub fn to_bool(&self, v: &Value) -> bool {
        match v {
            Value::Und | Value::Null => false,
            Value::Bool(b) => *b,
            Value::Num(n) => *n != 0.0 && !n.is_nan(),
            Value::Str(s) => !s.is_empty(),
            Value::Sym(..) | Value::Obj(_) => true,
        }
    }

    pub fn to_primitive(&mut self, v: &Value) -> ERes {
        let Value::Obj(o) = v else {
            return Ok(v.clone());
        };
        // Symbol.toPrimitive
        let stp = self.get_prop(v, &PropKey::Sym(SYM_TOPRIMITIVE))?;
        if self.is_callable(&stp) {
            let hint = Value::Str(Rc::from("default"));
            let r = self.call(&stp, v.clone(), vec![hint])?;
            if !matches!(r, Value::Obj(_)) {
                return Ok(r);
            }
            return Err(self.type_err("Cannot convert object to primitive value"));
        }
        // valueOf then toString (default hint)
        for name in ["valueOf", "toString"] {
            let m = self.get_prop(v, &PropKey::from_str(name))?;
            if self.is_callable(&m) {
                let r = self.call(&m, v.clone(), Vec::new())?;
                if !matches!(r, Value::Obj(_)) {
                    return Ok(r);
                }
            }
        }
        let _ = o;
        Err(self.type_err("Cannot convert object to primitive value"))
    }

    pub fn to_num(&mut self, v: &Value) -> Result<f64, Value> {
        Ok(match v {
            Value::Und => f64::NAN,
            Value::Null => 0.0,
            Value::Bool(b) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            Value::Num(n) => *n,
            Value::Str(s) => js_str_to_num(s),
            Value::Sym(..) => return Err(self.type_err("Cannot convert a Symbol value to a number")),
            Value::Obj(_) => {
                let p = self.to_primitive(v)?;
                self.to_num(&p)?
            }
        })
    }

    pub fn to_str(&mut self, v: &Value) -> Result<String, Value> {
        Ok(match v {
            Value::Und => "undefined".into(),
            Value::Null => "null".into(),
            Value::Bool(b) => b.to_string(),
            Value::Num(n) => js_num_str(*n),
            Value::Str(s) => s.to_string(),
            Value::Sym(_, d) => format!("Symbol({d})"),
            Value::Obj(o) => {
                if let ObjKind::Arr(items) = &o.borrow().kind {
                    let mut parts = Vec::new();
                    for i in items.clone() {
                        match i {
                            Value::Und | Value::Null => parts.push(String::new()),
                            other => parts.push(self.to_str(&other)?),
                        }
                    }
                    return Ok(parts.join(","));
                }
                let p = self.to_primitive(v)?;
                self.to_str(&p)?
            }
        })
    }

    pub fn to_propkey(&mut self, v: &Value) -> Result<PropKey, Value> {
        Ok(match v {
            Value::Sym(id, _) => PropKey::Sym(*id),
            Value::Num(n) => PropKey::from_str(&js_num_str(*n)),
            Value::Str(s) => PropKey::S(s.clone()),
            _ => {
                let s = self.to_str(v)?;
                PropKey::from_str(&s)
            }
        })
    }

    pub fn same_value(&mut self, a: &Value, b: &Value) -> Result<bool, Value> {
        use Value::*;
        Ok(match (a, b) {
            (Und, Und) | (Null, Null) => true,
            (Und, Null) | (Null, Und) => false,
            (Bool(x), Bool(y)) => x == y,
            (Num(x), Num(y)) => x == y,
            (Str(x), Str(y)) => x == y,
            (Sym(x, _), Sym(y, _)) => x == y,
            (Obj(x), Obj(y)) => Rc::ptr_eq(x, y),
            _ => false,
        })
    }

    fn loose_eq(&mut self, a: &Value, b: &Value) -> Result<bool, Value> {
        use Value::*;
        Ok(match (a, b) {
            (Und, Und) | (Null, Null) => true,
            (Und, Null) | (Null, Und) => true,
            (Num(x), Num(y)) => x == y,
            (Str(x), Str(y)) => x == y,
            (Bool(x), Bool(y)) => x == y,
            (Sym(x, _), Sym(y, _)) => x == y,
            (Obj(x), Obj(y)) => Rc::ptr_eq(x, y),
            (Num(_), Str(_)) => {
                let bn = self.to_num(b)?;
                self.loose_eq(a, &Num(bn))?
            }
            (Str(_), Num(_)) => {
                let an = self.to_num(a)?;
                self.loose_eq(&Num(an), b)?
            }
            (Bool(_), _) => {
                let an = self.to_num(a)?;
                self.loose_eq(&Num(an), b)?
            }
            (_, Bool(_)) => {
                let bn = self.to_num(b)?;
                self.loose_eq(a, &Num(bn))?
            }
            (Obj(_), Num(_)) | (Obj(_), Str(_)) => {
                let ap = self.to_primitive(a)?;
                self.loose_eq(&ap, b)?
            }
            (Num(_), Obj(_)) | (Str(_), Obj(_)) => {
                let bp = self.to_primitive(b)?;
                self.loose_eq(a, &bp)?
            }
            _ => false,
        })
    }

    fn strict_eq(&self, a: &Value, b: &Value) -> bool {
        use Value::*;
        match (a, b) {
            (Und, Und) | (Null, Null) => true,
            (Bool(x), Bool(y)) => x == y,
            (Num(x), Num(y)) => x == y, // NaN != NaN naturally
            (Str(x), Str(y)) => x == y,
            (Sym(x, _), Sym(y, _)) => x == y,
            (Obj(x), Obj(y)) => Rc::ptr_eq(x, y),
            _ => false,
        }
    }

    pub fn type_name(&self, v: &Value) -> &'static str {
        match v {
            Value::Und => "undefined",
            Value::Null => "object",
            Value::Bool(_) => "boolean",
            Value::Num(_) => "number",
            Value::Str(_) => "string",
            Value::Sym(..) => "symbol",
            Value::Obj(o) => {
                if o.borrow().is_callable() {
                    "function"
                } else {
                    "object"
                }
            }
        }
    }

    pub fn inspect(&mut self, v: &Value, depth: usize) -> String {
        match v {
            Value::Und => "undefined".into(),
            Value::Null => "null".into(),
            Value::Bool(b) => b.to_string(),
            Value::Num(n) => js_num_str(*n),
            Value::Str(s) => s.to_string(),
            Value::Sym(_, d) => format!("Symbol({d})"),
            Value::Obj(o) => {
                let kind = o.borrow().kind.clone();
                match kind {
                    ObjKind::Arr(items) => {
                        if depth > 3 {
                            return "[…]".into();
                        }
                        let inner: Vec<String> = items
                            .iter()
                            .map(|i| self.inspect(i, depth + 1))
                            .collect();
                        format!("[{}]", inner.join(", "))
                    }
                    ObjKind::Func(Func::User { def, .. }) => {
                        format!("[Function {}]", def.name.clone().unwrap_or_default())
                    }
                    ObjKind::Func(Func::Native { name, .. }) => {
                        format!("[Function {name}]")
                    }
                    ObjKind::Bound { .. } => "[Function bound]".into(),
                    ObjKind::Err => {
                        let b = o.borrow();
                        let name = b
                            .props
                            .get(&PropKey::from_str("name"))
                            .map(|p| match &p.val {
                                Value::Str(s) => s.to_string(),
                                _ => "Error".into(),
                            })
                            .unwrap_or_else(|| "Error".into());
                        let msg = b
                            .props
                            .get(&PropKey::from_str("message"))
                            .map(|p| match &p.val {
                                Value::Str(s) => s.to_string(),
                                _ => String::new(),
                            })
                            .unwrap_or_default();
                        format!("{name}: {msg}")
                    }
                    ObjKind::Promise(st) => match st {
                        PromiseState::Pending => "Promise { <pending> }".into(),
                        PromiseState::Fulfilled(_) => "Promise { <fulfilled> }".into(),
                        PromiseState::Rejected(_) => "Promise { <rejected> }".into(),
                    },
                    ObjKind::Regex(re) => {
                        let src = if re.source.is_empty() {
                            "(?:)".to_string()
                        } else {
                            re.source.clone()
                        };
                        format!("/{src}/{}", re.flags)
                    }
                    ObjKind::Iter { .. } => "[Iterator]".into(),
                    ObjKind::Map(m) => format!("Map({})", m.len()),
                    ObjKind::Set(s) => format!("Set({})", s.len()),
                    ObjKind::Host(tag) => format!("[{tag}]"),
                    ObjKind::Ord => {
                        if depth > 3 {
                            return "{…}".into();
                        }
                        let pairs: Vec<(String, Value)> = o
                            .borrow()
                            .props
                            .iter()
                            .filter(|(_, p)| p.enumerable)
                            .map(|(k, p)| {
                                let ks = match k {
                                    PropKey::S(s) => s.to_string(),
                                    PropKey::Sym(id) => format!("Symbol(#{id})"),
                                };
                                (ks, p.val.clone())
                            })
                            .collect();
                        let parts: Vec<String> = pairs
                            .iter()
                            .map(|(ks, v)| format!("{ks}: {}", self.inspect(v, depth + 1)))
                            .collect();
                        format!("{{{}}}", parts.join(", "))
                    }
                }
            }
        }
    }

    // -- calls -----------------------------------------------------------------
    pub fn call(&mut self, f: &Value, this: Value, args: Vec<Value>) -> ERes {
        if self.depth >= MAX_DEPTH {
            return Err(self.range_err("Maximum call stack size exceeded"));
        }
        let Value::Obj(o) = f else {
            let tn = self.type_name(f);
            return Err(self.type_err(format!("{tn} is not a function")));
        };
        let kind = o.borrow().kind.clone();
        match kind {
            ObjKind::Func(Func::Native { f: nf, name }) => {
                self.depth += 1;
                let r = nf(self, &this, &args);
                self.depth -= 1;
                let _ = name;
                r
            }
            ObjKind::Func(Func::User {
                def,
                env,
                home,
                ctor: _,
            }) => {
                let scope = Scope::new(Some(env), !def.is_arrow);
                if !def.is_arrow {
                    *scope.thisv.borrow_mut() = this;
                    *scope.home.borrow_mut() = home;
                    *scope.ctor_fn.borrow_mut() = f.clone();
                }
                // arrows inherit this/home/new.target lexically — is_fn=false
                // makes lookups walk through automatically.
                self.bind_params(&scope, &def.params, &args)?;
                if !def.is_arrow {
                    let arr = self.new_arr(args);
                    scope.vars.borrow_mut().insert(
                        Rc::from("arguments"),
                        Binding {
                            val: Value::Obj(arr),
                            mutable: true,
                        },
                    );
                }
                self.hoist_body(&def.body, &scope)?;
                if let Some(e) = &def.arrow_expr {
                    // `(x) => expr` — implicit return
                    let e = e.clone();
                    self.depth += 1;
                    let r = self.eval_expr(&e, &scope);
                    self.depth -= 1;
                    return r;
                }
                self.depth += 1;
                let r = self.exec_block(&def.body, &scope);
                self.depth -= 1;
                match r {
                    Ok(Completion::Return(v)) => Ok(v),
                    Ok(_) => Ok(Value::Und),
                    Err(e) => Err(e),
                }
            }
            ObjKind::Bound {
                target,
                thisv,
                extra,
            } => {
                let mut all = extra;
                all.extend(args);
                self.call(&target, thisv, all)
            }
            _ => Err(self.type_err("value is not a function")),
        }
    }

    /// `new f(args)` / class construction.
    /// `new_target` overrides the proto source for derived-class super() calls.
    pub fn construct(&mut self, f: &Value, args: Vec<Value>, new_target: Option<Value>) -> ERes {
        if self.depth >= MAX_DEPTH {
            return Err(self.range_err("Maximum call stack size exceeded"));
        }
        let Value::Obj(o) = f else {
            return Err(self.type_err("value is not a constructor"));
        };
        let kind = o.borrow().kind.clone();
        match kind {
            ObjKind::Func(Func::Native { f: nf, name }) => {
                // natives act as their own ctor (Error, Array(1,2), etc.)
                let proto_prop = self
                    .obj_get_raw(&o.clone(), &PropKey::from_str("prototype"))
                    .unwrap_or(Value::Und);
                let this = if let Value::Obj(proto) = &proto_prop {
                    Value::Obj(Rc::new(RefCell::new(Obj::new(
                        Some(proto.clone()),
                        ObjKind::Ord,
                    ))))
                } else {
                    Value::Und
                };
                self.depth += 1;
                let r = nf(self, &this, &args);
                self.depth -= 1;
                match r {
                    Ok(Value::Und) if !matches!(this, Value::Und) => Ok(this),
                    other => {
                        let _ = name;
                        other
                    }
                }
            }
            ObjKind::Func(Func::User {
                def,
                env,
                home: _,
                ctor,
            }) => {
                if def.is_arrow {
                    return Err(self.type_err("arrow function is not a constructor"));
                }
                let nt = new_target.unwrap_or_else(|| f.clone());
                // prototype for the new object: new_target.prototype or obj proto
                let proto = {
                    let nt_obj = if let Value::Obj(no) = &nt { Some(no.clone()) } else { None };
                    match nt_obj {
                        Some(no) => {
                            let pp = self.obj_get_raw(&no, &PropKey::from_str("prototype"));
                            match pp {
                                Some(Value::Obj(p)) => Some(p),
                                _ => Some(self.p.obj.clone()),
                            }
                        }
                        None => Some(self.p.obj.clone()),
                    }
                };
                let this = match ctor {
                    CtorKind::Derived => Value::Und, // allocated at super()
                    _ => Value::Obj(Rc::new(RefCell::new(Obj::new(
                        proto,
                        ObjKind::Ord,
                    )))),
                };
                let scope = Scope::new(Some(env), true);
                *scope.thisv.borrow_mut() = this;
                *scope.new_target.borrow_mut() = nt;
                *scope.ctor_fn.borrow_mut() = f.clone();
                self.bind_params(&scope, &def.params, &args)?;
                let arr = self.new_arr(args);
                scope.vars.borrow_mut().insert(
                    Rc::from("arguments"),
                    Binding {
                        val: Value::Obj(arr),
                        mutable: true,
                    },
                );
                self.hoist_body(&def.body, &scope)?;
                self.depth += 1;
                let r = self.exec_block(&def.body, &scope);
                self.depth -= 1;
                let this_out = scope.thisv.borrow().clone();
                match r {
                    Ok(Completion::Return(v)) if self.is_callable(&v) || matches!(v, Value::Obj(_)) => {
                        Ok(v)
                    }
                    Ok(Completion::Return(_)) | Ok(_) => {
                        if matches!(ctor, CtorKind::Derived) && matches!(this_out, Value::Und) {
                            Err(self.ref_err(
                                "Must call super constructor in derived class before accessing 'this'",
                            ))
                        } else {
                            Ok(this_out)
                        }
                    }
                    Err(e) => Err(e),
                }
            }
            ObjKind::Bound { target, extra, .. } => {
                let mut all = extra;
                all.extend(args);
                self.construct(&target, all, new_target)
            }
            _ => Err(self.type_err("value is not a constructor")),
        }
    }

    fn bind_params(&mut self, scope: &EnvRef, params: &[Param], args: &[Value]) -> Result<(), Value> {
        let mut i = 0;
        for p in params {
            match p {
                Param::Ident(name) => {
                    let v = args.get(i).cloned().unwrap_or(Value::Und);
                    scope.vars.borrow_mut().insert(
                        Rc::from(name.as_str()),
                        Binding { val: v, mutable: true },
                    );
                    i += 1;
                }
                Param::Default(pat, dflt) => {
                    let v = match args.get(i) {
                        Some(Value::Und) | None => self.eval_expr(dflt, scope)?,
                        Some(v) => v.clone(),
                    };
                    self.bind_pattern(scope, pat, v, VarKind::Let, true)?;
                    i += 1;
                }
                Param::Rest(pat) => {
                    let rest: Vec<Value> = args.iter().skip(i).cloned().collect();
                    let arr = Value::Obj(self.new_arr(rest));
                    self.bind_pattern(scope, pat, arr, VarKind::Let, true)?;
                }
                Param::Pattern(pat) => {
                    let v = args.get(i).cloned().unwrap_or(Value::Und);
                    self.bind_pattern(scope, pat, v, VarKind::Let, true)?;
                    i += 1;
                }
            }
        }
        Ok(())
    }

    /// bind a destructuring/ident pattern to a value.
    /// `declare_in_scope` → let/const semantics in `scope`; otherwise uses
    /// `kind` (var → fn scope). `overwrite` → assignment-mode (env_assign).
    pub fn bind_pattern(
        &mut self,
        scope: &EnvRef,
        pat: &Pattern,
        v: Value,
        kind: VarKind,
        _declare: bool,
    ) -> Result<(), Value> {
        match pat {
            Pattern::Ident(name) => match kind {
                VarKind::Var => self.declare_var(scope, name, v),
                _ => self.declare(scope, name, v, kind != VarKind::Const),
            },
            Pattern::Array(elems) => {
                let items = self.iter_values(&v)?;
                let mut idx = 0;
                for e in elems {
                    match e {
                        ArrayPatElem::Hole => idx += 1,
                        ArrayPatElem::Elem(p) => {
                            let it = items.get(idx).cloned().unwrap_or(Value::Und);
                            self.bind_pattern(scope, p, it, kind, true)?;
                            idx += 1;
                        }
                        ArrayPatElem::Default(p, d) => {
                            let it = match items.get(idx) {
                                Some(Value::Und) | None => self.eval_expr(d, scope)?,
                                Some(v) => v.clone(),
                            };
                            self.bind_pattern(scope, p, it, kind, true)?;
                            idx += 1;
                        }
                        ArrayPatElem::Rest(p) => {
                            let rest: Vec<Value> = items.iter().skip(idx).cloned().collect();
                            let arr = Value::Obj(self.new_arr(rest));
                            self.bind_pattern(scope, p, arr, kind, true)?;
                        }
                    }
                }
            }
            Pattern::Object(elems) => {
                for e in elems {
                    if let ast::PropKey::Str(s) = &e.key {
                        if s.as_str() == REST_PAT_KEY {
                            // `{...rest}` — copy remaining own enumerable props
                            let rest = self.new_obj();
                            if let Value::Obj(src) = &v {
                                let keys = self.own_keys(&v);
                                let taken: HashSet<String> = elems
                                    .iter()
                                    .filter_map(|x| match &x.key {
                                        ast::PropKey::Str(s) | ast::PropKey::Ident(s)
                                            if s.as_str() != REST_PAT_KEY =>
                                        {
                                            Some(s.to_string())
                                        }
                                        _ => None,
                                    })
                                    .collect();
                                for k in keys {
                                    if !taken.contains(&k) {
                                        let val = self.obj_get_raw(src, &PropKey::from_str(&k)).unwrap_or(Value::Und);
                                        self.set_data(&rest, &k, val);
                                    }
                                }
                            }
                            self.bind_pattern(scope, &e.val, Value::Obj(rest), kind, true)?;
                            continue;
                        }
                    }
                    let key = match &e.key {
                        ast::PropKey::Ident(s) | ast::PropKey::Str(s) => PropKey::from_str(s),
                        ast::PropKey::Num(n) => PropKey::from_str(&js_num_str(*n)),
                        ast::PropKey::Computed(ex) => {
                            let kv = self.eval_expr(ex, scope)?;
                            self.to_propkey(&kv)?
                        }
                    };
                    let got = self.get_prop(&v, &key)?;
                    let val = match (&got, &e.default) {
                        (Value::Und, Some(d)) => self.eval_expr(d, scope)?,
                        _ => got,
                    };
                    self.bind_pattern(scope, &e.val, val, kind, true)?;
                }
            }
        }
        Ok(())
    }

    /// assignment-mode destructuring (`[a,b] = arr`, `{x,y} = obj`)
    fn assign_pattern(&mut self, scope: &EnvRef, pat: &Pattern, v: Value) -> Result<(), Value> {
        match pat {
            Pattern::Ident(name) => self.env_assign(scope, name, v),
            Pattern::Array(elems) => {
                let items = self.iter_values(&v)?;
                let mut idx = 0;
                for e in elems {
                    match e {
                        ArrayPatElem::Hole => idx += 1,
                        ArrayPatElem::Elem(p) => {
                            let it = items.get(idx).cloned().unwrap_or(Value::Und);
                            self.assign_pattern(scope, p, it)?;
                            idx += 1;
                        }
                        ArrayPatElem::Default(p, d) => {
                            let it = match items.get(idx) {
                                Some(Value::Und) | None => self.eval_expr(d, scope)?,
                                Some(v) => v.clone(),
                            };
                            self.assign_pattern(scope, p, it)?;
                            idx += 1;
                        }
                        ArrayPatElem::Rest(p) => {
                            let rest: Vec<Value> = items.iter().skip(idx).cloned().collect();
                            let arr = Value::Obj(self.new_arr(rest));
                            self.assign_pattern(scope, p, arr)?;
                        }
                    }
                }
                Ok(())
            }
            Pattern::Object(elems) => {
                for e in elems {
                    if let ast::PropKey::Str(s) = &e.key {
                        if s.as_str() == REST_PAT_KEY {
                            let rest = self.new_obj();
                            if let Value::Obj(src) = &v {
                                let keys = self.own_keys(&v);
                                let taken: HashSet<String> = elems
                                    .iter()
                                    .filter_map(|x| match &x.key {
                                        ast::PropKey::Str(s) | ast::PropKey::Ident(s)
                                            if s.as_str() != REST_PAT_KEY =>
                                        {
                                            Some(s.to_string())
                                        }
                                        _ => None,
                                    })
                                    .collect();
                                for k in keys {
                                    if !taken.contains(&k) {
                                        let val = self.obj_get_raw(src, &PropKey::from_str(&k)).unwrap_or(Value::Und);
                                        self.set_data(&rest, &k, val);
                                    }
                                }
                            }
                            self.assign_pattern(scope, &e.val, Value::Obj(rest))?;
                            continue;
                        }
                    }
                    let key = match &e.key {
                        ast::PropKey::Ident(s) | ast::PropKey::Str(s) => PropKey::from_str(s),
                        ast::PropKey::Num(n) => PropKey::from_str(&js_num_str(*n)),
                        ast::PropKey::Computed(ex) => {
                            let kv = self.eval_expr(ex, scope)?;
                            self.to_propkey(&kv)?
                        }
                    };
                    let got = self.get_prop(&v, &key)?;
                    let val = match (&got, &e.default) {
                        (Value::Und, Some(d)) => self.eval_expr(d, scope)?,
                        _ => got,
                    };
                    match &e.val {
                        Pattern::Ident(name) => self.env_assign(scope, name, val)?,
                        other => self.assign_pattern(scope, other, val)?,
                    }
                }
                Ok(())
            }
        }
    }

    /// Convert an iterable to a Vec of values (for-of, spread, destructuring).
    pub fn iter_values(&mut self, v: &Value) -> Result<Vec<Value>, Value> {
        match v {
            Value::Str(s) => Ok(s.chars().map(|c| Value::Str(Rc::from(c.to_string().as_str()))).collect()),
            Value::Obj(o) => {
                // Symbol.iterator protocol
                let m = self.get_prop(v, &PropKey::Sym(SYM_ITERATOR))?;
                if self.is_callable(&m) {
                    let iter = self.call(&m, v.clone(), Vec::new())?;
                    let mut out = Vec::new();
                    loop {
                        if out.len() > 100_000 {
                            return Err(self.range_err("iteration limit exceeded"));
                        }
                        let next = self.get_prop(&iter, &PropKey::from_str("next"))?;
                        if !self.is_callable(&next) {
                            return Err(self.type_err("iterator.next is not a function"));
                        }
                        let r = self.call(&next, iter.clone(), Vec::new())?;
                        let done = self.get_prop(&r, &PropKey::from_str("done"))?;
                        if self.to_bool(&done) {
                            break;
                        }
                        let val = self.get_prop(&r, &PropKey::from_str("value"))?;
                        out.push(val);
                    }
                    return Ok(out);
                }
                if let ObjKind::Arr(items) = &o.borrow().kind {
                    return Ok(items.clone());
                }
                if let ObjKind::Iter { items, .. } = &o.borrow().kind {
                    return Ok(items.clone());
                }
                if let ObjKind::Set(items) = &o.borrow().kind {
                    return Ok(items.clone());
                }
                if let ObjKind::Map(m) = &o.borrow().kind {
                    let mut out = Vec::new();
                    for (k, v) in m.clone() {
                        out.push(Value::Obj(self.new_arr(vec![k, v])));
                    }
                    return Ok(out);
                }
                Err(self.type_err("value is not iterable"))
            }
            _ => Err(self.type_err("value is not iterable")),
        }
    }

    // -- hoisting ----------------------------------------------------------------
    fn hoist_body(&mut self, stmts: &[Stmt], scope: &EnvRef) -> Result<(), Value> {
        let mut fns: Vec<&Rc<FnDef>> = Vec::new();
        let mut vars: Vec<&str> = Vec::new();
        collect_hoisted(stmts, &mut fns, &mut vars);
        for name in vars {
            self.declare_var(scope, name, Value::Und);
        }
        for f in fns {
            if let Some(name) = f.name.clone() {
                let fv = self.make_fn(f.clone(), scope, CtorKind::NotClass);
                self.declare_var(scope, &name, fv);
            }
        }
        Ok(())
    }

    pub fn make_fn(&mut self, def: Rc<FnDef>, env: &EnvRef, ctor: CtorKind) -> Value {
        self.make_fn_home(def, env, None, ctor)
    }

    pub fn make_fn_home(
        &mut self,
        def: Rc<FnDef>,
        env: &EnvRef,
        home: Option<ObjRef>,
        ctor: CtorKind,
    ) -> Value {
        let o = Rc::new(RefCell::new(Obj::new(
            Some(self.p.func.clone()),
            ObjKind::Func(Func::User {
                def: def.clone(),
                env: env.clone(),
                home,
                ctor,
            }),
        )));
        let name = def.name.clone().unwrap_or_default();
        self.set_builtin(&o, "name", Value::Str(Rc::from(name.as_str())));
        self.set_builtin(&o, "length", Value::Num(param_count(&def.params) as f64));
        if !def.is_arrow {
            let proto = self.new_obj();
            self.set_builtin(&proto, "constructor", Value::Obj(o.clone()));
            self.set_builtin(&o, "prototype", Value::Obj(proto));
        }
        Value::Obj(o)
    }

    // -- statements --------------------------------------------------------------
    pub fn exec_block(&mut self, stmts: &[Stmt], env: &EnvRef) -> SRes {
        let mut last: Option<Value> = None;
        for s in stmts {
            match self.exec_stmt(s, env)? {
                Completion::Normal(v) => last = Some(v),
                Completion::Empty => {}
                other => return Ok(other),
            }
        }
        Ok(match last {
            Some(v) => Completion::Normal(v),
            None => Completion::Empty,
        })
    }

    fn exec_stmt(&mut self, s: &Stmt, env: &EnvRef) -> SRes {
        match s {
            Stmt::Empty => Ok(Completion::Empty),
            Stmt::Expr(e) => Ok(Completion::Normal(self.eval_expr(e, env)?)),
            Stmt::VarDecl { kind, decls } => {
                for (pat, init) in decls {
                    let v = match init {
                        Some(e) => self.eval_expr(e, env)?,
                        None => Value::Und,
                    };
                    match pat {
                        Pattern::Ident(name) => match kind {
                            VarKind::Var => self.declare_var(env, name, v.clone()),
                            _ => self.declare(env, name, v.clone(), *kind != VarKind::Const),
                        },
                        other => self.bind_pattern(env, other, v.clone(), *kind, true)?,
                    }
                }
                Ok(Completion::Empty)
            }
            Stmt::FnDecl(f) => {
                // hoisted already at scope entry; re-bind here for block-level
                if let Some(name) = &f.name {
                    if self.env_lookup(env, name).is_none() {
                        let fv = self.make_fn(f.clone(), env, CtorKind::NotClass);
                        self.declare(env, name, fv, true);
                    }
                }
                Ok(Completion::Empty)
            }
            Stmt::ClassDecl(c) => {
                let v = self.make_class(c, env)?;
                if let Some(n) = &c.name {
                    self.declare(env, n, v, true);
                }
                Ok(Completion::Empty)
            }
            Stmt::Block(stmts) => {
                let scope = Scope::new(Some(env.clone()), false);
                self.hoist_body(stmts, &scope)?;
                self.exec_block(stmts, &scope)
            }
            Stmt::If { c, t, e } => {
                let cv = self.eval_expr(c, env)?;
                if self.to_bool(&cv) {
                    self.exec_stmt(t, env)
                } else if let Some(e) = e {
                    self.exec_stmt(e, env)
                } else {
                    Ok(Completion::Normal(Value::Und))
                }
            }
            Stmt::While { c, body } => {
                let mut last = Value::Und;
                loop {
                    let cv = self.eval_expr(c, env)?;
                    if !self.to_bool(&cv) {
                        break;
                    }
                    match self.exec_stmt(body, env)? {
                        Completion::Normal(v) => last = v,
                        Completion::Empty => {}
                        Completion::Break(l) => {
                            if l.is_none() || self.labels.contains(&l.clone().unwrap_or_default()) {
                                break;
                            }
                            return Ok(Completion::Break(l));
                        }
                        Completion::Continue(l) => {
                            if l.is_none() || self.labels.contains(&l.clone().unwrap_or_default()) {
                                continue;
                            }
                            return Ok(Completion::Continue(l));
                        }
                        r => return Ok(r),
                    }
                }
                Ok(Completion::Normal(last))
            }
            Stmt::DoWhile { body, c } => {
                let mut last = Value::Und;
                loop {
                    match self.exec_stmt(body, env)? {
                        Completion::Normal(v) => last = v,
                        Completion::Empty => {}
                        Completion::Break(l) => {
                            if l.is_none() || self.labels.contains(&l.clone().unwrap_or_default()) {
                                break;
                            }
                            return Ok(Completion::Break(l));
                        }
                        Completion::Continue(l) => {
                            if !(l.is_none()
                                || self.labels.contains(&l.clone().unwrap_or_default()))
                            {
                                return Ok(Completion::Continue(l));
                            }
                        }
                        r => return Ok(r),
                    }
                    let cv = self.eval_expr(c, env)?;
                    if !self.to_bool(&cv) {
                        break;
                    }
                }
                Ok(Completion::Normal(last))
            }
            Stmt::For {
                init,
                test,
                update,
                body,
            } => {
                let scope = Scope::new(Some(env.clone()), false);
                if let Some(init) = init {
                    match init {
                        ForInit::Var(kind, decls) => {
                            for (pat, ie) in decls {
                                let v = match ie {
                                    Some(e) => self.eval_expr(e, &scope)?,
                                    None => Value::Und,
                                };
                                match pat {
                                    Pattern::Ident(n) => match kind {
                                        VarKind::Var => self.declare_var(&scope, n, v),
                                        _ => self.declare(
                                            &scope,
                                            n,
                                            v,
                                            *kind != VarKind::Const,
                                        ),
                                    },
                                    other => self.bind_pattern(&scope, other, v, *kind, true)?,
                                }
                            }
                        }
                        ForInit::Expr(e) => {
                            self.eval_expr(e, &scope)?;
                        }
                    }
                }
                let mut last = Value::Und;
                loop {
                    if let Some(t) = test {
                        let tv = self.eval_expr(t, &scope)?;
                        if !self.to_bool(&tv) {
                            break;
                        }
                    }
                    match self.exec_stmt(body, &scope)? {
                        Completion::Normal(v) => last = v,
                        Completion::Empty => {}
                        Completion::Break(l) => {
                            if l.is_none() || self.labels.contains(&l.clone().unwrap_or_default()) {
                                break;
                            }
                            return Ok(Completion::Break(l));
                        }
                        Completion::Continue(l) => {
                            if !(l.is_none()
                                || self.labels.contains(&l.clone().unwrap_or_default()))
                            {
                                return Ok(Completion::Continue(l));
                            }
                        }
                        r => return Ok(r),
                    }
                    if let Some(u) = update {
                        self.eval_expr(u, &scope)?;
                    }
                }
                Ok(Completion::Normal(last))
            }
            Stmt::ForIn { left, right, body } => {
                let rv = self.eval_expr(right, env)?;
                let keys = self.enum_keys(&rv);
                let scope = Scope::new(Some(env.clone()), false);
                let mut last = Value::Und;
                for k in keys {
                    self.bind_for_target(&scope, left, Value::Str(Rc::from(k.as_str())))?;
                    match self.exec_stmt(body, &scope)? {
                        Completion::Normal(v) => last = v,
                        Completion::Empty => {}
                        Completion::Break(l) => {
                            if l.is_none() || self.labels.contains(&l.clone().unwrap_or_default()) {
                                return Ok(Completion::Normal(last));
                            }
                            return Ok(Completion::Break(l));
                        }
                        Completion::Continue(l) => {
                            if !(l.is_none()
                                || self.labels.contains(&l.clone().unwrap_or_default()))
                            {
                                return Ok(Completion::Continue(l));
                            }
                        }
                        r => return Ok(r),
                    }
                }
                Ok(Completion::Normal(last))
            }
            Stmt::ForOf { left, right, body } => {
                let rv = self.eval_expr(right, env)?;
                let items = self.iter_values(&rv)?;
                let scope = Scope::new(Some(env.clone()), false);
                let mut last = Value::Und;
                for it in items {
                    self.bind_for_target(&scope, left, it)?;
                    match self.exec_stmt(body, &scope)? {
                        Completion::Normal(v) => last = v,
                        Completion::Empty => {}
                        Completion::Break(l) => {
                            if l.is_none() || self.labels.contains(&l.clone().unwrap_or_default()) {
                                return Ok(Completion::Normal(last));
                            }
                            return Ok(Completion::Break(l));
                        }
                        Completion::Continue(l) => {
                            if !(l.is_none()
                                || self.labels.contains(&l.clone().unwrap_or_default()))
                            {
                                return Ok(Completion::Continue(l));
                            }
                        }
                        r => return Ok(r),
                    }
                }
                Ok(Completion::Normal(last))
            }
            Stmt::Switch { disc, cases } => {
                let dv = self.eval_expr(disc, env)?;
                let scope = Scope::new(Some(env.clone()), false);
                // hoist across all case bodies (shared block scope)
                {
                    let mut fns: Vec<&Rc<FnDef>> = Vec::new();
                    let mut vars: Vec<&str> = Vec::new();
                    for c in cases {
                        collect_hoisted(&c.body, &mut fns, &mut vars);
                    }
                    for n in vars {
                        self.declare_var(&scope, n, Value::Und);
                    }
                    for f in fns {
                        if let Some(n) = f.name.clone() {
                            let fv = self.make_fn(f.clone(), &scope, CtorKind::NotClass);
                            self.declare_var(&scope, &n, fv);
                        }
                    }
                }
                // first matching case wins; otherwise jump to default
                let mut start: Option<usize> = None;
                let mut default_idx: Option<usize> = None;
                for (i, case) in cases.iter().enumerate() {
                    match &case.test {
                        Some(te) => {
                            if start.is_none() {
                                let tv = self.eval_expr(te, &scope)?;
                                if self.strict_eq(&dv, &tv) {
                                    start = Some(i);
                                }
                            }
                        }
                        None => {
                            if default_idx.is_none() {
                                default_idx = Some(i);
                            }
                        }
                    }
                }
                let mut last = Value::Und;
                if let Some(si) = start.or(default_idx) {
                    'cases: for case in &cases[si..] {
                        for st in &case.body {
                            match self.exec_stmt(st, &scope)? {
                                Completion::Normal(v) => last = v,
                                Completion::Empty => {}
                                Completion::Break(None) => break 'cases,
                                Completion::Break(l) => {
                                    if self.labels.contains(&l.clone().unwrap_or_default()) {
                                        break 'cases;
                                    }
                                    return Ok(Completion::Break(l));
                                }
                                other => return Ok(other),
                            }
                        }
                    }
                }
                Ok(Completion::Normal(last))
            }
            Stmt::Try {
                block,
                param,
                handler,
                finally,
            } => {
                let try_scope = Scope::new(Some(env.clone()), false);
                self.hoist_body(block, &try_scope)?;
                let mut res = self.exec_block(block, &try_scope);
                if let Err(exc) = res {
                    if let Some(h) = handler {
                        let catch_scope = Scope::new(Some(env.clone()), false);
                        if let Some(p) = param {
                            self.bind_pattern(&catch_scope, p, exc, VarKind::Let, true)?;
                        }
                        self.hoist_body(h, &catch_scope)?;
                        res = self.exec_block(h, &catch_scope);
                    } else {
                        res = Err(exc);
                    }
                }
                if let Some(f) = finally {
                    let fscope = Scope::new(Some(env.clone()), false);
                    self.hoist_body(f, &fscope)?;
                    let fres = self.exec_block(f, &fscope);
                    match fres {
                        Err(e) => return Err(e),
                        Ok(Completion::Normal(_)) | Ok(Completion::Empty) => {}
                        Ok(abrupt) => return Ok(abrupt),
                    }
                }
                res
            }
            Stmt::Throw(e) => {
                let v = self.eval_expr(e, env)?;
                Err(v)
            }
            Stmt::Labeled(label, inner) => {
                let is_loop = matches!(
                    &**inner,
                    Stmt::While { .. }
                        | Stmt::DoWhile { .. }
                        | Stmt::For { .. }
                        | Stmt::ForIn { .. }
                        | Stmt::ForOf { .. }
                );
                if is_loop {
                    self.labels.push(label.clone());
                }
                let res = self.exec_stmt(inner, env);
                if is_loop {
                    self.labels.pop();
                }
                match res {
                    Ok(Completion::Break(Some(n))) if n == *label => {
                        Ok(Completion::Empty)
                    }
                    other => other,
                }
            }
            Stmt::Return(e) => {
                let v = match e {
                    Some(e) => self.eval_expr(e, env)?,
                    None => Value::Und,
                };
                Ok(Completion::Return(v))
            }
            Stmt::Break(l) => Ok(Completion::Break(l.clone())),
            Stmt::Continue(l) => Ok(Completion::Continue(l.clone())),
            Stmt::Import(rec) => {
                self.modules.push(format!(
                    "import {} from {:?}",
                    rec.specifiers,
                    rec.source.clone().unwrap_or_default()
                ));
                Ok(Completion::Empty)
            }
            Stmt::Export(rec) => match rec {
                ExportRec::Decl(inner) => self.exec_stmt(inner, env),
                ExportRec::Default(e) => {
                    let v = self.eval_expr(e, env)?;
                    self.declare(env, "default", v, true);
                    Ok(Completion::Empty)
                }
                _ => {
                    self.modules.push(format!("export {rec:?}"));
                    Ok(Completion::Empty)
                }
            },
        }
    }

    /// for-of/for-in var targets: assign if resolvable, else declare var.
    fn for_var_assign(&mut self, scope: &EnvRef, name: &str, v: Value) -> Result<(), Value> {
        let mut cur = Some(scope.clone());
        while let Some(e) = cur {
            if e.vars.borrow().contains_key(name) {
                return self.env_assign(scope, name, v);
            }
            cur = e.parent.clone();
        }
        self.declare_var(scope, name, v);
        Ok(())
    }

    fn bind_for_target(&mut self, scope: &EnvRef, t: &ForTarget, v: Value) -> Result<(), Value> {
        match t {
            ForTarget::Var(kind, pat) => match pat {
                Pattern::Ident(n) => match kind {
                    VarKind::Var => self.for_var_assign(scope, n, v),
                    _ => {
                        // per-iteration binding — redeclare each time
                        scope.vars.borrow_mut().insert(
                            Rc::from(n.as_str()),
                            Binding {
                                val: v,
                                mutable: *kind != VarKind::Const,
                            },
                        );
                        Ok(())
                    }
                },
                other => {
                    // destructuring target — redeclare each iteration in scope
                    self.bind_pattern_iter(scope, other, v, *kind)
                }
            },
            ForTarget::Expr(e) => self.assign_target(scope, e, v).map(|_| ()),
        }
    }

    fn bind_pattern_iter(&mut self, scope: &EnvRef, pat: &Pattern, v: Value, kind: VarKind) -> Result<(), Value> {
        // like bind_pattern but overwrites let/const bindings each iteration
        match pat {
            Pattern::Ident(n) => {
                if kind == VarKind::Var {
                    return self.for_var_assign(scope, n, v);
                }
                scope.vars.borrow_mut().insert(
                    Rc::from(n.as_str()),
                    Binding {
                        val: v,
                        mutable: kind != VarKind::Const,
                    },
                );
                Ok(())
            }
            Pattern::Array(elems) => {
                let items = self.iter_values(&v)?;
                let mut idx = 0;
                for e in elems {
                    match e {
                        ArrayPatElem::Hole => idx += 1,
                        ArrayPatElem::Elem(p) => {
                            let it = items.get(idx).cloned().unwrap_or(Value::Und);
                            self.bind_pattern_iter(scope, p, it, kind)?;
                            idx += 1;
                        }
                        ArrayPatElem::Default(p, d) => {
                            let it = match items.get(idx) {
                                Some(Value::Und) | None => self.eval_expr(d, scope)?,
                                Some(v) => v.clone(),
                            };
                            self.bind_pattern_iter(scope, p, it, kind)?;
                            idx += 1;
                        }
                        ArrayPatElem::Rest(p) => {
                            let rest: Vec<Value> = items.iter().skip(idx).cloned().collect();
                            let arr = Value::Obj(self.new_arr(rest));
                            self.bind_pattern_iter(scope, p, arr, kind)?;
                        }
                    }
                }
                Ok(())
            }
            Pattern::Object(elems) => {
                for e in elems {
                    let key = match &e.key {
                        ast::PropKey::Ident(s) | ast::PropKey::Str(s) => PropKey::from_str(s),
                        ast::PropKey::Num(n) => PropKey::from_str(&js_num_str(*n)),
                        ast::PropKey::Computed(ex) => {
                            let kv = self.eval_expr(ex, scope)?;
                            self.to_propkey(&kv)?
                        }
                    };
                    let got = self.get_prop(&v, &key)?;
                    let val = match (&got, &e.default) {
                        (Value::Und, Some(d)) => self.eval_expr(d, scope)?,
                        _ => got,
                    };
                    self.bind_pattern_iter(scope, &e.val, val, kind)?;
                }
                Ok(())
            }
        }
    }

    // -- class construction ------------------------------------------------------
    fn make_class(&mut self, c: &ClassDef, env: &EnvRef) -> ERes {
        let superclass = match &c.superclass {
            Some(e) => Some(self.eval_expr(e, env)?),
            None => None,
        };
        let derived = superclass.is_some();
        let (super_ctor, super_proto) = match &superclass {
            Some(Value::Null) | None => (None, self.p.obj.clone()),
            Some(Value::Obj(sc)) => {
                let sp = self.get_prop(
                    &Value::Obj(sc.clone()),
                    &PropKey::from_str("prototype"),
                )?;
                let sp = match sp {
                    Value::Obj(p) => p,
                    _ => {
                        return Err(
                            self.type_err("Class extends value does not have a valid prototype")
                        )
                    }
                };
                (Some(sc.clone()), sp)
            }
            Some(_) => {
                return Err(self.type_err("Class extends value is not a constructor"))
            }
        };

        // find or synthesize constructor
        let mut ctor_def: Option<Rc<FnDef>> = None;
        for m in &c.members {
            if m.kind == MemberKind::Ctor {
                ctor_def = Some(m.func.clone());
            }
        }
        let ctor_def = ctor_def.unwrap_or_else(|| {
            let body = if derived {
                vec![Stmt::Expr(Expr::Call {
                    callee: Box::new(Expr::Super),
                    args: vec![Arg::Spread(Expr::Ident("args".into()))],
                })]
            } else {
                Vec::new()
            };
            Rc::new(FnDef {
                name: c.name.clone(),
                params: if derived {
                    vec![Param::Rest(Box::new(Pattern::Ident("args".into())))]
                } else {
                    Vec::new()
                },
                body,
                is_arrow: false,
                arrow_expr: None,
                is_async: false,
            })
        });

        // class scope for name binding inside methods
        let class_env = Scope::new(Some(env.clone()), false);

        // prototype first — the ctor's `home` points at it so `super.prop`
        // inside the constructor resolves against the parent prototype.
        let proto = Rc::new(RefCell::new(Obj::new(Some(super_proto), ObjKind::Ord)));
        let ctor_fn = self.make_fn_home(
            ctor_def,
            &class_env,
            Some(proto.clone()),
            if derived { CtorKind::Derived } else { CtorKind::Base },
        );
        let Value::Obj(ctor_obj) = ctor_fn.clone() else {
            unreachable!()
        };
        if let Some(sc) = &super_ctor {
            ctor_obj.borrow_mut().proto = Some(sc.clone());
        }
        self.set_builtin(&proto, "constructor", ctor_fn.clone());
        self.set_builtin(&ctor_obj, "prototype", Value::Obj(proto.clone()));
        // name
        if let Some(n) = &c.name {
            self.set_builtin(&ctor_obj, "name", Value::Str(Rc::from(n.as_str())));
        }

        for m in &c.members {
            if m.kind == MemberKind::Ctor {
                continue;
            }
            let target = if m.is_static {
                ctor_obj.clone()
            } else {
                proto.clone()
            };
            let key = match &m.key {
                ast::PropKey::Ident(s) | ast::PropKey::Str(s) => PropKey::from_str(s),
                ast::PropKey::Num(n) => PropKey::from_str(&js_num_str(*n)),
                ast::PropKey::Computed(e) => {
                    let kv = self.eval_expr(e, &class_env)?;
                    self.to_propkey(&kv)?
                }
            };
            match m.kind {
                MemberKind::Method => {
                    let f = self.make_fn_home(
                        m.func.clone(),
                        &class_env,
                        Some(if m.is_static { ctor_obj.clone() } else { proto.clone() }),
                        CtorKind::NotClass,
                    );
                    target.borrow_mut().props.insert(key, Prop::builtin(f));
                }
                MemberKind::Getter | MemberKind::Setter => {
                    let f = self.make_fn_home(
                        m.func.clone(),
                        &class_env,
                        Some(if m.is_static { ctor_obj.clone() } else { proto.clone() }),
                        CtorKind::NotClass,
                    );
                    let mut b = target.borrow_mut();
                    let ent = b.props.or_insert_with(key, || Prop::accessor(None, None));
                    if m.kind == MemberKind::Getter {
                        ent.get = Some(f);
                    } else {
                        ent.set = Some(f);
                    }
                }
                MemberKind::Ctor => unreachable!(),
            }
        }

        if let Some(n) = &c.name {
            class_env
                .vars
                .borrow_mut()
                .insert(Rc::from(n.as_str()), Binding { val: ctor_fn.clone(), mutable: false });
        }
        Ok(ctor_fn)
    }

    // -- expressions ---------------------------------------------------------------
    pub fn eval_expr(&mut self, e: &Expr, env: &EnvRef) -> ERes {
        match e {
            Expr::Num(n) => Ok(Value::Num(*n)),
            Expr::Str(s) => Ok(Value::Str(Rc::from(s.as_str()))),
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::Null => Ok(Value::Null),
            Expr::Ident(name) => {
                if name == "$new_target" {
                    let fs = self.find_fn_scope(env);
                    let v = fs.new_target.borrow().clone();
                    return Ok(v);
                }
                match self.env_lookup(env, name) {
                    Some(v) => Ok(v),
                    None => {
                        if name == "undefined" || name == "$import_meta" {
                            return Ok(Value::Und);
                        }
                        Err(self.ref_err(format!("{name} is not defined")))
                    }
                }
            }
            Expr::This => {
                let fs = self.find_fn_scope(env);
                let v = fs.thisv.borrow().clone();
                Ok(v)
            }
            Expr::Super => Err(self.syntax_err("'super' keyword unexpected here")),
            Expr::Regex { pattern, flags } => self.make_regexp(pattern, flags),
            Expr::Template(parts) => {
                let mut s = String::new();
                for p in parts {
                    match p {
                        TemplPart::Str(t) => s.push_str(t),
                        TemplPart::Expr(ex) => {
                            let v = self.eval_expr(ex, env)?;
                            s.push_str(&self.to_str(&v)?);
                        }
                    }
                }
                Ok(Value::Str(Rc::from(s.as_str())))
            }
            Expr::Array(elems) => {
                let mut items = Vec::new();
                for el in elems {
                    match el {
                        ArrElem::Expr(ex) => items.push(self.eval_expr(ex, env)?),
                        ArrElem::Hole => items.push(Value::Und),
                        ArrElem::Spread(ex) => {
                            let v = self.eval_expr(ex, env)?;
                            items.extend(self.iter_values(&v)?);
                        }
                    }
                }
                Ok(Value::Obj(self.new_arr(items)))
            }
            Expr::Object(props) => {
                let o = self.new_obj();
                for p in props {
                    match &p.val {
                        PropVal::Spread(ex) => {
                            let sv = self.eval_expr(ex, env)?;
                            if let Value::Obj(src) = &sv {
                                for k in self.own_keys(&sv) {
                                    if let Some(v) = self.obj_get_raw(src, &PropKey::from_str(&k)) {
                                        self.set_data(&o, &k, v);
                                    }
                                }
                                // symbol props
                                let symkeys: Vec<PropKey> = src
                                    .borrow()
                                    .props
                                    .iter()
                                    .filter(|(_, pr)| pr.enumerable)
                                    .filter(|(k, _)| matches!(k, PropKey::Sym(_)))
                                    .map(|(k, _)| k.clone())
                                    .collect();
                                for k in symkeys {
                                    if let Some(pr) = src.borrow().props.get(&k) {
                                        o.borrow_mut().props.insert(k, pr.clone());
                                    }
                                }
                            }
                        }
                        _ => {
                            let key = match &p.key {
                                ast::PropKey::Ident(s) | ast::PropKey::Str(s) => PropKey::from_str(s),
                                ast::PropKey::Num(n) => PropKey::from_str(&js_num_str(*n)),
                                ast::PropKey::Computed(ex) => {
                                    let kv = self.eval_expr(ex, env)?;
                                    self.to_propkey(&kv)?
                                }
                            };
                            match &p.val {
                                PropVal::Expr(ex) => {
                                    let v = self.eval_expr(ex, env)?;
                                    o.borrow_mut().props.insert(key, Prop::data(v));
                                }
                                PropVal::Method(def) => {
                                    let f = self.make_fn_home(
                                        def.clone(),
                                        env,
                                        Some(o.clone()),
                                        CtorKind::NotClass,
                                    );
                                    o.borrow_mut().props.insert(key, Prop::data(f));
                                }
                                PropVal::Get(def) | PropVal::Set(def) => {
                                    let f = self.make_fn_home(
                                        def.clone(),
                                        env,
                                        Some(o.clone()),
                                        CtorKind::NotClass,
                                    );
                                    let mut b = o.borrow_mut();
                                    let ent = b
                                        .props
                                        .or_insert_with(key, || Prop::accessor(None, None));
                                    if matches!(&p.val, PropVal::Get(_)) {
                                        ent.get = Some(f);
                                    } else {
                                        ent.set = Some(f);
                                    }
                                }
                                PropVal::Spread(_) => unreachable!(),
                            }
                        }
                    }
                }
                Ok(Value::Obj(o))
            }
            Expr::Fn(def) => {
                // named fn exprs bind their name only inside the function —
                // wrap the captured env in a private scope holding the name.
                let fenv = match &def.name {
                    Some(n) => {
                        let e = Scope::new(Some(env.clone()), false);
                        e.vars.borrow_mut().insert(
                            Rc::from(n.as_str()),
                            Binding {
                                val: Value::Und,
                                mutable: false,
                            },
                        );
                        e
                    }
                    None => env.clone(),
                };
                let f = self.make_fn(def.clone(), &fenv, CtorKind::NotClass);
                if let (Some(n), Value::Obj(fo)) = (&def.name, &f) {
                    if let ObjKind::Func(Func::User { env: fe, .. }) = &fo.borrow().kind {
                        fe.vars.borrow_mut().insert(
                            Rc::from(n.as_str()),
                            Binding {
                                val: f.clone(),
                                mutable: false,
                            },
                        );
                    }
                }
                Ok(f)
            }
            Expr::Class(c) => self.make_class(c, env),
            Expr::Unary { op, expr } => self.eval_unary(*op, expr, env),
            Expr::Update {
                inc,
                prefix,
                target,
            } => {
                let cur = self.eval_expr(target, env)?;
                let n = self.to_num(&cur)?;
                let new = if *inc { n + 1.0 } else { n - 1.0 };
                self.assign_target(env, target, Value::Num(new))?;
                Ok(if *prefix { Value::Num(new) } else { Value::Num(n) })
            }
            Expr::Binary { op, l, r } => {
                let lv = self.eval_expr(l, env)?;
                let rv = self.eval_expr(r, env)?;
                self.binary(*op, lv, rv)
            }
            Expr::Logical { op, l, r } => {
                let lv = self.eval_expr(l, env)?;
                match op {
                    LogOp::And => {
                        if self.to_bool(&lv) {
                            self.eval_expr(r, env)
                        } else {
                            Ok(lv)
                        }
                    }
                    LogOp::Or => {
                        if self.to_bool(&lv) {
                            Ok(lv)
                        } else {
                            self.eval_expr(r, env)
                        }
                    }
                    LogOp::Nullish => {
                        if matches!(lv, Value::Und | Value::Null) {
                            self.eval_expr(r, env)
                        } else {
                            Ok(lv)
                        }
                    }
                }
            }
            Expr::Assign { op, target, value } => self.eval_assign(*op, target, value, env),
            Expr::Cond { c, t, f } => {
                let cv = self.eval_expr(c, env)?;
                if self.to_bool(&cv) {
                    self.eval_expr(t, env)
                } else {
                    self.eval_expr(f, env)
                }
            }
            Expr::Call { callee, args } => self.eval_call(callee, args, env),
            Expr::Member {
                obj,
                prop,
                computed: _,
                optional,
            } => {
                if matches!(&**obj, Expr::Super) {
                    return self.eval_super_member(prop, env);
                }
                let ov = self.eval_expr(obj, env)?;
                if *optional && matches!(ov, Value::Und | Value::Null) {
                    return Ok(Value::Und);
                }
                let pv = self.eval_expr(prop, env)?;
                let key = self.to_propkey(&pv)?;
                self.get_prop(&ov, &key)
            }
            Expr::New { callee, args } => {
                let f = self.eval_expr(callee, env)?;
                let argv = self.eval_args(args, env)?;
                self.construct(&f, argv, None)
            }
            Expr::Seq(exprs) => {
                let mut last = Value::Und;
                for e in exprs {
                    last = self.eval_expr(e, env)?;
                }
                Ok(last)
            }
        }
    }

    fn eval_super_member(&mut self, prop: &Expr, env: &EnvRef) -> ERes {
        let Some((fscope, home)) = self.find_home(env) else {
            return Err(self.syntax_err("'super' keyword unexpected here"));
        };
        let parent = home.borrow().proto.clone();
        let Some(parent) = parent else {
            return Ok(Value::Und);
        };
        let pv = self.eval_expr(prop, env)?;
        let key = self.to_propkey(&pv)?;
        let this = fscope.thisv.borrow().clone();
        self.obj_get(&parent, &key, &this)
    }

    fn eval_args(&mut self, args: &[Arg], env: &EnvRef) -> Result<Vec<Value>, Value> {
        let mut out = Vec::new();
        for a in args {
            match a {
                Arg::Expr(e) => out.push(self.eval_expr(e, env)?),
                Arg::Spread(e) => {
                    let v = self.eval_expr(e, env)?;
                    out.extend(self.iter_values(&v)?);
                }
            }
        }
        Ok(out)
    }

    fn eval_call(&mut self, callee: &Expr, args: &[Arg], env: &EnvRef) -> ERes {
        // super(args) — derived-class parent ctor call
        if matches!(callee, Expr::Super) {
            let fscope = self.find_fn_scope(env);
            let ctor = fscope.ctor_fn.borrow().clone();
            let Value::Obj(ctor_obj) = &ctor else {
                return Err(self.syntax_err("super() call outside constructor"));
            };
            let parent = ctor_obj.borrow().proto.clone();
            let Some(parent) = parent else {
                return Err(self.type_err("super() called on non-derived class"));
            };
            let argv = self.eval_args(args, env)?;
            let nt = fscope.new_target.borrow().clone();
            let this = self.construct(&Value::Obj(parent), argv, Some(nt))?;
            *fscope.thisv.borrow_mut() = this.clone();
            return Ok(this);
        }
        match callee {
            Expr::Member {
                obj,
                prop,
                computed: _,
                optional,
            } => {
                if matches!(&**obj, Expr::Super) {
                    let f = self.eval_super_member(prop, env)?;
                    let argv = self.eval_args(args, env)?;
                    let fscope = self.find_fn_scope(env);
                    let this = fscope.thisv.borrow().clone();
                    let r = self.call(&f, this, argv);
                    return r;
                }
                let ov = self.eval_expr(obj, env)?;
                if *optional && matches!(ov, Value::Und | Value::Null) {
                    return Ok(Value::Und);
                }
                // `a?.()` sentinel
                if let Expr::Str(s) = &**prop {
                    if s == "\u{0}call" {
                        let argv = self.eval_args(args, env)?;
                        return self.call(&ov, Value::Und, argv);
                    }
                }
                let pv = self.eval_expr(prop, env)?;
                let key = self.to_propkey(&pv)?;
                let f = self.get_prop(&ov, &key)?;
                if !self.is_callable(&f) {
                    let ks = self.key_str(&key);
                    return Err(self.type_err(format!("{ks} is not a function")));
                }
                let argv = self.eval_args(args, env)?;
                self.call(&f, ov, argv)
            }
            _ => {
                let f = self.eval_expr(callee, env)?;
                let argv = self.eval_args(args, env)?;
                if !self.is_callable(&f) {
                    let tn = self.type_name(&f);
                    return Err(self.type_err(format!("{tn} is not a function")));
                }
                self.call(&f, Value::Und, argv)
            }
        }
    }

    fn eval_unary(&mut self, op: UnaryOp, expr: &Expr, env: &EnvRef) -> ERes {
        match op {
            UnaryOp::Neg => {
                let v = self.eval_expr(expr, env)?;
                Ok(Value::Num(-self.to_num(&v)?))
            }
            UnaryOp::Pos => {
                let v = self.eval_expr(expr, env)?;
                Ok(Value::Num(self.to_num(&v)?))
            }
            UnaryOp::Not => {
                let v = self.eval_expr(expr, env)?;
                Ok(Value::Bool(!self.to_bool(&v)))
            }
            UnaryOp::BitNot => {
                let v = self.eval_expr(expr, env)?;
                Ok(Value::Num(!(to_i32(self.to_num(&v)?)) as f64))
            }
            UnaryOp::Typeof => {
                // `typeof undefinedIdent` must not throw
                if let Expr::Ident(n) = expr {
                    match self.env_lookup(env, n) {
                        Some(v) => return Ok(Value::Str(Rc::from(self.type_name(&v)))),
                        None => return Ok(Value::Str(Rc::from("undefined"))),
                    }
                }
                let v = self.eval_expr(expr, env)?;
                Ok(Value::Str(Rc::from(self.type_name(&v))))
            }
            UnaryOp::Void => {
                self.eval_expr(expr, env)?;
                Ok(Value::Und)
            }
            UnaryOp::Await => self.eval_expr(expr, env),
            UnaryOp::Delete => match expr {
                Expr::Member { obj, prop, .. } => {
                    let ov = self.eval_expr(obj, env)?;
                    let pv = self.eval_expr(prop, env)?;
                    let key = self.to_propkey(&pv)?;
                    Ok(Value::Bool(self.delete_prop(&ov, &key)))
                }
                Expr::Ident(_) | Expr::This => Ok(Value::Bool(false)),
                _ => {
                    self.eval_expr(expr, env)?;
                    Ok(Value::Bool(true))
                }
            },
        }
    }

    fn binary(&mut self, op: BinOp, l: Value, r: Value) -> ERes {
        use BinOp::*;
        Ok(match op {
            Add => {
                let lp = self.to_primitive(&l)?;
                let rp = self.to_primitive(&r)?;
                if matches!(lp, Value::Str(_)) || matches!(rp, Value::Str(_)) {
                    let a = self.to_str(&lp)?;
                    let b = self.to_str(&rp)?;
                    Value::Str(Rc::from(format!("{a}{b}").as_str()))
                } else {
                    Value::Num(self.to_num(&lp)? + self.to_num(&rp)?)
                }
            }
            Sub => Value::Num(self.to_num(&l)? - self.to_num(&r)?),
            Mul => Value::Num(self.to_num(&l)? * self.to_num(&r)?),
            Div => Value::Num(self.to_num(&l)? / self.to_num(&r)?),
            Mod => {
                let a = self.to_num(&l)?;
                let b = self.to_num(&r)?;
                Value::Num(if b == 0.0 { f64::NAN } else { a % b })
            }
            Pow => Value::Num(self.to_num(&l)?.powf(self.to_num(&r)?)),
            Eq => Value::Bool(self.loose_eq(&l, &r)?),
            Ne => Value::Bool(!self.loose_eq(&l, &r)?),
            StrictEq => Value::Bool(self.strict_eq(&l, &r)),
            StrictNe => Value::Bool(!self.strict_eq(&l, &r)),
            Lt | Le | Gt | Ge => {
                let lp = self.to_primitive(&l)?;
                let rp = self.to_primitive(&r)?;
                if let (Value::Str(a), Value::Str(b)) = (&lp, &rp) {
                    let c = match op {
                        Lt => a < b,
                        Le => a <= b,
                        Gt => a > b,
                        Ge => a >= b,
                        _ => unreachable!(),
                    };
                    Value::Bool(c)
                } else {
                    let a = self.to_num(&lp)?;
                    let b = self.to_num(&rp)?;
                    if a.is_nan() || b.is_nan() {
                        Value::Bool(false)
                    } else {
                        Value::Bool(match op {
                            Lt => a < b,
                            Le => a <= b,
                            Gt => a > b,
                            Ge => a >= b,
                            _ => unreachable!(),
                        })
                    }
                }
            }
            BitAnd => Value::Num((to_i32(self.to_num(&l)?) & to_i32(self.to_num(&r)?)) as f64),
            BitOr => Value::Num((to_i32(self.to_num(&l)?) | to_i32(self.to_num(&r)?)) as f64),
            BitXor => Value::Num((to_i32(self.to_num(&l)?) ^ to_i32(self.to_num(&r)?)) as f64),
            Shl => Value::Num(
                (to_i32(self.to_num(&l)?).wrapping_shl(to_u32(self.to_num(&r)?) & 31)) as f64,
            ),
            Shr => Value::Num(
                (to_i32(self.to_num(&l)?).wrapping_shr(to_u32(self.to_num(&r)?) & 31)) as f64,
            ),
            Ushr => Value::Num(
                (to_u32(self.to_num(&l)?).wrapping_shr(to_u32(self.to_num(&r)?) & 31)) as f64,
            ),
            In => {
                if !matches!(r, Value::Obj(_)) {
                    return Err(self.type_err("Cannot use 'in' operator on non-object"));
                }
                let key = self.to_propkey(&l)?;
                Value::Bool(self.has_prop(&r, &key))
            }
            Instanceof => {
                let proto = match &r {
                    Value::Obj(o) if o.borrow().is_callable() => {
                        // bound functions → target's prototype
                        let mut cur = r.clone();
                        loop {
                            let t = {
                                let b = if let Value::Obj(co) = &cur {
                                    co.borrow().kind.clone()
                                } else {
                                    break;
                                };
                                match b {
                                    ObjKind::Bound { target, .. } => Some(target),
                                    _ => None,
                                }
                            };
                            match t {
                                Some(t) => cur = t,
                                None => break,
                            }
                        }
                        self.get_prop(&cur, &PropKey::from_str("prototype"))?
                    }
                    _ => return Err(self.type_err("Right-hand side of 'instanceof' is not an object")),
                };
                let Value::Obj(want) = proto else {
                    return Err(
                        self.type_err("Right-hand side of 'instanceof' is not callable")
                    );
                };
                let mut found = false;
                if let Value::Obj(mut cur) = l.clone() {
                    for _ in 0..10_000 {
                        let next = cur.borrow().proto.clone();
                        match next {
                            Some(n) => {
                                if Rc::ptr_eq(&n, &want) {
                                    found = true;
                                    break;
                                }
                                cur = n;
                            }
                            None => break,
                        }
                    }
                }
                Value::Bool(found)
            }
        })
    }

    fn eval_assign(&mut self, op: AssignOp, target: &Expr, value: &Expr, env: &EnvRef) -> ERes {
        if op == AssignOp::Assign {
            // destructuring targets arrive as Array/Object literal exprs
            match target {
                Expr::Array(_) | Expr::Object(_) => {
                    let v = self.eval_expr(value, env)?;
                    if let Some(pat) = expr_to_pattern(target) {
                        self.assign_pattern(env, &pat, v.clone())?;
                    } else {
                        return Err(self.syntax_err("invalid assignment target"));
                    }
                    return Ok(v);
                }
                _ => {
                    let v = self.eval_expr(value, env)?;
                    self.assign_target(env, target, v.clone())?;
                    return Ok(v);
                }
            }
        }
        // compound assignments
        let cur = self.eval_expr(target, env)?;
        let result = match op {
            AssignOp::LogAnd => {
                if !self.to_bool(&cur) {
                    return Ok(cur);
                }
                let v = self.eval_expr(value, env)?;
                self.assign_target(env, target, v.clone())?;
                return Ok(v);
            }
            AssignOp::LogOr => {
                if self.to_bool(&cur) {
                    return Ok(cur);
                }
                let v = self.eval_expr(value, env)?;
                self.assign_target(env, target, v.clone())?;
                return Ok(v);
            }
            AssignOp::Nullish => {
                if !matches!(cur, Value::Und | Value::Null) {
                    return Ok(cur);
                }
                let v = self.eval_expr(value, env)?;
                self.assign_target(env, target, v.clone())?;
                return Ok(v);
            }
            _ => {
                let rv = self.eval_expr(value, env)?;
                let bop = match op {
                    AssignOp::Add => BinOp::Add,
                    AssignOp::Sub => BinOp::Sub,
                    AssignOp::Mul => BinOp::Mul,
                    AssignOp::Div => BinOp::Div,
                    AssignOp::Mod => BinOp::Mod,
                    AssignOp::Pow => BinOp::Pow,
                    AssignOp::BitAnd => BinOp::BitAnd,
                    AssignOp::BitOr => BinOp::BitOr,
                    AssignOp::BitXor => BinOp::BitXor,
                    AssignOp::Shl => BinOp::Shl,
                    AssignOp::Shr => BinOp::Shr,
                    AssignOp::Ushr => BinOp::Ushr,
                    _ => unreachable!(),
                };
                self.binary(bop, cur, rv)?
            }
        };
        self.assign_target(env, target, result.clone())?;
        Ok(result)
    }

    /// assign `v` to an lvalue expression.
    fn assign_target(&mut self, env: &EnvRef, target: &Expr, v: Value) -> Result<Value, Value> {
        match target {
            Expr::Ident(name) => {
                self.env_assign(env, name, v.clone())?;
                Ok(v)
            }
            Expr::Member { obj, prop, .. } => {
                let ov = self.eval_expr(obj, env)?;
                let pv = self.eval_expr(prop, env)?;
                let key = self.to_propkey(&pv)?;
                self.set_prop(&ov, key, v.clone())?;
                Ok(v)
            }
            Expr::Array(_) | Expr::Object(_) => {
                if let Some(pat) = expr_to_pattern(target) {
                    self.assign_pattern(env, &pat, v.clone())?;
                    Ok(v)
                } else {
                    Err(self.syntax_err("invalid assignment target"))
                }
            }
            _ => Err(self.ref_err("invalid assignment target")),
        }
    }

    // -- top-level ---------------------------------------------------------------
    pub fn run(&mut self, src: &str) -> Result<Value, Error> {
        let stmts = parser::parse(src)?;
        let g = self.global.clone();
        if let Err(v) = self.hoist_body(&stmts, &g) {
            return Err(self.throw_to_error(v));
        }
        let mut last = Value::Und;
        for s in &stmts {
            match self.exec_stmt(s, &g) {
                Ok(Completion::Normal(v)) => last = v,
                Ok(Completion::Return(v)) => last = v,
                Ok(_) => {}
                Err(v) => return Err(self.throw_to_error(v)),
            }
        }
        Ok(last)
    }

    pub fn throw_to_error(&mut self, v: Value) -> Error {
        let msg = match &v {
            Value::Obj(_) => self.inspect(&v, 0),
            other => {
                self.to_str(other).unwrap_or_else(|_| "<exception>".into())
            }
        };
        Error::new(format!("Uncaught {msg}"), 0, 0)
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

pub fn arr_index(s: &str) -> Option<usize> {
    if s.is_empty() || s.len() > 10 {
        return None;
    }
    if !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if s.len() > 1 && s.starts_with('0') {
        return None;
    }
    s.parse::<usize>().ok().filter(|i| *i < (1 << 30))
}

pub fn to_i32(n: f64) -> i32 {
    if !n.is_finite() || n == 0.0 {
        return 0;
    }
    let n = n.trunc() % 4294967296.0;
    let n = if n < 0.0 { n + 4294967296.0 } else { n };
    if n >= 2147483648.0 {
        (n - 4294967296.0) as i64 as i32
    } else {
        n as i64 as i32
    }
}

pub fn to_u32(n: f64) -> u32 {
    if !n.is_finite() || n == 0.0 {
        return 0;
    }
    let n = n.trunc() % 4294967296.0;
    let n = if n < 0.0 { n + 4294967296.0 } else { n };
    n as u64 as u32
}

pub fn js_str_to_num(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" | "∞" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        "NaN" => return f64::NAN,
        _ => {}
    }
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return u64::from_str_radix(h, 16).map(|v| v as f64).unwrap_or(f64::NAN);
    }
    if let Some(h) = t.strip_prefix("0o").or_else(|| t.strip_prefix("0O")) {
        return u64::from_str_radix(h, 8).map(|v| v as f64).unwrap_or(f64::NAN);
    }
    if let Some(h) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
        return u64::from_str_radix(h, 2).map(|v| v as f64).unwrap_or(f64::NAN);
    }
    // reject bigint-style trailing n and weird chars
    let cleaned = t.trim_end_matches('n');
    cleaned.parse::<f64>().unwrap_or(f64::NAN)
}

pub fn js_num_str(n: f64) -> String {
    if n.is_nan() {
        return "NaN".into();
    }
    if n == f64::INFINITY {
        return "Infinity".into();
    }
    if n == f64::NEG_INFINITY {
        return "-Infinity".into();
    }
    if n == 0.0 {
        return "0".into();
    }
    if n == n.trunc() && n.abs() < 1e21 {
        return format!("{}", n as i64);
    }
    let s = format!("{n}");
    s
}

fn param_count(params: &[Param]) -> usize {
    params
        .iter()
        .take_while(|p| !matches!(p, Param::Rest(_)))
        .count()
}

fn collect_hoisted<'a>(stmts: &'a [Stmt], fns: &mut Vec<&'a Rc<FnDef>>, vars: &mut Vec<&'a str>) {
    for s in stmts {
        collect_stmt(s, fns, vars);
    }
}

fn collect_stmt<'a>(s: &'a Stmt, fns: &mut Vec<&'a Rc<FnDef>>, vars: &mut Vec<&'a str>) {
    match s {
        Stmt::FnDecl(f) => fns.push(f),
        Stmt::VarDecl { kind, decls } => {
            if *kind == VarKind::Var {
                for (p, _) in decls {
                    collect_pattern_names(p, vars);
                }
            }
        }
        Stmt::Block(ss) => collect_hoisted(ss, fns, vars),
        Stmt::If { t, e, .. } => {
            collect_stmt(t, fns, vars);
            if let Some(e) = e {
                collect_stmt(e, fns, vars);
            }
        }
        Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => collect_stmt(body, fns, vars),
        Stmt::For { init, body, .. } => {
            if let Some(ForInit::Var(VarKind::Var, decls)) = init {
                for (p, _) in decls {
                    collect_pattern_names(p, vars);
                }
            }
            collect_stmt(body, fns, vars);
        }
        Stmt::ForIn { left, body, .. } | Stmt::ForOf { left, body, .. } => {
            if let ForTarget::Var(VarKind::Var, p) = left {
                collect_pattern_names(p, vars);
            }
            collect_stmt(body, fns, vars);
        }
        Stmt::Switch { cases, .. } => {
            for c in cases {
                collect_hoisted(&c.body, fns, vars);
            }
        }
        Stmt::Try {
            block,
            handler,
            finally,
            ..
        } => {
            collect_hoisted(block, fns, vars);
            if let Some(h) = handler {
                collect_hoisted(h, fns, vars);
            }
            if let Some(f) = finally {
                collect_hoisted(f, fns, vars);
            }
        }
        Stmt::Labeled(_, inner) => collect_stmt(inner, fns, vars),
        _ => {}
    }
}

fn collect_pattern_names<'a>(p: &'a Pattern, out: &mut Vec<&'a str>) {
    match p {
        Pattern::Ident(n) => out.push(n),
        Pattern::Array(elems) => {
            for e in elems {
                match e {
                    ArrayPatElem::Elem(p) | ArrayPatElem::Rest(p) => collect_pattern_names(p, out),
                    ArrayPatElem::Default(p, _) => collect_pattern_names(p, out),
                    ArrayPatElem::Hole => {}
                }
            }
        }
        Pattern::Object(elems) => {
            for e in elems {
                collect_pattern_names(&e.val, out);
            }
        }
    }
}

/// Convert an array/object literal expression into a destructuring pattern
/// (for `[a,b] = x` / `{x, y} = o`).
fn expr_to_pattern(e: &Expr) -> Option<Pattern> {
    match e {
        Expr::Ident(n) => Some(Pattern::Ident(n.clone())),
        Expr::Array(elems) => {
            let mut out = Vec::new();
            for el in elems {
                match el {
                    ArrElem::Expr(ex) => match ex {
                        Expr::Assign {
                            op: AssignOp::Assign,
                            target,
                            value,
                        } => {
                            let p = expr_to_pattern(target)?;
                            out.push(ArrayPatElem::Default(p, (**value).clone()));
                        }
                        _ => out.push(ArrayPatElem::Elem(expr_to_pattern(ex)?)),
                    },
                    ArrElem::Hole => out.push(ArrayPatElem::Hole),
                    ArrElem::Spread(ex) => {
                        out.push(ArrayPatElem::Rest(expr_to_pattern(ex)?));
                    }
                }
            }
            Some(Pattern::Array(out))
        }
        Expr::Object(props) => {
            let mut out = Vec::new();
            for p in props {
                match &p.val {
                    PropVal::Spread(ex) => {
                        out.push(ObjPatElem {
                            key: ast::PropKey::Str(REST_PAT_KEY.into()),
                            val: expr_to_pattern(ex)?,
                            default: None,
                        });
                    }
                    PropVal::Expr(ex) => {
                        let (val, dflt) = match ex {
                            Expr::Assign {
                                op: AssignOp::Assign,
                                target,
                                value,
                            } => (expr_to_pattern(target)?, Some((**value).clone())),
                            Expr::Ident(n) => (Pattern::Ident(n.clone()), None),
                            // `{a: [x, y]} = o` / `{a: {b}} = o`
                            Expr::Array(_) | Expr::Object(_) => (expr_to_pattern(ex)?, None),
                            _ => return None,
                        };
                        out.push(ObjPatElem {
                            key: p.key.clone(),
                            val,
                            default: dflt,
                        });
                    }
                    _ => return None,
                }
            }
            Some(Pattern::Object(out))
        }
        Expr::Member { .. } => None, // member targets in destructuring unsupported
        _ => None,
    }
}

impl Default for Interp {
    fn default() -> Self {
        Self::new()
    }
}
