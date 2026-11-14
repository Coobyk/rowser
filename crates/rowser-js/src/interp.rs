//! Tree-walking evaluator.

use crate::ast::{self, *};
use crate::{parser, Error};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

// ---------------------------------------------------------------------------
// Fast hasher (rustc-hash/FxHash style) — SipHash is overkill for the
// short identifier/property keys that dominate interpreter lookups.
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
pub struct FxHasher {
    hash: u64,
}

const FX_SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl FxHasher {
    #[inline]
    fn add(&mut self, w: u64) {
        self.hash = (self.hash.rotate_left(5) ^ w).wrapping_mul(FX_SEED);
    }
}

impl std::hash::Hasher for FxHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
    #[inline]
    fn write(&mut self, mut b: &[u8]) {
        while b.len() >= 8 {
            self.add(u64::from_le_bytes(b[..8].try_into().unwrap()));
            b = &b[8..];
        }
        if b.len() >= 4 {
            self.add(u32::from_le_bytes(b[..4].try_into().unwrap()) as u64);
            b = &b[4..];
        }
        if b.len() >= 2 {
            self.add(u16::from_le_bytes(b[..2].try_into().unwrap()) as u64);
            b = &b[2..];
        }
        if !b.is_empty() {
            self.add(b[0] as u64);
        }
    }
    #[inline]
    fn write_u8(&mut self, n: u8) {
        self.add(n as u64);
    }
    #[inline]
    fn write_u16(&mut self, n: u16) {
        self.add(n as u64);
    }
    #[inline]
    fn write_u32(&mut self, n: u32) {
        self.add(n as u64);
    }
    #[inline]
    fn write_u64(&mut self, n: u64) {
        self.add(n);
    }
    #[inline]
    fn write_u128(&mut self, n: u128) {
        self.add(n as u64);
        self.add((n >> 64) as u64);
    }
    #[inline]
    fn write_usize(&mut self, n: usize) {
        self.add(n as u64);
    }
    #[inline]
    fn write_i8(&mut self, n: i8) {
        self.add(n as u64);
    }
    #[inline]
    fn write_i16(&mut self, n: i16) {
        self.add(n as u64);
    }
    #[inline]
    fn write_i32(&mut self, n: i32) {
        self.add(n as u64);
    }
    #[inline]
    fn write_i64(&mut self, n: i64) {
        self.add(n as u64);
    }
    #[inline]
    fn write_i128(&mut self, n: i128) {
        self.add(n as u64);
        self.add((n >> 64) as u64);
    }
    #[inline]
    fn write_isize(&mut self, n: isize) {
        self.add(n as u64);
    }
}

pub type FxBuild = std::hash::BuildHasherDefault<FxHasher>;
pub type FxMap<K, V> = HashMap<K, V, FxBuild>;
pub type FxSet<T> = HashSet<T, FxBuild>;

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
///
/// String keys live in `map` (keyed by `Rc<str>`) so lookups can borrow
/// `&str` directly — `get_str` hits the map with NO `Rc` allocation, which
/// is what every `obj.prop` member access uses. Symbol keys are rare and
/// live in a separate side map; `order` still holds both kinds.
#[derive(Default)]
pub struct PropMap {
    map: FxMap<Rc<str>, Prop>,
    syms: FxMap<u64, Prop>,
    order: Vec<PropKey>,
}

impl PropMap {
    pub fn new() -> PropMap {
        PropMap::default()
    }
    pub fn get(&self, k: &PropKey) -> Option<&Prop> {
        match k {
            PropKey::S(s) => self.map.get(s.as_ref()),
            PropKey::Sym(id) => self.syms.get(id),
        }
    }
    /// borrowed lookup by string key — no key allocation
    #[inline]
    pub fn get_str(&self, k: &str) -> Option<&Prop> {
        self.map.get(k)
    }
    #[inline]
    pub fn get_str_mut(&mut self, k: &str) -> Option<&mut Prop> {
        self.map.get_mut(k)
    }
    pub fn get_mut(&mut self, k: &PropKey) -> Option<&mut Prop> {
        match k {
            PropKey::S(s) => self.map.get_mut(s.as_ref()),
            PropKey::Sym(id) => self.syms.get_mut(id),
        }
    }
    pub fn insert(&mut self, k: PropKey, p: Prop) {
        match k {
            PropKey::S(s) => {
                if !self.map.contains_key(s.as_ref()) {
                    self.order.push(PropKey::S(s.clone()));
                }
                self.map.insert(s, p);
            }
            PropKey::Sym(id) => {
                if !self.syms.contains_key(&id) {
                    self.order.push(PropKey::Sym(id));
                }
                self.syms.insert(id, p);
            }
        }
    }
    /// insert by string key — allocates the key only when newly inserted
    pub fn insert_str(&mut self, s: &str, p: Prop) {
        if let Some(e) = self.map.get_mut(s) {
            *e = p;
            return;
        }
        self.order.push(PropKey::from_str(s));
        self.map.insert(Rc::from(s), p);
    }
    /// insert with an owned (e.g. interned) string key — one Rc clone
    pub fn insert_str_rc(&mut self, s: Rc<str>, p: Prop) {
        if !self.map.contains_key(s.as_ref()) {
            self.order.push(PropKey::S(s.clone()));
        }
        self.map.insert(s, p);
    }
    pub fn remove(&mut self, k: &PropKey) -> Option<Prop> {
        let r = match k {
            PropKey::S(s) => self.map.remove(s.as_ref()),
            PropKey::Sym(id) => self.syms.remove(id),
        };
        if r.is_some() {
            self.order.retain(|x| x != k);
        }
        r
    }
    pub fn remove_str(&mut self, k: &str) -> Option<Prop> {
        let r = self.map.remove(k);
        if r.is_some() {
            self.order
                .retain(|x| !matches!(x, PropKey::S(s) if s.as_ref() == k));
        }
        r
    }
    pub fn contains_key(&self, k: &PropKey) -> bool {
        match k {
            PropKey::S(s) => self.map.contains_key(s.as_ref()),
            PropKey::Sym(id) => self.syms.contains_key(id),
        }
    }
    #[inline]
    pub fn contains_str(&self, k: &str) -> bool {
        self.map.contains_key(k)
    }
    /// iteration in insertion order
    pub fn iter(&self) -> impl Iterator<Item = (&PropKey, &Prop)> + '_ {
        self.order
            .iter()
            .filter_map(move |k| self.get(k).map(|p| (k, p)))
    }
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut Prop> + '_ {
        self.map.values_mut().chain(self.syms.values_mut())
    }
    pub fn or_insert_with(&mut self, k: PropKey, f: impl FnOnce() -> Prop) -> &mut Prop {
        match k {
            PropKey::S(s) => {
                if !self.map.contains_key(s.as_ref()) {
                    self.order.push(PropKey::S(s.clone()));
                }
                self.map.entry(s).or_insert_with(f)
            }
            PropKey::Sym(id) => {
                if !self.syms.contains_key(&id) {
                    self.order.push(PropKey::Sym(id));
                }
                self.syms.entry(id).or_insert_with(f)
            }
        }
    }
    pub fn len(&self) -> usize {
        self.map.len() + self.syms.len()
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
    /// ArrayBuffer — shared byte store (Rc so views alias it)
    Bytes(Rc<RefCell<Vec<u8>>>),
    /// TypedArray view: buffer object (kind `Bytes`), byte offset,
    /// element count, element kind
    Typed {
        buf: ObjRef,
        off: usize,
        len: usize,
        kind: TAK,
    },
    /// DataView: buffer object, byte offset, byte length
    View {
        buf: ObjRef,
        off: usize,
        len: usize,
    },
}

/// TypedArray element kind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TAK {
    U8,
    U8C,
    I8,
    U16,
    I16,
    U32,
    I32,
    F32,
    F64,
}

impl TAK {
    pub fn size(self) -> usize {
        match self {
            TAK::U8 | TAK::U8C | TAK::I8 => 1,
            TAK::U16 | TAK::I16 => 2,
            TAK::U32 | TAK::I32 | TAK::F32 => 4,
            TAK::F64 => 8,
        }
    }
    pub fn type_name(self) -> &'static str {
        match self {
            TAK::U8 => "Uint8Array",
            TAK::U8C => "Uint8ClampedArray",
            TAK::I8 => "Int8Array",
            TAK::U16 => "Uint16Array",
            TAK::I16 => "Int16Array",
            TAK::U32 => "Uint32Array",
            TAK::I32 => "Int32Array",
            TAK::F32 => "Float32Array",
            TAK::F64 => "Float64Array",
        }
    }
    /// read one element (little-endian) at byte offset `boff`
    pub fn read(self, b: &[u8], boff: usize) -> f64 {
        match self {
            TAK::U8 | TAK::U8C => b[boff] as f64,
            TAK::I8 => b[boff] as i8 as f64,
            TAK::U16 => u16::from_le_bytes(b[boff..boff + 2].try_into().unwrap()) as f64,
            TAK::I16 => i16::from_le_bytes(b[boff..boff + 2].try_into().unwrap()) as f64,
            TAK::U32 => u32::from_le_bytes(b[boff..boff + 4].try_into().unwrap()) as f64,
            TAK::I32 => i32::from_le_bytes(b[boff..boff + 4].try_into().unwrap()) as f64,
            TAK::F32 => f32::from_le_bytes(b[boff..boff + 4].try_into().unwrap()) as f64,
            TAK::F64 => f64::from_le_bytes(b[boff..boff + 8].try_into().unwrap()),
        }
    }
    /// write one element (little-endian) with JS conversion semantics:
    /// ints wrap mod 2^bits, Uint8Clamped rounds+clamps, floats truncate.
    pub fn write(self, b: &mut [u8], boff: usize, v: f64) {
        match self {
            TAK::U8 => b[boff] = to_u32(v) as u8,
            TAK::U8C => {
                b[boff] = if v.is_nan() || v <= 0.0 {
                    0
                } else if v >= 255.0 {
                    255
                } else {
                    v.round() as u8
                };
            }
            TAK::I8 => b[boff] = to_i32(v) as i8 as u8,
            TAK::U16 => b[boff..boff + 2].copy_from_slice(&(to_u32(v) as u16).to_le_bytes()),
            TAK::I16 => b[boff..boff + 2].copy_from_slice(&(to_i32(v) as i16).to_le_bytes()),
            TAK::U32 => b[boff..boff + 4].copy_from_slice(&to_u32(v).to_le_bytes()),
            TAK::I32 => b[boff..boff + 4].copy_from_slice(&to_i32(v).to_le_bytes()),
            TAK::F32 => b[boff..boff + 4].copy_from_slice(&(v as f32).to_le_bytes()),
            TAK::F64 => b[boff..boff + 8].copy_from_slice(&v.to_le_bytes()),
        }
    }
}

/// If `o` is an ArrayBuffer (`ObjKind::Bytes`), its shared byte store.
pub fn buffer_bytes(o: &ObjRef) -> Option<Rc<RefCell<Vec<u8>>>> {
    if let ObjKind::Bytes(b) = &o.borrow().kind {
        Some(b.clone())
    } else {
        None
    }
}

/// Virtual own-props for array/binary kinds: `length`/`byteLength`/
/// `byteOffset`/`buffer` and integer-indexed reads. Returns `Some` when
/// `key` resolves on the object kind itself (before stored props).
/// `&str` version so literal member lookups never build a `PropKey`.
fn view_own_prop_str(o: &ObjRef, key: &str) -> Option<Value> {
    let b = o.borrow();
    match &b.kind {
        ObjKind::Arr(items) => {
            if key == "length" {
                return Some(Value::Num(items.len() as f64));
            }
            if let Some(i) = arr_index(key) {
                return Some(items.get(i).cloned().unwrap_or(Value::Und));
            }
            None
        }
        ObjKind::Typed {
            buf,
            off,
            len,
            kind,
        } => {
            match key {
                "length" => return Some(Value::Num(*len as f64)),
                "byteLength" => return Some(Value::Num((*len * kind.size()) as f64)),
                "byteOffset" => return Some(Value::Num(*off as f64)),
                "buffer" => return Some(Value::Obj(buf.clone())),
                _ => {}
            }
            if let Some(idx) = arr_index(key) {
                if idx >= *len {
                    return Some(Value::Und);
                }
                if let Some(bytes) = buffer_bytes(buf) {
                    let bb = bytes.borrow();
                    return Some(Value::Num(kind.read(&bb, *off + idx * kind.size())));
                }
                return Some(Value::Und);
            }
            None
        }
        ObjKind::Bytes(bytes) => {
            if key == "byteLength" {
                return Some(Value::Num(bytes.borrow().len() as f64));
            }
            None
        }
        ObjKind::View { buf, off, len } => {
            match key {
                "byteLength" => return Some(Value::Num(*len as f64)),
                "byteOffset" => return Some(Value::Num(*off as f64)),
                "buffer" => return Some(Value::Obj(buf.clone())),
                _ => {}
            }
            None
        }
        _ => None,
    }
}

fn view_own_prop(o: &ObjRef, key: &PropKey) -> Option<Value> {
    match key {
        PropKey::S(s) => view_own_prop_str(o, s),
        PropKey::Sym(_) => None,
    }
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
    pub vars: RefCell<FxMap<Rc<str>, Binding>>,
    pub parent: Option<EnvRef>,
    /// function scope: `this`/arguments/new.target/super boundary
    pub is_fn: bool,
    /// `var` declarations stop here — true for every function body scope
    /// INCLUDING arrows (arrows only inherit this/arguments/new.target,
    /// never their parent's var bindings).
    pub var_boundary: std::cell::Cell<bool>,
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
            vars: RefCell::new(FxMap::default()),
            parent,
            is_fn,
            var_boundary: std::cell::Cell::new(is_fn),
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

/// Precomputed per-body hoisting scan: `var` names to predeclare, function
/// decls to bind, and whether the body (or its params) mentions
/// `arguments` — which decides if the `arguments` object must be built.
struct Hoisted {
    fns: Vec<Rc<FnDef>>,
    vars: Vec<Rc<str>>,
    uses_arguments: bool,
}

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
    pub arraybuffer: ObjRef,
    pub global: ObjRef,
}

pub struct Interp {
    pub global: EnvRef,
    pub p: Protos,
    /// collected console output
    pub output: Vec<String>,
    /// recorded import/export specifiers
    pub modules: Vec<String>,
    /// Label set by `Labeled` for the loop statement it directly wraps;
    /// taken by the loop arm on entry so `continue <lbl>` is only
    /// consumed by the loop that actually carries the label.
    pub pending_label: Option<String>,
    pub depth: usize,
    pub sym_counter: u64,
    pub rng: u64,
    /// Embedder-owned state (e.g. the DOM bridge's `DomState`). Native
    /// functions receive `&mut Interp`, so host bindings stash their world
    /// here and downcast. Not touched by the interpreter itself.
    pub ext: Option<Box<dyn std::any::Any>>,
    /// ring buffer of recent call sites (debug aid for bundle triage)
    pub call_trace: std::collections::VecDeque<String>,
    /// call_trace snapshot at the most recent `throw` statement
    pub last_throw_trace: Vec<String>,
    /// recorded `x.$$typeof` reads: "value <- object" (debug)
    pub typeof_reads: Vec<String>,
    /// statements executed this session (drives deadline polling)
    pub steps: u64,
    /// once tripped every `exec_stmt` errors until cleared — makes the
    /// interrupt uncatchable by JS try/catch (each catch body re-errors)
    pub interrupted: bool,
    /// wall-clock bound for the current embedder turn; `None` = unbounded
    pub deadline: Option<std::time::Instant>,
    /// stmt being executed when the interrupt tripped (triage aid)
    pub interrupt_site: Option<String>,
    /// opt-in call tracing (set `ROWSER_CALL_TRACE=1`): when on, each call
    /// pushes a cheap callee tag into `call_trace`; when off the hot call
    /// path costs one branch and the trace stays empty
    pub trace_calls: bool,
    /// Rc<str> interner for identifier-shaped strings (binding names,
    /// string literals) — turns per-eval/per-binding Rc allocations into
    /// refcount bumps. Only program-source strings are interned, so the
    /// set stays bounded by source size.
    interned: FxSet<Rc<str>>,
    /// per-function-def hoist+`arguments` scan results, keyed by the
    /// `Rc<FnDef>` pointer. The stored `Rc<FnDef>` pins the def so its
    /// address can never be reused while an entry exists — the key is
    /// always unique and the cache needs no invalidation.
    fn_hoist: FxMap<usize, (Rc<FnDef>, Rc<Hoisted>)>,
    /// hoist scan results for `&[Stmt]` slices, keyed by (ptr, len) and
    /// cleared at the start of every `run`. Only slices owned by the
    /// parsed AST (blocks, try/catch/finally bodies) go through here —
    /// they live for the whole run; function bodies use `fn_hoist`.
    slice_hoist: FxMap<(usize, usize), Rc<Hoisted>>,
}

/// JS call-depth limit. Each JS call consumes substantial Rust stack in
/// debug builds; embedders should run the interpreter on a thread with a
/// large stack (e.g. 32+ MiB) if deep recursion is expected.
const MAX_DEPTH: usize = 400;

impl Interp {
    pub fn new() -> Interp {
        let mk = |proto: Option<ObjRef>| Rc::new(RefCell::new(Obj::new(proto, ObjKind::Ord)));
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
                arraybuffer: mk(Some(obj.clone())),
                global: mk(Some(obj.clone())),
                obj,
                func,
            },
            output: Vec::new(),
            modules: Vec::new(),
            pending_label: None,
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
            call_trace: std::collections::VecDeque::new(),
            last_throw_trace: Vec::new(),
            typeof_reads: Vec::new(),
            steps: 0,
            interrupted: false,
            deadline: None,
            interrupt_site: None,
            trace_calls: std::env::var_os("ROWSER_CALL_TRACE").is_some(),
            interned: FxSet::default(),
            fn_hoist: FxMap::default(),
            slice_hoist: FxMap::default(),
        }
    }

    /// Intern a string so repeated `Rc<str>` key/name uses become cheap
    /// refcount bumps instead of fresh allocations.
    pub fn intern(&mut self, s: &str) -> Rc<str> {
        if let Some(r) = self.interned.get(s).cloned() {
            return r;
        }
        let r: Rc<str> = Rc::from(s);
        self.interned.insert(r.clone());
        r
    }

    /// Poll the execution budget. Called once per statement: cheap while
    /// unbounded, a hard uncatchable unwind once the deadline trips —
    /// JS try/catch can't swallow it because subsequent statements keep
    /// erroring until the embedder clears `interrupted`.
    #[inline]
    pub fn tick(&mut self) -> Result<(), Value> {
        if self.interrupted {
            return Err(self.interrupt_err());
        }
        self.steps = self.steps.wrapping_add(1);
        if self.steps & 0x3ff == 0 {
            if let Some(d) = self.deadline {
                if std::time::Instant::now() >= d {
                    self.interrupted = true;
                    return Err(self.interrupt_err());
                }
            }
        }
        Ok(())
    }

    fn interrupt_err(&mut self) -> Value {
        self.range_err("script execution budget exceeded")
    }

    /// Arm a wall-clock budget for the next turn; clears a prior trip.
    pub fn arm_deadline(&mut self, budget: std::time::Duration) {
        self.deadline = Some(std::time::Instant::now() + budget);
        self.interrupted = false;
    }

    /// Disarm the budget at turn end (also clears a trip).
    pub fn disarm_deadline(&mut self) {
        self.deadline = None;
        self.interrupted = false;
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
        // spec: error own props are writable+configurable, non-enumerable
        let p = |v: Value| Prop {
            val: v,
            writable: true,
            enumerable: false,
            configurable: true,
            get: None,
            set: None,
        };
        o.borrow_mut()
            .props
            .insert(PropKey::from_str("message"), p(Value::Str(Rc::from(msg))));
        o.borrow_mut()
            .props
            .insert(PropKey::from_str("name"), p(Value::Str(Rc::from(name))));
        o.borrow_mut().props.insert(
            PropKey::from_str("stack"),
            p(Value::Str(Rc::from(format!("{name}: {msg}")))),
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
    /// Walk the scope chain by reference — no `Rc` clone per hop. Only the
    /// hit/miss results clone values.
    pub fn env_lookup(&self, env: &EnvRef, name: &str) -> Option<Value> {
        let mut cur: &EnvRef = env;
        loop {
            if let Some(b) = cur.vars.borrow().get(name) {
                return Some(b.val.clone());
            }
            match &cur.parent {
                Some(p) => cur = p,
                None => break,
            }
        }
        // fall back to global object properties (globalThis.x = …)
        self.obj_get_raw_str(&self.p.global, name)
    }

    pub fn env_assign(&mut self, env: &EnvRef, name: &str, v: Value) -> Result<(), Value> {
        let mut cur: &EnvRef = env;
        loop {
            {
                let mut vars = cur.vars.borrow_mut();
                if let Some(b) = vars.get_mut(name) {
                    if !b.mutable {
                        drop(vars);
                        return Err(
                            self.type_err(format!("Assignment to constant variable '{name}'"))
                        );
                    }
                    b.val = v;
                    return Ok(());
                }
            }
            match &cur.parent {
                Some(p) => cur = p,
                None => break,
            }
        }
        // sloppy: create global; mirror onto global object
        let k = self.intern(name);
        self.global.vars.borrow_mut().insert(
            k.clone(),
            Binding {
                val: v.clone(),
                mutable: true,
            },
        );
        self.p
            .global
            .borrow_mut()
            .props
            .insert(PropKey::S(k), Prop::data(v));
        Ok(())
    }

    /// `ident++`/`ident--` fast path: update a numeric binding in one scope
    /// walk. Returns `(old, new)`; `None` → fall back to generic
    /// eval+assign (binding miss or non-numeric value).
    fn env_update_num(
        &mut self,
        env: &EnvRef,
        name: &str,
        delta: f64,
    ) -> Option<Result<(f64, f64), Value>> {
        let mut cur: &EnvRef = env;
        loop {
            {
                let mut vars = cur.vars.borrow_mut();
                if let Some(b) = vars.get_mut(name) {
                    if !b.mutable {
                        drop(vars);
                        return Some(Err(
                            self.type_err(format!("Assignment to constant variable '{name}'"))
                        ));
                    }
                    if let Value::Num(n) = b.val {
                        let new = n + delta;
                        b.val = Value::Num(new);
                        return Some(Ok((n, new)));
                    }
                    return None;
                }
            }
            match &cur.parent {
                Some(p) => cur = p,
                None => return None,
            }
        }
    }

    /// declare in the given scope (let/const/block fns)
    pub fn declare(&mut self, env: &EnvRef, name: &str, v: Value, mutable: bool) {
        let k = self.intern(name);
        env.vars.borrow_mut().insert(
            k.clone(),
            Binding {
                val: v.clone(),
                mutable,
            },
        );
        if Rc::ptr_eq(env, &self.global) {
            self.p
                .global
                .borrow_mut()
                .props
                .insert(PropKey::S(k), Prop::data(v));
        }
    }

    /// `var` — walks to nearest function scope.
    pub fn declare_var(&mut self, env: &EnvRef, name: &str, v: Value) {
        let k = self.intern(name);
        self.declare_var_rc(env, &k, v);
    }

    /// `declare_var` with a pre-interned key (hoist cache reuse).
    fn declare_var_rc(&mut self, env: &EnvRef, name: &Rc<str>, v: Value) {
        let mut cur = env.clone();
        loop {
            if cur.var_boundary.get() || cur.parent.is_none() {
                break;
            }
            let next = cur.parent.clone().unwrap();
            cur = next;
        }
        let exists = cur.vars.borrow().contains_key(name.as_ref());
        if !exists {
            cur.vars.borrow_mut().insert(
                name.clone(),
                Binding {
                    val: v.clone(),
                    mutable: true,
                },
            );
        } else if !matches!(v, Value::Und) {
            cur.vars
                .borrow_mut()
                .get_mut(name.as_ref())
                .unwrap()
                .val = v.clone();
        }
        if Rc::ptr_eq(&cur, &self.global) {
            self.p
                .global
                .borrow_mut()
                .props
                .insert(PropKey::S(name.clone()), Prop::data(v));
        }
    }

    fn find_fn_scope(&self, env: &EnvRef) -> EnvRef {
        let mut cur: &EnvRef = env;
        loop {
            if cur.is_fn || cur.parent.is_none() {
                return cur.clone();
            }
            let next = cur.parent.as_ref().unwrap();
            cur = next;
        }
    }

    fn find_home(&self, env: &EnvRef) -> Option<(EnvRef, ObjRef)> {
        let mut cur: &EnvRef = env;
        loop {
            if let Some(h) = cur.home.borrow().clone() {
                return Some((cur.clone(), h));
            }
            if cur.is_fn {
                return None;
            }
            match &cur.parent {
                Some(p) => cur = p,
                None => return None,
            }
        }
    }

    // -- object helpers -------------------------------------------------------
    pub fn new_obj(&self) -> ObjRef {
        Rc::new(RefCell::new(Obj::new(
            Some(self.p.obj.clone()),
            ObjKind::Ord,
        )))
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
            Err(e) => Err(self.syntax_err(format!("Invalid regular expression: /{pattern}/: {e}"))),
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

    /// `set_builtin` with explicit attributes — for spec-mandated
    /// non-writable / non-configurable installs (constants, fn metadata).
    pub fn set_attr(&self, o: &ObjRef, key: &str, v: Value, writable: bool, configurable: bool) {
        o.borrow_mut().props.insert(
            PropKey::from_str(key),
            Prop {
                val: v,
                writable,
                enumerable: false,
                configurable,
                get: None,
                set: None,
            },
        );
    }

    pub fn native(&self, name: &str, f: NativeFn) -> Value {
        let o = Rc::new(RefCell::new(Obj::new(
            Some(self.p.func.clone()),
            ObjKind::Func(Func::Native {
                name: Rc::from(name),
                f,
            }),
        )));
        // fn .length/.name: {writable:false, enumerable:false, configurable:true}
        self.set_attr(&o, "length", Value::Num(0.0), false, true);
        self.set_attr(&o, "name", Value::Str(Rc::from(name)), false, true);
        Value::Obj(o)
    }

    /// raw own-prop lookup (no proto walk, no getters)
    pub fn obj_get_raw(&self, o: &ObjRef, key: &PropKey) -> Option<Value> {
        if let Some(v) = view_own_prop(o, key) {
            return Some(v);
        }
        o.borrow().props.get(key).map(|p| p.val.clone())
    }

    /// `obj_get_raw` by borrowed string key — no key allocation.
    pub fn obj_get_raw_str(&self, o: &ObjRef, key: &str) -> Option<Value> {
        if let Some(v) = view_own_prop_str(o, key) {
            return Some(v);
        }
        o.borrow().props.get_str(key).map(|p| p.val.clone())
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
                    if matches!(base, Value::Und) {
                        "undefined"
                    } else {
                        "null"
                    }
                )))
            }
            Value::Obj(o) => {
                let r = self.obj_get(o, key, base);
                // debug hook: watch $$typeof reads to triage element-type errors
                if let PropKey::S(k) = key {
                    if k.as_ref() == "$$typeof" {
                        let got = match &r {
                            Ok(v) => self.inspect(v, 0),
                            Err(_e) => "<throw>".to_string(),
                        };
                        let obj_desc = self.inspect(base, 0);
                        self.typeof_reads.push(format!("{got} <- {obj_desc:.160}"));
                    }
                }
                r
            }
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

    /// `get_prop` by borrowed string key — the `obj.name` fast path that
    /// avoids building a `PropKey`/interning an `Rc` per member access.
    pub fn get_prop_str(&mut self, base: &Value, key: &str) -> ERes {
        match base {
            Value::Und | Value::Null => Err(self.type_err(format!(
                "Cannot read properties of {} (reading '{key}')",
                if matches!(base, Value::Und) {
                    "undefined"
                } else {
                    "null"
                }
            ))),
            Value::Obj(o) => {
                let r = self.obj_get_str(o, key, base);
                // debug hook: watch $$typeof reads to triage element-type errors
                if key == "$$typeof" {
                    let got = match &r {
                        Ok(v) => self.inspect(v, 0),
                        Err(_e) => "<throw>".to_string(),
                    };
                    let obj_desc = self.inspect(base, 0);
                    self.typeof_reads.push(format!("{got} <- {obj_desc:.160}"));
                }
                r
            }
            Value::Str(s) => {
                if key == "length" {
                    return Ok(Value::Num(s.chars().count() as f64));
                }
                if let Some(i) = arr_index(key) {
                    return Ok(s
                        .chars()
                        .nth(i)
                        .map(|c| Value::Str(Rc::from(c.to_string().as_str())))
                        .unwrap_or(Value::Und));
                }
                let p = self.p.string.clone();
                self.obj_get_str(&p, key, base)
            }
            Value::Num(_) => {
                let p = self.p.number.clone();
                self.obj_get_str(&p, key, base)
            }
            Value::Bool(_) => {
                let p = self.p.boolean.clone();
                self.obj_get_str(&p, key, base)
            }
            Value::Sym(..) => {
                let p = self.p.symbol.clone();
                self.obj_get_str(&p, key, base)
            }
        }
    }

    /// proto-chain walk; `receiver` is the `this` for getters.
    fn obj_get(&mut self, o: &ObjRef, key: &PropKey, receiver: &Value) -> ERes {
        let mut cur = Some(o.clone());
        while let Some(c) = cur {
            {
                if let Some(v) = view_own_prop(&c, key) {
                    return Ok(v);
                }
                let b = c.borrow();
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

    /// `obj_get` by borrowed string key.
    fn obj_get_str(&mut self, o: &ObjRef, key: &str, receiver: &Value) -> ERes {
        let mut cur = Some(o.clone());
        while let Some(c) = cur {
            {
                if let Some(v) = view_own_prop_str(&c, key) {
                    return Ok(v);
                }
                let b = c.borrow();
                if let Some(p) = b.props.get_str(key) {
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
                    if matches!(base, Value::Und) {
                        "undefined"
                    } else {
                        "null"
                    }
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

    fn obj_set(
        &mut self,
        o: &ObjRef,
        key: PropKey,
        v: Value,
        receiver: &Value,
    ) -> Result<(), Value> {
        // integer-indexed writes + read-only virtuals on binary views
        {
            let b = o.borrow();
            match &b.kind {
                ObjKind::Typed {
                    buf,
                    off,
                    len,
                    kind,
                } => {
                    if let PropKey::S(s) = &key {
                        if let Some(idx) = arr_index(s) {
                            let (buf, off, len, kind) = (buf.clone(), *off, *len, *kind);
                            drop(b);
                            if idx >= len {
                                return Ok(()); // OOB write: silently ignored
                            }
                            let n = self.to_num(&v)?;
                            if let Some(bytes) = buffer_bytes(&buf) {
                                kind.write(&mut bytes.borrow_mut(), off + idx * kind.size(), n);
                            }
                            return Ok(());
                        }
                        if matches!(
                            s.as_ref(),
                            "length" | "byteLength" | "byteOffset" | "buffer"
                        ) {
                            return Ok(()); // accessor-only props: sloppy no-op
                        }
                    }
                }
                ObjKind::Bytes(_) => {
                    if let PropKey::S(s) = &key {
                        if s.as_ref() == "byteLength" {
                            return Ok(());
                        }
                    }
                }
                ObjKind::View { .. } => {
                    if let PropKey::S(s) = &key {
                        if matches!(s.as_ref(), "byteLength" | "byteOffset" | "buffer") {
                            return Ok(());
                        }
                    }
                }
                _ => {}
            }
        }
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
                    // dense backing — writes past the cap are holey-array
                    // semantics we can't honor; drop instead of OOMing on
                    // arr[2**31] = x
                    if i >= items.len() && i <= 8_000_000 {
                        items.resize(i + 1, Value::Und);
                    }
                    if i < items.len() {
                        items[i] = v;
                    }
                    return Ok(());
                }
                if s.as_ref() == "length" {
                    if let Value::Num(n) = v {
                        let n = (n.max(0.0) as usize).min(8_000_000);
                        items.resize(n, Value::Und);
                        return Ok(());
                    }
                }
            }
        }
        b.props.insert(key, Prop::data(v));
        Ok(())
    }

    /// `set_prop` by borrowed string key — `obj.name = v` fast path.
    pub fn set_prop_str(&mut self, base: &Value, key: &str, v: Value) -> Result<(), Value> {
        match base {
            Value::Und | Value::Null => Err(self.type_err(format!(
                "Cannot set properties of {} (setting '{key}')",
                if matches!(base, Value::Und) {
                    "undefined"
                } else {
                    "null"
                }
            ))),
            Value::Obj(o) => self.obj_set_str(o, key, v, base),
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
                    if let Some(p) = c.borrow().props.get_str(key) {
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

    /// `obj_set` by borrowed string key.
    fn obj_set_str(&mut self, o: &ObjRef, key: &str, v: Value, receiver: &Value) -> Result<(), Value> {
        // integer-indexed writes + read-only virtuals on binary views
        {
            let b = o.borrow();
            match &b.kind {
                ObjKind::Typed {
                    buf,
                    off,
                    len,
                    kind,
                } => {
                    if let Some(idx) = arr_index(key) {
                        let (buf, off, len, kind) = (buf.clone(), *off, *len, *kind);
                        drop(b);
                        if idx >= len {
                            return Ok(()); // OOB write: silently ignored
                        }
                        let n = self.to_num(&v)?;
                        if let Some(bytes) = buffer_bytes(&buf) {
                            kind.write(&mut bytes.borrow_mut(), off + idx * kind.size(), n);
                        }
                        return Ok(());
                    }
                    if matches!(key, "length" | "byteLength" | "byteOffset" | "buffer") {
                        return Ok(()); // accessor-only props: sloppy no-op
                    }
                }
                ObjKind::Bytes(_) => {
                    if key == "byteLength" {
                        return Ok(());
                    }
                }
                ObjKind::View { .. } => {
                    if matches!(key, "byteLength" | "byteOffset" | "buffer") {
                        return Ok(());
                    }
                }
                _ => {}
            }
        }
        // walk chain looking for existing prop (setter or data)
        let mut cur = Some(o.clone());
        while let Some(c) = cur {
            let (found_accessor, found_data, is_own) = {
                let b = c.borrow();
                match b.props.get_str(key) {
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
                    if !writable {
                        return Ok(()); // non-writable: sloppy no-op
                    }
                    b.props.get_str_mut(key).unwrap().val = v;
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
            if let Some(i) = arr_index(key) {
                if i >= items.len() && i <= 8_000_000 {
                    items.resize(i + 1, Value::Und);
                }
                if i < items.len() {
                    items[i] = v;
                }
                return Ok(());
            }
            if key == "length" {
                if let Value::Num(n) = v {
                    let n = (n.max(0.0) as usize).min(8_000_000);
                    items.resize(n, Value::Und);
                    return Ok(());
                }
            }
        }
        b.props.insert_str(key, Prop::data(v));
        Ok(())
    }

    /// `delete_prop` by borrowed string key.
    pub fn delete_prop_str(&mut self, base: &Value, key: &str) -> bool {
        let Value::Obj(o) = base else {
            return true;
        };
        let mut b = o.borrow_mut();
        if let ObjKind::Arr(items) = &mut b.kind {
            if let Some(i) = arr_index(key) {
                if i < items.len() {
                    items[i] = Value::Und;
                    return true;
                }
            }
        }
        if b.props.get_str(key).is_some_and(|p| !p.configurable) {
            return false;
        }
        b.props.remove_str(key);
        true
    }

    pub fn has_prop(&mut self, base: &Value, key: &PropKey) -> bool {
        let Value::Obj(o) = base else {
            return false;
        };
        let mut cur = Some(o.clone());
        while let Some(c) = cur {
            {
                let b = c.borrow();
                match &b.kind {
                    ObjKind::Arr(items) => {
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
                    ObjKind::Typed { len, .. } => {
                        if let PropKey::S(s) = key {
                            if matches!(
                                s.as_ref(),
                                "length" | "byteLength" | "byteOffset" | "buffer"
                            ) {
                                return true;
                            }
                            if let Some(i) = arr_index(s) {
                                if i < *len {
                                    return true;
                                }
                            }
                        }
                    }
                    ObjKind::Bytes(_) => {
                        if let PropKey::S(s) = key {
                            if s.as_ref() == "byteLength" {
                                return true;
                            }
                        }
                    }
                    ObjKind::View { .. } => {
                        if let PropKey::S(s) = key {
                            if matches!(s.as_ref(), "byteLength" | "byteOffset" | "buffer") {
                                return true;
                            }
                        }
                    }
                    _ => {}
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
        if b.props.get(key).is_some_and(|p| !p.configurable) {
            return false;
        }
        b.props.remove(key);
        true
    }

    /// enumerable own + inherited string keys (for..in), deduped
    pub fn enum_keys(&self, base: &Value) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen: FxSet<String> = FxSet::default();
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
                match &b.kind {
                    ObjKind::Arr(items) => {
                        for i in 0..items.len() {
                            let k = i.to_string();
                            if seen.insert(k.clone()) {
                                out.push(k);
                            }
                        }
                    }
                    ObjKind::Typed { len, .. } => {
                        for i in 0..*len {
                            let k = i.to_string();
                            if seen.insert(k.clone()) {
                                out.push(k);
                            }
                        }
                    }
                    _ => {}
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
        match &b.kind {
            ObjKind::Arr(items) => {
                for i in 0..items.len() {
                    out.push(i.to_string());
                }
            }
            ObjKind::Typed { len, .. } => {
                for i in 0..*len {
                    out.push(i.to_string());
                }
            }
            _ => {}
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
            Value::Sym(..) => {
                return Err(self.type_err("Cannot convert a Symbol value to a number"))
            }
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
                        let inner: Vec<String> =
                            items.iter().map(|i| self.inspect(i, depth + 1)).collect();
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
                    ObjKind::Bytes(b) => format!("ArrayBuffer({})", b.borrow().len()),
                    ObjKind::Typed { kind, len, .. } => {
                        format!("{}({len})", kind.type_name())
                    }
                    ObjKind::View { .. } => "DataView {}".into(),
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
            let iv = self.inspect(f, 0);
            return Err(self.type_err(format!("{tn} is not a function (callee {iv})")));
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
                scope.var_boundary.set(true);
                if !def.is_arrow {
                    *scope.thisv.borrow_mut() = this;
                    *scope.home.borrow_mut() = home;
                    *scope.ctor_fn.borrow_mut() = f.clone();
                }
                // arrows inherit this/home/new.target lexically — is_fn=false
                // makes lookups walk through automatically.
                self.bind_params(&scope, &def.params, &args)?;
                let h = self.hoisted_fn(&def);
                if !def.is_arrow && h.uses_arguments {
                    let arr = self.new_arr(args);
                    let k = self.intern("arguments");
                    scope.vars.borrow_mut().insert(
                        k,
                        Binding {
                            val: Value::Obj(arr),
                            mutable: true,
                        },
                    );
                }
                self.apply_hoisted(&h, &scope);
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
                // — but only ctors (make_ctor sets `.prototype`); plain
                // methods like Math.abs are not [[Construct]]-able
                let proto_prop = self
                    .obj_get_raw(&o.clone(), &PropKey::from_str("prototype"))
                    .unwrap_or(Value::Und);
                if matches!(proto_prop, Value::Und) {
                    return Err(self.type_err(format!("{name} is not a constructor")));
                }
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
                    let nt_obj = if let Value::Obj(no) = &nt {
                        Some(no.clone())
                    } else {
                        None
                    };
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
                    _ => Value::Obj(Rc::new(RefCell::new(Obj::new(proto, ObjKind::Ord)))),
                };
                let scope = Scope::new(Some(env), true);
                *scope.thisv.borrow_mut() = this;
                *scope.new_target.borrow_mut() = nt;
                *scope.ctor_fn.borrow_mut() = f.clone();
                self.bind_params(&scope, &def.params, &args)?;
                let h = self.hoisted_fn(&def);
                if h.uses_arguments {
                    let arr = self.new_arr(args);
                    let k = self.intern("arguments");
                    scope.vars.borrow_mut().insert(
                        k,
                        Binding {
                            val: Value::Obj(arr),
                            mutable: true,
                        },
                    );
                }
                self.apply_hoisted(&h, &scope);
                self.depth += 1;
                let r = self.exec_block(&def.body, &scope);
                self.depth -= 1;
                let this_out = scope.thisv.borrow().clone();
                match r {
                    Ok(Completion::Return(v))
                        if self.is_callable(&v) || matches!(v, Value::Obj(_)) =>
                    {
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

    fn bind_params(
        &mut self,
        scope: &EnvRef,
        params: &[Param],
        args: &[Value],
    ) -> Result<(), Value> {
        let mut i = 0;
        for p in params {
            match p {
                Param::Ident(name) => {
                    let v = args.get(i).cloned().unwrap_or(Value::Und);
                    let k = self.intern(name);
                    scope.vars.borrow_mut().insert(
                        k,
                        Binding {
                            val: v,
                            mutable: true,
                        },
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
                                        let val = self
                                            .obj_get_raw(src, &PropKey::from_str(&k))
                                            .unwrap_or(Value::Und);
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
                                        let val = self
                                            .obj_get_raw(src, &PropKey::from_str(&k))
                                            .unwrap_or(Value::Und);
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
            Value::Str(s) => Ok(s
                .chars()
                .map(|c| Value::Str(Rc::from(c.to_string().as_str())))
                .collect()),
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
    pub fn hoist_body(&mut self, stmts: &[Stmt], scope: &EnvRef) -> Result<(), Value> {
        // uncached path — safe for arbitrary caller-owned slices
        let h = collect_hoisted_owned(stmts);
        self.apply_hoisted(&h, scope);
        Ok(())
    }

    /// declare the precomputed hoisted vars/fns into `scope`'s var boundary.
    fn apply_hoisted(&mut self, h: &Hoisted, scope: &EnvRef) {
        for name in &h.vars {
            self.declare_var_rc(scope, name, Value::Und);
        }
        for f in &h.fns {
            if let Some(name) = f.name.clone() {
                let fv = self.make_fn(f.clone(), scope, CtorKind::NotClass);
                self.declare_var(scope, &name, fv);
            }
        }
    }

    /// Cached hoist scan for a function body. Called on EVERY function
    /// call, so the AST walk happens once per `FnDef`, not per call.
    fn hoisted_fn(&mut self, def: &Rc<FnDef>) -> Rc<Hoisted> {
        let key = Rc::as_ptr(def) as usize;
        if let Some((_, h)) = self.fn_hoist.get(&key) {
            return h.clone();
        }
        let mut h = collect_hoisted_owned(&def.body);
        h.uses_arguments = !def.is_arrow && fndef_uses_arguments(def);
        let h = Rc::new(h);
        // store the def Rc alongside the entry: pinning it keeps the
        // pointer from ever being reused, so the key stays unique forever
        self.fn_hoist.insert(key, (def.clone(), h.clone()));
        h
    }

    /// Cached hoist scan for parsed-AST statement slices (block/try bodies).
    /// Safe because parsed AST lives for the whole `run` and the cache is
    /// cleared at run start; function bodies use `hoisted_fn` instead.
    fn hoisted_slice(&mut self, stmts: &[Stmt]) -> Rc<Hoisted> {
        let key = (stmts.as_ptr() as usize, stmts.len());
        if let Some(h) = self.slice_hoist.get(&key) {
            return h.clone();
        }
        let h = Rc::new(collect_hoisted_owned(stmts));
        self.slice_hoist.insert(key, h.clone());
        h
    }

    fn hoist_stmts(&mut self, stmts: &[Stmt], scope: &EnvRef) -> Result<(), Value> {
        let h = self.hoisted_slice(stmts);
        self.apply_hoisted(&h, scope);
        Ok(())
    }

    /// Execute a braced body (block / try / finally). A fresh child scope is
    /// only needed when the body would declare lexical bindings into it
    /// (let/const/class, incl. single-statement-position decls like
    /// `if (c) let y`); `var`/`function` hoisting resolves to the same
    /// var-boundary scope either way, so decl-free bodies run in `env`.
    fn exec_hoisted_block(&mut self, stmts: &[Stmt], env: &EnvRef) -> SRes {
        let needs_scope = stmts.iter().any(stmt_binds_lexically);
        if needs_scope {
            let scope = Scope::new(Some(env.clone()), false);
            self.hoist_stmts(stmts, &scope)?;
            self.exec_block(stmts, &scope)
        } else {
            self.hoist_stmts(stmts, env)?;
            self.exec_block(stmts, env)
        }
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
        self.set_attr(&o, "name", Value::Str(Rc::from(name.as_str())), false, true);
        self.set_attr(&o, "length", Value::Num(param_count(&def.params) as f64), false, true);
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

    pub fn exec_stmt(&mut self, s: &Stmt, env: &EnvRef) -> SRes {
        if self.interrupted {
            return Err(self.interrupt_err());
        }
        self.steps = self.steps.wrapping_add(1);
        if self.steps & 0x3ff == 0 {
            if let Some(d) = self.deadline {
                if std::time::Instant::now() >= d {
                    self.interrupted = true;
                    let mut dbg = format!("{s:?}");
                    dbg.truncate(400);
                    self.interrupt_site = Some(dbg);
                    return Err(self.interrupt_err());
                }
            }
        }
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
            Stmt::Block(stmts) => self.exec_hoisted_block(stmts, env),
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
                let my_label = self.pending_label.take();
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
                            if l.is_some() {
                                return Ok(Completion::Break(l));
                            }
                            break;
                        }
                        Completion::Continue(l) => {
                            if l.is_none() || l.as_deref() == my_label.as_deref() {
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
                let my_label = self.pending_label.take();
                let mut last = Value::Und;
                loop {
                    match self.exec_stmt(body, env)? {
                        Completion::Normal(v) => last = v,
                        Completion::Empty => {}
                        Completion::Break(l) => {
                            if l.is_some() {
                                return Ok(Completion::Break(l));
                            }
                            break;
                        }
                        Completion::Continue(l) => {
                            if !(l.is_none() || l.as_deref() == my_label.as_deref()) {
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
                let my_label = self.pending_label.take();
                // `let`/`const` in the init needs a per-loop scope, as does
                // a lexical decl in single-statement body position; `var`
                // and expression inits resolve identically on `env`.
                let loop_scope;
                let scope: &EnvRef = match init {
                    Some(ForInit::Var(kind, _)) if *kind != VarKind::Var => {
                        loop_scope = Scope::new(Some(env.clone()), false);
                        &loop_scope
                    }
                    _ if stmt_binds_lexically(body) => {
                        loop_scope = Scope::new(Some(env.clone()), false);
                        &loop_scope
                    }
                    _ => env,
                };
                if let Some(init) = init {
                    match init {
                        ForInit::Var(kind, decls) => {
                            for (pat, ie) in decls {
                                let v = match ie {
                                    Some(e) => self.eval_expr(e, scope)?,
                                    None => Value::Und,
                                };
                                match pat {
                                    Pattern::Ident(n) => match kind {
                                        VarKind::Var => self.declare_var(scope, n, v),
                                        _ => self.declare(scope, n, v, *kind != VarKind::Const),
                                    },
                                    other => self.bind_pattern(scope, other, v, *kind, true)?,
                                }
                            }
                        }
                        ForInit::Expr(e) => {
                            self.eval_expr(e, scope)?;
                        }
                    }
                }
                let mut last = Value::Und;
                loop {
                    if let Some(t) = test {
                        let tv = self.eval_expr(t, scope)?;
                        if !self.to_bool(&tv) {
                            break;
                        }
                    }
                    match self.exec_stmt(body, scope)? {
                        Completion::Normal(v) => last = v,
                        Completion::Empty => {}
                        Completion::Break(l) => {
                            if l.is_some() {
                                return Ok(Completion::Break(l));
                            }
                            break;
                        }
                        Completion::Continue(l) => {
                            if !(l.is_none() || l.as_deref() == my_label.as_deref()) {
                                return Ok(Completion::Continue(l));
                            }
                        }
                        r => return Ok(r),
                    }
                    if let Some(u) = update {
                        self.eval_expr(u, scope)?;
                    }
                }
                Ok(Completion::Normal(last))
            }
            Stmt::ForIn { left, right, body } => {
                let my_label = self.pending_label.take();
                let rv = self.eval_expr(right, env)?;
                let keys = self.enum_keys(&rv);
                // `let`/`const` loop targets need a per-loop scope, as does
                // a lexical decl in single-statement body position; `var`
                // and expression targets resolve identically on `env`.
                let loop_scope;
                let scope: &EnvRef = match left {
                    ForTarget::Var(kind, _) if *kind != VarKind::Var => {
                        loop_scope = Scope::new(Some(env.clone()), false);
                        &loop_scope
                    }
                    _ if stmt_binds_lexically(body) => {
                        loop_scope = Scope::new(Some(env.clone()), false);
                        &loop_scope
                    }
                    _ => env,
                };
                let mut last = Value::Und;
                for k in keys {
                    self.bind_for_target(scope, left, Value::Str(Rc::from(k.as_str())))?;
                    match self.exec_stmt(body, scope)? {
                        Completion::Normal(v) => last = v,
                        Completion::Empty => {}
                        Completion::Break(l) => {
                            if l.is_some() {
                                return Ok(Completion::Break(l));
                            }
                            return Ok(Completion::Normal(last));
                        }
                        Completion::Continue(l) => {
                            if !(l.is_none() || l.as_deref() == my_label.as_deref()) {
                                return Ok(Completion::Continue(l));
                            }
                        }
                        r => return Ok(r),
                    }
                }
                Ok(Completion::Normal(last))
            }
            Stmt::ForOf { left, right, body } => {
                let my_label = self.pending_label.take();
                let rv = self.eval_expr(right, env)?;
                let items = self.iter_values(&rv)?;
                // `let`/`const` loop targets need a per-loop scope, as does
                // a lexical decl in single-statement body position; `var`
                // and expression targets resolve identically on `env`.
                let loop_scope;
                let scope: &EnvRef = match left {
                    ForTarget::Var(kind, _) if *kind != VarKind::Var => {
                        loop_scope = Scope::new(Some(env.clone()), false);
                        &loop_scope
                    }
                    _ if stmt_binds_lexically(body) => {
                        loop_scope = Scope::new(Some(env.clone()), false);
                        &loop_scope
                    }
                    _ => env,
                };
                let mut last = Value::Und;
                for it in items {
                    self.bind_for_target(scope, left, it)?;
                    match self.exec_stmt(body, scope)? {
                        Completion::Normal(v) => last = v,
                        Completion::Empty => {}
                        Completion::Break(l) => {
                            if l.is_some() {
                                return Ok(Completion::Break(l));
                            }
                            return Ok(Completion::Normal(last));
                        }
                        Completion::Continue(l) => {
                            if !(l.is_none() || l.as_deref() == my_label.as_deref()) {
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
                                other @ Completion::Break(_) => return Ok(other),
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
                let mut res = self.exec_hoisted_block(block, env);
                if let Err(exc) = res {
                    if let Some(h) = handler {
                        if let Some(p) = param {
                            // `catch (e)` param binds into the catch scope
                            let catch_scope = Scope::new(Some(env.clone()), false);
                            self.bind_pattern(&catch_scope, p, exc, VarKind::Let, true)?;
                            self.hoist_stmts(h, &catch_scope)?;
                            res = self.exec_block(h, &catch_scope);
                        } else {
                            res = self.exec_hoisted_block(h, env);
                        }
                    } else {
                        res = Err(exc);
                    }
                }
                if let Some(f) = finally {
                    let fres = self.exec_hoisted_block(f, env);
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
                self.last_throw_trace = self.call_trace.iter().cloned().collect::<Vec<_>>();
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
                    self.pending_label = Some(label.clone());
                }
                let res = self.exec_stmt(inner, env);
                if is_loop && self.pending_label.as_deref() == Some(label.as_str()) {
                    self.pending_label = None;
                }
                match res {
                    Ok(Completion::Break(Some(n))) if n == *label => Ok(Completion::Empty),
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
        let mut cur: &EnvRef = scope;
        loop {
            let exists = cur.vars.borrow().contains_key(name);
            if exists {
                return self.env_assign(scope, name, v);
            }
            match &cur.parent {
                Some(p) => cur = p,
                None => break,
            }
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
                        let k = self.intern(n);
                        scope.vars.borrow_mut().insert(
                            k,
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

    fn bind_pattern_iter(
        &mut self,
        scope: &EnvRef,
        pat: &Pattern,
        v: Value,
        kind: VarKind,
    ) -> Result<(), Value> {
        // like bind_pattern but overwrites let/const bindings each iteration
        match pat {
            Pattern::Ident(n) => {
                if kind == VarKind::Var {
                    return self.for_var_assign(scope, n, v);
                }
                let k = self.intern(n);
                scope.vars.borrow_mut().insert(
                    k,
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
                let sp = self.get_prop(&Value::Obj(sc.clone()), &PropKey::from_str("prototype"))?;
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
            Some(_) => return Err(self.type_err("Class extends value is not a constructor")),
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
            if derived {
                CtorKind::Derived
            } else {
                CtorKind::Base
            },
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
                        Some(if m.is_static {
                            ctor_obj.clone()
                        } else {
                            proto.clone()
                        }),
                        CtorKind::NotClass,
                    );
                    target.borrow_mut().props.insert(key, Prop::builtin(f));
                }
                MemberKind::Getter | MemberKind::Setter => {
                    let f = self.make_fn_home(
                        m.func.clone(),
                        &class_env,
                        Some(if m.is_static {
                            ctor_obj.clone()
                        } else {
                            proto.clone()
                        }),
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
            class_env.vars.borrow_mut().insert(
                Rc::from(n.as_str()),
                Binding {
                    val: ctor_fn.clone(),
                    mutable: false,
                },
            );
        }
        Ok(ctor_fn)
    }

    // -- expressions ---------------------------------------------------------------
    pub fn eval_expr(&mut self, e: &Expr, env: &EnvRef) -> ERes {
        match e {
            Expr::Num(n) => Ok(Value::Num(*n)),
            // interned: string literals become refcount bumps, not allocs
            Expr::Str(s) => Ok(Value::Str(self.intern(s))),
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
                let mut items = Vec::with_capacity(elems.len());
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
                                ast::PropKey::Ident(s) | ast::PropKey::Str(s) => {
                                    PropKey::from_str(s)
                                }
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
                                    let ent =
                                        b.props.or_insert_with(key, || Prop::accessor(None, None));
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
                // `i++`/`++i` on a numeric ident binding: single scope walk
                if let Expr::Ident(name) = &**target {
                    let delta = if *inc { 1.0 } else { -1.0 };
                    if let Some(r) = self.env_update_num(env, name, delta) {
                        let (old, new) = r?;
                        return Ok(Value::Num(if *prefix { new } else { old }));
                    }
                }
                let cur = self.eval_expr(target, env)?;
                let n = self.to_num(&cur)?;
                let new = if *inc { n + 1.0 } else { n - 1.0 };
                self.assign_target(env, target, Value::Num(new))?;
                Ok(if *prefix {
                    Value::Num(new)
                } else {
                    Value::Num(n)
                })
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
                // `obj.name` — the parser stores dot-props as `Expr::Str`;
                // look the name up directly without building a PropKey
                if let Expr::Str(s) = &**prop {
                    return self.get_prop_str(&ov, s);
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
        if let Expr::Str(s) = prop {
            let this = fscope.thisv.borrow().clone();
            return self.obj_get_str(&parent, s, &this);
        }
        let pv = self.eval_expr(prop, env)?;
        let key = self.to_propkey(&pv)?;
        let this = fscope.thisv.borrow().clone();
        self.obj_get(&parent, &key, &this)
    }

    fn eval_args(&mut self, args: &[Arg], env: &EnvRef) -> Result<Vec<Value>, Value> {
        let mut out = Vec::with_capacity(args.len());
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
        // post-mortem debug trace — opt-in; zero cost when off
        if self.trace_calls {
            if self.call_trace.len() >= 4096 {
                self.call_trace.pop_front();
            }
            self.call_trace.push_back(callee_tag(callee));
        }
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
                // `obj.name(...)` — dot-prop fast path (no PropKey alloc)
                if let Expr::Str(s) = &**prop {
                    // `a?.()` sentinel
                    if s == "\u{0}call" {
                        let argv = self.eval_args(args, env)?;
                        return self.call(&ov, Value::Und, argv);
                    }
                    let f = self.get_prop_str(&ov, s)?;
                    if !self.is_callable(&f) {
                        let iv = self.inspect(&f, 0);
                        return Err(
                            self.type_err(format!("{s} is not a function (got {iv})"))
                        );
                    }
                    let argv = self.eval_args(args, env)?;
                    return self.call(&f, ov, argv);
                }
                let pv = self.eval_expr(prop, env)?;
                let key = self.to_propkey(&pv)?;
                let f = self.get_prop(&ov, &key)?;
                if !self.is_callable(&f) {
                    let ks = self.key_str(&key);
                    let iv = self.inspect(&f, 0);
                    return Err(self.type_err(format!("{ks} is not a function (got {iv})")));
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
                        Some(v) => {
                            let tn = self.type_name(&v);
                            return Ok(Value::Str(self.intern(tn)));
                        }
                        None => return Ok(Value::Str(self.intern("undefined"))),
                    }
                }
                let v = self.eval_expr(expr, env)?;
                let tn = self.type_name(&v);
                Ok(Value::Str(self.intern(tn)))
            }
            UnaryOp::Void => {
                self.eval_expr(expr, env)?;
                Ok(Value::Und)
            }
            UnaryOp::Await => self.eval_expr(expr, env),
            UnaryOp::Delete => match expr {
                Expr::Member { obj, prop, .. } => {
                    let ov = self.eval_expr(obj, env)?;
                    if let Expr::Str(s) = &**prop {
                        return Ok(Value::Bool(self.delete_prop_str(&ov, s)));
                    }
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
                // numeric fast path — skips to_primitive/to_num round-trips
                if let (Value::Num(a), Value::Num(b)) = (&l, &r) {
                    Value::Num(*a + *b)
                } else {
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
                // numeric fast path (NaN comparisons are false in Rust,
                // matching the explicit is_nan check in the general path)
                if let (Value::Num(a), Value::Num(b)) = (&l, &r) {
                    return Ok(Value::Bool(match op {
                        Lt => a < b,
                        Le => a <= b,
                        Gt => a > b,
                        Ge => a >= b,
                        _ => unreachable!(),
                    }));
                }
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
                    _ => {
                        return Err(
                            self.type_err("Right-hand side of 'instanceof' is not an object")
                        )
                    }
                };
                let Value::Obj(want) = proto else {
                    return Err(self.type_err("Right-hand side of 'instanceof' is not callable"));
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
                if let Expr::Str(s) = &**prop {
                    self.set_prop_str(&ov, s, v.clone())?;
                    return Ok(v);
                }
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
        // slice-hoist keys are raw AST pointers: entries from prior runs
        // could collide with freshly parsed addresses, so reset per run
        // (the new AST is fully allocated before this point)
        self.slice_hoist.clear();
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
            other => self.to_str(other).unwrap_or_else(|_| "<exception>".into()),
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
        return u64::from_str_radix(h, 16)
            .map(|v| v as f64)
            .unwrap_or(f64::NAN);
    }
    if let Some(h) = t.strip_prefix("0o").or_else(|| t.strip_prefix("0O")) {
        return u64::from_str_radix(h, 8)
            .map(|v| v as f64)
            .unwrap_or(f64::NAN);
    }
    if let Some(h) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
        return u64::from_str_radix(h, 2)
            .map(|v| v as f64)
            .unwrap_or(f64::NAN);
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
    let abs = n.abs();
    if (1e-6..1e21).contains(&abs) {
        // decimal notation — integral values print full digits ({:.0}
        // handles magnitudes past i64::MAX where `as i64` saturates);
        // non-integral values use Display's shortest round-trip form
        return if n == n.trunc() {
            format!("{n:.0}")
        } else {
            format!("{n}")
        };
    }
    // exponential: Rust's {:e} gives "1e21"/"1.5e-7"; JS wants an
    // explicit '+' on positive exponents and no leading-zero padding
    let s = format!("{n:e}");
    match s.find('e') {
        Some(i) => {
            let (m, e) = (&s[..i], &s[i + 1..]);
            if let Some(d) = e.strip_prefix('-') {
                format!("{m}e-{d}")
            } else {
                format!("{m}e+{e}")
            }
        }
        None => s,
    }
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

/// Owned (cacheable) form of `collect_hoisted`.
fn collect_hoisted_owned(stmts: &[Stmt]) -> Hoisted {
    let mut fns: Vec<&Rc<FnDef>> = Vec::new();
    let mut vars: Vec<&str> = Vec::new();
    collect_hoisted(stmts, &mut fns, &mut vars);
    Hoisted {
        fns: fns.into_iter().cloned().collect(),
        vars: vars.into_iter().map(|s| Rc::<str>::from(s)).collect(),
        uses_arguments: false,
    }
}

/// Cheap per-call trace tag — replaces the old `format!("{callee:?}")`
/// which recursively debug-formatted whole expression subtrees.
fn callee_tag(callee: &Expr) -> String {
    match callee {
        Expr::Ident(n) => n.clone(),
        Expr::Member { prop, .. } => match &**prop {
            Expr::Str(s) => format!(".{s}"),
            _ => "<computed>".into(),
        },
        Expr::Super => "super".into(),
        _ => "<expr>".into(),
    }
}

// -- conservative `arguments` detection ---------------------------------------
// `call` only builds the `arguments` object when a function can observe it.
// Any identifier occurrence OR binding of the name anywhere inside the def
// (including nested function bodies, param defaults, destructured param
// names) counts — over-approximation merely falls back to the old
// always-create behavior, so detection can't change semantics.

fn fndef_uses_arguments(def: &FnDef) -> bool {
    for p in &def.params {
        match p {
            Param::Ident(n) => {
                if n == "arguments" {
                    return true;
                }
            }
            Param::Rest(p) => {
                if pat_uses_arguments(p) {
                    return true;
                }
            }
            Param::Pattern(p) => {
                if pat_uses_arguments(p) {
                    return true;
                }
            }
            Param::Default(p, d) => {
                if pat_uses_arguments(p) || expr_uses_arguments(d) {
                    return true;
                }
            }
        }
    }
    if let Some(e) = &def.arrow_expr {
        if expr_uses_arguments(e) {
            return true;
        }
    }
    def.body.iter().any(stmt_uses_arguments)
}

fn pat_uses_arguments(p: &Pattern) -> bool {
    match p {
        Pattern::Ident(n) => n == "arguments",
        Pattern::Array(elems) => elems.iter().any(|e| match e {
            ArrayPatElem::Hole => false,
            ArrayPatElem::Elem(p) | ArrayPatElem::Rest(p) => pat_uses_arguments(p),
            ArrayPatElem::Default(p, d) => {
                pat_uses_arguments(p) || expr_uses_arguments(d)
            }
        }),
        Pattern::Object(elems) => elems.iter().any(|e| {
            let key_hit = matches!(&e.key, ast::PropKey::Computed(ex) if expr_uses_arguments(ex));
            let dflt_hit = e.default.as_ref().map_or(false, expr_uses_arguments);
            key_hit || dflt_hit || pat_uses_arguments(&e.val)
        }),
    }
}

fn expr_uses_arguments(e: &Expr) -> bool {
    match e {
        Expr::Ident(n) => n == "arguments",
        Expr::Num(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Null
        | Expr::This
        | Expr::Super
        | Expr::Regex { .. } => false,
        Expr::Template(parts) => parts
            .iter()
            .any(|p| matches!(p, TemplPart::Expr(e) if expr_uses_arguments(e))),
        Expr::Array(elems) => elems.iter().any(|el| match el {
            ArrElem::Expr(e) | ArrElem::Spread(e) => expr_uses_arguments(e),
            ArrElem::Hole => false,
        }),
        Expr::Object(props) => props.iter().any(|p| {
            if matches!(&p.key, ast::PropKey::Computed(ex) if expr_uses_arguments(ex)) {
                return true;
            }
            match &p.val {
                PropVal::Expr(e) | PropVal::Spread(e) => expr_uses_arguments(e),
                PropVal::Method(d) | PropVal::Get(d) | PropVal::Set(d) => {
                    fndef_uses_arguments(d)
                }
            }
        }),
        Expr::Fn(def) => fndef_uses_arguments(def),
        Expr::Class(c) => class_uses_arguments(c),
        Expr::Unary { expr, .. } => expr_uses_arguments(expr),
        Expr::Update { target, .. } => expr_uses_arguments(target),
        Expr::Binary { l, r, .. } | Expr::Logical { l, r, .. } => {
            expr_uses_arguments(l) || expr_uses_arguments(r)
        }
        Expr::Assign { target, value, .. } => {
            expr_uses_arguments(target) || expr_uses_arguments(value)
        }
        Expr::Cond { c, t, f } => {
            expr_uses_arguments(c) || expr_uses_arguments(t) || expr_uses_arguments(f)
        }
        Expr::Call { callee, args } | Expr::New { callee, args } => {
            expr_uses_arguments(callee)
                || args.iter().any(|a| match a {
                    Arg::Expr(e) | Arg::Spread(e) => expr_uses_arguments(e),
                })
        }
        Expr::Member { obj, prop, .. } => {
            expr_uses_arguments(obj) || expr_uses_arguments(prop)
        }
        Expr::Seq(exprs) => exprs.iter().any(expr_uses_arguments),
    }
}

fn class_uses_arguments(c: &ClassDef) -> bool {
    if c.superclass.as_ref().map_or(false, expr_uses_arguments) {
        return true;
    }
    c.members.iter().any(|m| {
        matches!(&m.key, ast::PropKey::Computed(e) if expr_uses_arguments(e))
            || fndef_uses_arguments(&m.func)
    })
}

/// Would executing this statement bind a lexical (let/const/class)
/// declaration into `env`? The parser accepts declarations in
/// single-statement position (`if (c) let y`), which bind into whatever
/// scope is current — so we look through scope-transparent contexts.
/// `for`/`for-in`/`for-of` never contribute: they create their own scope
/// whenever this check says their body needs one. `Block`/`Try`/`Switch`
/// manage their own scope; `function`/`var` hoisting lands at the same
/// var-boundary either way.
fn stmt_binds_lexically(s: &Stmt) -> bool {
    match s {
        Stmt::VarDecl { kind, .. } => *kind != VarKind::Var,
        Stmt::ClassDecl(_) => true,
        Stmt::If { t, e, .. } => {
            stmt_binds_lexically(t)
                || e.as_ref().map_or(false, |s| stmt_binds_lexically(s))
        }
        Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => {
            stmt_binds_lexically(body)
        }
        Stmt::Labeled(_, inner) => stmt_binds_lexically(inner),
        Stmt::Export(ExportRec::Decl(inner)) => stmt_binds_lexically(inner),
        Stmt::Export(ExportRec::Default(_)) => true,
        _ => false,
    }
}

fn stmt_uses_arguments(s: &Stmt) -> bool {
    match s {
        Stmt::Expr(e) | Stmt::Throw(e) => expr_uses_arguments(e),
        Stmt::VarDecl { decls, .. } => decls
            .iter()
            .any(|(p, i)| pat_uses_arguments(p) || i.as_ref().map_or(false, expr_uses_arguments)),
        Stmt::FnDecl(f) => fndef_uses_arguments(f),
        Stmt::ClassDecl(c) => class_uses_arguments(c),
        Stmt::If { c, t, e } => {
            expr_uses_arguments(c)
                || stmt_uses_arguments(t)
                || e.as_ref().map_or(false, |s| stmt_uses_arguments(s))
        }
        Stmt::Block(ss) => ss.iter().any(stmt_uses_arguments),
        Stmt::Return(e) => e.as_ref().map_or(false, expr_uses_arguments),
        Stmt::Break(_) | Stmt::Continue(_) | Stmt::Empty | Stmt::Import(_) => false,
        Stmt::While { c, body } | Stmt::DoWhile { body, c } => {
            expr_uses_arguments(c) || stmt_uses_arguments(body)
        }
        Stmt::For {
            init,
            test,
            update,
            body,
        } => {
            init.as_ref().map_or(false, |i| match i {
                ForInit::Var(_, decls) => decls.iter().any(|(p, e)| {
                    pat_uses_arguments(p) || e.as_ref().map_or(false, expr_uses_arguments)
                }),
                ForInit::Expr(e) => expr_uses_arguments(e),
            }) || test.as_ref().map_or(false, expr_uses_arguments)
                || update.as_ref().map_or(false, expr_uses_arguments)
                || stmt_uses_arguments(body)
        }
        Stmt::ForIn {
            left, right, body, ..
        }
        | Stmt::ForOf {
            left, right, body, ..
        } => {
            (match left {
                ForTarget::Var(_, p) => pat_uses_arguments(p),
                ForTarget::Expr(e) => expr_uses_arguments(e),
            }) || expr_uses_arguments(right)
                || stmt_uses_arguments(body)
        }
        Stmt::Switch { disc, cases } => {
            expr_uses_arguments(disc)
                || cases.iter().any(|c| {
                    c.test.as_ref().map_or(false, expr_uses_arguments)
                        || c.body.iter().any(stmt_uses_arguments)
                })
        }
        Stmt::Try {
            block,
            param,
            handler,
            finally,
        } => {
            block.iter().any(stmt_uses_arguments)
                || param.as_ref().map_or(false, pat_uses_arguments)
                || handler
                    .as_ref()
                    .map_or(false, |h| h.iter().any(stmt_uses_arguments))
                || finally
                    .as_ref()
                    .map_or(false, |f| f.iter().any(stmt_uses_arguments))
        }
        Stmt::Labeled(_, inner) => stmt_uses_arguments(inner),
        Stmt::Export(rec) => match rec {
            ExportRec::Decl(inner) => stmt_uses_arguments(inner),
            ExportRec::Default(e) => expr_uses_arguments(e),
            _ => false,
        },
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
