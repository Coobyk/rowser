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
pub const SYM_IS_CONCAT_SPREADABLE: u64 = 10;
pub const SYM_SPECIES: u64 = 11;
pub const SYM_UNSCOPABLES: u64 = 12;
/// private-name symbol ids live at/above this bound — disjoint from
/// well-known ids (1..) and every user-allocated symbol, so
/// getOwnPropertySymbols / Reflect.ownKeys can keep them invisible
pub const PRIV_BASE: u64 = 1 << 62;
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

/// A resolved member assignment reference — evaluating a destructuring
/// target's base and key is a separate phase from storing the value.
enum LRef {
    Prop(Value, PropKey),
    /// member base + unevaluated-key value — ToPropertyKey is deferred
    /// to PutValue (ES2019+ assignment evaluation order)
    PropV(Value, Value),
    /// `super.x` — home object and receiver captured at eval time
    Super(ObjRef, Value, PropKey),
    SuperV(ObjRef, Value, Value),
    /// identifier resolved to a specific env record (the reference's
    /// [[Base]] survives mid-RHS mutation of the scope chain)
    Env(EnvRef, Rc<str>),
    /// identifier resolved through a `with` object env record — the
    /// base is the env record, so PutValue re-checks HasProperty at
    /// store time (strict: missing → ReferenceError)
    WithObj(ObjRef, Rc<str>),
    /// identifier found nowhere — strict PutValue throws, sloppy writes
    /// a global property
    Unres(Rc<str>),
}

/// A destructuring element whose member target has already resolved its
/// reference (the value read/binding step runs after).
enum PrepPat<'a> {
    Pat(&'a Pattern),
    Resolved(LRef),
}

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

/// A Property Descriptor in field form — each `Option` carries the
/// field's *presence*; `get`/`set` keep the raw value (a present
/// `undefined` differs from an absent field).
#[derive(Clone, Default)]
pub struct PDesc {
    pub value: Option<Value>,
    pub writable: Option<bool>,
    pub get: Option<Value>,
    pub set: Option<Value>,
    pub enumerable: Option<bool>,
    pub configurable: Option<bool>,
}

impl PDesc {
    /// IsAccessorDescriptor — has a [[Get]] or [[Set]] field
    pub fn is_accessor(&self) -> bool {
        self.get.is_some() || self.set.is_some()
    }
    /// IsDataDescriptor — has a [[Value]] or [[Writable]] field
    pub fn is_data(&self) -> bool {
        self.value.is_some() || self.writable.is_some()
    }
    /// IsGenericDescriptor — neither accessor nor data fields
    pub fn is_generic(&self) -> bool {
        !self.is_accessor() && !self.is_data()
    }
    /// an empty descriptor "has no fields" (ValidateAndApply early-out)
    pub fn is_empty(&self) -> bool {
        self.value.is_none()
            && self.writable.is_none()
            && self.get.is_none()
            && self.set.is_none()
            && self.enumerable.is_none()
            && self.configurable.is_none()
    }
    /// CompletePropertyDescriptor — fills absent fields: accessor descs
    /// get [[Get]]/[[Set]] = undefined, data/generic descs get
    /// [[Value]] = undefined + [[Writable]] = false; both default
    /// [[Enumerable]]/[[Configurable]] to false.
    pub fn complete(&mut self) {
        if self.is_accessor() {
            self.get.get_or_insert(Value::Und);
            self.set.get_or_insert(Value::Und);
        } else {
            self.value.get_or_insert(Value::Und);
            self.writable.get_or_insert(false);
        }
        self.enumerable.get_or_insert(false);
        self.configurable.get_or_insert(false);
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
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
    /// iteration in insertion order
    pub fn iter(&self) -> impl Iterator<Item = (&PropKey, &Prop)> + '_ {
        self.order
            .iter()
            .filter_map(move |k| self.get(k).map(|p| (k, p)))
    }
    pub fn values(&self) -> impl Iterator<Item = &Prop> + '_ {
        self.map.values().chain(self.syms.values())
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
    /// spec [[Extensible]] — false after preventExtensions/seal/freeze
    pub extensible: bool,
    /// deleted indices on array kinds (holes); `None` when dense
    pub holes: Option<std::collections::HashSet<usize>>,
    /// ArrayBuffer [[ArrayBufferData]] taken by TransferArrayBuffer
    pub detached: bool,
    /// Immutable ArrayBuffer (transferToImmutable / sliceToImmutable)
    pub immutable: bool,
    /// Resizable-ArrayBuffer max byte length (`{maxByteLength}` option)
    pub rab_max: Option<usize>,
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
        /// class instance fields: evaluated-key + initializer, run at
        /// construction (base ctor: before body; derived: after super())
        inst_fields: Option<Rc<ClassFields>>,
        /// defined while a class-field-initializer context was running —
        /// direct `eval` in this fn may not contain `arguments`
        /// (propagates to arrows created inside the initializer)
        ban_eval_args: bool,
    },
    Native {
        name: Rc<str>,
        f: NativeFn,
        /// has [[Construct]] — independent of an own `prototype` prop
        /// (Proxy is a ctor with NO `prototype` property)
        ctor: bool,
    },
}

/// instance elements captured on a class ctor — run on `this` in
/// declaration order at construction (base ctor: during BindThis;
/// derived: right after super() binds `this`)
pub struct ClassFields {
    pub elems: Vec<InstElem>,
    /// scope the initializers/methods close over (the class scope —
    /// class-name binding + computed-key environment)
    pub env: EnvRef,
    /// the class prototype — `home` for `super.prop` in initializers
    pub home: ObjRef,
}

/// one entry of ClassFields — InitializeInstanceElements runs these in
/// order: fields define an own data prop; private methods get a fresh
/// fn object per instance; private accessors merge get/set on one slot
pub enum InstElem {
    /// key, initializer, is_private (private field → non-enum/non-cfg
    /// slot), source name for NamedEvaluation on the initializer
    Field(PropKey, Option<crate::ast::Expr>, bool, Rc<str>),
    /// private instance method — ONE fn object shared across instances
    /// (private methods are defined once during class evaluation; each
    /// instance's [[PrivateElements]] entry points at the same object)
    Method(PropKey, Value),
    /// private accessor — get/set fns evaluated once at class eval
    Access(PropKey, Option<Value>, Option<Value>),
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
    /// LIVE-view iterator (Array/Map/Set keys/values/entries + arguments):
    /// `len`/elements/entries are read through `obj` fresh per step so
    /// mid-iteration mutation is observable (spec iterator semantics).
    /// mode: 0=values 1=keys 2=entries(array) 3=entries(map) 4=values(map)
    ///       5=values(set) 6=entries(set)
    IterLive { obj: ObjRef, idx: usize, mode: u8 },
    /// lazy protocol iterator — Iterator helpers, `Iterator.from` wraps,
    /// zip/concat. Steps the source's own `next` per pull.
    IterLazy(IterLazy),
    /// sentinel while a lazy iterator's next()/return() is executing —
    /// reentrant calls throw TypeError (GeneratorResume on running gen)
    IterBusy,
    /// opaque host data bag (dates etc.)
    Host(Rc<str>),
    Map(Vec<(Value, Value)>),
    Set(Vec<Value>),
    /// compiled regular expression
    Regex(Rc<crate::regexp::Regex>),
    /// IsHTMLDDA-style object — emulates `undefined` (typeof, ToBoolean,
    /// `==` null) while remaining an object and callable.
    Und,
    /// ArrayBuffer — shared byte store (Rc so views alias it)
    Bytes(Rc<RefCell<Vec<u8>>>),
    /// TypedArray view: buffer object (kind `Bytes`), byte offset,
    /// element count, element kind. `track` marks a length-tracking
    /// view on a resizable buffer (created without an explicit length):
    /// its effective length is recomputed from the buffer's current
    /// byte size on every access.
    Typed {
        buf: ObjRef,
        off: usize,
        len: usize,
        kind: TAK,
        track: bool,
    },
    /// DataView: buffer object, byte offset, byte length
    View {
        buf: ObjRef,
        off: usize,
        len: usize,
    },
    /// Proxy: target + handler objects; every MOP op consults the
    /// handler's trap first and falls back to the target when absent
    Proxy {
        target: Value,
        handler: Value,
    },
    /// suspended generator — the body runs on a coroutine stack and
    /// `yield` parks it mid-execution
    Gen(Rc<RefCell<GenCell>>),
    /// `arguments` object — indexed slots like an array; `map` links
    /// index i to a parameter binding in `env` (sloppy simple-param
    /// functions only — `env`/`map` empty for strict/unmapped)
    Args {
        items: Vec<Value>,
        env: Option<EnvRef>,
        map: Vec<Option<Rc<str>>>,
    },
}

/// what `next(v)`/`return(v)`/`throw(e)` send into a suspended generator
#[derive(Clone)]
pub enum GenIn {
    Next(Value),
    Return(Value),
    Throw(Value),
}

/// generator body completion: Ok = `return` value, Err = thrown/aborted
pub type GenRet = Result<Value, Value>;
pub type GenCoro = corosensei::Coroutine<GenIn, Value, GenRet>;

pub struct GenCell {
    /// taken out during resume so a reentrant `next()` sees `running`
    /// instead of tripping a RefCell double-borrow
    pub coro: Option<GenCoro>,
    /// not-yet-started body — the coroutine stack is only allocated on
    /// first resume so `function* g(){}` calls that are never stepped
    /// cost no stack memory
    pub pending: Option<(Rc<FnDef>, EnvRef)>,
    pub running: bool,
    /// completed — next() → {value: undefined, done: true}
    pub done: bool,
}

/// sentinel error value — a `return()` injected at a suspended yield
/// unwinds the body as an abrupt completion user try/catch can't grab
pub const GEN_ABORT_ID: u64 = u64::MAX;
pub fn gen_abort_v() -> Value {
    Value::Sym(GEN_ABORT_ID, Rc::from("generator abort"))
}
pub fn is_gen_abort(v: &Value) -> bool {
    matches!(v, Value::Sym(id, _) if *id == GEN_ABORT_ID)
}

/// Lazy-iterator state machine. `src` pulls raw values; `op` transforms.
/// Helpers keep the *source iterator object* (not a snapshot) so pulls
/// share consumption state and `return` closes propagate correctly.
#[derive(Clone)]
pub struct IterLazy {
    pub src: IterSrc,
    pub op: IterOp,
    /// helper has yielded at least once (suspended-yield) — `return()` on it
    /// closes while "executing"; before the first yield it closes while
    /// "completed" (generator state semantics)
    pub started: bool,
}

impl IterLazy {
    pub fn id(src: IterSrc) -> Self {
        IterLazy { src, op: IterOp::Id, started: false }
    }
}

/// Where a lazy iterator pulls values from.
#[derive(Clone)]
pub enum IterSrc {
    /// always done (used as a swap-out placeholder)
    Empty,
    /// materialized list (strings / already-drained values)
    List { items: Vec<Value>, idx: usize },
    /// protocol iterator — `next.call(iter)` per step; close calls
    /// `iter.return()` when present
    Proto { iter: Value, next: Value, done: bool },
    /// live-view array iteration (for-of over arrays reflects mutation)
    Arr { arr: ObjRef, idx: usize },
}

/// The transform a lazy iterator applies to its source stream.
#[derive(Clone)]
pub enum IterOp {
    Id,
    Map { f: Value, n: f64 },
    Filter { f: Value, n: f64 },
    FlatMap { f: Value, n: f64, inner: Option<Box<IterLazy>> },
    Take { left: usize },
    Drop { left: usize },
    /// `[index, value]` pairs
    Indexed { n: f64 },
    Chunks { size: usize },
    Windows { size: usize, partial: bool, buf: std::collections::VecDeque<Value> },
    /// remaining sources after the current one exhausts
    /// `Iterator.concat` — arg values wrap lazily as reached
    /// concat: (iterable, its @@iterator method-or-undefined) — GetMethod
    /// was already run eagerly at call time; only the call is lazy
    Cat {
        rest: std::collections::VecDeque<(Value, Value)>,
    },
    Zip { srcs: Vec<IterLazy>, done: Vec<bool>, mode: ZipMode, padding: Vec<Value> },
    ZipKeyed { cols: Vec<(PropKey, IterLazy)>, done: Vec<bool>, mode: ZipMode, padding: Vec<Value> },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ZipMode {
    Shortest,
    Longest,
    Strict,
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
                    v.round_ties_even() as u8
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

/// Live element capacity of a TypedArray view (`ObjKind::Typed`
/// internals). Length-tracking views on resizable buffers recompute
/// their length from the buffer's current byte size; fixed-length views
/// go out-of-bounds when the buffer shrinks below their byte range.
/// `None` = detached, out-of-bounds, or malformed buffer — the state
/// spec `ValidateTypedArray`/`IsValidIntegerIndex` tests reject;
/// `Some(n)` = current element capacity (may be 0 for a tracking view
/// that still covers the buffer's end).
pub(crate) fn typed_state(
    buf: &ObjRef,
    off: usize,
    len: usize,
    kind: TAK,
    track: bool,
) -> Option<usize> {
    if buf.borrow().detached {
        return None;
    }
    let blen = buffer_bytes(buf)?.borrow().len();
    if track {
        if off > blen {
            None
        } else {
            Some((blen - off) / kind.size())
        }
    } else if off + len.saturating_mul(kind.size()) > blen {
        None
    } else {
        Some(len)
    }
}

/// Virtual own-props for array/binary kinds: `length`/`byteLength`/
/// `byteOffset`/`buffer` and integer-indexed reads. Returns `Some` when
/// `key` resolves on the object kind itself (before stored props).
/// `&str` version so literal member lookups never build a `PropKey`.
fn view_own_prop_str(i: &Interp, o: &ObjRef, key: &str) -> Option<Value> {
    let b = o.borrow();
    match &b.kind {
        ObjKind::Arr(items) => {
            if key == "length" {
                // sparse arrays: a declared length > items.len() lives as a
                // shadow prop; effective length is the max of the two
                let n = match b.props.get(&PropKey::from_str("length")) {
                    Some(p) => match &p.val {
                        Value::Num(n) => (*n as usize).max(items.len()),
                        _ => items.len(),
                    },
                    None => items.len(),
                };
                return Some(Value::Num(n as f64));
            }
            if let Some(i) = arr_index(key) {
                // a materialized props entry owns the slot (attr changes,
                // accessor defines) — defer to the normal props path
                if b.props.contains_str(key) {
                    return None;
                }
                if b.holes.as_ref().is_some_and(|h| h.contains(&i)) {
                    return None; // hole → not an own prop
                }
                // out-of-range: NOT an own prop — a shadowing props entry
                // (accessor define) or the proto chain handles it
                return items.get(i).cloned();
            }
            None
        }
        ObjKind::Args { items, env, map } => {
            if let Some(i) = arr_index(key) {
                // a materialized prop only shadows the value once unmapped;
                // a still-mapped index keeps reading the parameter binding
                if let (Some(e), Some(Some(name))) = (env, map.get(i)) {
                    if let Some(b) = e.vars.borrow().get(name.as_ref()) {
                        return Some(b.val.clone());
                    }
                }
                if let Some(p) = b.props.get_str(key) {
                    // accessor props fall through to the normal props path
                    // so the getter actually runs
                    if !p.is_accessor() {
                        return Some(p.val.clone());
                    }
                    return None;
                }
                if i >= items.len() || b.holes.as_ref().is_some_and(|h| h.contains(&i)) {
                    return None;
                }
                return Some(items[i].clone());
            }
            None
        }
        ObjKind::Typed {
            buf,
            off,
            len,
            kind,
            track,
        } => {
            // 10.4.5.5: canonical numeric strings answer solely from the
            // element store — no props, no proto chain.
            if let Some(ni) = canonical_numeric_index(key) {
                let eff =
                    typed_state(buf, *off, *len, *kind, *track).unwrap_or(0);
                let idx = ni as usize;
                if valid_int_index(ni, eff) {
                    if let Some(bytes) = buffer_bytes(buf) {
                        let bb = bytes.borrow();
                        let n = kind.read(&bb, *off + idx * kind.size());
                        // [[ContentType]]: bigint-kind TAs surface bigint
                        // stand-in objects — rebuilt from the exact
                        // 64-bit pattern stored bit-cast in the f64
                        // slot, never from the f64 magnitude
                        return Some(crate::binary::ta_stored_pub(
                            i,
                            &Value::Obj(o.clone()),
                            n,
                        ));
                    }
                }
                return Some(Value::Und);
            }
            None
        }
        ObjKind::Bytes(_) | ObjKind::View { .. } => None,
        _ => None,
    }
}

/// bigint stand-in objects (the `__bigint` slot) compare by magnitude —
/// `8n === 8n` even across distinct object identities.
pub(crate) fn bigint_mag_eq(a: &Value, b: &Value) -> bool {
    crate::binary::bigint_cmp(a, b) == Some(std::cmp::Ordering::Equal)
}

/// engine-internal slot names — present as props for engine bookkeeping
/// but never surface as JS-visible own property keys. Gated on the
/// object's kind or the slot's value so ordinary user props that happen
/// to share the name still enumerate.
pub(crate) fn is_internal_key(i: &Interp, b: &Obj, s: &str) -> bool {
    match s {
        // TypedArray-iterator bookkeeping (`mk_ta_iter`) — the __src+__m
        // pair is the object's signature
        "__src" | "__i" | "__m" | "__done" => {
            b.props.contains_key(&PropKey::from_str("__src"))
                && b.props.contains_key(&PropKey::from_str("__m"))
        }
        // bigint-kind marker on TAs/protos — the stored value is the
        // intrinsic name
        "__name" => b.props.get_str("__name").is_some_and(|p| {
            matches!(&p.val,
                Value::Str(s) if matches!(s.as_ref(),
                    "BigInt64Array" | "BigUint64Array"))
        }),
        "__shared" => matches!(b.kind, ObjKind::Bytes(_)),
        // bigint stand-ins carry the magnitude; the proto pins identity
        "__bigint" | "__bigint_x" | "__bigint_wrap" => b
            .proto
            .as_ref()
            .is_some_and(|p| Rc::ptr_eq(p, &i.p.bigint))
            || b.props.contains_key(&PropKey::from_str("__bigint_wrap")),
        // the lazy-installed iterator proto hangs off Object.prototype
        "__taIterProto" => true,
        // wrapper/promise/date internal slots — engine installs them
        // non-enumerable; a non-enum `$`-slot is the signature
        "$value" | "$handlers" | "$ms" | "__wm" | "__ws" => b
            .props
            .get_str(s)
            .is_some_and(|p| !p.enumerable),
        _ => false,
    }
}

/// spec IsValidIntegerIndex — canonical integer, non-negative, not -0,
/// in bounds against the live element capacity.
pub(crate) fn valid_int_index(ni: f64, eff: usize) -> bool {
    ni.fract() == 0.0
        && ni >= 0.0
        && !(ni == 0.0 && ni.is_sign_negative())
        && (ni as usize) < eff
}

fn view_own_prop(i: &Interp, o: &ObjRef, key: &PropKey) -> Option<Value> {
    match key {
        PropKey::S(s) => view_own_prop_str(i, o, s),
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
            extensible: true,
            holes: None,
            detached: false,
            immutable: false,
            rab_max: None,
        }
    }
    pub fn is_callable(&self) -> bool {
        if matches!(self.kind, ObjKind::Und) {
            return true;
        }
        match &self.kind {
            ObjKind::Func(_) | ObjKind::Bound { .. } => true,
            // proxies forward callability to their target
            ObjKind::Proxy { target, .. } => {
                matches!(target, Value::Obj(t) if t.borrow().is_callable())
            }
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

pub struct Binding {
    pub val: Value,
    pub mutable: bool,
    /// in the temporal dead zone — reading throws ReferenceError
    /// (parameter bindings instantiate uninit before defaults run)
    pub uninit: bool,
    /// `delete x` succeeds — eval-declared var/fn bindings only
    pub deletable: bool,
    /// let/const/class/catch/block-fn binding — `eval("var x")` colliding
    /// with a lexical binding in scope is a SyntaxError
    pub lexical: bool,
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
    /// this scope is a function's parameter environment while defaults are
    /// being evaluated — `eval("var arguments")` / `var eval` inside it is
    /// a SyntaxError per EvalDeclarationInstantiation
    pub param_env: std::cell::Cell<bool>,
    /// `with (obj)` object environment record — bindings resolve as
    /// properties of `obj` (filtered through @@unscopables)
    pub obj_env: Option<ObjRef>,
    /// derived-ctor `this` TDZ: `this`/`super.prop`/`super()` before the
    /// super() call throw ReferenceError; cleared when super() binds it
    pub this_uninit: std::cell::Cell<bool>,
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
            param_env: std::cell::Cell::new(false),
            obj_env: None,
            this_uninit: std::cell::Cell::new(false),
        })
    }

    /// `with (obj)` scope — bindings resolve through `obj`'s properties
    pub fn with_obj(parent: EnvRef, obj: ObjRef) -> EnvRef {
        Rc::new(Scope {
            vars: RefCell::new(FxMap::default()),
            parent: Some(parent),
            is_fn: false,
            var_boundary: std::cell::Cell::new(false),
            thisv: RefCell::new(Value::Und),
            new_target: RefCell::new(Value::Und),
            home: RefCell::new(None),
            ctor_fn: RefCell::new(Value::Und),
            param_env: std::cell::Cell::new(false),
            obj_env: Some(obj),
            this_uninit: std::cell::Cell::new(false),
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
    /// break/continue carry the accumulated completion value
    /// (UpdateEmpty) — `None` = empty
    Break(Option<String>, Option<Value>),
    Continue(Option<String>, Option<Value>),
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
    /// lexical (let/const/class) declarations at this level — instantiate
    /// as uninitialized bindings at scope entry (the TDZ)
    lets: Vec<Rc<str>>,
    /// Annex B block-level function names — hoisted `undefined` into the
    /// var environment in sloppy code (block-scoped binding lives in the
    /// block; see `exec_hoisted_block` + `Stmt::FnDecl` exec)
    block_fns: Vec<Rc<str>>,
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
    pub eval_error: ObjRef,
    pub uri_error: ObjRef,
    pub agg_error: ObjRef,
    pub suppressed_error: ObjRef,
    pub promise: ObjRef,
    pub iterator: ObjRef,
    /// %ArrayIteratorPrototype% — proto of array/typed-array iterators
    pub array_iter: ObjRef,
    /// %MapIteratorPrototype%
    pub map_iter: ObjRef,
    /// %SetIteratorPrototype%
    pub set_iter: ObjRef,
    /// %StringIteratorPrototype%
    pub string_iter: ObjRef,
    /// %GeneratorPrototype% — proto of objects `function*` calls return
    pub generator: ObjRef,
    /// %GeneratorFunction.prototype% — proto of generator *functions*;
    /// its `.prototype` slot is %GeneratorPrototype%
    pub gen_func: ObjRef,
    /// %IteratorHelperPrototype% — proto of lazy map/filter/take/… results
    pub iter_helper: ObjRef,
    /// %WrapForValidIteratorPrototype% — proto of `Iterator.from` wraps
    pub wrap_iter: ObjRef,
    pub regexp: ObjRef,
    pub arraybuffer: ObjRef,
    pub global: ObjRef,
    /// %BigInt.prototype% — minimal BigInt: values are Ord objects here
    pub bigint: ObjRef,
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
    /// private-name id source — ids live in `PRIV_BASE..`, disjoint from
    /// every user-space symbol id, so enumeration can filter them
    pub priv_ctr: u64,
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
    /// a `[[Set]]` that completed without effect (sloppy-mode silent
    /// failure) — lets `Reflect.set` report the spec boolean
    pub set_failed: bool,
    /// strict-mode of the currently executing code (function bodies set it
    /// from `FnDef.strict`; script/eval set it from the directive prologue)
    pub cur_strict: bool,
    /// inside eval declaration instantiation — var/fn bindings created now
    /// are `deletable` (and global props `configurable`)
    pub eval_decl: bool,
    /// >0 while a class-field-initializer (or a fn created inside one)
    /// runs — direct `eval` source must not contain `arguments`
    pub eval_args_ban: u32,
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
    /// the installed `eval` function — direct `eval(...)` calls compare
    /// against this to pick the caller-environment evaluation path
    pub eval_fn: Option<Value>,
    /// eager-generator yield collectors: `function*` bodies push one Vec
    /// per running generator call; `$yield` appends to the top.
    /// yielder stack of in-flight generator coroutines — `$yield` suspends
    /// the innermost one. Raw pointers are only dereferenced while the
    /// owning coroutine is on-stack inside a resume call.
    pub gen_yielders: Vec<*const corosensei::Yielder<GenIn, Value>>,
    /// step budget for the innermost running generator body (infinite
    /// `for(;;) yield` bodies are cut off); `u64::MAX` = no active gen.
    pub(crate) gen_cap: u64,
    /// set when `gen_cap` tripped — the gen call boundary converts the
    /// resulting unwind into "return what was collected"
    pub(crate) gen_cap_hit: bool,
    /// `yield*` suspended with a raw inner iterator result — `gen_resume`
    /// returns it to the consumer verbatim instead of wrapping it as
    /// `{value, done: false}`
    pub(crate) yieldstar_raw: bool,
    /// `yield*` saw `inner.return()` produce a done result — overrides the
    /// value `gen_resume` reports for the injected return-completion
    pub(crate) gen_ret_override: Option<Value>,
    /// NewTarget while a native constructor body runs — `Some(f)` when
    /// `f` was `new`-ed without an explicit newTarget; `None` for calls.
    /// Lets natives implement "NewTarget === active function" checks
    /// (Iterator, Promise-without-new, etc).
    pub new_target_native: Option<Value>,
}

/// JS call-depth limit. Each JS call consumes substantial Rust stack in
/// debug builds; embedders should run the interpreter on a thread with a
/// large stack (e.g. 32+ MiB) if deep recursion is expected.
const MAX_DEPTH: usize = 400;

/// Statement budget per eager `function*` body — bounds infinite
/// `for(;;) yield` sources at ~1M statements (~500K collected items).
pub(crate) const GEN_STEP_BUDGET: u64 = 1_000_000;

impl Interp {
    pub fn new() -> Interp {
        let mk = |proto: Option<ObjRef>| Rc::new(RefCell::new(Obj::new(proto, ObjKind::Ord)));
        let obj = mk(None);
        // %Function.prototype% is itself a function (calling it returns
        // undefined) — is_callable must see it
        fn empty_fn(
            _i: &mut Interp,
            _t: &Value,
            _a: &[Value],
        ) -> Result<Value, Value> {
            Ok(Value::Und)
        }
        let func = Rc::new(RefCell::new(Obj::new(
            Some(obj.clone()),
            ObjKind::Func(Func::Native {
                name: Rc::from(""),
                f: empty_fn,
                ctor: false,
            }),
        )));
        let iterator = mk(Some(obj.clone()));
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
                eval_error: mk(Some(obj.clone())),
                uri_error: mk(Some(obj.clone())),
                agg_error: mk(Some(obj.clone())),
                suppressed_error: mk(Some(obj.clone())),
                promise: mk(Some(obj.clone())),
                array_iter: mk(Some(iterator.clone())),
                map_iter: mk(Some(iterator.clone())),
                set_iter: mk(Some(iterator.clone())),
                string_iter: mk(Some(iterator.clone())),
                generator: mk(Some(iterator.clone())),
                gen_func: mk(Some(func.clone())),
                iter_helper: mk(Some(iterator.clone())),
                wrap_iter: mk(Some(iterator.clone())),
                iterator,
                regexp: mk(Some(obj.clone())),
                arraybuffer: mk(Some(obj.clone())),
                global: mk(Some(obj.clone())),
                bigint: mk(Some(obj.clone())),
                obj,
                func,
            },
            output: Vec::new(),
            modules: Vec::new(),
            pending_label: None,
            cur_strict: false,
            eval_decl: false,
            eval_args_ban: 0,
            depth: 0,
            sym_counter: 16,
            priv_ctr: 0,
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
            set_failed: false,
            deadline: None,
            interrupt_site: None,
            trace_calls: std::env::var_os("ROWSER_CALL_TRACE").is_some(),
            interned: FxSet::default(),
            fn_hoist: FxMap::default(),
            slice_hoist: FxMap::default(),
            eval_fn: None,
            gen_yielders: Vec::new(),
            gen_cap: u64::MAX,
            yieldstar_raw: false,
            gen_ret_override: None,
            gen_cap_hit: false,
            new_target_native: None,
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
        if self.steps >= self.gen_cap {
            // eager-generator budget: unwind the body; stays tripped
            // (like `interrupted`) until the gen call boundary resets it
            self.gen_cap_hit = true;
            return Err(self.range_err("generator step budget exceeded"));
        }
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

    /// spec IteratorClose — call `iter.return()` when present; a
    /// non-callable non-null `return` is a TypeError, a non-object
    /// result likewise. Errors propagate (they replace the original
    /// completion per spec).
    pub fn iter_close_obj(&mut self, v: &Value) -> Result<(), Value> {
        let m = self.get_prop(v, &PropKey::from_str("return"))?;
        if matches!(m, Value::Und | Value::Null) {
            return Ok(());
        }
        if !self.is_callable(&m) {
            return Err(self.type_err("iterator return is not a function"));
        }
        let r = self.call(&m, v.clone(), Vec::new())?;
        if !matches!(r, Value::Obj(_)) {
            return Err(self.type_err("iterator return must return an object"));
        }
        Ok(())
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
        self.err_obj_opt(proto, name, Some(msg))
    }

    /// `msg: None` — the Error ctor was called with `undefined` message:
    /// spec does not install an own `message` prop in that case.
    pub fn err_obj_opt(
        &mut self,
        proto: &ObjRef,
        name: &str,
        msg: Option<&str>,
    ) -> Value {
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
        if let Some(msg) = msg {
            o.borrow_mut()
                .props
                .insert(PropKey::from_str("message"), p(Value::Str(Rc::from(msg))));
        }
        // spec installs no own `name` — it resolves via
        // Error.prototype.name / <Sub>Error.prototype.name
        o.borrow_mut().props.insert(
            PropKey::from_str("stack"),
            p(Value::Str(Rc::from(
                format!("{name}: {}", msg.unwrap_or("")).as_str(),
            ))),
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
    /// lookup + TDZ state (parameter bindings are uninit until their
    /// initializers run — reading them is a ReferenceError).
    /// raw own+proto-chain property read — no getters/proxies, used by
    /// `&self` paths (with-scope checks, fast lookups)
    fn obj_get_raw_chain(&self, o: &ObjRef, key: &PropKey) -> Option<Value> {
        let mut c = Some(o.clone());
        while let Some(cur) = c {
            if let PropKey::S(k) = key {
                if let Some(v) = view_own_prop_str(self, &cur, k.as_ref()) {
                    return Some(v);
                }
            }
            if let Some(p) = cur.borrow().props.get(key) {
                return Some(p.val.clone());
            }
            let next = cur.borrow().proto.clone();
            c = next;
        }
        None
    }

    /// WithEnvironmentRecord.HasBinding — `name` binds on the with-object
    /// iff `HasProperty(obj, name)` and `obj[@@unscopables][name]` isn't
    /// truthy (raw lookups — spec traps/getters run in the `&mut` paths)
    /// HasProperty on the with-object — a proxy fires its `has` trap.
    fn with_has_prop(&mut self, o: &ObjRef, name: &str) -> Result<bool, Value> {
        let proxy = {
            let b = o.borrow();
            match &b.kind {
                ObjKind::Proxy { target, handler } => {
                    Some((target.clone(), handler.clone()))
                }
                _ => None,
            }
        };
        if let Some((t, h)) = proxy {
            return self.proxy_has(&t, &h, &PropKey::from_str(name));
        }
        self.has_prop(&Value::Obj(o.clone()), &PropKey::from_str(name))
    }

    /// WithEnvironmentRecord.HasBinding — HasProperty(N), then
    /// Get(bindings, @@unscopables) and Get(unscopables, N) — all through
    /// the full MOP so proxies trap and getters fire.
    fn with_has(&mut self, o: &ObjRef, name: &str) -> Result<bool, Value> {
        if !self.with_has_prop(o, name)? {
            return Ok(false);
        }
        let u = self.get_prop(
            &Value::Obj(o.clone()),
            &PropKey::Sym(SYM_UNSCOPABLES),
        )?;
        if let Value::Obj(uo) = &u {
            let v = self.get_prop(
                &Value::Obj(uo.clone()),
                &PropKey::from_str(name),
            )?;
            if self.to_bool(&v) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// WithEnvironmentRecord.GetBindingValue — HasProperty(N) then
    /// Get(bindings, N), both observable.
    fn with_get(&mut self, o: &ObjRef, name: &str) -> Result<Value, Value> {
        if !self.with_has_prop(o, name)? {
            // GetBindingValue: missing binding → ReferenceError in
            // strict code, undefined otherwise (9.1.1.2.4 step 5)
            if self.cur_strict {
                return Err(self.ref_err(format!("{name} is not defined")));
            }
            return Ok(Value::Und);
        }
        self.get_prop(&Value::Obj(o.clone()), &PropKey::from_str(name))
    }

    pub fn env_lookup_st(
        &mut self,
        env: &EnvRef,
        name: &str,
    ) -> Result<Option<(Value, bool)>, Value> {
        let mut cur: &EnvRef = env;
        loop {
            if let Some(o) = &cur.obj_env {
                if self.with_has(o, name)? {
                    return self.with_get(o, name).map(|v| Some((v, false)));
                }
            } else if let Some(b) = cur.vars.borrow().get(name) {
                return Ok(Some((b.val.clone(), b.uninit)));
            }
            match &cur.parent {
                Some(p) => cur = p,
                None => break,
            }
        }
        // fall back to global object properties (globalThis.x = …)
        Ok(self
            .obj_get_raw_str(&self.p.global, name)
            .map(|v| (v, false)))
    }

    /// `set_prop` on the global object must update a var-declared env
    /// binding too (script `var x` and `globalThis.x` are one slot).
    /// Called by the set paths after the prop write succeeds.
    pub fn sync_global_binding(&mut self, name: &str, v: &Value) {
        let mut b = self.global.vars.borrow_mut();
        if let Some(bind) = b.get_mut(name) {
            if !bind.lexical {
                bind.val = v.clone();
            }
        }
    }

    pub fn env_lookup(
        &mut self,
        env: &EnvRef,
        name: &str,
    ) -> Result<Option<Value>, Value> {
        Ok(self.env_lookup_st(env, name)?.map(|(v, _)| v))
    }

    pub fn env_assign(&mut self, env: &EnvRef, name: &str, v: Value) -> Result<(), Value> {
        let mut cur: &EnvRef = env;
        loop {
            if let Some(o) = &cur.obj_env {
                // SetMutableBinding on an object env: plain HasProperty —
                // @@unscopables is only consulted by HasBinding at resolve time
                if self.has_prop(&Value::Obj(o.clone()), &PropKey::from_str(name))? {
                    let is_glob = Rc::ptr_eq(o, &self.p.global);
                    let o = Value::Obj(o.clone());
                    self.set_prop_str(&o, name, v.clone())?;
                    // writes through `with (globalThis)` stay in sync with
                    // the env binding (compound assigns take this path —
                    // eval_lref_ident uses set_prop directly)
                    if is_glob {
                        self.sync_global_binding(name, &v);
                    }
                    return Ok(());
                }
            }
            {
                let mut vars = cur.vars.borrow_mut();
                if let Some(b) = vars.get_mut(name) {
                    if b.uninit {
                        drop(vars);
                        return Err(self.ref_err(format!(
                            "Cannot access '{name}' before initialization"
                        )));
                    }
                    if !b.mutable {
                        // SetMutableBinding: immutable + caller strict →
                        // TypeError; sloppy → silently ignored. `const`
                        // bindings (lexical) are strict-immutable and
                        // always throw; named-fn-expr names aren't.
                        let hard = b.lexical;
                        drop(vars);
                        if hard || self.cur_strict {
                            return Err(
                                self.type_err(format!("Assignment to constant variable '{name}'"))
                            );
                        }
                        return Ok(());
                    }
                    let lexical = b.lexical;
                    b.val = v.clone();
                    // global `var` bindings mirror the global object's
                    // props — `this.x`/`window.x` must see the assignment.
                    // `let`/`const` bindings never become object props.
                    if !lexical && Rc::ptr_eq(cur, &self.global) {
                        let g = self.p.global.clone();
                        let pk = PropKey::S(self.intern(name));
                        let mut gb = g.borrow_mut();
                        match gb.props.get_mut(&pk) {
                            Some(p) => p.val = v,
                            None => {
                                gb.props.insert(pk, Prop::data(v));
                            }
                        }
                    }
                    return Ok(());
                }
            }
            match &cur.parent {
                Some(p) => cur = p,
                None => break,
            }
        }
        if self.cur_strict {
            return Err(self.ref_err(format!("{name} is not defined")));
        }
        // sloppy: create global; mirror onto global object. An
        // undeclared global is a configurable own prop of the global
        // object — `delete x` removes it
        let k = self.intern(name);
        self.global.vars.borrow_mut().insert(
            k.clone(),
            Binding {
                val: v.clone(),
                mutable: true, uninit: false, deletable: true, lexical: false,
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
            if cur.obj_env.is_some() {
                return None;
            }
            {
                let mut vars = cur.vars.borrow_mut();
                if let Some(b) = vars.get_mut(name) {
                    if !b.mutable {
                        // sloppy write to a non-lexical immutable (named
                        // fn-expr name): silently no-op — the update
                        // value still yields without mutating the binding
                        let hard = b.lexical;
                        let old = b.val.clone();
                        drop(vars);
                        if hard || self.cur_strict {
                            return Some(Err(
                                self.type_err(format!("Assignment to constant variable '{name}'"))
                            ));
                        }
                        if let Value::Num(n) = old {
                            return Some(Ok((n, n + delta)));
                        }
                        return None;
                    }
                    if let Value::Num(n) = b.val {
                        let new = n + delta;
                        let lexical = b.lexical;
                        b.val = Value::Num(new);
                        // global `var` bindings mirror the global-object
                        // prop — a stale prop would shadow the binding for
                        // `with (globalThis)` lookups
                        if !lexical && Rc::ptr_eq(cur, &self.global) {
                            let g = self.p.global.clone();
                            let pk = PropKey::S(self.intern(name));
                            let mut gb = g.borrow_mut();
                            match gb.props.get_mut(&pk) {
                                Some(p) => p.val = Value::Num(new),
                                None => {
                                    gb.props.insert(pk, Prop::data(Value::Num(new)));
                                }
                            }
                        }
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
    /// `let`/`const`/`class` and other lexical binding creation — never
    /// mirrors onto the global object (lex declarations don't create
    /// global-object properties). Use `define_global`/`declare_var` for
    /// bindings that should also be object properties.
    pub fn declare(&mut self, env: &EnvRef, name: &str, v: Value, mutable: bool) {
        let k = self.intern(name);
        env.vars.borrow_mut().insert(
            k.clone(),
            Binding {
                val: v.clone(),
                mutable,
                uninit: false, deletable: false, lexical: true,
            },
        );
    }

    /// Global-object property install for a global-scope binding: used by
    /// host code (`define_global`, DOM globals) where the name must exist
    /// as both an env binding and a global-object property.
    pub fn declare_global_prop(&mut self, name: &str, v: Value) {
        let k = self.intern(name);
        self.p
            .global
            .borrow_mut()
            .props
            .insert(PropKey::S(k), Prop::data(v));
    }

    /// `var` — walks to nearest function scope.
    pub fn declare_var(&mut self, env: &EnvRef, name: &str, v: Value) {
        let k = self.intern(name);
        self.declare_var_rc(env, &k, v, false);
    }

    /// `declare_var` for hoisted FUNCTION decls — a configurable existing
    /// global prop is fully redefined, not just value-updated
    pub fn declare_var_fn(&mut self, env: &EnvRef, name: &str, v: Value) {
        let k = self.intern(name);
        self.declare_var_rc(env, &k, v, true);
    }

    /// `declare_var` with a pre-interned key (hoist cache reuse).
    fn declare_var_rc(&mut self, env: &EnvRef, name: &Rc<str>, v: Value, is_fn: bool) {
        let mut cur = env.clone();
        loop {
            if cur.var_boundary.get() || cur.parent.is_none() {
                break;
            }
            let next = cur.parent.clone().unwrap();
            cur = next;
        }
        let exists = cur.vars.borrow().contains_key(name.as_ref());
        let deletable = self.eval_decl;
        if !exists {
            cur.vars.borrow_mut().insert(
                name.clone(),
                Binding {
                    val: v.clone(),
                    mutable: true, uninit: false, deletable, lexical: false,
                },
            );
        } else if !matches!(v, Value::Und) {
            let mut vv = cur.vars.borrow_mut();
            let b = vv.get_mut(name.as_ref()).unwrap();
            b.val = v.clone();
            b.deletable = deletable;
        }
        if Rc::ptr_eq(&cur, &self.global) {
            // CreateGlobalVarBinding/CreateGlobalFunctionBinding checks:
            // non-extensible → TypeError; existing non-configurable props
            // may only be overwritten by a compatible var binding
            let g = self.p.global.clone();
            let existing = g.borrow().props.get(&PropKey::S(name.clone())).map(|p| {
                (p.configurable, p.writable, p.get.is_some())
            });
            if !g.borrow().extensible && existing.is_none() {
                // TypeError — but declare_var returns (), so record via a
                // side channel?  (kept simple: skip and let the assignment
                // surface)
            }
            // script var/fn globals are non-configurable; eval-declared ones
            // are configurable per CreateGlobalVarBinding(D=true). An
            // existing global prop keeps its descriptor — the binding
            // update only writes the value (CreateGlobalVarBinding step 7).
            let mut gb = g.borrow_mut();
            match gb.props.get_mut(&PropKey::S(name.clone())) {
                // hoist passes Und — an existing global prop keeps its
                // value until the initializer assigns it
                Some(_) if matches!(v, Value::Und) => {}
                Some(p) if is_fn && p.configurable => {
                    // CreateGlobalFunctionBinding: configurable existing
                    // props are fully REDEFINED with default attrs
                    *p = Prop::data(v);
                    p.configurable = deletable;
                }
                // non-configurable (or var decl): only the value updates —
                // existing descriptor attributes are preserved
                Some(p) => p.val = v,
                None => {
                    let mut p = Prop::data(v);
                    p.configurable = deletable;
                    gb.props.insert(PropKey::S(name.clone()), p);
                }
            }
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
    /// BigInt values are Ord objects on %BigInt.prototype% — enough for
    /// `typeof`-blind uses (iterator tests, wrappers) until real BigInt
    /// lands. `typeof` reports 'object', not 'bigint'.
    pub fn bigint_obj(&self, n: f64) -> Value {
        let (neg, mag, _) = crate::binary::f64_bits(n.trunc());
        self.bigint_obj_b(neg, &mag)
    }
    /// construct a stand-in from exact bits — writes both the coarse
    /// f64 `__bigint` cache and the exact `__bigint_x` bit-string
    pub fn bigint_obj_b(&self, neg: bool, mag: &str) -> Value {
        let mag = mag.trim_start_matches('0');
        let mag = if mag.is_empty() { "0" } else { mag };
        let neg = neg && mag != "0";
        let o = Rc::new(RefCell::new(Obj::new(
            Some(self.p.bigint.clone()),
            ObjKind::Ord,
        )));
        let mut n = 0f64;
        for c in mag.bytes() {
            n = n * 2.0 + (c - b'0') as f64;
        }
        let x = format!("{}{}", if neg { "-" } else { "" }, mag);
        self.set_attr(&o, "__bigint", Value::Num(if neg { -n } else { n }), false, false);
        self.set_attr(&o, "__bigint_x", Value::Str(Rc::from(x.as_str())), false, false);
        Value::Obj(o)
    }
    /// bigint literal text ("123", "0x1f", "0b101") → stand-in
    pub fn bigint_obj_lit(&self, text: &str) -> Value {
        match crate::binary::lit_to_bits_sep(text, true) {
            Some((neg, mag)) => self.bigint_obj_b(neg, &mag),
            None => self.bigint_obj_b(false, "0"),
        }
    }
    pub fn new_arr(&self, items: Vec<Value>) -> ObjRef {
        Rc::new(RefCell::new(Obj::new(
            Some(self.p.array.clone()),
            ObjKind::Arr(items),
        )))
    }
    /// `arguments` object. Mapped (sloppy, simple-ident params): indexed
    /// props alias the parameter bindings in `scope`; unmapped (strict
    /// fns, or any param that's not a bare identifier): exotic object
    /// with throwing `caller`/`callee` accessors and no aliasing.
    pub fn new_args_obj(
        &mut self,
        args: &[Value],
        scope: &EnvRef,
        def: &FnDef,
        callee: &Value,
    ) -> ObjRef {
        fn throw_type_err(i: &mut Interp, _t: &Value, _a: &[Value]) -> ERes {
            Err(i.type_err("'caller' and 'arguments' are restricted"))
        }
        let strict = def.strict || crate::parser::stmts_strict(&def.body);
        // spec: a non-simple parameter list (defaults, destructuring,
        // rest) yields an UNMAPPED arguments object even when sloppy
        let simple_params =
            def.params.iter().all(|p| matches!(p, Param::Ident(_)));
        let mapped = !strict && simple_params;
        let mut map: Vec<Option<Rc<str>>> = Vec::new();
        if mapped {
            // spec: index i maps to the parameter name when i < param
            // count, the param is a bare identifier, and the name hasn't
            // already been claimed by an earlier parameter
            let mut seen: std::collections::HashSet<Rc<str>> = Default::default();
            map = vec![None; args.len()];
            for (i, p) in def.params.iter().enumerate() {
                if i >= args.len() {
                    break;
                }
                if let Param::Ident(n) = p {
                    if n == "arguments" {
                        continue;
                    }
                    let k = self.intern(n);
                    if seen.insert(k.clone()) {
                        map[i] = Some(k);
                    }
                }
            }
        }
        let o = Rc::new(RefCell::new(Obj::new(
            Some(self.p.obj.clone()),
            ObjKind::Args {
                items: args.to_vec(),
                env: Some(scope.clone()),
                map,
            },
        )));
        // length — spec attrs {writable, configurable, !enumerable}
        o.borrow_mut()
            .props
            .insert(PropKey::from_str("length"), Prop::builtin(Value::Num(args.len() as f64)));
        if !mapped {
            // unmapped: throwing caller/callee accessors (own props,
            // non-configurable)
            let thrower = self.native("ThrowTypeError", throw_type_err);
            for k in ["caller", "callee"] {
                o.borrow_mut().props.insert(
                    PropKey::from_str(k),
                    Prop {
                        val: Value::Und,
                        writable: false,
                        enumerable: false,
                        configurable: false,
                        get: Some(thrower.clone()),
                        set: Some(thrower.clone()),
                    },
                );
            }
        } else {
            // sloppy mapped: callee = the function itself
            o.borrow_mut().props.insert(
                PropKey::from_str("callee"),
                Prop::builtin(callee.clone()),
            );
        }
        // @@iterator → Array.prototype.values (same native as arrays)
        let itv = self
            .p
            .array
            .borrow()
            .props
            .get(&PropKey::Sym(SYM_ITERATOR))
            .map(|p| p.val.clone());
        if let Some(pv) = itv {
            o.borrow_mut()
                .props
                .insert(PropKey::Sym(SYM_ITERATOR), Prop::builtin(pv));
        }
        o
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
                ctor: false,
            }),
        )));
        // fn .length/.name: {writable:false, enumerable:false, configurable:true}
        self.set_attr(&o, "length", Value::Num(0.0), false, true);
        self.set_attr(&o, "name", Value::Str(Rc::from(name)), false, true);
        Value::Obj(o)
    }

    /// raw own-prop lookup (no proto walk, no getters)
    pub fn obj_get_raw(&self, o: &ObjRef, key: &PropKey) -> Option<Value> {
        if let Some(v) = view_own_prop(self, o, key) {
            return Some(v);
        }
        o.borrow().props.get(key).map(|p| p.val.clone())
    }

    /// `obj_get_raw` by borrowed string key — no key allocation.
    pub fn obj_get_raw_str(&self, o: &ObjRef, key: &str) -> Option<Value> {
        if let Some(v) = view_own_prop_str(self, o, key) {
            return Some(v);
        }
        o.borrow().props.get_str(key).map(|p| p.val.clone())
    }

    pub fn is_callable(&self, v: &Value) -> bool {
        matches!(v, Value::Obj(o) if o.borrow().is_callable())
    }

    /// spec GetMethod on a proxy handler: returns the trap fn, `None`
    /// when absent (→ fall back to the target), TypeError when a
    /// non-callable value sits in the trap slot.
    pub(crate) fn proxy_trap(
        &mut self,
        handler: &Value,
        name: &str,
    ) -> Result<Option<Value>, Value> {
        match self.get_prop_str(handler, name)? {
            Value::Und | Value::Null => Ok(None),
            m if self.is_callable(&m) => Ok(Some(m)),
            _ => Err(self.type_err("proxy trap is not callable")),
        }
    }

    // -- proxy trap machinery (spec 10.5.x) ------------------------------------
    //
    // A REVOKED proxy carries `target: Null, handler: Null` — `proxy_trap`
    // then fails on the Null handler with a TypeError, which is exactly
    // the spec's "[[ProxyHandler]] is null → throw" first step of every
    // internal method.

    /// spec ToPropertyDescriptor — the six fields read through
    /// HasProperty/Get (getters fire); get/set must be
    /// callable-or-undefined; accessor+data mixing throws.
    pub(crate) fn pdesc_from_obj(&mut self, desc: &Value) -> Result<PDesc, Value> {
        let mut d = PDesc::default();
        if self.has_prop(desc, &PropKey::from_str("enumerable"))? {
            let v = self.get_prop_str(desc, "enumerable")?;
            d.enumerable = Some(self.to_bool(&v));
        }
        if self.has_prop(desc, &PropKey::from_str("configurable"))? {
            let v = self.get_prop_str(desc, "configurable")?;
            d.configurable = Some(self.to_bool(&v));
        }
        if self.has_prop(desc, &PropKey::from_str("value"))? {
            d.value = Some(self.get_prop_str(desc, "value")?);
        }
        if self.has_prop(desc, &PropKey::from_str("writable"))? {
            let v = self.get_prop_str(desc, "writable")?;
            d.writable = Some(self.to_bool(&v));
        }
        if self.has_prop(desc, &PropKey::from_str("get"))? {
            let g = self.get_prop_str(desc, "get")?;
            if !matches!(g, Value::Und) && !self.is_callable(&g) {
                return Err(self.type_err("Getter must be a function"));
            }
            d.get = Some(g);
        }
        if self.has_prop(desc, &PropKey::from_str("set"))? {
            let s = self.get_prop_str(desc, "set")?;
            if !matches!(s, Value::Und) && !self.is_callable(&s) {
                return Err(self.type_err("Setter must be a function"));
            }
            d.set = Some(s);
        }
        if (d.get.is_some() || d.set.is_some())
            && (d.value.is_some() || d.writable.is_some())
        {
            return Err(self.type_err(
                "Invalid property descriptor. Cannot both specify accessors and a value or writable attribute",
            ));
        }
        Ok(d)
    }

    /// spec FromPropertyDescriptor — a fresh object carrying only the
    /// descriptor's present fields, in spec emission order.
    pub(crate) fn pdesc_to_obj(&mut self, d: &PDesc) -> ObjRef {
        let o = self.new_obj();
        if let Some(v) = &d.value {
            self.set_data(&o, "value", v.clone());
        }
        if let Some(w) = d.writable {
            self.set_data(&o, "writable", Value::Bool(w));
        }
        if let Some(g) = &d.get {
            self.set_data(&o, "get", g.clone());
        }
        if let Some(s) = &d.set {
            self.set_data(&o, "set", s.clone());
        }
        if let Some(e) = d.enumerable {
            self.set_data(&o, "enumerable", Value::Bool(e));
        }
        if let Some(c) = d.configurable {
            self.set_data(&o, "configurable", Value::Bool(c));
        }
        o
    }

    /// `target.[[GetOwnProperty]](P)` through the full MOP — a proxy
    /// target fires its own trap (invariants applied at that level too).
    pub(crate) fn proxy_target_desc(
        &mut self,
        t: &Value,
        key: &PropKey,
    ) -> Result<Option<PDesc>, Value> {
        let kv = self.key_to_value(key);
        let d = crate::builtins::obj_get_own_prop_desc(
            self,
            &Value::Und,
            &[t.clone(), kv],
        )?;
        match d {
            Value::Und => Ok(None),
            _ => Ok(Some(self.pdesc_from_obj(&d)?)),
        }
    }

    /// `IsExtensible(target)` through the MOP — proxy targets fire their
    /// own `isExtensible` trap.
    pub(crate) fn proxy_target_ext(&mut self, t: &Value) -> Result<bool, Value> {
        match t {
            Value::Obj(o) => crate::builtins::ord_is_extensible(self, o),
            _ => Ok(false),
        }
    }

    /// `target.[[GetPrototypeOf]]()` through the MOP.
    pub(crate) fn proxy_target_proto(&mut self, t: &Value) -> Result<Value, Value> {
        crate::builtins::obj_get_prototype_of(
            self,
            &Value::Und,
            &[t.clone()],
        )
    }

    /// spec IsCompatiblePropertyDescriptor — can `d` apply over an
    /// existing (complete) `cur` on a target with `extensible`?
    fn desc_compatible(
        &mut self,
        extensible: bool,
        d: &PDesc,
        cur: Option<&PDesc>,
    ) -> Result<bool, Value> {
        let Some(c) = cur else {
            return Ok(extensible);
        };
        if d.is_empty() {
            return Ok(true);
        }
        if c.configurable == Some(false) {
            if d.configurable == Some(true) {
                return Ok(false);
            }
            if let Some(e) = d.enumerable {
                if e != c.enumerable.unwrap_or(false) {
                    return Ok(false);
                }
            }
            if !d.is_generic() && d.is_accessor() != c.is_accessor() {
                return Ok(false);
            }
            if c.is_accessor() {
                if let Some(g) = &d.get {
                    let cg = c.get.clone().unwrap_or(Value::Und);
                    if !self.same_value(g, &cg)? {
                        return Ok(false);
                    }
                }
                if let Some(s) = &d.set {
                    let cs = c.set.clone().unwrap_or(Value::Und);
                    if !self.same_value(s, &cs)? {
                        return Ok(false);
                    }
                }
            } else if c.writable == Some(false) {
                if d.writable == Some(true) {
                    return Ok(false);
                }
                if let Some(v) = &d.value {
                    let cv = c.value.clone().unwrap_or(Value::Und);
                    if !self.same_value(v, &cv)? {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
    }

    /// proxy [[Get]](P, Receiver) — the `get` trap plus the 10.5.8
    /// invariants (non-config non-writable data ⇒ SameValue result;
    /// non-config getter-less accessor ⇒ undefined result).
    pub(crate) fn proxy_get(
        &mut self,
        t: &Value,
        h: &Value,
        key: &PropKey,
        receiver: &Value,
    ) -> ERes {
        match self.proxy_trap(h, "get")? {
            Some(trap) => {
                let kv = self.key_to_value(key);
                let r = self.call(
                    &trap,
                    h.clone(),
                    vec![t.clone(), kv, receiver.clone()],
                )?;
                if let Some(td) = self.proxy_target_desc(t, key)? {
                    if td.configurable == Some(false) {
                        if !td.is_accessor() && td.writable == Some(false) {
                            let tv = td.value.clone().unwrap_or(Value::Und);
                            if !self.same_value(&r, &tv)? {
                                return Err(self.type_err(
                                    "'get' on proxy: property is a non-configurable, non-writable own data property of the target",
                                ));
                            }
                        } else if td.is_accessor()
                            && matches!(td.get.as_ref(), None | Some(Value::Und))
                            && !matches!(r, Value::Und)
                        {
                            return Err(self.type_err(
                                "'get' on proxy: property is a non-configurable accessor property of the target without a getter",
                            ));
                        }
                    }
                }
                Ok(r)
            }
            // spec fallback: target.[[Get]](P, Receiver)
            None => match t {
                Value::Obj(to) => {
                    let to = to.clone();
                    self.obj_get(&to, key, receiver)
                }
                _ => Ok(Value::Und),
            },
        }
    }

    /// proxy [[Set]](P, V, Receiver) — the `set` trap plus the 10.5.9
    /// invariants. A falsy trap result routes through `set_fail` so
    /// strict callers throw and Reflect.set reads `false`.
    pub(crate) fn proxy_set(
        &mut self,
        t: &Value,
        h: &Value,
        key: &PropKey,
        v: Value,
        receiver: &Value,
    ) -> Result<(), Value> {
        match self.proxy_trap(h, "set")? {
            Some(trap) => {
                let kv = self.key_to_value(key);
                let r = self.call(
                    &trap,
                    h.clone(),
                    vec![t.clone(), kv, v.clone(), receiver.clone()],
                )?;
                if !self.to_bool(&r) {
                    let kn = self.key_to_name(key);
                    return self.set_fail(&kn);
                }
                if let Some(td) = self.proxy_target_desc(t, key)? {
                    if td.configurable == Some(false) {
                        if !td.is_accessor() && td.writable == Some(false) {
                            let tv = td.value.clone().unwrap_or(Value::Und);
                            if !self.same_value(&v, &tv)? {
                                return Err(self.type_err(
                                    "'set' on proxy: trap returned truish for a non-configurable, non-writable property with a different value",
                                ));
                            }
                        } else if td.is_accessor()
                            && matches!(td.set.as_ref(), None | Some(Value::Und))
                        {
                            return Err(self.type_err(
                                "'set' on proxy: trap returned truish for a non-configurable accessor property without a setter",
                            ));
                        }
                    }
                }
                Ok(())
            }
            // spec fallback: target.[[Set]](P, V, Receiver)
            None => match t {
                Value::Obj(to) => {
                    let to = to.clone();
                    self.obj_set(&to, key.clone(), v, receiver)
                }
                _ => Ok(()),
            },
        }
    }

    /// proxy [[HasProperty]](P) — the `has` trap plus the 10.5.7
    /// invariant (a falsy result is illegal when the target has the key
    /// non-configurably, or on a non-extensible target at all).
    pub(crate) fn proxy_has(
        &mut self,
        t: &Value,
        h: &Value,
        key: &PropKey,
    ) -> Result<bool, Value> {
        match self.proxy_trap(h, "has")? {
            Some(trap) => {
                let kv = self.key_to_value(key);
                let r = self.call(&trap, h.clone(), vec![t.clone(), kv])?;
                let br = self.to_bool(&r);
                if !br {
                    if let Some(td) = self.proxy_target_desc(t, key)? {
                        if td.configurable == Some(false) {
                            return Err(self.type_err(
                                "'has' on proxy: trap returned falsish for a non-configurable property",
                            ));
                        }
                        if !self.proxy_target_ext(t)? {
                            return Err(self.type_err(
                                "'has' on proxy: trap returned falsish for an existing property on a non-extensible target",
                            ));
                        }
                    }
                }
                Ok(br)
            }
            None => self.has_prop(t, key),
        }
    }

    /// proxy [[Delete]](P) — the `deleteProperty` trap plus the 10.5.10
    /// invariants (can't report a non-configurable target prop deleted,
    /// nor any prop on a non-extensible target).
    pub(crate) fn proxy_delete(
        &mut self,
        t: &Value,
        h: &Value,
        key: &PropKey,
    ) -> Result<bool, Value> {
        match self.proxy_trap(h, "deleteProperty")? {
            Some(trap) => {
                let kv = self.key_to_value(key);
                let r = self.call(&trap, h.clone(), vec![t.clone(), kv])?;
                if !self.to_bool(&r) {
                    return Ok(false);
                }
                match self.proxy_target_desc(t, key)? {
                    None => Ok(true),
                    Some(td) => {
                        if td.configurable == Some(false) {
                            return Err(self.type_err(
                                "'deleteProperty' on proxy: trap returned truish for a non-configurable property",
                            ));
                        }
                        if !self.proxy_target_ext(t)? {
                            return Err(self.type_err(
                                "'deleteProperty' on proxy: trap returned truish for a property on a non-extensible target",
                            ));
                        }
                        Ok(true)
                    }
                }
            }
            None => self.delete_prop(t, key),
        }
    }

    /// proxy [[GetOwnProperty]](P) — the `getOwnPropertyDescriptor` trap
    /// plus the 10.5.5 invariants. Returns the completed result
    /// descriptor (None = the trap reported the key missing).
    pub(crate) fn proxy_gopd(
        &mut self,
        t: &Value,
        h: &Value,
        key: &PropKey,
    ) -> Result<Option<PDesc>, Value> {
        match self.proxy_trap(h, "getOwnPropertyDescriptor")? {
            Some(trap) => {
                let kv = self.key_to_value(key);
                let r = self.call(&trap, h.clone(), vec![t.clone(), kv])?;
                if !matches!(r, Value::Und | Value::Obj(_)) {
                    return Err(self.type_err(
                        "proxy getOwnPropertyDescriptor trap must return object or undefined",
                    ));
                }
                let td = self.proxy_target_desc(t, key)?;
                if matches!(r, Value::Und) {
                    let Some(td) = td else {
                        return Ok(None);
                    };
                    if td.configurable == Some(false) {
                        return Err(self.type_err(
                            "'getOwnPropertyDescriptor' on proxy: cannot report a non-configurable property as missing",
                        ));
                    }
                    if !self.proxy_target_ext(t)? {
                        return Err(self.type_err(
                            "'getOwnPropertyDescriptor' on proxy: cannot report an existing property as missing on a non-extensible target",
                        ));
                    }
                    return Ok(None);
                }
                let ext = self.proxy_target_ext(t)?;
                let mut rd = self.pdesc_from_obj(&r)?;
                rd.complete();
                if !self.desc_compatible(ext, &rd, td.as_ref())? {
                    return Err(self.type_err(
                        "'getOwnPropertyDescriptor' on proxy: reported descriptor is incompatible with the target property",
                    ));
                }
                if rd.configurable == Some(false) {
                    let bad = match &td {
                        None => true,
                        Some(td) => td.configurable != Some(false),
                    };
                    if bad {
                        return Err(self.type_err(
                            "'getOwnPropertyDescriptor' on proxy: cannot report a configurable or missing property as non-configurable",
                        ));
                    }
                    if rd.writable == Some(false)
                        && td.as_ref().is_some_and(|td| td.writable == Some(true))
                    {
                        return Err(self.type_err(
                            "'getOwnPropertyDescriptor' on proxy: cannot report a writable property as non-writable",
                        ));
                    }
                }
                Ok(Some(rd))
            }
            // spec fallback: target.[[GetOwnProperty]](P)
            None => self.proxy_target_desc(t, key),
        }
    }

    /// proxy [[DefineOwnProperty]] invariant pass (10.5.6) — run after a
    /// truish `defineProperty` trap result; `desc` is the descriptor the
    /// calling abstract op passed in.
    pub(crate) fn proxy_define_invariant(
        &mut self,
        t: &Value,
        key: &PropKey,
        desc: &PDesc,
    ) -> Result<(), Value> {
        let td = self.proxy_target_desc(t, key)?;
        let ext = self.proxy_target_ext(t)?;
        let cfg_false = desc.configurable == Some(false);
        match &td {
            None => {
                if !ext {
                    return Err(self.type_err(
                        "'defineProperty' on proxy: trap returned truish for adding a property to a non-extensible target",
                    ));
                }
                if cfg_false {
                    return Err(self.type_err(
                        "'defineProperty' on proxy: trap returned truish for defining a non-configurable property absent from the target",
                    ));
                }
            }
            Some(td) => {
                if !self.desc_compatible(ext, desc, Some(td))? {
                    return Err(self.type_err(
                        "'defineProperty' on proxy: trap returned truish for an incompatible property descriptor",
                    ));
                }
                if cfg_false && td.configurable != Some(false) {
                    return Err(self.type_err(
                        "'defineProperty' on proxy: trap returned truish for defining a configurable target property as non-configurable",
                    ));
                }
                if td.configurable == Some(false)
                    && !td.is_accessor()
                    && td.writable == Some(false)
                    && desc.writable == Some(true)
                {
                    return Err(self.type_err(
                        "'defineProperty' on proxy: trap returned truish for making a non-configurable, non-writable property writable",
                    ));
                }
            }
        }
        Ok(())
    }

    /// proxy [[GetPrototypeOf]] invariant — on a non-extensible target
    /// the trap result must equal the target's real prototype.
    pub(crate) fn proxy_getproto_invariant(
        &mut self,
        t: &Value,
        r: &Value,
    ) -> Result<(), Value> {
        if self.proxy_target_ext(t)? {
            return Ok(());
        }
        let tp = self.proxy_target_proto(t)?;
        if !self.same_value(r, &tp)? {
            return Err(self.type_err(
                "'getPrototypeOf' on proxy: trap result does not match the non-extensible target's prototype",
            ));
        }
        Ok(())
    }

    /// proxy [[SetPrototypeOf]] invariant — a truish result on a
    /// non-extensible target must set the SAME prototype it already has.
    pub(crate) fn proxy_setproto_invariant(
        &mut self,
        t: &Value,
        v: &Value,
    ) -> Result<(), Value> {
        if self.proxy_target_ext(t)? {
            return Ok(());
        }
        let tp = self.proxy_target_proto(t)?;
        if !self.same_value(v, &tp)? {
            return Err(self.type_err(
                "'setPrototypeOf' on proxy: trap returned truish for a different prototype on a non-extensible target",
            ));
        }
        Ok(())
    }

    /// PropKey → the JS value traps receive (`"a"` → Str, symbol → Sym)
    /// human-readable key name for error messages
    pub fn key_to_name(&self, k: &PropKey) -> String {
        match k {
            PropKey::S(s) => s.to_string(),
            PropKey::Sym(id) => format!("Symbol({id})"),
        }
    }

    pub fn key_to_value(&self, k: &PropKey) -> Value {
        match k {
            PropKey::S(s) => {
                if let Some(n) = arr_index(s) {
                    // spec: integer keys arrive as canonical strings
                    Value::Str(Rc::from(n.to_string().as_str()))
                } else {
                    Value::Str(s.clone())
                }
            }
            PropKey::Sym(id) => {
                let d: &str = match *id {
                    SYM_ITERATOR => "Symbol.iterator",
                    SYM_TOPRIMITIVE => "Symbol.toPrimitive",
                    SYM_TOSTRINGTAG => "Symbol.toStringTag",
                    SYM_HASINSTANCE => "Symbol.hasInstance",
                    SYM_MATCH => "Symbol.match",
                    SYM_REPLACE => "Symbol.replace",
                    SYM_SPLIT => "Symbol.split",
                    SYM_SEARCH => "Symbol.search",
                    SYM_MATCHALL => "Symbol.matchAll",
                    SYM_IS_CONCAT_SPREADABLE => "Symbol.isConcatSpreadable",
                    SYM_SPECIES => "Symbol.species",
                    SYM_UNSCOPABLES => "Symbol.unscopables",
                    _ => "",
                };
                Value::Sym(*id, Rc::from(d))
            }
        }
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

    /// spec ToObject: primitives box to wrapper objects carrying the
    /// matching prototype (Boolean/Number/String/Symbol.prototype).
    pub fn to_object(&mut self, v: &Value) -> ERes {
        match v {
            Value::Obj(_) => Ok(v.clone()),
            Value::Und | Value::Null => Err(self.type_err(
                "Cannot convert undefined or null to object",
            )),
            _ => {
                let proto = match v {
                    Value::Bool(_) => self.p.boolean.clone(),
                    Value::Num(_) => self.p.number.clone(),
                    Value::Str(_) => self.p.string.clone(),
                    Value::Sym(..) => self.p.symbol.clone(),
                    _ => unreachable!(),
                };
                let obj = Rc::new(RefCell::new(Obj::new(Some(proto), ObjKind::Ord)));
                // wrappers carry the primitive so valueOf/toString and
                // ToPrimitive work on them (5 >>> 0 on a boxed this)
                obj.borrow_mut().props.insert(
                    PropKey::from_str("$value"),
                    Prop {
                        val: v.clone(),
                        writable: false,
                        enumerable: false,
                        configurable: false,
                        get: None,
                        set: None,
                    },
                );
                // String wrappers expose length + indexed characters —
                // string-exotic attrs: chars {W:false,E:true,C:false},
                // length {W:false,E:false,C:false}
                if let Value::Str(s) = v {
                    let len = s.chars().count();
                    for (k, c) in s.chars().enumerate() {
                        obj.borrow_mut().props.insert(
                            PropKey::from_str(&k.to_string()),
                            Prop {
                                val: Value::Str(Rc::from(c.to_string().as_str())),
                                writable: false,
                                enumerable: true,
                                configurable: false,
                                get: None,
                                set: None,
                            },
                        );
                    }
                    obj.borrow_mut().props.insert(
                        PropKey::from_str("length"),
                        Prop {
                            val: Value::Num(len as f64),
                            writable: false,
                            enumerable: false,
                            configurable: false,
                            get: None,
                            set: None,
                        },
                    );
                }
                Ok(Value::Obj(obj))
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
    pub(crate) fn obj_get(&mut self, o: &ObjRef, key: &PropKey, receiver: &Value) -> ERes {
        let mut cur = Some(o.clone());
        while let Some(c) = cur {
            {
                if let Some(v) = view_own_prop(self, &c, key) {
                    return Ok(v);
                }
                let b = c.borrow();
                if let ObjKind::Proxy { target, handler } = &b.kind {
                    let (t, h, r) =
                        (target.clone(), handler.clone(), receiver.clone());
                    drop(b);
                    return self.proxy_get(&t, &h, key, &r);
                }
                if let Some(p) = b.props.get(key) {
                    if p.is_accessor() {
                        match &p.get {
                            // an accessor defined with `get: undefined`
                            // stores Some(Und) — present but not callable
                            Some(g) if self.is_callable(g) => {
                                let g = g.clone();
                                drop(b);
                                return self.call(&g, receiver.clone(), Vec::new());
                            }
                            _ => return Ok(Value::Und),
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
                if let Some(v) = view_own_prop_str(self, &c, key) {
                    return Ok(v);
                }
                let b = c.borrow();
                if let ObjKind::Proxy { target, handler } = &b.kind {
                    let (t, h, r) =
                        (target.clone(), handler.clone(), receiver.clone());
                    drop(b);
                    return self.proxy_get(
                        &t,
                        &h,
                        &PropKey::from_str(key),
                        &r,
                    );
                }
                if let Some(p) = b.props.get_str(key) {
                    if p.is_accessor() {
                        match &p.get {
                            Some(g) if self.is_callable(g) => {
                                let g = g.clone();
                                drop(b);
                                return self.call(&g, receiver.clone(), Vec::new());
                            }
                            _ => return Ok(Value::Und),
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
            Value::Obj(o) => {
                self.obj_set(o, key.clone(), v.clone(), base)?;
                // a write to the global object updates a var-declared
                // global binding — `var x` and `globalThis.x` are one slot
                if Rc::ptr_eq(o, &self.p.global) {
                    if let PropKey::S(s) = &key {
                        self.sync_global_binding(s, &v);
                    }
                }
                Ok(())
            }
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
                            if self.is_callable(&s) {
                                return self
                                    .call(&s, base.clone(), vec![v])
                                    .map(|_| ());
                            }
                        }
                        // non-setter accessor or data prop on the proto —
                        // primitives can't create own props → [[Set]] fails
                        return self.set_fail(&self.key_to_name(&key));
                    }
                    cur = c.borrow().proto.clone();
                }
                // no prop anywhere: CreateDataProperty on the primitive
                // receiver fails
                self.set_fail(&self.key_to_name(&key))
            }
        }
    }

    pub fn obj_set(
        &mut self,
        o: &ObjRef,
        key: PropKey,
        v: Value,
        receiver: &Value,
    ) -> Result<(), Value> {
        // proxy set trap fires before anything else (borrow scoped —
        // a trap may revoke/mutate the proxy mid-call)
        let proxy = {
            let b = o.borrow();
            match &b.kind {
                ObjKind::Proxy { target, handler } => {
                    Some((target.clone(), handler.clone()))
                }
                _ => None,
            }
        };
        if let Some((t, h)) = proxy {
            let r = receiver.clone();
            return self.proxy_set(&t, &h, &key, v, &r);
        }
        // integer-indexed writes + read-only virtuals on binary views
        {
            let b = o.borrow();
            match &b.kind {
                ObjKind::Typed {
                    buf,
                    off,
                    len,
                    kind,
                    track,
                } => {
                    if let PropKey::S(s) = &key {
                        // 10.4.5.6: canonical numeric strings are
                        // intercepted — TypedArraySetElement when
                        // receiver === O, silent for invalid indices;
                        // a valid index with a distinct receiver falls
                        // through to OrdinarySet (shadows on receiver).
                        if let Some(ni) = canonical_numeric_index(s) {
                            let same_recv = matches!(receiver,
                                Value::Obj(ro) if Rc::ptr_eq(ro, o));
                            let (buf, off, len, kind, track) =
                                (buf.clone(), *off, *len, *kind, *track);
                            drop(b);
                            if same_recv {
                                // IntegerIndexedElementSet: the value
                                // conversion runs even when the index is
                                // out of bounds (a valueOf may detach /
                                // resize), then a still-valid index
                                // writes the element store.
                                let n = crate::binary::ta_elem_write(
                                    self,
                                    &Value::Obj(o.clone()),
                                    &v,
                                )?;
                                if typed_state(&buf, off, len, kind, track)
                                    .is_some_and(|eff| valid_int_index(ni, eff))
                                {
                                    if buf.borrow().immutable {
                                        return Err(self.type_err(
                                            "Cannot write to an immutable ArrayBuffer",
                                        ));
                                    }
                                    if let Some(bytes) = buffer_bytes(&buf) {
                                        kind.write(
                                            &mut bytes.borrow_mut(),
                                            off + ni as usize * kind.size(),
                                            n,
                                        );
                                    }
                                }
                                return Ok(());
                            }
                            // distinct receiver: a valid index falls
                            // through to OrdinarySet → receiver shadows;
                            // an invalid one is simply ignored.
                            if !typed_state(&buf, off, len, kind, track)
                                .is_some_and(|eff| valid_int_index(ni, eff))
                            {
                                return Ok(());
                            }
                            let mut b = o.borrow_mut();
                            let _ = &mut b;
                        }
                        if matches!(
                            s.as_ref(),
                            "length" | "byteLength" | "byteOffset" | "buffer"
                        ) {
                            return self.set_fail(&self.key_to_name(&key));
                        }
                    }
                }
                ObjKind::Bytes(_) => {
                    if let PropKey::S(s) = &key {
                        if s.as_ref() == "byteLength" {
                            return self.set_fail(&self.key_to_name(&key));
                        }
                    }
                }
                ObjKind::View { .. } => {
                    if let PropKey::S(s) = &key {
                        if matches!(s.as_ref(), "byteLength" | "byteOffset" | "buffer") {
                            return self.set_fail(&self.key_to_name(&key));
                        }
                    }
                }
                _ => {}
            }
        }
        // private-name writes (PRIV_BASE syms): the slot must exist and
        // be writable (or have a setter) — checked at PutValue time, not
        // at reference resolution, so a field installed mid-destructure
        // (e.g. a getter that constructs on `this`) still lands
        if let PropKey::Sym(id) = &key {
            if *id >= PRIV_BASE {
                let ok = o
                    .borrow()
                    .props
                    .get(&key)
                    .is_some_and(|p| p.writable || p.set.is_some());
                if !ok {
                    return Err(self.type_err(
                        "Cannot write private member to an object whose class did not declare it"
                    ));
                }
            }
        }
        // arguments-object virtual index props are own props — they
        // shadow proto accessors entirely (10.6-11-b-1)
        {
            let mut b = o.borrow_mut();
            let args_i = if let PropKey::S(sk) = &key {
                arr_index(sk)
            } else {
                None
            };
            let args_hole = args_i
                .is_some_and(|i| b.holes.as_ref().is_some_and(|h| h.contains(&i)));
            // materialized slots take the normal walk so their attrs apply
            let args_mat = b.props.get(&key).is_some();
            if let ObjKind::Args { items, env, map } = &mut b.kind {
                if let PropKey::S(sk) = &key {
                    if let Some(i) = args_i {
                        if i < items.len() && !args_hole && !args_mat {
                            items[i] = v.clone();
                            if let (Some(e), Some(Some(name))) =
                                (&*env, map.get(i))
                            {
                                let name = name.clone();
                                if let Some(bnd) = e
                                    .vars
                                    .borrow_mut()
                                    .get_mut(name.as_ref())
                                {
                                    bnd.val = v;
                                }
                            }
                            return Ok(());
                        }
                    }
                }
            }
        }
        // walk chain looking for existing prop (setter or data)
        let mut cur = Some(o.clone());
        while let Some(c) = cur {
            // a TypedArray in the chain intercepts canonical numeric
            // strings via its own [[Set]] (10.4.5.6) — the recursion
            // parent.[[Set]](P, V, receiver) never reaches the tail
            if let PropKey::S(sk) = &key {
                let (ta_hit, write_elem) = {
                    let b = c.borrow();
                    if let ObjKind::Typed { buf, off, len, kind, track } =
                        &b.kind
                    {
                        if let Some(ni) = canonical_numeric_index(sk) {
                            let idx = ni as usize;
                            let valid = typed_state(buf, *off, *len, *kind, *track)
                                .is_some_and(|eff| valid_int_index(ni, eff));
                            let same = matches!(receiver,
                                Value::Obj(ro) if Rc::ptr_eq(ro, &c));
                            (
                                true,
                                if valid && same {
                                    Some((buf.clone(), *off, *len, *kind, *track, idx))
                                } else {
                                    None
                                },
                            )
                        } else {
                            (false, None)
                        }
                    } else {
                        (false, None)
                    }
                };
                if ta_hit {
                    match write_elem {
                        Some((buf, off, len, kind, track, idx)) => {
                            let n = crate::binary::ta_elem_write(
                                self,
                                &Value::Obj(c.clone()),
                                &v,
                            )?;
                            if typed_state(&buf, off, len, kind, track)
                                .is_some_and(|eff| idx < eff)
                            {
                                if let Some(bytes) = buffer_bytes(&buf) {
                                    kind.write(
                                        &mut bytes.borrow_mut(),
                                        off + idx * kind.size(),
                                        n,
                                    );
                                }
                            }
                            return Ok(());
                        }
                        // same receiver + invalid index: the coercion
                        // still runs (element-set semantics) then the
                        // write is dropped; a distinct receiver with an
                        // invalid index is left untouched entirely;
                        // a distinct receiver + valid index falls
                        // through so OrdinarySet shadows the receiver.
                        None => {
                            let ni = canonical_numeric_index(sk).unwrap();
                            let idx = ni as usize;
                            let eff_ok = {
                                let b = c.borrow();
                                if let ObjKind::Typed { buf, off, len, kind, track } =
                                    &b.kind
                                {
                                    typed_state(buf, *off, *len, *kind, *track)
                                        .is_some_and(|eff| valid_int_index(ni, eff))
                                } else {
                                    false
                                }
                            };
                            if !eff_ok {
                                if matches!(receiver,
                                    Value::Obj(ro) if Rc::ptr_eq(ro, &c))
                                {
                                    crate::binary::ta_elem_write(
                                        self,
                                        &Value::Obj(c.clone()),
                                        &v,
                                    )?;
                                }
                                return Ok(());
                            }
                            break;
                        }
                    }
                }
            }
            let (found_accessor, found_data, is_own) = {
                let b = c.borrow();
                match b.props.get(&key) {
                    // `set: undefined` stores Some(Und) — present in the
                    // descriptor but not callable → [[Set]] fails like None
                    Some(p) if p.is_accessor() => (
                        Some(p
                            .set
                            .clone()
                            .filter(|s| matches!(s, Value::Obj(_)))),
                        None,
                        false,
                    ),
                    Some(p) => (None, Some(p.writable), Rc::ptr_eq(&c, o)),
                    None => {
                        // a live dense element is an own writable data
                        // prop even though it lives outside `props` — it
                        // stops the chain walk like any other own prop
                        let virt = if let PropKey::S(sk) = &key {
                            arr_index(sk).is_some_and(|i| {
                                let items = match &b.kind {
                                    ObjKind::Arr(items) => Some(items.len()),
                                    ObjKind::Args { items, .. } => Some(items.len()),
                                    _ => None,
                                };
                                items.is_some_and(|n| {
                                    i < n && !b.holes.as_ref().is_some_and(|h| h.contains(&i))
                                })
                            })
                        } else {
                            false
                        };
                        if virt {
                            (None, Some(true), Rc::ptr_eq(&c, o))
                        } else {
                            (None, None, false)
                        }
                    }
                }
            };
            if let Some(setter) = found_accessor {
                return match setter {
                    Some(s) => self.call(&s, receiver.clone(), vec![v]).map(|_| ()),
                    None => self.set_fail(&self.key_to_name(&key)),
                };
            }
            if let Some(writable) = found_data {
                if is_own {
                    // OrdinarySet: a distinct receiver shadows via its own
                    // [[DefineOwnProperty]] instead of touching `o`
                    if let Value::Obj(ro) = receiver {
                        if !Rc::ptr_eq(ro, o) {
                            return self.set_on_receiver(receiver, key, v);
                        }
                    } else {
                        // OrdinarySetWithOwnDescriptor 4b: a primitive
                        // receiver can't hold the shadow — [[Set]] fails
                        return self.set_fail(&self.key_to_name(&key));
                    }
                    // a virtual dense element has no props entry — route
                    // the write through obj_define_data so items[i] (and
                    // a mapped param binding) stay the source of truth
                    let no_props_entry = c.borrow().props.get(&key).is_none();
                    if no_props_entry {
                        return self.obj_define_data(o, key, v);
                    }
                    // ArraySetLength coercion — a.length = v runs
                    // ToNumber → uint32 and rejects non-integral values
                    let mut v = v;
                    let arr_len = matches!(&c.borrow().kind, ObjKind::Arr(_))
                        && matches!(&key, PropKey::S(s) if s.as_ref() == "length");
                    if arr_len && writable {
                        let nl = self.to_num(&v)?;
                        let nlu = to_u32(nl) as f64;
                        if nl != nlu {
                            return Err(self.range_err("Invalid array length"));
                        }
                        v = Value::Num(nlu);
                        // the full ArraySetLength dance (element deletes,
                        // partial shrink on non-configurable) lives in
                        // obj_define_data
                        return self.obj_define_data(o, key, v);
                    }
                    // update own prop
                    let mut b = c.borrow_mut();
                    if !writable {
                        return self.set_fail(&self.key_to_name(&key));
                    }
                    if let ObjKind::Arr(items) = &mut b.kind {
                        if let PropKey::S(s) = &key {
                            // a materialized dense element keeps items[i]
                            // in sync — the props entry owns the attrs
                            if let Some(i) = arr_index(s) {
                                if i < items.len() {
                                    items[i] = v.clone();
                                }
                            }
                        }
                    }
                    // materialized but still-mapped arguments index:
                    // keep items + the parameter binding in sync
                    if let ObjKind::Args { items, env, map } = &mut b.kind {
                        if let PropKey::S(sk) = &key {
                            if let Some(i) = arr_index(sk) {
                                if i < items.len() {
                                    items[i] = v.clone();
                                    if let (Some(e), Some(Some(name))) =
                                        (&*env, map.get(i))
                                    {
                                        let name = name.clone();
                                        if let Some(bnd) = e
                                            .vars
                                            .borrow_mut()
                                            .get_mut(name.as_ref())
                                        {
                                            bnd.val = v.clone();
                                        }
                                    }
                                }
                            }
                        }
                    }
                    b.props.get_mut(&key).unwrap().val = v;
                    return Ok(());
                }
                // prop lives on a proto → writable: shadow with own
                // data prop; non-writable: [[Set]] fails
                if writable {
                    break;
                }
                return self.set_fail(&self.key_to_name(&key));
            }
            let next = c.borrow().proto.clone();
            cur = next;
        }
        // chain walk found nothing settable — OrdinarySet creates the
        // own property on the *receiver* (a proxy traps; a distinct
        // receiver like `super` or Reflect.set gets the shadow)
        self.set_on_receiver(receiver, key, v)
    }

    /// the dense/props insertion half of a set — writes a data prop on
    /// `o` itself (receiver == object, or an ordinary receiver)
    fn obj_define_data(
        &mut self,
        o: &ObjRef,
        key: PropKey,
        v: Value,
    ) -> Result<(), Value> {
        // ArraySetLength coercion — writing `length` on an array routes
        // the value through ToNumber → uint32 first
        let mut v = v;
        if matches!(&o.borrow().kind, ObjKind::Arr(_))
            && matches!(&key, PropKey::S(s) if s.as_ref() == "length")
        {
            let nl = self.to_num(&v)?;
            let nlu = to_u32(nl) as f64;
            if nl != nlu {
                return Err(self.range_err("Invalid array length"));
            }
            v = Value::Num(nlu);
        }
        let mut b = o.borrow_mut();
        // no blanket extensible gate here — existing virtual slots
        // (dense elements, mapped args, `length`) stay writable on
        // non-extensible objects; only NEW props are checked below
        let materialized = matches!(b.kind, ObjKind::Args { .. })
            && b.props.get(&key).is_some();
        if materialized {
            if let Some(p) = b.props.get_mut(&key) {
                p.val = v.clone();
            }
        }
        let args_hole = matches!(b.kind, ObjKind::Args { .. })
            && matches!(&key, PropKey::S(s) if arr_index(s)
                .is_some_and(|i| b.holes.as_ref().is_some_and(|h| h.contains(&i))));
        match &mut b.kind {
            ObjKind::Args { items, env, map } => {
                if let PropKey::S(s) = &key {
                    if let Some(i) = arr_index(s) {
                        // a deleted (holed) index isn't an element — the
                        // write must materialize an ordinary prop instead
                        // materialized prop: updated above; when still
                        // mapped the parameter binding stays in sync
                        if i < items.len() && !args_hole {
                            items[i] = v.clone();
                            // mapped index → write through to the param binding
                            if let (Some(e), Some(Some(name))) = (env, map.get(i)) {
                                let name = name.clone();
                                if let Some(bnd) =
                                    e.vars.borrow_mut().get_mut(name.as_ref())
                                {
                                    bnd.val = v;
                                }
                            }
                            return Ok(());
                        }
                    }
                }
            }
            _ => {}
        }
        let ta_cni = match (&b.kind, &key) {
            (ObjKind::Typed { .. }, PropKey::S(s)) => {
                canonical_numeric_index(s)
            }
            _ => None,
        };
        if let Some(ni) = ta_cni {
            let (buf, off, len, kind, track) = match &b.kind {
                ObjKind::Typed { buf, off, len, kind, track } => {
                    (buf.clone(), *off, *len, *kind, *track)
                }
                _ => unreachable!(),
            };
            drop(b);
            // integer-indexed define of a full data desc: invalid index
            // fails the define; valid index coerces + writes
            if !typed_state(&buf, off, len, kind, track)
                .is_some_and(|eff| valid_int_index(ni, eff))
            {
                return self.set_fail(&self.key_to_name(&key));
            }
            let n = crate::binary::ta_elem_write(
                self,
                &Value::Obj(o.clone()),
                &v,
            )?;
            if typed_state(&buf, off, len, kind, track)
                .is_some_and(|eff| valid_int_index(ni, eff))
            {
                if buf.borrow().immutable {
                    return Err(self.type_err(
                        "Cannot write to an immutable ArrayBuffer",
                    ));
                }
                if let Some(bytes) = buffer_bytes(&buf) {
                    kind.write(
                        &mut bytes.borrow_mut(),
                        off + ni as usize * kind.size(),
                        n,
                    );
                }
            }
            return Ok(());
        }
        {
            let bb = &mut *b;
        if let ObjKind::Arr(items) = &mut bb.kind {
            if let PropKey::S(s) = &key {
                // spec integer indices run to 2^32-2 — wider than the
                // dense store, so classify with `js_int_index` and only
                // back the small ones densely
                if let Some(i) = js_int_index(s) {
                    let lkey = PropKey::from_str("length");
                    let (cur_len, len_w) = match bb.props.get(&lkey) {
                        Some(p) => {
                            let n = match &p.val {
                                Value::Num(n) => {
                                    (*n as usize).max(items.len())
                                }
                                _ => items.len(),
                            };
                            (n, p.writable)
                        }
                        None => (items.len(), true),
                    };
                    // an index at/past length only defines when length is
                    // writable (spec 10.4.2.1 step 3.e)
                    if i >= cur_len && !len_w {
                        return self.set_fail(&self.key_to_name(&key));
                    }
                    // writing a hole or growing past the dense end
                    // creates a prop — blocked on non-extensible objects
                    let slot_exists =
                        i < items.len() && !bb.holes.as_ref().is_some_and(|h| h.contains(&i));
                    if !slot_exists && !bb.extensible {
                        return self.set_fail(&self.key_to_name(&key));
                    }
                    if i <= 8_000_000 {
                        if i >= items.len() {
                            // growth: the gap between the old end and the
                            // new element is holes, not undefined slots
                            let old = items.len();
                            items.resize(i + 1, Value::Und);
                            if i > old {
                                let h = bb.holes.get_or_insert_with(Default::default);
                                for k in old..i {
                                    h.insert(k);
                                }
                            }
                        }
                        items[i] = v.clone();
                        if let Some(h) = &mut bb.holes {
                            h.remove(&i);
                        }
                        // a materialized index prop stays authoritative —
                        // mirror the value into its entry
                        if let Some(p) = bb.props.get_mut(&key) {
                            p.val = v.clone();
                        }
                    } else {
                        // too sparse for dense storage — an ordinary props
                        // entry carries the value (extensibility checked
                        // via slot_exists above)
                        bb.props.insert(key.clone(), Prop::data(v.clone()));
                    }
                    if i + 1 > cur_len {
                        match bb.props.get_mut(&lkey) {
                            Some(p) => {
                                p.val = Value::Num((i + 1) as f64);
                            }
                            None => {
                                bb.props.insert(
                                    lkey,
                                    Prop {
                                        val: Value::Num((i + 1) as f64),
                                        writable: true,
                                        enumerable: false,
                                        configurable: false,
                                        get: None,
                                        set: None,
                                    },
                                );
                            }
                        }
                    }
                    return Ok(());
                }
                if s.as_ref() == "length" {
                    if let Value::Num(n) = v {
                        let n = n.max(0.0).min(4294967295.0) as usize;
                        let eff_len = items.len().max(match bb
                            .props
                            .get(&key)
                        {
                            Some(p) => match &p.val {
                                Value::Num(m) => *m as usize,
                                _ => 0,
                            },
                            None => 0,
                        });
                        if n < eff_len {
                            // spec deletes indices in [n, eff_len)
                            // descending; a non-configurable index aborts
                            // the shrink and the write itself fails
                            let doomed: Vec<(usize, PropKey)> = bb
                                .props
                                .iter()
                                .map(|(k, _)| k)
                                .filter_map(|k| match k {
                                    PropKey::S(s) => js_int_index(s.as_ref())
                                        .filter(|ix| *ix >= n && *ix < eff_len)
                                        .map(|ix| (ix, k.clone())),
                                    _ => None,
                                })
                                .collect();
                            let fail_at = doomed
                                .iter()
                                .filter(|(_, k)| {
                                    !bb.props
                                        .get(k)
                                        .map(|p| p.configurable)
                                        .unwrap_or(true)
                                })
                                .map(|(ix, _)| *ix)
                                .max();
                            if let Some(k) = fail_at {
                                let keep: Vec<PropKey> = doomed
                                    .iter()
                                    .filter(|(ix, _)| *ix > k)
                                    .map(|(_, k)| k.clone())
                                    .collect();
                                for pk in keep {
                                    bb.props.remove(&pk);
                                }
                                if k + 1 < items.len() {
                                    items.truncate(k + 1);
                                }
                                if let Some(h) = &mut bb.holes {
                                    h.retain(|&h| h < k + 1);
                                }
                                match bb.props.get_mut(&key) {
                                    Some(p) => {
                                        p.val = Value::Num((k + 1) as f64)
                                    }
                                    None => {
                                        bb.props.insert(
                                            key.clone(),
                                            Prop {
                                                val: Value::Num(
                                                    (k + 1) as f64,
                                                ),
                                                writable: true,
                                                enumerable: false,
                                                configurable: false,
                                                get: None,
                                                set: None,
                                            },
                                        );
                                    }
                                }
                                return self
                                    .set_fail(&self.key_to_name(&key));
                            }
                            for (_, pk) in doomed {
                                bb.props.remove(&pk);
                            }
                            if n < items.len() {
                                items.truncate(n);
                            }
                            if let Some(h) = &mut bb.holes {
                                h.retain(|&h| h < n);
                            }
                        }
                        // n > items.len() → sparse: record the declared
                        // length without dense allocation; a materialized
                        // entry keeps its attribute flags
                        match bb.props.get_mut(&key) {
                            Some(p) => p.val = Value::Num(n as f64),
                            None => {
                                bb.props.insert(
                                    key.clone(),
                                    Prop {
                                        val: Value::Num(n as f64),
                                        writable: true,
                                        enumerable: false,
                                        configurable: false,
                                        get: None,
                                        set: None,
                                    },
                                );
                            }
                        }
                        return Ok(());
                    }
                }
            }
        }
        }
        if let Some(p) = b.props.get_mut(&key) {
            // existing prop via a partial {value} define — only the
            // value merges; the stored attributes stand
            p.val = v;
            return Ok(());
        }
        // brand-new prop — the one place extensibility is enforced
        if !b.extensible {
            return self.set_fail(&self.key_to_name(&key));
        }
        b.props.insert(key, Prop::data(v));
        Ok(())
    }

    /// DefineField — class field install: `{writable, enumerable,
    /// configurable}` for ordinary names, but private `#x` slots are
    /// non-enumerable + non-configurable so they never leak through
    /// for-in / Object.keys / delete
    /// AddPrivateField presence check — the object's private-name slot
    /// must be empty before a private element installs
    fn priv_double_init_guard(&mut self, o: &ObjRef, key: &PropKey) -> Result<(), Value> {
        let b = o.borrow();
        let ks = || self.key_str(key);
        if b.props.get(key).is_some() {
            return Err(self.type_err(format!(
                "Cannot add private name {} twice to the same object",
                ks()
            )));
        }
        // private elements install on non-extensible receivers too? no —
        // AddPrivateField honors [[IsExtensible]] of the target
        if !b.extensible {
            return Err(self.type_err(format!(
                "Cannot add private name {} to a non-extensible object",
                ks()
            )));
        }
        Ok(())
    }

    fn define_field_prop(
        &mut self,
        o: &ObjRef,
        key: PropKey,
        v: Value,
        is_priv: bool,
    ) -> Result<(), Value> {
        if is_priv {
            // AddPrivateField — installing onto an object that already
            // has the private name is a TypeError (fields/methods install
            // once per instance; re-install = double initialisation)
            let mut b = o.borrow_mut();
            if b.props.get(&key).is_some() {
                let ks = self.key_str(&key);
                return Err(self.type_err(format!(
                    "Cannot add private name {ks} twice to the same object"
                )));
            }
            // AddPrivateField still honors non-extensible receivers
            if !b.extensible {
                let ks = self.key_str(&key);
                return Err(self.type_err(format!(
                    "Cannot add private name {ks} to a non-extensible object"
                )));
            }
            b.props.insert(
                key,
                Prop {
                    val: v,
                    writable: true,
                    enumerable: false,
                    configurable: false,
                    get: None,
                    set: None,
                },
            );
            Ok(())
        } else {
            // CreateDataPropertyOrThrow → [[DefineOwnProperty]] — a
            // proxy `this` (derived ctor returning a proxy) fires the
            // defineProperty trap with the full descriptor
            if matches!(&o.borrow().kind, ObjKind::Proxy { .. }) {
                self.recv_define(o, &key, v, true)
            } else {
                self.obj_define_data(o, key, v)
            }
        }
    }

    /// [[GetOwnProperty]] on the receiver for OrdinarySet's
    /// existingDescriptor check — a proxy receiver's
    /// `getOwnPropertyDescriptor` trap fires. Returns
    /// Some((is_accessor, writable)) or None.
    fn recv_own_desc(
        &mut self,
        ro: &ObjRef,
        key: &PropKey,
    ) -> Result<Option<(bool, bool)>, Value> {
        let proxy = {
            let b = ro.borrow();
            match &b.kind {
                ObjKind::Proxy { target, handler } => {
                    Some((target.clone(), handler.clone()))
                }
                _ => None,
            }
        };
        if let Some((t, h)) = proxy {
            return match self.proxy_gopd(&t, &h, key)? {
                Some(d) => {
                    Ok(Some((d.is_accessor(), d.writable.unwrap_or(false))))
                }
                None => Ok(None),
            };
        }
        // ordinary: a stored props entry wins over dense/virtual slots —
        // materialized indices carry real attributes
        {
            let b = ro.borrow();
            if let Some(p) = b.props.get(key) {
                return Ok(Some((p.is_accessor(), p.writable)));
            }
            if let PropKey::S(sk) = key {
                if let Some(i) = arr_index(sk) {
                    let virt = match &b.kind {
                        ObjKind::Arr(items) => {
                            i < items.len()
                                && !b.holes.as_ref().is_some_and(|h| h.contains(&i))
                        }
                        ObjKind::Args { items, .. } => i < items.len()
                            && !b.holes.as_ref().is_some_and(|h| h.contains(&i)),
                        ObjKind::Typed { buf, off, len, kind, track } => {
                            typed_state(buf, *off, *len, *kind, *track)
                                .is_some_and(|eff| i < eff)
                        }
                        _ => false,
                    };
                    if virt {
                        return Ok(Some((false, true)));
                    }
                }
            }
            Ok(None)
        }
    }

    /// [[DefineOwnProperty]](P, Desc) on the receiver — a proxy fires its
    /// `defineProperty` trap with a descriptor object.
    fn recv_define(
        &mut self,
        ro: &ObjRef,
        key: &PropKey,
        v: Value,
        full: bool,
    ) -> Result<(), Value> {
        let proxy = {
            let b = ro.borrow();
            match &b.kind {
                ObjKind::Proxy { target, handler } => {
                    Some((target.clone(), handler.clone()))
                }
                _ => None,
            }
        };
        if let Some((t, h)) = proxy {
            match self.proxy_trap(&h, "defineProperty")? {
                Some(trap) => {
                    let d = self.new_obj();
                    self.set_data(&d, "value", v.clone());
                    if full {
                        self.set_data(&d, "writable", Value::Bool(true));
                        self.set_data(&d, "enumerable", Value::Bool(true));
                        self.set_data(&d, "configurable", Value::Bool(true));
                    }
                    let kv = self.key_to_value(key);
                    let r =
                        self.call(&trap, h, vec![t.clone(), kv, Value::Obj(d)])?;
                    if self.to_bool(&r) {
                        // 10.5.6 invariants — a truish trap result must be
                        // backed by a compatible target-side define
                        let pd = PDesc {
                            value: Some(v),
                            writable: full.then_some(true),
                            get: None,
                            set: None,
                            enumerable: full.then_some(true),
                            configurable: full.then_some(true),
                        };
                        self.proxy_define_invariant(&t, key, &pd)?;
                        Ok(())
                    } else {
                        self.set_fail(&self.key_to_name(key))
                    }
                }
                None => match t {
                    Value::Obj(to) => {
                        self.recv_define(&to, key, v, full)
                    }
                    _ => self.set_fail(&self.key_to_name(key)),
                },
            }
        } else {
            self.obj_define_data(ro, key.clone(), v)
        }
    }

    /// OrdinarySet tail — walk produced no settable prop: the own prop
    /// materializes on `receiver` per spec (existingDescriptor → partial
    /// define, missing → CreateDataProperty).
    fn set_on_receiver(
        &mut self,
        receiver: &Value,
        key: PropKey,
        v: Value,
    ) -> Result<(), Value> {
        let Value::Obj(ro) = receiver else {
            return self.set_fail(&self.key_to_name(&key));
        };
        match self.recv_own_desc(ro, &key)? {
            Some((true, _)) | Some((_, false)) => {
                self.set_fail(&self.key_to_name(&key))
            }
            Some(_) => self.recv_define(ro, &key, v, false),
            None => self.recv_define(ro, &key, v, true),
        }
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
                            if self.is_callable(&s) {
                                return self
                                    .call(&s, base.clone(), vec![v])
                                    .map(|_| ());
                            }
                        }
                        // non-setter accessor or data prop on the proto —
                        // primitives can't create own props → [[Set]] fails
                        return self.set_fail(key);
                    }
                    cur = c.borrow().proto.clone();
                }
                self.set_fail(key)
            }
        }
    }

    /// `obj_set` by borrowed string key.
    fn obj_set_str(&mut self, o: &ObjRef, key: &str, v: Value, receiver: &Value) -> Result<(), Value> {
        // proxy set trap fires before anything else (borrow scoped —
        // a trap may revoke/mutate the proxy mid-call)
        let proxy = {
            let b = o.borrow();
            match &b.kind {
                ObjKind::Proxy { target, handler } => {
                    Some((target.clone(), handler.clone()))
                }
                _ => None,
            }
        };
        if let Some((t, h)) = proxy {
            let r = receiver.clone();
            return self.proxy_set(&t, &h, &PropKey::from_str(key), v, &r);
        }
        // integer-indexed writes + read-only virtuals on binary views
        {
            let b = o.borrow();
            match &b.kind {
                ObjKind::Typed {
                    buf,
                    off,
                    len,
                    kind,
                    track,
                } => {
                    if let Some(ni) = canonical_numeric_index(key) {
                        let idx = ni as usize;
                        let valid = typed_state(buf, *off, *len, *kind, *track)
                            .is_some_and(|eff| valid_int_index(ni, eff));
                        let same_recv = matches!(receiver,
                            Value::Obj(ro) if Rc::ptr_eq(ro, o));
                        if !(valid && !same_recv) {
                        let (buf, off, len, kind, track) =
                            (buf.clone(), *off, *len, *kind, *track);
                        drop(b);
                        // ToNumber/ToBigInt before the bounds check (spec
                        // IntegerIndexedElementSet honors [[ContentType]]).
                        let n = crate::binary::ta_elem_write(
                            self,
                            &Value::Obj(o.clone()),
                            &v,
                        )?;
                        if valid
                            && typed_state(&buf, off, len, kind, track)
                                .is_some_and(|eff| idx < eff)
                        {
                            if buf.borrow().immutable {
                                return Err(self.type_err(
                                    "Cannot write to an immutable ArrayBuffer",
                                ));
                            }
                            if let Some(bytes) = buffer_bytes(&buf) {
                                kind.write(
                                    &mut bytes.borrow_mut(),
                                    off + idx * kind.size(),
                                    n,
                                );
                            }
                        }
                        return Ok(());
                        }
                    }
                    if matches!(key, "length" | "byteLength" | "byteOffset" | "buffer") {
                        return self.set_fail(key);
                    }
                }
                ObjKind::Bytes(_) => {
                    if key == "byteLength" {
                        return self.set_fail(key);
                    }
                }
                ObjKind::View { .. } => {
                    if matches!(key, "byteLength" | "byteOffset" | "buffer") {
                        return self.set_fail(key);
                    }
                }
                _ => {}
            }
        }
        // arguments-object virtual index props are own props — they
        // shadow proto accessors entirely (10.6-11-b-1); materialized
        // slots go through the normal props walk (attrs apply)
        {
            let mut b = o.borrow_mut();
            let mat = b.props.get_str(key).is_some();
            let args_i = arr_index(key);
            let args_hole = args_i
                .is_some_and(|i| b.holes.as_ref().is_some_and(|h| h.contains(&i)));
            if let ObjKind::Args { items, env, map } = &mut b.kind {
                if let Some(i) = args_i {
                    if i < items.len() && !args_hole && !mat {
                        items[i] = v.clone();
                        if let (Some(e), Some(Some(name))) =
                            (&*env, map.get(i))
                        {
                            let name = name.clone();
                            if let Some(bnd) =
                                e.vars.borrow_mut().get_mut(name.as_ref())
                            {
                                bnd.val = v;
                            }
                        }
                        return Ok(());
                    }
                }
            }
        }
        // walk chain looking for existing prop (setter or data)
        let mut cur = Some(o.clone());
        while let Some(c) = cur {
            // TypedArray chain interception (10.4.5.6) — see obj_set
            {
                let (ta_hit, write_elem) = {
                    let b = c.borrow();
                    if let ObjKind::Typed { buf, off, len, kind, track } =
                        &b.kind
                    {
                        if let Some(ni) = canonical_numeric_index(key) {
                            let idx = ni as usize;
                            let valid = typed_state(buf, *off, *len, *kind, *track)
                                .is_some_and(|eff| valid_int_index(ni, eff));
                            let same = matches!(receiver,
                                Value::Obj(ro) if Rc::ptr_eq(ro, &c));
                            (
                                true,
                                if valid && same {
                                    Some((buf.clone(), *off, *len, *kind, *track, idx))
                                } else {
                                    None
                                },
                            )
                        } else {
                            (false, None)
                        }
                    } else {
                        (false, None)
                    }
                };
                if ta_hit {
                    match write_elem {
                        Some((buf, off, len, kind, track, idx)) => {
                            let n = crate::binary::ta_elem_write(
                                self,
                                &Value::Obj(c.clone()),
                                &v,
                            )?;
                            if typed_state(&buf, off, len, kind, track)
                                .is_some_and(|eff| idx < eff)
                            {
                                if let Some(bytes) = buffer_bytes(&buf) {
                                    kind.write(
                                        &mut bytes.borrow_mut(),
                                        off + idx * kind.size(),
                                        n,
                                    );
                                }
                            }
                            return Ok(());
                        }
                        None => {
                            let ni = canonical_numeric_index(key).unwrap();
                            let idx = ni as usize;
                            let eff_ok = {
                                let b = c.borrow();
                                if let ObjKind::Typed { buf, off, len, kind, track } =
                                    &b.kind
                                {
                                    typed_state(buf, *off, *len, *kind, *track)
                                        .is_some_and(|eff| valid_int_index(ni, eff))
                                } else {
                                    false
                                }
                            };
                            if !eff_ok {
                                if matches!(receiver,
                                    Value::Obj(ro) if Rc::ptr_eq(ro, &c))
                                {
                                    crate::binary::ta_elem_write(
                                        self,
                                        &Value::Obj(c.clone()),
                                        &v,
                                    )?;
                                }
                                return Ok(());
                            }
                            break;
                        }
                    }
                }
            }
            let (found_accessor, found_data, is_own) = {
                let b = c.borrow();
                match b.props.get_str(key) {
                    Some(p) if p.is_accessor() => (
                        Some(p
                            .set
                            .clone()
                            .filter(|s| matches!(s, Value::Obj(_)))),
                        None,
                        false,
                    ),
                    Some(p) => (None, Some(p.writable), Rc::ptr_eq(&c, o)),
                    None => {
                        // a live dense element is an own writable data
                        // prop even though it lives outside `props` — it
                        // stops the chain walk like any other own prop
                        let virt = arr_index(key).is_some_and(|i| {
                            let items = match &b.kind {
                                ObjKind::Arr(items) => Some(items.len()),
                                ObjKind::Args { items, .. } => Some(items.len()),
                                _ => None,
                            };
                            items.is_some_and(|n| {
                                i < n && !b.holes.as_ref().is_some_and(|h| h.contains(&i))
                            })
                        });
                        if virt {
                            (None, Some(true), Rc::ptr_eq(&c, o))
                        } else {
                            (None, None, false)
                        }
                    }
                }
            };
            if let Some(setter) = found_accessor {
                return match setter {
                    Some(s) => self.call(&s, receiver.clone(), vec![v]).map(|_| ()),
                    None => self.set_fail(key),
                };
            }
            if let Some(writable) = found_data {
                if is_own {
                    if let Value::Obj(ro) = receiver {
                        if !Rc::ptr_eq(ro, o) {
                            return self.set_on_receiver(
                                receiver,
                                PropKey::from_str(key),
                                v,
                            );
                        }
                    }
                    // a virtual dense element has no props entry — route
                    // the write through obj_define_data so items[i] (and
                    // a mapped param binding) stay the source of truth
                    let no_props_entry =
                        c.borrow().props.get_str(key).is_none();
                    if no_props_entry {
                        return self.obj_define_data(
                            o,
                            PropKey::from_str(key),
                            v,
                        );
                    }
                    // ArraySetLength coercion — a.length = v runs
                    // ToNumber → uint32 and rejects non-integral values
                    let mut v = v;
                    let arr_len = matches!(&c.borrow().kind, ObjKind::Arr(_))
                        && key == "length";
                    if arr_len && writable {
                        let nl = self.to_num(&v)?;
                        let nlu = to_u32(nl) as f64;
                        if nl != nlu {
                            return Err(self.range_err("Invalid array length"));
                        }
                        v = Value::Num(nlu);
                        // the full ArraySetLength dance (element deletes,
                        // partial shrink on non-configurable) lives in
                        // obj_define_data
                        return self.obj_define_data(
                            o,
                            PropKey::from_str(key),
                            v,
                        );
                    }
                    // update own prop
                    let mut b = c.borrow_mut();
                    if !writable {
                        return self.set_fail(key);
                    }
                    if let ObjKind::Arr(items) = &mut b.kind {
                        // a materialized dense element keeps items[i] in
                        // sync — the props entry owns the attrs
                        if let Some(i) = arr_index(key) {
                            if i < items.len() {
                                items[i] = v.clone();
                            }
                        }
                    }
                    // materialized but still-mapped arguments index:
                    // the parameter binding stays the source of truth
                    if let ObjKind::Args { items, env, map } = &mut b.kind {
                        if let Some(i) = arr_index(key) {
                            if i < items.len() {
                                items[i] = v.clone();
                                if let (Some(e), Some(Some(name))) =
                                    (&*env, map.get(i))
                                {
                                    let name = name.clone();
                                    if let Some(bnd) = e
                                        .vars
                                        .borrow_mut()
                                        .get_mut(name.as_ref())
                                    {
                                        bnd.val = v.clone();
                                    }
                                }
                            }
                        }
                    }
                    b.props.get_str_mut(key).unwrap().val = v;
                    return Ok(());
                }
                // prop lives on a proto → writable: shadow with own
                // data prop; non-writable: [[Set]] fails
                if writable {
                    break;
                }
                return self.set_fail(key);
            }
            let next = c.borrow().proto.clone();
            cur = next;
        }
        // chain walk found nothing settable — the own prop is created
        // on the receiver per OrdinarySet
        self.set_on_receiver(receiver, PropKey::from_str(key), v)
    }

    /// `delete_prop` by borrowed string key.
    pub fn delete_prop_str(&mut self, base: &Value, key: &str) -> bool {
        let Value::Obj(o) = base else {
            return true;
        };
        let mut b = o.borrow_mut();
        if let ObjKind::Typed { buf, off, len, kind, track } = &b.kind {
            if let Some(ni) = canonical_numeric_index(key) {
                return !typed_state(buf, *off, *len, *kind, *track)
                    .is_some_and(|eff| valid_int_index(ni, eff));
            }
        }
        if let Some(i) = arr_index(key) {
            let hole = b.holes.as_ref().is_some_and(|h| h.contains(&i));
            let materialized = b.props.get_str(key).is_some();
            if let ObjKind::Args { items, map, .. } = &mut b.kind {
                // materialized props fall through to the configurable check
                if i < items.len() && !hole && !materialized {
                    // delete: unmap the param link, mark the slot gone —
                    // `length` is a separate data prop and stays
                    if let Some(m) = map.get_mut(i) {
                        *m = None;
                    }
                    b.holes.get_or_insert_with(Default::default).insert(i);
                    return true;
                }
            }
            // materialized array indices fall through to the props path
            // so configurable:false is honoured
            if !materialized {
                if matches!(&b.kind, ObjKind::Arr(it) if i < it.len()) {
                    // spec: delete NEVER shrinks an array's length — the
                    // slot becomes a hole (even the last one)
                    b.holes.get_or_insert_with(Default::default).insert(i);
                    return true;
                }
            }
        }
        if matches!(b.kind, ObjKind::Arr(_)) && key == "length" {
            return false;
        }
        if b.props.get_str(key).is_some_and(|p| !p.configurable) {
            return false;
        }
        b.props.remove_str(key);
        // a deleted dense element leaves a hole in the backing store
        if matches!(&b.kind, ObjKind::Arr(_)) {
            if let Some(i) = arr_index(key) {
                b.holes.get_or_insert_with(Default::default).insert(i);
            }
        }
        // a materialized arguments index deleted from props also severs
        // the parameter link and leaves the slot a hole
        if let ObjKind::Args { map, .. } = &mut b.kind {
            if let Some(i) = arr_index(key) {
                if let Some(m) = map.get_mut(i) {
                    *m = None;
                }
                b.holes.get_or_insert_with(Default::default).insert(i);
            }
        }
        true
    }

    pub fn has_prop(&mut self, base: &Value, key: &PropKey) -> Result<bool, Value> {
        let Value::Obj(o) = base else {
            return Ok(false);
        };
        let mut cur = Some(o.clone());
        while let Some(c) = cur {
            {
                let b = c.borrow();
                if let ObjKind::Proxy { target, handler } = &b.kind {
                    let (t, h) = (target.clone(), handler.clone());
                    drop(b);
                    return self.proxy_has(&t, &h, key);
                }
                match &b.kind {
                    ObjKind::Arr(items) => {
                        if let PropKey::S(s) = key {
                            if s.as_ref() == "length" {
                                return Ok(true);
                            }
                            if let Some(i) = arr_index(s) {
                                if i < items.len()
                                    && !b.holes.as_ref().is_some_and(|h| h.contains(&i))
                                {
                                    return Ok(true);
                                }
                            }
                        }
                    }
                    ObjKind::Typed {
                        buf,
                        off,
                        len,
                        kind,
                        track,
                    } => {
                        if let PropKey::S(s) = key {
                            if let Some(ni) = canonical_numeric_index(s) {
                                // spec [[HasProperty]]: a canonical
                                // numeric index answers solely with
                                // IsValidIntegerIndex — never props/proto.
                                return Ok(typed_state(buf, *off, *len, *kind, *track)
                                    .is_some_and(|eff| valid_int_index(ni, eff)));
                            }
                        }
                    }
                    ObjKind::Args { items, .. } => {
                        if let PropKey::S(s) = key {
                            if let Some(i) = arr_index(s) {
                                if i < items.len()
                                    && !b.holes.as_ref().is_some_and(|h| h.contains(&i))
                                {
                                    return Ok(true);
                                }
                            }
                        }
                    }
                    _ => {}
                }
                if b.props.contains_key(key) {
                    return Ok(true);
                }
            }
            cur = c.borrow().proto.clone();
        }
        Ok(false)
    }

    pub fn delete_prop(&mut self, base: &Value, key: &PropKey) -> Result<bool, Value> {
        let Value::Obj(o) = base else {
            return Ok(true);
        };
        let proxy = {
            let b = o.borrow();
            match &b.kind {
                ObjKind::Proxy { target, handler } => {
                    Some((target.clone(), handler.clone()))
                }
                _ => None,
            }
        };
        if let Some((t, h)) = proxy {
            return self.proxy_delete(&t, &h, key);
        }
        let mut b = o.borrow_mut();
        if let PropKey::S(s) = key {
            if let ObjKind::Typed { buf, off, len, kind, track } = &b.kind {
                // 10.4.5.x integer-indexed [[Delete]]: a valid element
                // index is non-configurable → false; an invalid
                // canonical key deletes nothing → true.
                if let Some(ni) = canonical_numeric_index(s) {
                    return Ok(!typed_state(buf, *off, *len, *kind, *track)
                        .is_some_and(|eff| valid_int_index(ni, eff)));
                }
            }
            if let Some(i) = arr_index(s) {
                let hole = b.holes.as_ref().is_some_and(|h| h.contains(&i));
                let materialized = b.props.get(&key).is_some();
                if let ObjKind::Args { items, map, .. } = &mut b.kind {
                    if i < items.len() && !hole && !materialized {
                        if let Some(m) = map.get_mut(i) {
                            *m = None;
                        }
                        b.holes.get_or_insert_with(Default::default).insert(i);
                        return Ok(true);
                    }
                }
                // materialized array indices fall through to the props
                // path so configurable:false is honoured
                if !materialized {
                    if matches!(&b.kind, ObjKind::Arr(it) if i < it.len()) {
                        // spec: delete NEVER shrinks an array's length —
                        // even the last slot becomes a hole (10.4.2.2
                        // via ordinary [[Delete]])
                        b.holes.get_or_insert_with(Default::default).insert(i);
                        return Ok(true);
                    }
                }
            }
        }
        if matches!(b.kind, ObjKind::Arr(_))
            && matches!(&key, PropKey::S(s) if s.as_ref() == "length")
        {
            return Ok(false);
        }
        if b.props.get(key).is_some_and(|p| !p.configurable) {
            return Ok(false);
        }
        b.props.remove(key);
        // a deleted dense element leaves a hole in the backing store
        if matches!(&b.kind, ObjKind::Arr(_)) {
            if let PropKey::S(s) = key {
                if let Some(i) = arr_index(s) {
                    b.holes.get_or_insert_with(Default::default).insert(i);
                }
            }
        }
        if let ObjKind::Args { map, .. } = &mut b.kind {
            if let PropKey::S(s) = key {
                if let Some(i) = arr_index(s) {
                    if let Some(m) = map.get_mut(i) {
                        *m = None;
                    }
                    b.holes.get_or_insert_with(Default::default).insert(i);
                }
            }
        }
        Ok(true)
    }

    /// enumerable own + inherited string keys (for..in), deduped
    pub fn enum_keys(&mut self, base: &Value) -> Result<Vec<String>, Value> {
        let mut out = Vec::new();
        let mut seen: FxSet<String> = FxSet::default();
        if let Value::Str(s) = base {
            for i in 0..s.chars().count() {
                out.push(i.to_string());
            }
            return Ok(out);
        }
        let Value::Obj(o) = base else {
            return Ok(out);
        };
        let mut cur: Option<ObjRef> = Some(o.clone());
        while let Some(c) = cur {
            // a proxy level enumerates through traps per spec
            // EnumerateObjectProperties: [[OwnPropertyKeys]] → per-key
            // [[GetOwnProperty]] (enumerable filter) → [[GetPrototypeOf]].
            // (the borrow() is scoped — traps may touch the object)
            if { matches!(&c.borrow().kind, ObjKind::Proxy { .. }) } {
                let cv = Value::Obj(c.clone());
                for k in crate::builtins::own_keys_all(self, &cv)? {
                    let PropKey::S(s) = &k else {
                        continue;
                    };
                    let sname = s.to_string();
                    if seen.contains(&sname) {
                        continue;
                    }
                    let kv = self.key_to_value(&k);
                    let d = crate::builtins::obj_get_own_prop_desc(
                        self,
                        &Value::Und,
                        &[cv.clone(), kv],
                    )?;
                    // spec: a key reported by ownKeys but missing from
                    // [[GetOwnProperty]] does NOT block the proto chain
                    if let Value::Obj(dv) = d {
                        seen.insert(sname.clone());
                        let en =
                            self.get_prop_str(&Value::Obj(dv), "enumerable")?;
                        if self.to_bool(&en) {
                            out.push(sname);
                        }
                    }
                }
                let pv = crate::builtins::obj_get_prototype_of(
                    self,
                    &Value::Und,
                    &[cv],
                )?;
                cur = match pv {
                    Value::Obj(po) => Some(po),
                    _ => None,
                };
                continue;
            }
            {
                let b = c.borrow();
                // spec for-in order per level: integer-index keys ascending
                // numerically FIRST, then string keys in insertion order
                let mut ints: Vec<(u64, String)> = Vec::new();
                let mut strs: Vec<String> = Vec::new();
                match &b.kind {
                    ObjKind::Arr(items) => {
                        for i in 0..items.len() {
                            if b.holes.as_ref().is_some_and(|h| h.contains(&i)) {
                                continue;
                            }
                            let k = i.to_string();
                            // materialized indices enumerate via the props
                            // loop so their enumerable flag applies
                            if b.props.get_str(&k).is_some() {
                                continue;
                            }
                            ints.push((i as u64, k));
                        }
                    }
                    ObjKind::Typed {
                        buf,
                        off,
                        len,
                        kind,
                        track,
                    } => {
                        let eff =
                            typed_state(buf, *off, *len, *kind, *track).unwrap_or(0);
                        for i in 0..eff {
                            ints.push((i as u64, i.to_string()));
                        }
                    }
                    ObjKind::Args { items, .. } => {
                        for i in 0..items.len() {
                            if b.holes.as_ref().is_some_and(|h| h.contains(&i)) {
                                continue;
                            }
                            let k = i.to_string();
                            if b.props.get_str(&k).is_some() {
                                continue;
                            }
                            ints.push((i as u64, k));
                        }
                    }
                    _ => {}
                }
                let mut nonenum: Vec<String> = Vec::new();
                for (k, p) in b.props.iter() {
                    if let PropKey::S(s) = k {
                        if !p.enumerable {
                            // shadowed or not, a non-enum own prop still
                            // blocks the same name from enumerating higher
                            // up the proto chain
                            nonenum.push(s.to_string());
                            continue;
                        }
                        if is_internal_key(self, &b, s) {
                            continue;
                        }
                        // canonical numeric strings (incl. "-0", "1.5", big
                        // ints) order as ordinary string keys — only true
                        // array indices join the integer run
                        if let Some(i) = js_int_index(s) {
                            ints.push((i as u64, s.to_string()));
                        } else {
                            strs.push(s.to_string());
                        }
                    }
                }
                ints.sort_by_key(|(n, _)| *n);
                for (_, k) in ints {
                    if seen.insert(k.clone()) {
                        out.push(k);
                    }
                }
                for s in strs {
                    if seen.insert(s.clone()) {
                        out.push(s);
                    }
                }
                for s in nonenum {
                    seen.insert(s);
                }
            }
            cur = c.borrow().proto.clone();
        }
        Ok(out)
    }

    /// enumerable own string keys
    pub fn own_keys(&self, base: &Value) -> Vec<String> {
        let mut out = Vec::new();
        let Value::Obj(o) = base else {
            return out;
        };
        let b = o.borrow();
        // [[OwnPropertyKeys]] order: integer indices ascending, then
        // string keys in insertion order (symbols are separate)
        let mut ints: Vec<(usize, String)> = Vec::new();
        let mut rest: Vec<String> = Vec::new();
        match &b.kind {
            ObjKind::Arr(items) => {
                for i in 0..items.len() {
                    if b.holes.as_ref().is_some_and(|h| h.contains(&i)) {
                        continue;
                    }
                    let k = i.to_string();
                    // materialized indices enumerate via the props loop
                    // so their enumerable flag applies
                    if b.props.get_str(&k).is_some() {
                        continue;
                    }
                    ints.push((i, k));
                }
            }
            ObjKind::Typed {
                buf,
                off,
                len,
                kind,
                track,
            } => {
                let eff = typed_state(buf, *off, *len, *kind, *track).unwrap_or(0);
                for i in 0..eff {
                    ints.push((i, i.to_string()));
                }
            }
            ObjKind::Args { items, .. } => {
                for i in 0..items.len() {
                    if b.holes.as_ref().is_some_and(|h| h.contains(&i)) {
                        continue;
                    }
                    let k = i.to_string();
                    // materialized indices flow through the props loop so
                    // their stored enumerable flag is honoured
                    if b.props.get_str(&k).is_some() {
                        continue;
                    }
                    ints.push((i, k));
                }
            }
            _ => {}
        }
        for (k, p) in b.props.iter() {
            if !p.enumerable {
                continue;
            }
            if let PropKey::S(s) = k {
                if is_internal_key(self, &b, s) {
                    continue;
                }
                if let Some(i) = js_int_index(s) {
                    ints.push((i, s.to_string()));
                } else {
                    rest.push(s.to_string());
                }
            }
        }
        ints.sort_by_key(|(i, _)| *i);
        out.extend(ints.into_iter().map(|(_, s)| s));
        out.extend(rest);
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
            Value::Sym(..) => true,
            Value::Obj(o) => {
                let b = o.borrow();
                if b.props.contains_key(&PropKey::from_str("__bigint"))
                    && !b.props.contains_key(&PropKey::from_str("__bigint_wrap"))
                {
                    let n = b
                        .props
                        .get_str("__bigint")
                        .and_then(|p| match p.val {
                            Value::Num(n) => Some(n),
                            _ => None,
                        });
                    return n.is_some_and(|n| n != 0.0);
                }
                drop(b);
                !matches!(o.borrow().kind, ObjKind::Und)
            }
        }
    }

    pub fn to_primitive(&mut self, v: &Value) -> ERes {
        let Value::Obj(o) = v else {
            return Ok(v.clone());
        };
        if matches!(o.borrow().kind, ObjKind::Und) {
            return Ok(Value::Und);
        }
        // a BigInt stand-in is already a primitive — valueOf would
        // unwrap it to a Number and lose the type
        if crate::binary::bigint_magnitude(v).is_some() {
            return Ok(v.clone());
        }
        self.to_primitive_hint(v, o.clone(), "default")
    }

    /// ToPrimitive with an explicit hint — "string" tries `toString`
    /// before `valueOf`; "number"/"default" the reverse. The hint is
    /// also forwarded to `@@toPrimitive`.
    pub fn to_primitive_hint(&mut self, v: &Value, o: ObjRef, hint: &str) -> ERes {
        // Symbol.toPrimitive
        let stp = self.get_prop(v, &PropKey::Sym(SYM_TOPRIMITIVE))?;
        if self.is_callable(&stp) {
            let hint = Value::Str(Rc::from(hint));
            let r = self.call(&stp, v.clone(), vec![hint])?;
            if !matches!(r, Value::Obj(_))
                || crate::binary::bigint_magnitude(&r).is_some()
            {
                return Ok(r);
            }
            return Err(self.type_err("Cannot convert object to primitive value"));
        }
        // a present-but-non-callable @@toPrimitive is a TypeError —
        // falling through to valueOf/toString is only allowed when the
        // property is absent entirely
        if !matches!(stp, Value::Und | Value::Null) {
            return Err(self.type_err("Symbol.toPrimitive is not a function"));
        }
        // valueOf then toString — "string" hint flips the order
        let order: [&str; 2] = if hint == "string" {
            ["toString", "valueOf"]
        } else {
            ["valueOf", "toString"]
        };
        for name in order {
            let m = self.get_prop(v, &PropKey::from_str(name))?;
            if self.is_callable(&m) {
                let r = self.call(&m, v.clone(), Vec::new())?;
                if !matches!(r, Value::Obj(_))
                    || crate::binary::bigint_magnitude(&r).is_some()
                {
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
            Value::Obj(o) => {
                if let Some(m) = crate::binary::bigint_magnitude(v) {
                    return Ok(m);
                }
                let p = self.to_primitive_hint(v, o.clone(), "number")?;
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
            Value::Sym(..) => {
                return Err(self.type_err("Cannot convert a Symbol value to a string"))
            }
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
                if let Some(s) = crate::binary::bigint_dec_string(v) {
                    return Ok(s);
                }
                let p = self.to_primitive_hint(v, o.clone(), "string")?;
                self.to_str(&p)?
            }
        })
    }

    pub fn to_propkey(&mut self, v: &Value) -> Result<PropKey, Value> {
        Ok(match v {
            Value::Sym(id, _) => PropKey::Sym(*id),
            Value::Num(n) => PropKey::from_str(&js_num_str(*n)),
            Value::Str(s) => PropKey::S(s.clone()),
            // ToPropertyKey: ToPrimitive with a *string* hint (toString
            // before valueOf) — a Symbol result keys the property by
            // that symbol, anything else stringifies
            Value::Obj(o) => {
                if crate::binary::bigint_magnitude(v).is_some() {
                    // `x[2n]` — ToPropertyKey stringifies the bigint
                    let s = self.to_str(v)?;
                    return Ok(PropKey::from_str(&s));
                }
                let prim = self.to_primitive_hint(v, o.clone(), "string")?;
                return self.to_propkey(&prim);
            }
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
            // spec SameValue: NaN ≈ NaN, +0/-0 are NOT the same
            (Num(x), Num(y)) => {
                if x.is_nan() && y.is_nan() {
                    true
                } else if *x == 0.0 && *y == 0.0 {
                    x.is_sign_negative() == y.is_sign_negative()
                } else {
                    x == y
                }
            }
            (Str(x), Str(y)) => x == y,
            (Sym(x, _), Sym(y, _)) => x == y,
            (Obj(x), Obj(y)) => {
                Rc::ptr_eq(x, y)
                    || bigint_mag_eq(
                        &Value::Obj(x.clone()),
                        &Value::Obj(y.clone()),
                    )
            }
            _ => false,
        })
    }

    fn loose_eq(&mut self, a: &Value, b: &Value) -> Result<bool, Value> {
        use Value::*;
        Ok(match (a, b) {
            (Und, Und) | (Null, Null) => true,
            (Und, Null) | (Null, Und) => true,
            (Obj(x), Und) | (Und, Obj(x)) | (Obj(x), Null) | (Null, Obj(x))
                if matches!(x.borrow().kind, ObjKind::Und) =>
            {
                true
            }
            (Num(x), Num(y)) => x == y,
            (Str(x), Str(y)) => x == y,
            (Bool(x), Bool(y)) => x == y,
            (Sym(x, _), Sym(y, _)) => x == y,
            (Obj(x), Obj(y)) => {
                if Rc::ptr_eq(x, y) {
                    true
                } else {
                    let ab = crate::binary::bigint_magnitude(a).is_some();
                    let bb = crate::binary::bigint_magnitude(b).is_some();
                    match (ab, bb) {
                        (true, true) => {
                            crate::binary::bigint_cmp(a, b)
                                == Some(std::cmp::Ordering::Equal)
                        }
                        // one side a BigInt primitive, the other a real
                        // object — ToPrimitive the object then retry
                        (true, false) => {
                            let bp = self.to_primitive(b)?;
                            self.loose_eq(a, &bp)?
                        }
                        (false, true) => {
                            let ap = self.to_primitive(a)?;
                            self.loose_eq(&ap, b)?
                        }
                        (false, false) => false,
                    }
                }
            }
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
            (Obj(_), Sym(..)) => {
                // a BigInt primitive vs a Symbol — different primitive
                // types are simply not equal (no coercion runs)
                if crate::binary::bigint_magnitude(a).is_some() {
                    return Ok(false);
                }
                let ap = self.to_primitive(a)?;
                self.loose_eq(&ap, b)?
            }
            (Sym(..), Obj(_)) => {
                if crate::binary::bigint_magnitude(b).is_some() {
                    return Ok(false);
                }
                let bp = self.to_primitive(b)?;
                self.loose_eq(a, &bp)?
            }
            (Obj(_), Num(_)) | (Obj(_), Str(_)) => {
                let ap = self.to_primitive(a)?;
                // BigInt stand-ins stay Obj after ToPrimitive — exact
                // compare against the numeric/string side
                if crate::binary::bigint_magnitude(&ap).is_some() {
                    return Ok(crate::binary::bigint_loose_cmp(&ap, b)
                        .unwrap_or(false));
                }
                self.loose_eq(&ap, b)?
            }
            (Num(_), Obj(_)) | (Str(_), Obj(_)) => {
                let bp = self.to_primitive(b)?;
                if crate::binary::bigint_magnitude(&bp).is_some() {
                    return Ok(crate::binary::bigint_loose_cmp(a, &bp)
                        .unwrap_or(false));
                }
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
            (Obj(x), Obj(y)) => {
                Rc::ptr_eq(x, y)
                    || bigint_mag_eq(
                        &Value::Obj(x.clone()),
                        &Value::Obj(y.clone()),
                    )
            }
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
                let b = o.borrow();
                if b.props.contains_key(&PropKey::from_str("__bigint"))
                    && !b.props.contains_key(&PropKey::from_str("__bigint_wrap"))
                {
                    return "bigint";
                }
                if matches!(b.kind, ObjKind::Und) {
                    "undefined"
                } else if b.is_callable() {
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
                    ObjKind::Und => "undefined".into(),
                    ObjKind::IterLive { .. } => "Iterator {}".into(),
                    ObjKind::Args { items, .. } => {
                        let inner: Vec<String> = items
                            .iter()
                            .map(|v| self.inspect(v, depth + 1))
                            .collect();
                        format!("[{}]", inner.join(", "))
                    }
                    ObjKind::Proxy { target, .. } => self.inspect(&target, depth),
            ObjKind::Gen(_) => "[object Generator]".to_string(),
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
                    ObjKind::Iter { .. } | ObjKind::IterLazy(_) | ObjKind::IterBusy => {
                        "[Iterator]".into()
                    }
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
            ObjKind::Func(Func::Native { f: nf, name, .. }) => {
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
                ctor,
                inst_fields: _,
                ban_eval_args: _,
            }) => {
                // class constructors are [[Call]]-rejected per spec —
                // only [[Construct]] is defined for them
                if !matches!(ctor, CtorKind::NotClass) {
                    return Err(self.type_err(
                        "Class constructor cannot be invoked without 'new'",
                    ));
                }
                // strict-mode call semantics: `this` passes through verbatim
                // in strict code; sloppy code substitutes global for
                // undefined/null and boxes primitives
                let this = if def.is_arrow
                    || def.strict
                    || parser::stmts_strict(&def.body)
                {
                    this
                } else {
                    match this {
                        Value::Und | Value::Null => Value::Obj(self.p.global.clone()),
                        Value::Obj(_) => this,
                        v => self.to_object(&v)?,
                    }
                };
                // effective strictness = inherited (def.strict) or the
                // body's own "use strict" directive prologue
                let eff_strict =
                    def.strict || parser::stmts_strict(&def.body);
                let saved_strict = std::mem::replace(&mut self.cur_strict, eff_strict);
                let r = self.call_user_body(f, o, &def, &env, home, this, args);
                self.cur_strict = saved_strict;
                r
            }
            _ => self.call_dispatch_end(o, this, args),
        }
    }

    /// The `Func::User` arm of `call`, extracted so `call` can restore
    /// `cur_strict` on every exit path (including `?` early returns).
    fn call_user_body(
        &mut self,
        f: &Value,
        o: &ObjRef,
        def: &Rc<crate::ast::FnDef>,
        env: &EnvRef,
        home: Option<ObjRef>,
        this: Value,
        args: Vec<Value>,
    ) -> ERes {
        // a fn defined inside a class field initializer inherits the
        // `arguments`-in-eval ban — bump the context counter for the call
        let ban = matches!(
            o.borrow().kind,
            ObjKind::Func(Func::User {
                ban_eval_args: true,
                ..
            })
        );
        if ban {
            self.eval_args_ban += 1;
        }
        let r = self.call_user_body_inner(f, o, def, env, home, this, args);
        if ban {
            self.eval_args_ban -= 1;
        }
        r
    }

    fn call_user_body_inner(
        &mut self,
        f: &Value,
        o: &ObjRef,
        def: &Rc<crate::ast::FnDef>,
        env: &EnvRef,
        home: Option<ObjRef>,
        this: Value,
        args: Vec<Value>,
    ) -> ERes {
        {
                let scope = Scope::new(Some(env.clone()), !def.is_arrow);
                scope.var_boundary.set(true);
                if !def.is_arrow {
                    *scope.thisv.borrow_mut() = this;
                    *scope.home.borrow_mut() = home;
                    *scope.ctor_fn.borrow_mut() = f.clone();
                }
                // arrows inherit this/home/new.target lexically — is_fn=false
                // makes lookups walk through automatically.
                let h = self.hoisted_fn(&def);
                // `arguments` is visible inside parameter initializers —
                // bind it before defaults evaluate
                if !def.is_arrow && h.uses_arguments {
                    let arr = self.new_args_obj(&args, &scope, &def, f);
                    let k = self.intern("arguments");
                    scope.vars.borrow_mut().insert(
                        k,
                        Binding {
                            val: Value::Obj(arr),
                            mutable: true, uninit: false, deletable: false, lexical: false,
                        },
                    );
                }
                // a non-simple parameter environment rejects
                // `eval("var arguments")`/`eval("var eval")` — non-arrows
                // always have the implicit binding; arrows only when a
                // parameter is literally named arguments/eval. Set BEFORE
                // defaults evaluate: eval() in an initializer must see it.
                if def.params.iter().any(|p| matches!(p, Param::Default(..)))
                    && (!def.is_arrow
                        || def.params.iter().any(|p| {
                            matches!(p, Param::Ident(n) if n == "arguments" || n == "eval")
                        }))
                {
                    scope.param_env.set(true);
                }
                self.bind_params(&scope, &def.params, &args)?;
                // non-simple formals get a separate var environment —
                // body `var` decls must not leak into the param scope
                let body_scope: EnvRef = if def
                    .params
                    .iter()
                    .any(|p| !matches!(p, Param::Ident(_)))
                {
                    // arrows keep is_fn=false so `this`/`new.target`/
                    // `home`/`arguments` lookups pass through to the
                    // enclosing function scope — their scope's own slots
                    // were never initialized
                    let bs = Scope::new(Some(scope.clone()), !def.is_arrow);
                    bs.var_boundary.set(true);
                    if !def.is_arrow {
                        *bs.thisv.borrow_mut() = scope.thisv.borrow().clone();
                        *bs.new_target.borrow_mut() =
                            scope.new_target.borrow().clone();
                        *bs.home.borrow_mut() = scope.home.borrow().clone();
                        *bs.ctor_fn.borrow_mut() = scope.ctor_fn.borrow().clone();
                    }
                    bs
                } else {
                    scope.clone()
                };
                self.apply_hoisted(&h, &body_scope);
                if def.is_gen {
                    // lazy generator: the body runs on a coroutine stack
                    // allocated at first resume; `yield` suspends back to
                    // the resumer. The Interp is passed by raw pointer —
                    // only dereferenced while a resume() is on the stack,
                    // on this same thread.
                    // instances inherit the function's own `prototype`
                    // (proto %GeneratorPrototype%), not the shared proto
                    let gproto = match self
                        .obj_get_raw(&o, &PropKey::from_str("prototype"))
                    {
                        Some(Value::Obj(p)) => p,
                        _ => self.p.generator.clone(),
                    };
                    return Ok(Value::Obj(Rc::new(RefCell::new(Obj::new(
                        Some(gproto),
                        ObjKind::Gen(Rc::new(RefCell::new(GenCell {
                            coro: None,
                            pending: Some((def.clone(), body_scope)),
                            running: false,
                            done: false,
                        }))),
                    )))));
                }
                if let Some(e) = &def.arrow_expr {
                    // `(x) => expr` — implicit return
                    let e = e.clone();
                    self.depth += 1;
                    let r = self.eval_expr(&e, &body_scope);
                    self.depth -= 1;
                    return if def.is_async {
                        Ok(self.async_wrap(r))
                    } else {
                        r
                    };
                }
                self.depth += 1;
                let r = self.exec_block(&def.body, &body_scope);
                self.depth -= 1;
                let r = match r {
                    Ok(Completion::Return(v)) => Ok(v),
                    Ok(_) => Ok(Value::Und),
                    Err(e) => Err(e),
                };
                if def.is_async {
                    return Ok(self.async_wrap(r));
                }
                r
        }
    }

    /// AsyncFunction [[Call]] result: a Promise fulfilled with the return
    /// value / rejected with the thrown error. Promises settle
    /// synchronously in this engine.
    fn async_wrap(&mut self, r: ERes) -> Value {
        let state = match r {
            Ok(v) => PromiseState::Fulfilled(Box::new(v)),
            Err(e) => PromiseState::Rejected(Box::new(e)),
        };
        Value::Obj(Rc::new(RefCell::new(Obj::new(
            Some(self.p.promise.clone()),
            ObjKind::Promise(state),
        ))))
    }

    fn call_dispatch_end(&mut self, o: &ObjRef, this: Value, args: Vec<Value>) -> ERes {
        // scoped borrow — a proxy's `apply` trap / `revoke()` may
        // mutate the object while the call runs
        let kind = { o.borrow().kind.clone() };
        match kind {
            ObjKind::Und => Ok(Value::Und),
            ObjKind::Bound {
                target,
                thisv,
                extra,
            } => {
                let mut all = extra;
                all.extend(args);
                self.call(&target, thisv, all)
            }
            ObjKind::Proxy { target, handler } => {
                // spec: a proxy is callable only when its target is —
                // reaching [[Call]] on a non-callable proxy (or a
                // revoked one) is "not a function" before any trap
                if !matches!(&target, Value::Obj(t) if t.borrow().is_callable()) {
                    return Err(self.type_err("value is not a function"));
                }
                match self.proxy_trap(&handler, "apply")? {
                    Some(trap) => {
                        let args_arr = Value::Obj(self.new_arr(args));
                        self.call(&trap, handler, vec![target, this, args_arr])
                    }
                    None => self.call(&target, this, args),
                }
            }
            _ => Err(self.type_err("value is not a function")),
        }
    }

    /// spec IsConstructor: user fns unless arrow/method (home set),
    /// natives only with an own `.prototype`, bound fns delegate.
    pub fn is_constructor(&self, v: &Value) -> bool {
        let Value::Obj(o) = v else { return false };
        match &o.borrow().kind {
            ObjKind::Func(Func::User { def, home, ctor, .. }) => {
                !def.is_arrow
                    && !def.is_gen
                    && !def.is_async
                    && (!matches!(ctor, CtorKind::NotClass) || home.is_none())
            }
            ObjKind::Func(Func::Native { ctor, .. }) => *ctor,
            ObjKind::Bound { target, .. } => self.is_constructor(target),
            ObjKind::Proxy { target, .. } => self.is_constructor(target),
            _ => false,
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
        if let Some(nt) = &new_target {
            if !self.is_constructor(nt) {
                return Err(self.type_err("newTarget is not a constructor"));
            }
        }
        // scoped borrow — a proxy's `construct` trap / `revoke()` may
        // mutate the object mid-call
        let kind = { o.borrow().kind.clone() };
        match kind {
            ObjKind::Proxy { target, handler } => {
                // spec: [[Construct]] exists on a proxy only when the
                // target is a constructor (a revoked target fails here)
                if !matches!(&target, Value::Obj(t) if self.is_constructor(&Value::Obj(t.clone()))) {
                    return Err(self.type_err("value is not a constructor"));
                }
                match self.proxy_trap(&handler, "construct")? {
                    Some(trap) => {
                        let args_arr = Value::Obj(self.new_arr(args));
                        let nt = new_target.unwrap_or_else(|| f.clone());
                        let r = self.call(
                            &trap,
                            handler,
                            vec![target, args_arr, nt],
                        )?;
                        if !matches!(r, Value::Obj(_)) {
                            return Err(self.type_err(
                                "proxy construct trap must return an object",
                            ));
                        }
                        Ok(r)
                    }
                    // spec: absent trap forwards to target.[[Construct]]
                    // with the ORIGINAL newTarget — for `new p()` that is
                    // the proxy itself (its `prototype` get traps)
                    None => {
                        let nt = new_target.or_else(|| Some(f.clone()));
                        self.construct(&target, args, nt)
                    }
                }
            }
            ObjKind::Func(Func::Native { f: nf, name, ctor: nctor }) => {
                // natives act as their own ctor (Error, Array(1,2), etc.)
                // — but only ctors; plain methods like Math.abs are not
                // [[Construct]]-able. `ctor` is authoritative — Proxy has
                // [[Construct]] but NO own `prototype` prop.
                let proto_prop = self
                    .obj_get_raw(&o.clone(), &PropKey::from_str("prototype"))
                    .unwrap_or(Value::Und);
                if !nctor {
                    return Err(self.type_err(format!("{name} is not a constructor")));
                }
                // OrdinaryCreateFromConstructor: new_target.prototype wins —
                // read through [[Get]] so a proxy newTarget fires its
                // `get` trap (revoked / throwing traps propagate)
                let nt_proto = match &new_target {
                    Some(ntv @ Value::Obj(_)) => {
                        self.get_prop(ntv, &PropKey::from_str("prototype"))?
                    }
                    _ => Value::Und,
                };
                let proto_v = if matches!(nt_proto, Value::Obj(_)) {
                    nt_proto
                } else {
                    proto_prop
                };
                let this = if let Value::Obj(proto) = &proto_v {
                    Value::Obj(Rc::new(RefCell::new(Obj::new(
                        Some(proto.clone()),
                        ObjKind::Ord,
                    ))))
                } else {
                    Value::Und
                };
                self.depth += 1;
                let prev_nt = self.new_target_native.replace(
                    new_target.clone().unwrap_or_else(|| f.clone()),
                );
                let r = nf(self, &this, &args);
                self.new_target_native = prev_nt;
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
                home,
                ctor,
                inst_fields: _,
                ban_eval_args: _,
            }) => {
                if def.is_arrow
                    || def.is_gen
                    || def.is_async
                    || (home.is_some() && matches!(ctor, CtorKind::NotClass))
                {
                    return Err(self.type_err("value is not a constructor"));
                }
                let eff_strict =
                    def.strict || parser::stmts_strict(&def.body);
                let saved_strict = std::mem::replace(&mut self.cur_strict, eff_strict);
                let r = self.construct_user_body(f, o, def, env, home, ctor, new_target, args);
                self.cur_strict = saved_strict;
                r
            }
            ObjKind::Bound { target, extra, .. } => {
                let mut all = extra;
                all.extend(args);
                self.construct(&target, all, new_target)
            }
            _ => Err(self.type_err("value is not a constructor")),
        }
    }

    /// The `Func::User` arm of `construct` (strict save/restore lives in the
    /// caller for the same reason as `call_user_body`).
    #[allow(clippy::too_many_arguments)]
    fn construct_user_body(
        &mut self,
        f: &Value,
        _o: &ObjRef,
        def: Rc<crate::ast::FnDef>,
        env: EnvRef,
        home: Option<ObjRef>,
        ctor: CtorKind,
        new_target: Option<Value>,
        args: Vec<Value>,
    ) -> ERes {
        {
                let nt = new_target.unwrap_or_else(|| f.clone());
                // prototype for the new object: new_target.prototype or obj proto
                // GetPrototypeFromConstructor reads newTarget.prototype
                // through [[Get]] — a proxy newTarget fires its `get`
                // trap; a non-object result falls back to %Object.prototype%
                let proto = match &nt {
                    Value::Obj(_) => {
                        match self
                            .get_prop(&nt, &PropKey::from_str("prototype"))?
                        {
                            Value::Obj(p) => Some(p),
                            _ => Some(self.p.obj.clone()),
                        }
                    }
                    _ => Some(self.p.obj.clone()),
                };
                let this = match ctor {
                    CtorKind::Derived => Value::Und, // allocated at super()
                    _ => Value::Obj(Rc::new(RefCell::new(Obj::new(proto, ObjKind::Ord)))),
                };
                let scope = Scope::new(Some(env), true);
                *scope.thisv.borrow_mut() = this.clone();
                *scope.new_target.borrow_mut() = nt.clone();
                *scope.home.borrow_mut() = home;
                *scope.ctor_fn.borrow_mut() = f.clone();
                // derived ctor: `this` is in TDZ until super() binds it
                if matches!(ctor, CtorKind::Derived) {
                    scope.this_uninit.set(true);
                }
                // base ctor: instance fields init in BindThis — before
                // parameters bind (InitializeInstanceElements, [[Construct]]#7)
                if let Value::Obj(fo) = &f {
                    if matches!(ctor, CtorKind::Base) {
                        self.init_class_fields(&this, fo, &nt)?;
                    }
                }
                let h = self.hoisted_fn(&def);
                // `arguments` is visible inside parameter initializers —
                // bind it before defaults evaluate
                if h.uses_arguments {
                    let arr = self.new_args_obj(&args, &scope, &def, f);
                    let k = self.intern("arguments");
                    scope.vars.borrow_mut().insert(
                        k,
                        Binding {
                            val: Value::Obj(arr),
                            mutable: true, uninit: false, deletable: false, lexical: false,
                        },
                    );
                }
                // a non-simple parameter environment rejects
                // `eval("var arguments")`/`eval("var eval")` — non-arrows
                // always have the implicit binding; arrows only when a
                // parameter is literally named arguments/eval. Set BEFORE
                // defaults evaluate: eval() in an initializer must see it.
                if def.params.iter().any(|p| matches!(p, Param::Default(..)))
                    && (!def.is_arrow
                        || def.params.iter().any(|p| {
                            matches!(p, Param::Ident(n) if n == "arguments" || n == "eval")
                        }))
                {
                    scope.param_env.set(true);
                }
                self.bind_params(&scope, &def.params, &args)?;
                let body_scope: EnvRef = if def
                    .params
                    .iter()
                    .any(|p| !matches!(p, Param::Ident(_)))
                {
                    let bs = Scope::new(Some(scope.clone()), true);
                    bs.var_boundary.set(true);
                    *bs.thisv.borrow_mut() = scope.thisv.borrow().clone();
                    *bs.new_target.borrow_mut() =
                        scope.new_target.borrow().clone();
                    *bs.home.borrow_mut() = scope.home.borrow().clone();
                    *bs.ctor_fn.borrow_mut() = scope.ctor_fn.borrow().clone();
                    bs.this_uninit.set(scope.this_uninit.get());
                    bs
                } else {
                    scope.clone()
                };
                self.apply_hoisted(&h, &body_scope);
                self.depth += 1;
                let r = self.exec_block(&def.body, &body_scope);
                self.depth -= 1;
                let this_out = body_scope.thisv.borrow().clone();
                // derived ctor: `return <primitive ≠ undefined>` is a
                // TypeError ([[Construct]] 9.2.2#13c); base ctors
                // silently ignore primitive returns
                let bad_prim = matches!(
                    &r,
                    Ok(Completion::Return(v)) if !matches!(v, Value::Und | Value::Obj(_))
                        && !self.is_callable(v)
                );
                match r {
                    Ok(Completion::Return(v))
                        if self.is_callable(&v) || matches!(v, Value::Obj(_)) =>
                    {
                        Ok(v)
                    }
                    Ok(_) => {
                        if matches!(ctor, CtorKind::Derived) && bad_prim {
                            return Err(self.type_err(
                                "Derived constructors may only return object or undefined",
                            ));
                        }
                        if matches!(ctor, CtorKind::Derived)
                            && body_scope.this_uninit.get()
                        {
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
    }

    fn bind_params(
        &mut self,
        scope: &EnvRef,
        params: &[Param],
        args: &[Value],
    ) -> Result<(), Value> {
        // instantiate all parameter bindings uninitialized (TDZ) —
        // a default initializer may not read a not-yet-bound param
        {
            let mut vs = scope.vars.borrow_mut();
            for p in params {
                let mut names: Vec<&str> = Vec::new();
                crate::parser::pattern_names(p, &mut names);
                for n in names {
                    let k = self.intern(n);
                    vs.insert(
                        k,
                        Binding {
                            val: Value::Und,
                            mutable: true,
                            uninit: true, deletable: false, lexical: false,
                        },
                    );
                }
            }
        }
        // (parameter-environment flag is set by the callers, who know the
        //  FnDef — arrows have no implicit `arguments` binding)
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
                            mutable: true, uninit: false, deletable: false, lexical: false,
                        },
                    );
                    i += 1;
                }
                Param::Default(pat, dflt) => {
                    let v = match args.get(i) {
                        Some(Value::Und) | None => self.eval_expr(dflt, scope)?,
                        Some(v) => v.clone(),
                    };
                    if let Pattern::Ident(n) = pat.as_ref() {
                        if let Some(Value::Und) | None = args.get(i) {
                            if Self::is_anon_fn_def(dflt) {
                                self.maybe_name(&v, n);
                            }
                        }
                    }
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
        self.pat_bind(scope, pat, v, BindMode::Decl(kind))
    }

    /// IsAnonymousFunctionDefinition — only direct anon fn/class/arrow
    /// exprs get NamedEvaluation (`(0, function(){})` does not).
    fn is_anon_fn_def(e: &Expr) -> bool {
        match e {
            // spec IsAnonymousFunctionDefinition looks through parens
            Expr::Paren(inner) => Self::is_anon_fn_def(inner),
            Expr::Fn(d) => d.name.is_none(),
            Expr::Class(c) => c.name.is_none(),
            _ => false,
        }
    }

    /// evaluate `e` applying NamedEvaluation — anonymous classes get the
    /// name DURING class evaluation (inner binding + ctor `name` are set
    /// before static elements run); other anon fns take it post-eval
    fn eval_expr_named(&mut self, e: &Expr, env: &EnvRef, name: &str) -> ERes {
        if let Expr::Class(c) = e {
            return self.make_class_named(c, env, Some(name));
        }
        let v = self.eval_expr(e, env)?;
        self.maybe_name(&v, name);
        Ok(v)
    }

    /// NamedEvaluation — an anonymous fn/class bound to a name takes it,
    /// unless it already has an own `name` (e.g. `static name` members).
    fn maybe_name(&mut self, v: &Value, name: &str) {
        if let Value::Obj(o) = v {
            let b = o.borrow();
            let needs = match &b.kind {
                ObjKind::Func(Func::User { def, .. }) => {
                    def.name.is_none()
                        && matches!(
                            b.props.get(&PropKey::from_str("name")),
                            Some(p) if matches!(&p.val, Value::Str(s) if s.is_empty())
                        )
                }
                _ => false,
            };
            drop(b);
            if needs {
                self.set_attr(
                    o,
                    "name",
                    Value::Str(Rc::from(name)),
                    false,
                    true,
                );
            }
        }
    }

    /// Resolve a member expression into an assignable reference — the
    /// base and key evaluate now; the store happens later (PutValue).
    /// Destructuring evaluates the target ref BEFORE pulling iterator
    /// values / reading object keys, so the two phases are split.
    fn eval_lref(&mut self, e: &Expr, env: &EnvRef) -> Result<LRef, Value> {
        let Expr::Member {
            obj,
            prop,
            computed,
            ..
        } = e
        else {
            return Err(self.syntax_err("invalid assignment target"));
        };
        if matches!(&**obj, Expr::Super) {
            let Some((fscope, home)) = self.find_home(env) else {
                return Err(self.syntax_err("'super' keyword unexpected here"));
            };
            if fscope.this_uninit.get() {
                return Err(self.ref_err(
                    "Must call super constructor in derived class before accessing 'this'",
                ));
            }
            let this = fscope.thisv.borrow().clone();
            if let Expr::Str(s) = &**prop {
                return Ok(LRef::Super(home, this, PropKey::from_str(s)));
            }
            let pv = self.eval_expr(prop, env)?;
            return Ok(LRef::SuperV(home, this, pv));
        }
        let ov = self.eval_expr(obj, env)?;
        if let Expr::Str(s) = &**prop {
            // `o.#x` — resolve the PrivateName through the class env to
            // its Sym slot; `o["#x"]` stays an ordinary string-key write.
            if !*computed && s.starts_with('#') {
                let Some((Value::Sym(id, _), _)) = self.env_lookup_st(env, s.as_str())?
                else {
                    return Err(self.syntax_err(format!("Private member {s} must be declared")));
                };
                // the slot check runs at PutValue time — destructuring
                // evaluates the RHS (getters, calls) before the write,
                // and the field may be installed mid-destructure
                if !matches!(&ov, Value::Obj(_)) {
                    return Err(self.type_err(format!(
                        "Cannot write private member {s}"
                    )));
                }
                return Ok(LRef::Prop(ov, PropKey::Sym(id)));
            }
            return Ok(LRef::Prop(ov, PropKey::from_str(s)));
        }
        let pv = self.eval_expr(prop, env)?;
        Ok(LRef::PropV(ov, pv))
    }

    /// Resolve an identifier reference — the binding location is
    /// captured now (an object-env base becomes a property reference on
    /// the object itself) so PutValue writes through the initially
    /// created reference per spec
    fn eval_lref_ident(
        &mut self,
        env: &EnvRef,
        name: &str,
    ) -> Result<LRef, Value> {
        let mut cur: &EnvRef = env;
        loop {
            if let Some(o) = &cur.obj_env {
                if self.with_has(o, name)? {
                    return Ok(LRef::WithObj(o.clone(), self.intern(name)));
                }
            }
            if cur.vars.borrow().contains_key(name) {
                return Ok(LRef::Env(cur.clone(), self.intern(name)));
            }
            match &cur.parent {
                Some(p) => cur = p,
                None => break,
            }
        }
        // the global env's object record: an unresolved name still binds
        // to a global-object property — `x = 1`/`x += 1` where x is a
        // prop (or getter) on globalThis. WithObj matches
        // ObjectEnvironmentRecord PutValue semantics (strict missing →
        // ReferenceError, sloppy → [[Set]] creates the prop)
        if self.has_prop(
            &Value::Obj(self.p.global.clone()),
            &PropKey::from_str(name),
        )? {
            return Ok(LRef::WithObj(self.p.global.clone(), self.intern(name)));
        }
        Ok(LRef::Unres(self.intern(name)))
    }

    fn member_key(&mut self, prop: &Expr, env: &EnvRef) -> Result<PropKey, Value> {
        if let Expr::Str(s) = prop {
            return Ok(PropKey::from_str(s));
        }
        let pv = self.eval_expr(prop, env)?;
        self.to_propkey(&pv)
    }

    /// A [[Set]] that spec-returns false: silent in sloppy, TypeError in strict.
    fn set_fail(&mut self, key: &str) -> Result<(), Value> {
        self.set_failed = true;
        if self.cur_strict {
            Err(self.type_err(format!(
                "Cannot assign to read only property '{key}'"
            )))
        } else {
            Ok(())
        }
    }

    /// GetValue on a resolved reference — compound assignments pin the
    /// ref before reading, so an object-env base keeps pointing at the
    /// object even if HasProperty flips mid-operation
    fn get_lref(&mut self, lr: &mut LRef) -> ERes {
        match lr {
            LRef::Prop(b, k) => self.get_prop(b, k),
            LRef::PropV(b, kv) => {
                // ES2019 order: a nullish base throws before the key
                // expression is coerced; ToPropertyKey runs once — the
                // resolved key is baked into the ref for PutValue
                if matches!(b, Value::Und | Value::Null) {
                    return self.get_prop(&b.clone(), &PropKey::from_str(""));
                }
                let k = self.to_propkey(kv)?;
                let b = b.clone();
                *lr = LRef::Prop(b.clone(), k.clone());
                self.get_prop(&b, &k)
            }
            LRef::Super(home, this, k) => {
                let parent = home.borrow().proto.clone();
                match parent {
                    Some(p) => self.obj_get(&p, k, this),
                    None => Ok(Value::Und),
                }
            }
            LRef::SuperV(home, this, kv) => {
                let k = self.to_propkey(kv)?;
                let (home, this) = (home.clone(), this.clone());
                *lr = LRef::Super(home.clone(), this.clone(), k.clone());
                let parent = home.borrow().proto.clone();
                match parent {
                    Some(p) => self.obj_get(&p, &k, &this),
                    None => Ok(Value::Und),
                }
            }
            LRef::Env(sc, name) => {
                let st = sc
                    .vars
                    .borrow()
                    .get(name.as_ref())
                    .map(|b| (b.val.clone(), b.uninit));
                match st {
                    Some((_, true)) => Err(self.ref_err(format!(
                        "Cannot access '{name}' before initialization"
                    ))),
                    Some((v, false)) => Ok(v),
                    None => Err(self.ref_err(format!("{name} is not defined"))),
                }
            }
            LRef::WithObj(o, name) => self.with_get(o, name),
            LRef::Unres(name) => {
                Err(self.ref_err(format!("{name} is not defined")))
            }
        }
    }

    fn put_lref(&mut self, lr: LRef, v: Value) -> Result<(), Value> {
        match lr {
            LRef::Prop(b, k) => self.set_prop(&b, k, v),
            LRef::PropV(b, kv) => {
                let k = self.to_propkey(&kv)?;
                self.set_prop(&b, k, v)
            }
            LRef::Super(home, this, k) => {
                let parent = home.borrow().proto.clone();
                let Some(parent) = parent else {
                    return Err(self.type_err(
                        "Cannot set property of null (via super)"
                    ));
                };
                self.obj_set(&parent, k, v, &this)
            }
            LRef::SuperV(home, this, kv) => {
                let parent = home.borrow().proto.clone();
                let Some(parent) = parent else {
                    return Err(self.type_err(
                        "Cannot set property of null (via super)"
                    ));
                };
                let k = self.to_propkey(&kv)?;
                self.obj_set(&parent, k, v, &this)
            }
            LRef::Env(sc, name) => {
                let mut vars = sc.vars.borrow_mut();
                match vars.get_mut(name.as_ref()) {
                    Some(b) => {
                        if b.uninit {
                            drop(vars);
                            return Err(self.ref_err(format!(
                                "Cannot access '{name}' before initialization"
                            )));
                        }
                        if !b.mutable {
                            let hard = b.lexical;
                            drop(vars);
                            if hard || self.cur_strict {
                                return Err(self.type_err(format!(
                                    "Assignment to constant variable '{name}'"
                                )));
                            }
                            return Ok(());
                        }
                        let lexical = b.lexical;
                        b.val = v.clone();
                        drop(vars);
                        // global `var` bindings mirror the global-object prop
                        if !lexical && Rc::ptr_eq(&sc, &self.global) {
                            let g = self.p.global.clone();
                            let pk = PropKey::S(name.clone());
                            let mut gb = g.borrow_mut();
                            match gb.props.get_mut(&pk) {
                                Some(p) => p.val = v,
                                None => {
                                    gb.props.insert(pk, Prop::data(v));
                                }
                            }
                        }
                        Ok(())
                    }
                    None => {
                        vars.insert(
                            name,
                            Binding {
                                val: v, mutable: true, uninit: false,
                                deletable: false, lexical: false,
                            },
                        );
                        Ok(())
                    }
                }
            }
            LRef::WithObj(o, name) => {
                // SetMutableBinding on the object env record:
                // stillExists = HasProperty (traps fire), strict throws
                // ReferenceError when it went missing
                if !self.with_has_prop(&o, &name)? && self.cur_strict {
                    return Err(
                        self.ref_err(format!("{name} is not defined"))
                    );
                }
                self.set_prop(&Value::Obj(o), PropKey::S(name), v)
            }
            LRef::Unres(name) => {
                if self.cur_strict {
                    return Err(self.ref_err(format!("{name} is not defined")));
                }
                // unresolved sloppy write creates a global property
                // (configurable — `delete x` removes it)
                self.global.vars.borrow_mut().insert(
                    name.clone(),
                    Binding {
                        val: v.clone(), mutable: true, uninit: false,
                        deletable: true, lexical: false,
                    },
                );
                self.declare_global_prop(&name, v);
                Ok(())
            }
        }
    }

    /// Evaluate a pattern's member-target ref eagerly where the spec
    /// requires lref evaluation before the value read (array elements,
    /// rest, keyed object properties).
    fn prep_pat<'a>(
        &mut self,
        scope: &EnvRef,
        p: &'a Pattern,
    ) -> Result<PrepPat<'a>, Value> {
        if let Pattern::Target(e) = p {
            Ok(PrepPat::Resolved(self.eval_lref(e, scope)?))
        } else {
            Ok(PrepPat::Pat(p))
        }
    }

    fn pat_bind_prepped(
        &mut self,
        scope: &EnvRef,
        pp: PrepPat<'_>,
        v: Value,
        mode: BindMode,
    ) -> Result<(), Value> {
        match pp {
            PrepPat::Pat(p) => self.pat_bind(scope, p, v, mode),
            PrepPat::Resolved(lr) => self.put_lref(lr, v),
        }
    }

    fn pat_bind(
        &mut self,
        scope: &EnvRef,
        pat: &Pattern,
        v: Value,
        mode: BindMode,
    ) -> Result<(), Value> {
        match pat {
            Pattern::Target(e) => {
                if !matches!(mode, BindMode::Assign) {
                    return Err(self.syntax_err("invalid binding target"));
                }
                let lr = self.eval_lref(e, scope)?;
                self.put_lref(lr, v)?;
            }
            Pattern::Ident(name) => match mode {
                BindMode::Assign => self.env_assign(scope, name, v)?,
                // var initializers assign through the scope chain —
                // catch params and `with` objects intercept; the binding
                // itself was already created at the var boundary by hoisting
                BindMode::Decl(VarKind::Var) => {
                    self.env_assign(scope, name, v)?
                }
                BindMode::Decl(kind) => {
                    self.declare(scope, name, v, kind != VarKind::Const)
                }
            },
            Pattern::Array(elems) => {
                // lazy protocol pulls — IteratorClose on early completion
                // (an infinite source must not be drained) and on abrupt
                // PATTERN errors; iterator-internal errors (a throwing
                // next()) propagate directly without a close call
                let mut it = crate::builtins::for_of_iter(self, &v)?;
                let mut done = false;
                let mut it_err = false;
                let res = (|| -> Result<(), Value> {
                    for e in elems {
                        macro_rules! pull {
                            () => {{
                                if done {
                                    Value::Und
                                } else {
                                    match crate::builtins::iter_lazy_step(
                                        self, &mut it,
                                    ) {
                                        Ok(Some(x)) => x,
                                        Ok(None) => {
                                            done = true;
                                            Value::Und
                                        }
                                        Err(e) => {
                                            it_err = true;
                                            return Err(e);
                                        }
                                    }
                                }
                            }};
                        }
                        match e {
                            ArrayPatElem::Hole => {
                                let _ = pull!();
                            }
                            ArrayPatElem::Elem(p) => {
                                // target lref evaluates BEFORE the pull
                                let pp = self.prep_pat(scope, p)?;
                                let x = pull!();
                                self.pat_bind_prepped(scope, pp, x, mode)?;
                            }
                            ArrayPatElem::Default(p, d) => {
                                let pp = self.prep_pat(scope, p)?;
                                let x = pull!();
                                let x = if matches!(x, Value::Und) {
                                    let x = self.eval_expr(d, scope)?;
                                    if let Pattern::Ident(n) = p {
                                        if Self::is_anon_fn_def(d) {
                                            self.maybe_name(&x, n);
                                        }
                                    }
                                    x
                                } else {
                                    x
                                };
                                self.pat_bind_prepped(scope, pp, x, mode)?;
                            }
                            ArrayPatElem::Rest(p) => {
                                // the rest target's lref evaluates before
                                // the iterator drains
                                let pp = self.prep_pat(scope, p)?;
                                // rest drains the iterator — it ends done
                                let mut rest = Vec::new();
                                loop {
                                    match crate::builtins::iter_lazy_step(
                                        self, &mut it,
                                    ) {
                                        Ok(Some(x)) => rest.push(x),
                                        Ok(None) => {
                                            done = true;
                                            break;
                                        }
                                        Err(e) => {
                                            it_err = true;
                                            return Err(e);
                                        }
                                    }
                                }
                                let arr = Value::Obj(self.new_arr(rest));
                                self.pat_bind_prepped(scope, pp, arr, mode)?;
                            }
                        }
                    }
                    Ok(())
                })();
                if !done && !it_err {
                    // pattern finished while the iterator is still open —
                    // IteratorClose (normal completion). On abrupt the
                    // close error is swallowed per spec — EXCEPT a
                    // suspended `return()` (gen-abort): that completion
                    // is a return, not a throw, so a close error wins.
                    match (&res, crate::builtins::iter_lazy_close(self, &mut it)) {
                        (Ok(()), Err(ce)) => return Err(ce),
                        (Err(e), Err(ce)) if is_gen_abort(e) => return Err(ce),
                        _ => {}
                    }
                }
                res?;
            }
            Pattern::Object(elems) => {
                if matches!(v, Value::Und | Value::Null) {
                    return Err(self.type_err(
                        "cannot destructure null or undefined",
                    ));
                }
                for e in elems {
                    if let ast::PropKey::Str(s) = &e.key {
                        if s.as_str() == REST_PAT_KEY {
                            // the rest target's lref evaluates before
                            // CopyDataProperties runs
                            let pp_rest = self.prep_pat(scope, &e.val)?;
                            // `{...rest}` — CopyDataProperties over own
                            // enumerable keys, string AND symbol, minus
                            // the keys already consumed by the pattern.
                            // a primitive source is boxed (its own
                            // enumerable props still copy)
                            let v = if matches!(v, Value::Obj(_)) {
                                v.clone()
                            } else {
                                self.to_object(&v)?
                            };
                            let rest = self.new_obj();
                            if let Value::Obj(_src) = &v {
                                let mut taken: HashSet<PropKey> = elems
                                    .iter()
                                    .filter_map(|x| match &x.key {
                                        ast::PropKey::Str(s) | ast::PropKey::Ident(s)
                                            if s.as_str() != REST_PAT_KEY =>
                                        {
                                            Some(PropKey::from_str(s))
                                        }
                                        _ => None,
                                    })
                                    .collect();
                                // computed keys evaluate in source order —
                                // their resolved names are also taken
                                for x in elems {
                                    if let ast::PropKey::Computed(ex) = &x.key {
                                        let kv = self.eval_expr(ex, scope)?;
                                        if let Ok(pk) =
                                            self.to_propkey(&kv)
                                        {
                                            taken.insert(pk);
                                        }
                                    }
                                }
                                // CopyDataProperties: [[OwnPropertyKeys]] +
                                // gOPD-enum filter + [[Get]] per key — a
                                // proxy source runs the full trap set
                                for (k, val) in
                                    crate::builtins::own_enum_entries_full(
                                        self, &v, true,
                                    )?
                                {
                                    if taken.contains(&k) {
                                        continue;
                                    }
                                    match &k {
                                        PropKey::S(s) => {
                                            self.set_data(
                                                &rest,
                                                &s.to_string(),
                                                val,
                                            );
                                        }
                                        PropKey::Sym(id) => {
                                            let mut pb = rest.borrow_mut();
                                            pb.props.insert(
                                                PropKey::Sym(*id),
                                                crate::interp::Prop::data(val),
                                            );
                                        }
                                    }
                                }
                            }
                            self.pat_bind_prepped(scope, pp_rest, Value::Obj(rest), mode)?;
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
                    // the binding target's lref evaluates before the
                    // source property [[Get]]
                    let pp = self.prep_pat(scope, &e.val)?;
                    let got = self.get_prop(&v, &key)?;
                    let val = match (&got, &e.default) {
                        (Value::Und, Some(d)) => {
                            let val = self.eval_expr(d, scope)?;
                            if let Pattern::Ident(n) = &e.val {
                                if Self::is_anon_fn_def(d) {
                                    self.maybe_name(&val, n.as_str());
                                }
                            }
                            val
                        }
                        _ => got,
                    };
                    self.pat_bind_prepped(scope, pp, val, mode)?;
                }
            }
        }
        Ok(())
    }

    /// assignment-mode destructuring (`[a,b] = arr`, `{x,y} = obj`)
    fn assign_pattern(&mut self, scope: &EnvRef, pat: &Pattern, v: Value) -> Result<(), Value> {
        self.pat_bind(scope, pat, v, BindMode::Assign)
    }

    /// Convert an iterable to a Vec of values (for-of, spread, destructuring).
    pub fn iter_values(&mut self, v: &Value) -> Result<Vec<Value>, Value> {
        match v {
            Value::Str(s) => Ok(s
                .chars()
                .map(|c| Value::Str(Rc::from(c.to_string().as_str())))
                .collect()),
            Value::Obj(o) => {
                // lazy iterators drain through their state machine
                if matches!(o.borrow().kind, ObjKind::IterLazy(_)) {
                    return crate::builtins::iter_drain_lazy(self, v);
                }
                // Symbol.iterator protocol
                let m = self.get_prop(v, &PropKey::Sym(SYM_ITERATOR))?;
                if self.is_callable(&m) {
                    let iter = self.call(&m, v.clone(), Vec::new())?;
                    return self.drain_iter(iter);
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
            // GetV wraps primitives for the @@iterator lookup; the raw
            // primitive stays `this` when the method is invoked
            _ => {
                let m = self.get_prop(v, &PropKey::Sym(SYM_ITERATOR))?;
                if self.is_callable(&m) {
                    let iter = self.call(&m, v.clone(), Vec::new())?;
                    return self.drain_iter(iter);
                }
                Err(self.type_err("value is not iterable"))
            }
        }
    }

    /// step `iter` via its `next` method until done — shared by the
    /// @@iterator paths above (close on any abrupt protocol error)
    fn drain_iter(&mut self, iter: Value) -> Result<Vec<Value>, Value> {
        let mut out = Vec::new();
        loop {
            if out.len() > 100_000 {
                return Err(self.range_err("iteration limit exceeded"));
            }
            let next = self.get_prop(&iter, &PropKey::from_str("next"))?;
            if !self.is_callable(&next) {
                return Err(self.type_err("iterator.next is not a function"));
            }
            let r = match self.call(&next, iter.clone(), Vec::new()) {
                Ok(r) => r,
                Err(e) => {
                    let _ = self.iter_close_obj(&iter);
                    return Err(e);
                }
            };
            let done = match self.get_prop(&r, &PropKey::from_str("done")) {
                Ok(d) => d,
                Err(e) => {
                    let _ = self.iter_close_obj(&iter);
                    return Err(e);
                }
            };
            if self.to_bool(&done) {
                break;
            }
            match self.get_prop(&r, &PropKey::from_str("value")) {
                Ok(val) => out.push(val),
                Err(e) => {
                    let _ = self.iter_close_obj(&iter);
                    return Err(e);
                }
            }
        }
        Ok(out)
    }

    // -- hoisting ----------------------------------------------------------------
    pub fn hoist_body(&mut self, stmts: &[Stmt], scope: &EnvRef) -> Result<(), Value> {
        // uncached path — safe for arbitrary caller-owned slices
        let h = collect_hoisted_owned(stmts);
        self.check_global_decls(&h, scope)?;
        self.apply_hoisted(&h, scope);
        Ok(())
    }

    /// CanDeclareGlobalVar/CanDeclareGlobalFunction checks: declaring a
    /// `var`/`function` global must fail on a non-extensible global object
    /// or collide with an existing non-configurable property (fns can't
    /// replace one; `var` may only shadow it).
    fn check_global_decls(&mut self, h: &Hoisted, scope: &EnvRef) -> Result<(), Value> {
        // the var boundary these declarations will land on
        let mut cur = scope.clone();
        while !cur.var_boundary.get() && cur.parent.is_some() {
            let next = cur.parent.clone().unwrap();
            cur = next;
        }
        if !Rc::ptr_eq(&cur, &self.global) {
            return Ok(());
        }
        let g = self.p.global.clone();
        let ext = g.borrow().extensible;
        let mut names: Vec<(Rc<str>, bool)> =
            h.vars.iter().map(|n| (n.clone(), false)).collect();
        for f in &h.fns {
            if let Some(n) = f.name.as_deref() {
                names.push((self.intern(n), true));
            }
        }
        if !self.cur_strict {
            for n in &h.block_fns {
                names.push((n.clone(), true));
            }
        }
        for (name, is_fn) in names {
            let key = PropKey::S(name.clone());
            let existing = g
                .borrow()
                .props
                .get(&key)
                .map(|p| (p.configurable, p.writable, p.enumerable, p.is_accessor()));
            match existing {
                None if !ext => {
                    return Err(self.type_err(format!(
                        "Cannot define global variable `{name}`: object is not extensible"
                    )))
                }
                // CanDeclareGlobalFunction: a non-configurable existing
                // prop still allows the fn decl when it is a writable +
                // enumerable DATA property
                Some((false, w, e, acc)) if is_fn && !(w && e && !acc) => {
                    return Err(self.type_err(format!(
                        "Cannot define global function `{name}`: property is non-configurable"
                    )))
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// declare the precomputed hoisted vars/fns into `scope`'s var boundary.
    fn apply_hoisted(&mut self, h: &Hoisted, scope: &EnvRef) {
        self.apply_hoisted_mode(h, scope, false);
    }

    /// `block_scope`: fns bind lexically in `scope` (the Annex-B block
    /// binding) instead of the var boundary. Annex-B nested fn names are
    /// initialized `undefined` at the var boundary in sloppy code.
    fn apply_hoisted_mode(&mut self, h: &Hoisted, scope: &EnvRef, block_scope: bool) {
        // lexical declarations instantiate as uninitialized bindings at
        // scope entry — reads/writes before their decl throws (TDZ)
        if !h.lets.is_empty() {
            let mut vars = scope.vars.borrow_mut();
            for name in &h.lets {
                vars.entry(name.clone()).or_insert(Binding {
                    val: Value::Und,
                    mutable: true,
                    uninit: true,
                    deletable: false,
                    lexical: true,
                });
            }
        }
        for name in &h.vars {
            self.declare_var_rc(scope, name, Value::Und, false);
        }
        if !self.cur_strict {
            for name in &h.block_fns {
                self.declare_var_rc(scope, name, Value::Und, false);
            }
        }
        for f in &h.fns {
            if let Some(name) = f.name.clone() {
                let fv = self.make_fn(f.clone(), scope, CtorKind::NotClass);
                if block_scope {
                    self.declare(scope, &name, fv, true);
                } else {
                    self.declare_var_fn(scope, &name, fv);
                }
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
        let h = self.hoisted_slice(stmts);
        // a block containing function declarations always needs its own
        // scope: the block-scoped binding lives there in both modes
        let needs_scope =
            !h.fns.is_empty() || stmts.iter().any(stmt_binds_lexically);
        if needs_scope {
            let scope = Scope::new(Some(env.clone()), false);
            self.apply_hoisted_mode(&h, &scope, true);
            self.exec_block(stmts, &scope)
        } else {
            self.apply_hoisted_mode(&h, env, false);
            self.exec_block(stmts, env)
        }
    }

    /// spec SetFunctionName: `name` {writable:false, enumerable:false,
    /// configurable:true}; `prefix` is "get "/"set " for accessors
    pub fn set_fn_name(&self, f: &Value, name: &str, prefix: &str) {
        if let Value::Obj(o) = f {
            self.set_attr(
                o,
                "name",
                Value::Str(Rc::from(format!("{prefix}{name}").as_str())),
                false,
                true,
            );
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
                home: home.clone(),
                ctor,
                inst_fields: None,
                ban_eval_args: self.eval_args_ban > 0,
            }),
        )));
        let name = def.name.clone().unwrap_or_default();
        // insertion order matters for getOwnPropertyNames — spec order
        // is `length` then `name` then `prototype`
        self.set_attr(&o, "length", Value::Num(param_count(&def.params) as f64), false, true);
        self.set_attr(&o, "name", Value::Str(Rc::from(name.as_str())), false, true);
        if def.is_gen && home.is_none() {
            // generator fns: proto is %GeneratorFunction.prototype%; the
            // `.prototype` prop is an ordinary object whose proto is
            // %GeneratorPrototype% — no `constructor` back-ref.
            // Descriptor: {writable: true, enumerable: false,
            // configurable: false}
            o.borrow_mut().proto = Some(self.p.gen_func.clone());
            let proto = self.new_obj();
            proto.borrow_mut().proto = Some(self.p.generator.clone());
            self.set_attr(&o, "prototype", Value::Obj(proto), true, false);
        } else if def.is_gen {
            // generator METHOD: %GeneratorFunction.prototype% proto but
            // no `.prototype` property (methods aren't constructable)
            o.borrow_mut().proto = Some(self.p.gen_func.clone());
        } else if !def.is_arrow && !def.is_async && home.is_none() {
            // ordinary fn (or class ctor — make_class overwrites the
            // descriptor with {!writable,!configurable} afterwards).
            // async fns carry no `.prototype` (they aren't ctors).
            let proto = self.new_obj();
            self.set_builtin(&proto, "constructor", Value::Obj(o.clone()));
            // `.prototype` on ordinary fns: {writable, !enumerable,
            // !configurable}
            self.set_attr(&o, "prototype", Value::Obj(proto), true, false);
        }
        // methods (home set) get no `.prototype` — they aren't ctors
        Value::Obj(o)
    }

    // -- statements --------------------------------------------------------------
    pub fn exec_block(&mut self, stmts: &[Stmt], env: &EnvRef) -> SRes {
        let mut last: Option<Value> = None;
        for s in stmts {
            match self.exec_stmt(s, env)? {
                Completion::Normal(v) => last = Some(v),
                Completion::Empty => {}
                // UpdateEmpty: an abrupt with empty [[value]] picks up
                // the block's accumulated normal completion
                Completion::Break(l, v) => {
                    return Ok(Completion::Break(l, v.or(last)));
                }
                Completion::Continue(l, v) => {
                    return Ok(Completion::Continue(l, v.or(last)));
                }
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
        if self.steps >= self.gen_cap {
            self.gen_cap_hit = true;
            return Err(self.range_err("generator step budget exceeded"));
        }
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
                    // spec 14.3.2.4 step 2: ResolveBinding runs BEFORE the
                    // initializer — a `with (obj)` binding pins the object's
                    // property while it still exists (e.g. `var x = delete
                    // obj.x` still writes back to `obj.x`)
                    let early_lr = if let (Pattern::Ident(n), Some(_)) = (pat, init) {
                        Some(self.eval_lref_ident(env, n)?)
                    } else {
                        None
                    };
                    let v = match init {
                        Some(e) => match pat {
                            Pattern::Ident(n) if Self::is_anon_fn_def(e) => {
                                self.eval_expr_named(e, env, n)?
                            }
                            _ => self.eval_expr(e, env)?,
                        },
                        None => Value::Und,
                    };
                    match pat {
                        Pattern::Ident(name) => match kind {
                            // `var x;` (no init) leaves the hoisted binding
                            // untouched — only initializers assign
                            VarKind::Var if init.is_none() => {}
                            VarKind::Var => match early_lr.unwrap() {
                                LRef::Env(sc, n2) => self.env_assign(&sc, &n2, v.clone())?,
                                lr => self.put_lref(lr, v.clone())?,
                            },
                            _ => self.declare(env, name, v.clone(), *kind != VarKind::Const),
                        },
                        other => self.bind_pattern(env, other, v.clone(), *kind, true)?,
                    }
                }
                Ok(Completion::Empty)
            }
            Stmt::FnDecl(f) => {
                if let Some(name) = &f.name {
                    let k = self.intern(name);
                    // instantiated = an own binding holding the fn object —
                    // the Annex-B `undefined` placeholder doesn't count.
                    // Top-level decls hoist to the var boundary, block-level
                    // ones to the block scope — check both so neither is
                    // re-declared into a shadowing ad-hoc binding.
                    let mut ve = env.clone();
                    while !ve.var_boundary.get() && ve.parent.is_some() {
                        let next = ve.parent.clone().unwrap();
                        ve = next;
                    }
                    let has_own = env
                        .vars
                        .borrow()
                        .get(k.as_ref())
                        .is_some_and(|b| matches!(b.val, Value::Obj(_)))
                        || ve.vars.borrow().get(k.as_ref())
                            .is_some_and(|b| matches!(b.val, Value::Obj(_)));
                    if !has_own {
                        // single-statement position (`if (c) function f(){}`,
                        // labels): the fn binds into the ad-hoc branch scope
                        let fv = self.make_fn(f.clone(), env, CtorKind::NotClass);
                        self.declare(env, name, fv, true);
                    }
                    // Annex B.3.3: executing a block-level fn decl syncs the
                    // block-scoped value into the var-environment binding —
                    // plain functions only; generators/async fns stay purely
                    // block-scoped
                    if !self.cur_strict && !f.is_gen && !f.is_async {
                        if let Some(fv) = self.env_lookup(env, name)? {
                            self.declare_var(env, name, fv);
                        }
                    }
                }
                Ok(Completion::Empty)
            }
            Stmt::ClassDecl(c) => {
                let v = self.make_class(c, env)?;
                if let Some(n) = &c.name {
                    // the outer class binding is a mutable `let`-style
                    // binding — only the class-scope inner binding is
                    // immutable
                    self.declare(env, n, v, true);
                }
                Ok(Completion::Empty)
            }
            Stmt::Block(stmts) => self.exec_hoisted_block(stmts, env),
            Stmt::If { c, t, e } => {
                let cv = self.eval_expr(c, env)?;
                let r = if self.to_bool(&cv) {
                    self.exec_stmt(t, env)?
                } else if let Some(e) = e {
                    self.exec_stmt(e, env)?
                } else {
                    Completion::Empty
                };
                // spec 14.6.2: `UpdateEmpty(stmtCompletion, undefined)` —
                // an empty value materializes as `undefined` even on
                // abrupt completions
                Ok(match r {
                    Completion::Empty => Completion::Normal(Value::Und),
                    Completion::Break(l, v) => Completion::Break(l, v.or(Some(Value::Und))),
                    Completion::Continue(l, v) => {
                        Completion::Continue(l, v.or(Some(Value::Und)))
                    }
                    r => r,
                })
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
                        Completion::Break(l, v) => {
                            if l.is_some() {
                                return Ok(Completion::Break(l, v));
                            }
                            if let Some(v) = v {
                                last = v;
                            }
                            break;
                        }
                        Completion::Continue(l, v) => {
                            if l.is_none() || l.as_deref() == my_label.as_deref() {
                                if let Some(v) = v {
                                    last = v;
                                }
                                continue;
                            }
                            return Ok(Completion::Continue(l, v));
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
                        Completion::Break(l, v) => {
                            if l.is_some() {
                                return Ok(Completion::Break(l, v));
                            }
                            if let Some(v) = v {
                                last = v;
                            }
                            break;
                        }
                        Completion::Continue(l, v) => {
                            if !(l.is_none() || l.as_deref() == my_label.as_deref()) {
                                return Ok(Completion::Continue(l, v));
                            }
                            if let Some(v) = v {
                                last = v;
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
                // `let`/`const` in the init needs a per-iteration scope
                // (CreatePerIterationEnvironment): each iteration's env is
                // seeded from the previous one's for-head bindings so
                // closures capture per-iteration values. `var`/expr inits
                // resolve identically on `env`.
                let per_iter =
                    matches!(init, Some(ForInit::Var(kind, _)) if *kind != VarKind::Var);
                let loop_scope;
                let scope: &EnvRef = match init {
                    _ if per_iter || stmt_binds_lexically(body) => {
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
                let head_names: Vec<Rc<str>> = match init {
                    Some(ForInit::Var(kind, decls)) if *kind != VarKind::Var => {
                        let mut out = Vec::new();
                        for (p, _) in decls {
                            collect_pattern_names(p, &mut out);
                        }
                        out.iter().map(|n| self.intern(n)).collect()
                    }
                    _ => Vec::new(),
                };
                let mut last = Value::Und;
                let mut env_i = scope.clone();
                if per_iter {
                    // CreatePerIterationEnvironment: even the first
                    // iteration runs in a fresh env copied from the head
                    // env — head-env bindings stay untouched by writes in
                    // test/body/update
                    let s = Scope::new(Some(env.clone()), false);
                    {
                        let pv = scope.vars.borrow();
                        let mut cur = s.vars.borrow_mut();
                        for n in &head_names {
                            if let Some(b) = pv.get(n.as_ref()) {
                                cur.insert(
                                    n.clone(),
                                    Binding {
                                        val: b.val.clone(),
                                        mutable: b.mutable,
                                        uninit: b.uninit,
                                        deletable: false,
                                        lexical: true,
                                    },
                                );
                            }
                        }
                    }
                    env_i = s;
                }
                loop {
                    if let Some(t) = test {
                        let tv = self.eval_expr(t, &env_i)?;
                        if !self.to_bool(&tv) {
                            break;
                        }
                    }
                    match self.exec_stmt(body, &env_i)? {
                        Completion::Normal(v) => last = v,
                        Completion::Empty => {}
                        Completion::Break(l, v) => {
                            if l.is_some() {
                                return Ok(Completion::Break(l, v));
                            }
                            if let Some(v) = v {
                                last = v;
                            }
                            break;
                        }
                        Completion::Continue(l, v) => {
                            if !(l.is_none() || l.as_deref() == my_label.as_deref()) {
                                return Ok(Completion::Continue(l, v));
                            }
                            if let Some(v) = v {
                                last = v;
                            }
                        }
                        r => return Ok(r),
                    }
                    if per_iter {
                        // CreatePerIterationEnvironment runs BEFORE the
                        // update: next iteration's env copies the current
                        // head bindings, then `update` evaluates inside it
                        let s = Scope::new(Some(env.clone()), false);
                        {
                            let pv = env_i.vars.borrow();
                            let mut cur = s.vars.borrow_mut();
                            for n in &head_names {
                                if let Some(b) = pv.get(n.as_ref()) {
                                    cur.insert(
                                        n.clone(),
                                        Binding {
                                            val: b.val.clone(),
                                            mutable: b.mutable,
                                            uninit: b.uninit,
                                            deletable: false,
                                            lexical: true,
                                        },
                                    );
                                }
                            }
                        }
                        env_i = s;
                    }
                    if let Some(u) = update {
                        self.eval_expr(u, &env_i)?;
                    }
                }
                Ok(Completion::Normal(last))
            }
            Stmt::ForIn { left, right, body } => {
                let my_label = self.pending_label.take();
                // ForIn/OfHeadEvaluation: a `let`/`const` target creates a
                // declarative env holding the bound names UNINITIALIZED
                // before the head expression evaluates — reading them there
                // throws ReferenceError (TDZ)
                let tdz_scope;
                let head_env: &EnvRef = match left {
                    ForTarget::Var(kind, pat) if *kind != VarKind::Var => {
                        let s = Scope::new(Some(env.clone()), false);
                        let mut names = Vec::new();
                        collect_pattern_names(pat, &mut names);
                        {
                            let mut vars = s.vars.borrow_mut();
                            for n in names {
                                vars.entry(self.intern(n)).or_insert(Binding {
                                    val: Value::Und,
                                    mutable: true,
                                    uninit: true,
                                    deletable: false,
                                    lexical: true,
                                });
                            }
                        }
                        tdz_scope = s;
                        &tdz_scope
                    }
                    _ => env,
                };
                let rv = self.eval_expr(right, head_env)?;
                let keys = self.enum_keys(&rv)?;
                // `let`/`const` loop targets need a per-iteration scope
                // (CreatePerIterationEnvironment), seeded from the previous
                // iteration's for-head bindings; `var`/expr targets resolve
                // on `env`.
                let per_iter = matches!(left, ForTarget::Var(kind, _) if *kind != VarKind::Var);
                let loop_scope;
                let scope: &EnvRef = match left {
                    _ if per_iter || stmt_binds_lexically(body) => {
                        loop_scope = Scope::new(Some(env.clone()), false);
                        &loop_scope
                    }
                    _ => env,
                };
                let head_names: Vec<Rc<str>> = match left {
                    ForTarget::Var(_, pat) => {
                        let mut out = Vec::new();
                        collect_pattern_names(pat, &mut out);
                        out.iter().map(|n| self.intern(n)).collect()
                    }
                    _ => Vec::new(),
                };
                let mut last = Value::Und;
                let mut prev_iter: Option<EnvRef> = None;
                // existence re-check needs an object — primitives (string
                // indices) box here; null/undefined yield empty `keys`
                let rv_obj = self.to_object(&rv).unwrap_or(Value::Und);
                for k in keys {
                    let mut iter_scope: Option<EnvRef> = None;
                    let scope: &EnvRef = if per_iter {
                        let s = Scope::new(Some(env.clone()), false);
                        if let Some(pe) = &prev_iter {
                            let pv = pe.vars.borrow();
                            let mut cur = s.vars.borrow_mut();
                            for n in &head_names {
                                if let Some(b) = pv.get(n.as_ref()) {
                                    cur.insert(
                                        n.clone(),
                                        Binding {
                                            val: b.val.clone(),
                                            mutable: b.mutable,
                                            uninit: b.uninit,
                                            deletable: false,
                                            lexical: true,
                                        },
                                    );
                                }
                            }
                        }
                        iter_scope = Some(s);
                        iter_scope.as_ref().unwrap()
                    } else {
                        scope
                    };
                    // spec: keys deleted since enumeration are skipped;
                    // keys added during iteration are never visited. A
                    // proxy re-checks through [[GetOwnProperty]] (the
                    // trap keeps seeing each key exactly as for-in does)
                    let rv_is_proxy = match &rv_obj {
                        Value::Obj(ro) => {
                            matches!(&ro.borrow().kind, ObjKind::Proxy { .. })
                        }
                        _ => false,
                    };
                    let keep = if rv_is_proxy {
                        let d = crate::builtins::obj_get_own_prop_desc(
                            self,
                            &Value::Und,
                            &[
                                rv_obj.clone(),
                                Value::Str(Rc::from(k.as_str())),
                            ],
                        )?;
                        match d {
                            Value::Obj(dv) => {
                                let en = self.get_prop_str(
                                    &Value::Obj(dv),
                                    "enumerable",
                                )?;
                                self.to_bool(&en)
                            }
                            _ => false,
                        }
                    } else {
                        self.has_prop(&rv_obj, &PropKey::from_str(k.as_str()))?
                    };
                    if !keep {
                        continue;
                    }
                    self.bind_for_target(scope, left, Value::Str(Rc::from(k.as_str())))?;
                    let r = self.exec_stmt(body, scope);
                    if per_iter {
                        prev_iter = iter_scope;
                    }
                    match r? {
                        Completion::Normal(v) => last = v,
                        Completion::Empty => {}
                        Completion::Break(l, v) => {
                            if l.is_some() {
                                return Ok(Completion::Break(l, v));
                            }
                            if let Some(v) = v {
                                last = v;
                            }
                            return Ok(Completion::Normal(last));
                        }
                        Completion::Continue(l, v) => {
                            if !(l.is_none() || l.as_deref() == my_label.as_deref()) {
                                return Ok(Completion::Continue(l, v));
                            }
                            if let Some(v) = v {
                                last = v;
                            }
                        }
                        r => return Ok(r),
                    }
                }
                Ok(Completion::Normal(last))
            }
            Stmt::ForOf { left, right, body } => {
                let my_label = self.pending_label.take();
                // ForIn/OfHeadEvaluation: a `let`/`const` target creates a
                // declarative env holding the bound names UNINITIALIZED
                // before the head expression evaluates — reading them there
                // throws ReferenceError (TDZ)
                let tdz_scope;
                let head_env: &EnvRef = match left {
                    ForTarget::Var(kind, pat) if *kind != VarKind::Var => {
                        let s = Scope::new(Some(env.clone()), false);
                        let mut names = Vec::new();
                        collect_pattern_names(pat, &mut names);
                        {
                            let mut vars = s.vars.borrow_mut();
                            for n in names {
                                vars.entry(self.intern(n)).or_insert(Binding {
                                    val: Value::Und,
                                    mutable: true,
                                    uninit: true,
                                    deletable: false,
                                    lexical: true,
                                });
                            }
                        }
                        tdz_scope = s;
                        &tdz_scope
                    }
                    _ => env,
                };
                let rv = self.eval_expr(right, head_env)?;
                // spec: lazy protocol iteration — `next()` per step,
                // IteratorClose on every abrupt completion
                let mut lz = crate::builtins::for_of_iter(self, &rv)?;
                // `let`/`const` loop targets need a per-iteration scope
                // (CreatePerIterationEnvironment): each iteration gets a
                // fresh env seeded with the previous iteration's for-head
                // bindings so closures capture distinct values. `var` and
                // expression targets resolve identically on `env`.
                let per_iter = matches!(left, ForTarget::Var(kind, _) if *kind != VarKind::Var);
                let loop_scope;
                let scope: &EnvRef = match left {
                    _ if per_iter || stmt_binds_lexically(body) => {
                        loop_scope = Scope::new(Some(env.clone()), false);
                        &loop_scope
                    }
                    _ => env,
                };
                let head_names: Vec<Rc<str>> = match left {
                    ForTarget::Var(_, pat) => {
                        let mut out = Vec::new();
                        collect_pattern_names(pat, &mut out);
                        out.iter().map(|n| self.intern(n)).collect()
                    }
                    _ => Vec::new(),
                };
                let mut last = Value::Und;
                let mut prev_iter: Option<EnvRef> = None;
                loop {
                    let it = match crate::builtins::iter_lazy_step(self, &mut lz) {
                        Ok(Some(v)) => v,
                        Ok(None) => break,
                        // `? IteratorStepValue` — the iterator threw itself;
                        // NO IteratorClose runs on step/result-attr errors
                        Err(e) => return Err(e),
                    };
                    let mut iter_scope: Option<EnvRef> = None;
                    let scope: &EnvRef = if per_iter {
                        let s = Scope::new(Some(env.clone()), false);
                        if let Some(pe) = &prev_iter {
                            let pv = pe.vars.borrow();
                            let mut cur = s.vars.borrow_mut();
                            for n in &head_names {
                                if let Some(b) = pv.get(n.as_ref()) {
                                    cur.insert(
                                        n.clone(),
                                        Binding {
                                            val: b.val.clone(),
                                            mutable: b.mutable,
                                            uninit: b.uninit,
                                            deletable: false,
                                            lexical: true,
                                        },
                                    );
                                }
                            }
                        }
                        iter_scope = Some(s);
                        iter_scope.as_ref().unwrap()
                    } else {
                        scope
                    };
                    if let Err(e) = self.bind_for_target(scope, left, it) {
                        let _ = crate::builtins::iter_lazy_close(self, &mut lz);
                        return Err(e);
                    }
                    let r = self.exec_stmt(body, scope);
                    if per_iter {
                        prev_iter = iter_scope;
                    }
                    match r {
                        Ok(Completion::Normal(v)) => last = v,
                        Ok(Completion::Empty) => {}
                        Ok(Completion::Break(l, v)) => {
                            // non-throw abrupt: a close error REPLACES it
                            crate::builtins::iter_lazy_close(self, &mut lz)?;
                            if l.is_some() {
                                return Ok(Completion::Break(l, v));
                            }
                            if let Some(v) = v {
                                last = v;
                            }
                            return Ok(Completion::Normal(last));
                        }
                        Ok(Completion::Continue(l, v)) => {
                            if !(l.is_none() || l.as_deref() == my_label.as_deref()) {
                                crate::builtins::iter_lazy_close(self, &mut lz)?;
                                return Ok(Completion::Continue(l, v));
                            }
                            if let Some(v) = v {
                                last = v;
                            }
                        }
                        Ok(r) => {
                            // non-throw abrupt (return): a close error
                            // REPLACES the completion; throw completions
                            // keep the original error (Err arm below)
                            crate::builtins::iter_lazy_close(self, &mut lz)?;
                            return Ok(r);
                        }
                        Err(e) => {
                            let _ = crate::builtins::iter_lazy_close(self, &mut lz);
                            return Err(e);
                        }
                    }
                }
                Ok(Completion::Normal(last))
            }
            Stmt::Switch { disc, cases } => {
                let dv = self.eval_expr(disc, env)?;
                let scope = Scope::new(Some(env.clone()), false);
                // hoist across all case bodies (shared block scope): case
                // fns bind block-locally (B.3.5), nested-block fns get the
                // Annex-B `undefined` var binding, `var`s hoist normally
                {
                    let mut fns: Vec<&Rc<FnDef>> = Vec::new();
                    let mut block_fns: Vec<&str> = Vec::new();
                    let mut vars: Vec<&str> = Vec::new();
                    let mut lets: Vec<&str> = Vec::new();
                    for c in cases {
                        collect_hoisted(&c.body, &mut fns, &mut block_fns, &mut vars, &mut lets);
                    }
                    // case-body lexicals share the switch's one block scope
                    for n in lets {
                        let k = self.intern(n);
                        scope.vars.borrow_mut().insert(
                            k,
                            Binding {
                                val: Value::Und, mutable: true,
                                uninit: true, deletable: false, lexical: true,
                            },
                        );
                    }
                    for n in vars {
                        self.declare_var(&scope, n, Value::Und);
                    }
                    if !self.cur_strict {
                        for n in block_fns {
                            let k = self.intern(n);
                            self.declare_var_rc(&scope, &k, Value::Und, false);
                        }
                    }
                    for f in fns {
                        if let Some(n) = f.name.clone() {
                            let fv = self.make_fn(f.clone(), &scope, CtorKind::NotClass);
                            self.declare(&scope, &n, fv, true);
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
                                Completion::Break(None, v) => {
                                    if let Some(v) = v {
                                        last = v;
                                    }
                                    break 'cases;
                                }
                                // UpdateEmpty(R, V): a propagating abrupt
                                // picks up the accumulated value when empty
                                Completion::Break(l, v) => {
                                    return Ok(Completion::Break(l, v.or(Some(last))))
                                }
                                Completion::Continue(l, v) => {
                                    return Ok(Completion::Continue(l, v.or(Some(last))))
                                }
                                other => return Ok(other),
                            }
                        }
                    }
                }
                Ok(Completion::Normal(last))
            }
            Stmt::With { obj, body } => {
                let v = self.eval_expr(obj, env)?;
                let o = match self.to_object(&v)? {
                    Value::Obj(o) => o,
                    _ => unreachable!(),
                };
                let sc = Scope::with_obj(env.clone(), o);
                // UpdateEmpty(C, undefined): an empty completion
                // materializes as `undefined`
                match self.exec_stmt(body, &sc)? {
                    Completion::Empty => Ok(Completion::Normal(Value::Und)),
                    Completion::Break(l, None) => {
                        Ok(Completion::Break(l, Some(Value::Und)))
                    }
                    Completion::Continue(l, None) => {
                        Ok(Completion::Continue(l, Some(Value::Und)))
                    }
                    other => Ok(other),
                }
            }
            Stmt::Try {
                block,
                param,
                handler,
                finally,
            } => {
                let mut res = self.exec_hoisted_block(block, env);
                if let Err(exc) = res {
                    if is_gen_abort(&exc) {
                        res = Err(exc);
                    } else if let Some(h) = handler {
                        if let Some(p) = param {
                            // `catch (e)` param binds into the catch scope
                            let catch_scope = Scope::new(Some(env.clone()), false);
                            self.bind_pattern(&catch_scope, p, exc, VarKind::Let, true)?;
                            // catch-block lexicals live in a child env of
                            // the param env — param-default closures must
                            // NOT see the body's `let`s
                            let body_scope = Scope::new(Some(catch_scope.clone()), false);
                            self.hoist_stmts(h, &body_scope)?;
                            res = self.exec_block(h, &body_scope);
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
                        // UpdateEmpty(F, undefined): an abrupt finally keeps
                        // its abruptness; an empty value becomes undefined
                        Ok(Completion::Break(l, v)) => {
                            return Ok(Completion::Break(l, v.or(Some(Value::Und))))
                        }
                        Ok(Completion::Continue(l, v)) => {
                            return Ok(Completion::Continue(l, v.or(Some(Value::Und))))
                        }
                        Ok(abrupt) => return Ok(abrupt),
                    }
                }
                // UpdateEmpty(C, undefined): the try/catch result's empty
                // completion materializes as `undefined`
                Ok(match res? {
                    Completion::Empty => Completion::Normal(Value::Und),
                    Completion::Break(l, v) => {
                        Completion::Break(l, v.or(Some(Value::Und)))
                    }
                    Completion::Continue(l, v) => {
                        Completion::Continue(l, v.or(Some(Value::Und)))
                    }
                    r => r,
                })
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
                    Ok(Completion::Break(Some(n), v)) if n == *label => {
                        Ok(Completion::Normal(v.unwrap_or(Value::Und)))
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
            Stmt::Break(l) => Ok(Completion::Break(l.clone(), None)),
            Stmt::Continue(l) => Ok(Completion::Continue(l.clone(), None)),
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
                                mutable: *kind != VarKind::Const, uninit: false, deletable: false, lexical: true,
                            },
                        );
                        Ok(())
                    }
                },
                other => {
                    // destructuring target — the full pat_bind path gives
                    // lazy protocol pulls, IteratorClose and default-init
                    // name inference; the per-iteration scope makes each
                    // `declare` a fresh binding
                    self.pat_bind(scope, other, v, BindMode::Decl(*kind))
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
            Pattern::Target(e) => {
                let lr = self.eval_lref(e, scope)?;
                self.put_lref(lr, v)?;
                return Ok(());
            }
            Pattern::Ident(n) => {
                if kind == VarKind::Var {
                    return self.for_var_assign(scope, n, v);
                }
                let k = self.intern(n);
                scope.vars.borrow_mut().insert(
                    k,
                    Binding {
                        val: v,
                        mutable: kind != VarKind::Const, uninit: false, deletable: false,
                        lexical: kind != VarKind::Var,
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
                if matches!(v, Value::Und | Value::Null) {
                    return Err(self.type_err(
                        "cannot destructure null or undefined",
                    ));
                }
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
        self.make_class_named(c, env, None)
    }

    /// class eval — `hint` is the NamedEvaluation name (`var C = class
    /// {}` names the ctor `C` AND the inner class-name binding `C`,
    /// set before static elements run)
    fn make_class_named(
        &mut self,
        c: &ClassDef,
        env: &EnvRef,
        hint: Option<&str>,
    ) -> ERes {
        // the class scope exists BEFORE the heritage expression runs —
        // the class-name binding is created uninitialized so heritage /
        // computed member keys referencing the name hit the TDZ
        let class_env = Scope::new(Some(env.clone()), false);
        let eff_name: Option<String> =
            c.name.clone().or_else(|| hint.map(|h| h.to_string()));
        let eff_name = eff_name.as_deref();
        if let Some(n) = eff_name {
            class_env.vars.borrow_mut().insert(
                Rc::from(n),
                Binding {
                    val: Value::Und,
                    mutable: false, uninit: true, deletable: false, lexical: true,
                },
            );
        }
        // PrivateEnv: each declared `#name` gets a fresh unique id per
        // class EVALUATION — the class-env binding `#x` → Sym(id) is the
        // runtime PrivateName; member slots are Sym-keyed so no string
        // property can ever reach them and brands stay per-class
        let mut priv_names: FxMap<Rc<str>, u64> = FxMap::default();
        for m in &c.members {
            if let ast::PropKey::Ident(s) = &m.key {
                if s.starts_with('#') && !priv_names.contains_key(s.as_str()) {
                    let id = PRIV_BASE + self.priv_ctr;
                    self.priv_ctr += 1;
                    priv_names.insert(Rc::from(s.as_str()), id);
                }
            }
        }
        for (n, id) in &priv_names {
            class_env.vars.borrow_mut().insert(
                n.clone(),
                Binding {
                    val: Value::Sym(*id, n.clone()),
                    mutable: false, uninit: false, deletable: false, lexical: true,
                },
            );
        }
        let superclass = match &c.superclass {
            Some(e) => Some(self.eval_expr(e, &class_env)?),
            None => None,
        };
        let derived = superclass.is_some();
        // super_proto = the new prototype's [[Prototype]] — `extends null`
        // and a ctor whose `.prototype` is null both yield a null proto
        let (super_ctor, super_proto): (Option<ObjRef>, Option<ObjRef>) =
            match &superclass {
                None => (None, Some(self.p.obj.clone())),
                Some(Value::Null) => (None, None),
                Some(Value::Obj(sc)) => {
                    if !self.is_constructor(&Value::Obj(sc.clone())) {
                        return Err(self.type_err(
                            "Class extends value is not a constructor",
                        ));
                    }
                    let sp = self.get_prop(
                        &Value::Obj(sc.clone()),
                        &PropKey::from_str("prototype"),
                    )?;
                    match sp {
                        Value::Obj(p) => (Some(sc.clone()), Some(p)),
                        Value::Null => (Some(sc.clone()), None),
                        _ => {
                            return Err(self.type_err(
                                "Class extends value does not have a valid prototype",
                            ))
                        }
                    }
                }
                Some(_) => {
                    return Err(self.type_err(
                        "Class extends value is not a constructor",
                    ))
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
                    // `\u{E000}` can't appear in a source identifier —
                    // unforgeable marker so eval_args forwards the rest
                    // array's items WITHOUT observable @@iterator (the
                    // default ctor does not use argument evaluation)
                    args: vec![Arg::Spread(Expr::Ident("\u{e000}args".into()))],
                })]
            } else {
                Vec::new()
            };
            Rc::new(FnDef {
                name: c.name.clone(),
                params: if derived {
                    vec![Param::Rest(Box::new(Pattern::Ident(
                        "\u{e000}args".into(),
                    )))]
                } else {
                    Vec::new()
                },
                body,
                is_arrow: false,
                arrow_expr: None,
                is_async: false,
                is_gen: false,
                strict: true, // default class ctor — class bodies are always strict
            })
        });

        // prototype first — the ctor's `home` points at it so `super.prop`
        // inside the constructor resolves against the parent prototype.
        let proto = Rc::new(RefCell::new(Obj::new(super_proto, ObjKind::Ord)));
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
        // class `prototype` property: {!writable,!enumerable,!configurable}
        self.set_attr(&ctor_obj, "prototype", Value::Obj(proto.clone()), false, false);
        // the ctor function's name is the class name — including the
        // NamedEvaluation-inferred name for anonymous classes
        let cname = eff_name.unwrap_or_default().to_string();
        self.set_attr(&ctor_obj, "name", Value::Str(Rc::from(cname.as_str())), false, true);

        // static elements execute in member order at class-definition
        // time; instance fields record (key, init, env) for construct/super
        enum StaticItem {
            Field(PropKey, Option<ast::Expr>, bool, Rc<str>),
            Block(Rc<FnDef>),
        }
        let mut statics: Vec<StaticItem> = Vec::new();
        let mut inst_fields: Vec<InstElem> = Vec::new();
        for m in &c.members {
            if m.kind == MemberKind::Ctor {
                continue;
            }
            let target = if m.is_static {
                ctor_obj.clone()
            } else {
                proto.clone()
            };
            // declared `#x` keys resolve to the class's fresh Sym id —
            // `is_priv` is AST-key-based so computed `[Symbol()]` members
            // still install as ordinary symbol-keyed props
            let is_priv =
                matches!(&m.key, ast::PropKey::Ident(s) if s.starts_with('#'));
            let (key, key_val) = match &m.key {
                ast::PropKey::Ident(s) if s.starts_with('#') => {
                    (PropKey::Sym(priv_names[s.as_str()]), None)
                }
                ast::PropKey::Ident(s) | ast::PropKey::Str(s) => {
                    (PropKey::from_str(s), None)
                }
                ast::PropKey::Num(n) => {
                    (PropKey::from_str(&js_num_str(*n)), None)
                }
                ast::PropKey::Computed(e) => {
                    let kv = self.eval_expr(e, &class_env)?;
                    (self.to_propkey(&kv)?, Some(kv))
                }
            };
            // a computed static key resolving to `prototype` is a
            // runtime TypeError (the early error only covers literals)
            if m.is_static && matches!(&key, PropKey::S(s) if &**s == "prototype") {
                return Err(self.type_err(
                    "Classes may not have a static property named 'prototype'"
                ));
            }
            // NamedEvaluation source name — `#x` for private names, the
            // literal/computed key text otherwise
            let fname: Rc<str> = match &m.key {
                ast::PropKey::Ident(s) | ast::PropKey::Str(s) => Rc::from(s.as_str()),
                ast::PropKey::Num(n) => Rc::from(js_num_str(*n).as_str()),
                ast::PropKey::Computed(_) => match &key_val {
                    // SetFunctionName: symbol key → `[desc]`; a symbol
                    // with no description yields the empty name
                    Some(Value::Sym(_, d)) if !d.is_empty() => {
                        Rc::from(format!("[{d}]"))
                    }
                    Some(Value::Sym(_, _)) => Rc::from(""),
                    _ => Rc::from(self.key_to_name(&key).as_str()),
                },
            };
            match m.kind {
                MemberKind::Method => {
                    if is_priv && !m.is_static {
                        // private instance method — evaluate once now;
                        // every instance's private slot holds the same fn
                        let f = self.make_fn_home(
                            m.func.clone(),
                            &class_env,
                            Some(proto.clone()),
                            CtorKind::NotClass,
                        );
                        self.set_fn_name(&f, &fname, "");
                        inst_fields.push(InstElem::Method(key, f));
                        continue;
                    }
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
                    self.set_fn_name(&f, &fname, "");
                    if is_priv {
                        // private static method — non-writable slot
                        target.borrow_mut().props.insert(
                            key,
                            Prop {
                                val: f,
                                writable: false,
                                enumerable: false,
                                configurable: false,
                                get: None,
                                set: None,
                            },
                        );
                    } else {
                        target.borrow_mut().props.insert(key, Prop::builtin(f));
                    }
                }
                MemberKind::Getter | MemberKind::Setter => {
                    if is_priv && !m.is_static {
                        // private instance accessor — get/set share the
                        // private name; fns evaluated once at class eval
                        let f = self.make_fn_home(
                            m.func.clone(),
                            &class_env,
                            Some(proto.clone()),
                            CtorKind::NotClass,
                        );
                        self.set_fn_name(
                            &f,
                            &fname,
                            if m.kind == MemberKind::Getter { "get " } else { "set " },
                        );
                        let ent = inst_fields.iter_mut().find(|e| {
                            matches!(e, InstElem::Access(k2, _, _) if *k2 == key)
                        });
                        if let Some(InstElem::Access(_, g, s)) = ent {
                            if m.kind == MemberKind::Getter {
                                *g = Some(f);
                            } else {
                                *s = Some(f);
                            }
                        } else {
                            let (g, s) = if m.kind == MemberKind::Getter {
                                (Some(f), None)
                            } else {
                                (None, Some(f))
                            };
                            inst_fields.push(InstElem::Access(key, g, s));
                        }
                        continue;
                    }
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
                    self.set_fn_name(
                        &f,
                        &fname,
                        if m.kind == MemberKind::Getter { "get " } else { "set " },
                    );
                    let mut b = target.borrow_mut();
                    let ent = b.props.or_insert_with(key, || Prop::accessor(None, None));
                    if is_priv {
                        // private slots aren't configurable/deletable
                        ent.configurable = false;
                    }
                    if m.kind == MemberKind::Getter {
                        ent.get = Some(f);
                    } else {
                        ent.set = Some(f);
                    }
                }
                MemberKind::Field => {
                    if m.is_static {
                        statics.push(StaticItem::Field(key, m.init.clone(), is_priv, fname));
                    } else {
                        inst_fields.push(InstElem::Field(key, m.init.clone(), is_priv, fname));
                    }
                }
                MemberKind::StaticBlock => {
                    statics.push(StaticItem::Block(m.func.clone()));
                }
                MemberKind::Ctor => unreachable!(),
            }
        }

        // the class-name binding initializes between member definition
        // and static-element evaluation (spec step order)
        if let Some(n) = eff_name {
            if let Some(b) = class_env.vars.borrow_mut().get_mut(n) {
                b.val = ctor_fn.clone();
                b.uninit = false;
            }
        }

        // static elements: this = ctor, home = ctor — run in member order
        for item in statics {
            match item {
                StaticItem::Field(key, init, is_priv, fname) => {
                    let v = match init {
                        Some(e) => {
                            let fs = Scope::new(Some(class_env.clone()), true);
                            *fs.thisv.borrow_mut() = ctor_fn.clone();
                            *fs.home.borrow_mut() = Some(ctor_obj.clone());
                            // static-field-initializer context — direct
                            // eval can't use `arguments`; anonymous fn
                            // initializers take the field name
                            self.eval_args_ban += 1;
                            let r = if Self::is_anon_fn_def(&e) {
                                self.eval_expr_named(&e, &fs, &fname)
                            } else {
                                self.eval_expr(&e, &fs)
                            };
                            self.eval_args_ban -= 1;
                            r?
                        }
                        None => Value::Und,
                    };
                    self.define_field_prop(&ctor_obj, key, v, is_priv)?;
                }
                StaticItem::Block(def) => {
                    let f = self.make_fn_home(
                        def,
                        &class_env,
                        Some(ctor_obj.clone()),
                        CtorKind::NotClass,
                    );
                    self.call(&f, ctor_fn.clone(), vec![])?;
                }
            }
        }
        if !inst_fields.is_empty() {
            if let ObjKind::Func(Func::User {
                inst_fields: slot, ..
            }) = &mut ctor_obj.borrow_mut().kind
            {
                *slot = Some(Rc::new(ClassFields {
                    elems: inst_fields,
                    env: class_env.clone(),
                    home: proto.clone(),
                }));
            }
        }
        Ok(ctor_fn)
    }

    /// 10.2.1.2 InitializeInstanceElements — install private elements
    /// and run field initializers on `this` in declaration order (base
    /// ctor: during BindThis; derived: right after super() binds `this`).
    /// `nt` is the ctor's [[NewTarget]] — unused: `new.target` inside an
    /// initializer is undefined (internal [[Call]], no NewTarget).
    fn init_class_fields(&mut self, this: &Value, ctor: &ObjRef, _nt: &Value) -> Result<(), Value> {
        let cf = match &ctor.borrow().kind {
            ObjKind::Func(Func::User {
                inst_fields: Some(cf),
                ..
            }) => cf.clone(),
            _ => return Ok(()),
        };
        let Value::Obj(to) = this else {
            return Ok(());
        };
        // pass 1: install private methods/accessors before ANY
        // initializer runs — `a = this.#m()` finds the method even when
        // #m is declared after the field. Private FIELDS install in
        // declaration order (reading one early is a TypeError).
        for el in cf.elems.iter() {
            match el {
                InstElem::Method(key, f) => {
                    // the shared fn object — private methods are
                    // non-writable slots (TypeError on assign); a second
                    // install on the same object is a TypeError
                    self.priv_double_init_guard(to, key)?;
                    to.borrow_mut().props.insert(
                        key.clone(),
                        Prop {
                            val: f.clone(),
                            writable: false,
                            enumerable: false,
                            configurable: false,
                            get: None,
                            set: None,
                        },
                    );
                }
                InstElem::Access(key, get, set) => {
                    let g = get.clone();
                    let s = set.clone();
                    self.priv_double_init_guard(to, key)?;
                    to.borrow_mut().props.insert(
                        key.clone(),
                        Prop {
                            val: Value::Und,
                            writable: false,
                            enumerable: false,
                            configurable: false,
                            get: g,
                            set: s,
                        },
                    );
                }
                InstElem::Field(..) => {}
            }
        }
        // pass 2: field initializers run in declaration order — private
        // fields write into their pre-installed slot, public fields
        // define a fresh own property
        for el in cf.elems.iter() {
            match el {
                InstElem::Field(key, init, is_priv, fname) => {
                    let v = match init {
                        Some(e) => {
                            let fs = Scope::new(Some(cf.env.clone()), true);
                            *fs.thisv.borrow_mut() = this.clone();
                            // field inits are invoked by an internal
                            // [[Call]] — `new.target` is undefined inside
                            *fs.new_target.borrow_mut() = Value::Und;
                            *fs.home.borrow_mut() = Some(cf.home.clone());
                            self.eval_args_ban += 1;
                            // `x = () => {}` names the fn `x` — and
                            // `#x = () => {}` names it `#x`
                            let r = if Self::is_anon_fn_def(e) {
                                self.eval_expr_named(e, &fs, fname)
                            } else {
                                self.eval_expr(e, &fs)
                            };
                            self.eval_args_ban -= 1;
                            r?
                        }
                        None => Value::Und,
                    };
                    if *is_priv {
                        self.define_field_prop(to, key.clone(), v, true)?;
                    } else {
                        self.define_field_prop(to, key.clone(), v, false)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    // -- expressions ---------------------------------------------------------------
    pub fn eval_expr(&mut self, e: &Expr, env: &EnvRef) -> ERes {
        match e {
            Expr::Paren(inner) => return self.eval_expr(inner, env),
            Expr::Num(n) => Ok(Value::Num(*n)),
            Expr::BigInt(n) => Ok(self.bigint_obj_lit(n)),
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
                match self.env_lookup_st(env, name)? {
                    Some((_, true)) => Err(self.ref_err(format!(
                        "cannot access '{name}' before initialization"
                    ))),
                    Some((v, false)) => Ok(v),
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
                if fs.this_uninit.get() {
                    return Err(self.ref_err(
                        "Must call super constructor in derived class before accessing 'this'",
                    ));
                }
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
                let mut hole_idx: Vec<usize> = Vec::new();
                for el in elems {
                    match el {
                        ArrElem::Expr(ex) => items.push(self.eval_expr(ex, env)?),
                        ArrElem::Hole => {
                            hole_idx.push(items.len());
                            items.push(Value::Und);
                        }
                        ArrElem::Spread(ex) | ArrElem::SpreadTrail(ex) => {
                            let v = self.eval_expr(ex, env)?;
                            items.extend(self.iter_values(&v)?);
                        }
                    }
                }
                let arr = self.new_arr(items);
                if !hole_idx.is_empty() {
                    let mut h = arr.borrow_mut();
                    for k in hole_idx {
                        h.holes.get_or_insert_with(Default::default).insert(k);
                    }
                }
                Ok(Value::Obj(arr))
            }
            Expr::ObjDup(_) => {
                Err(self.syntax_err("duplicate __proto__ property"))
            }
            Expr::Object(props) => {
                let o = self.new_obj();
                for p in props {
                    match &p.val {
                        PropVal::Spread(ex) | PropVal::SpreadTrail(ex) => {
                            let sv = self.eval_expr(ex, env)?;
                            if let Value::Obj(_src) = &sv {
                                // CopyDataProperties: [[OwnPropertyKeys]] +
                                // gOPD-enum filter + [[Get]] per key — a
                                // proxy source runs the full trap set
                                for (k, v) in
                                    crate::builtins::own_enum_entries_full(
                                        self, &sv, true,
                                    )?
                                {
                                    match &k {
                                        PropKey::S(s) => {
                                            self.set_data(
                                                &o,
                                                &s.to_string(),
                                                v,
                                            );
                                        }
                                        PropKey::Sym(id) => {
                                            o.borrow_mut().props.insert(
                                                PropKey::Sym(*id),
                                                Prop::data(v),
                                            );
                                        }
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
                                    // `{__proto__: v}` sets the prototype
                                    // instead of creating a property
                                    if matches!(&p.key, ast::PropKey::Ident(s) | ast::PropKey::Str(s) if s == "__proto__")
                                    {
                                        if let Value::Obj(pv) = &v {
                                            o.borrow_mut().proto = Some(pv.clone());
                                        } else if matches!(v, Value::Null) {
                                            o.borrow_mut().proto = None;
                                        }
                                    } else {
                                        o.borrow_mut().props.insert(key, Prop::data(v));
                                    }
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
                                    // literal accessors are enumerable
                                    ent.enumerable = true;
                                    if matches!(&p.val, PropVal::Get(_)) {
                                        ent.get = Some(f);
                                    } else {
                                        ent.set = Some(f);
                                    }
                                }
                                PropVal::Spread(_) | PropVal::SpreadTrail(_) => unreachable!(),
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
                                mutable: false, uninit: false, deletable: false, lexical: false,
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
                                mutable: false, uninit: false, deletable: false, lexical: false,
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
                // resolve the reference once — GetValue/PutValue use the
                // same lref even if the scope chain mutates mid-operation
                let mut lr = match Self::unparen(target) {
                    Expr::Member { .. } => Some(self.eval_lref(target, env)?),
                    Expr::Ident(n) => Some(self.eval_lref_ident(env, n)?),
                    _ => None,
                };
                let cur = match &mut lr {
                    Some(lr) => self.get_lref(lr)?,
                    None => self.eval_expr(target, env)?,
                };
                let delta = if *inc { 1.0 } else { -1.0 };
                // `++x`/`--x` on a BigInt stays a BigInt (x ± 1n)
                let big_n = crate::binary::bigint_bits(&cur);
                if let Some((neg, mag)) = big_n {
                    let (n2, m2) = crate::binary::bigint_inc((neg, &mag), *inc);
                    let nv = self.bigint_obj_b(n2, &m2);
                    match lr {
                        Some(lr) => self.put_lref(lr, nv.clone())?,
                        None => {
                            self.assign_target(env, target, nv.clone())?;
                        }
                    }
                    return Ok(if *prefix { nv } else { cur });
                }
                let n = self.to_num(&cur)?;
                let new = n + delta;
                match lr {
                    Some(lr) => self.put_lref(lr, Value::Num(new))?,
                    None => {
                        self.assign_target(env, target, Value::Num(new))?;
                    }
                }
                Ok(if *prefix {
                    Value::Num(new)
                } else {
                    Value::Num(n)
                })
            }
            Expr::Binary { op, l, r } => {
                // `#x in obj` — private-brand check: resolve the
                // PrivateName in the class env → own Sym-keyed slot
                if matches!(op, BinOp::In) {
                    if let Some(n) = parser::priv_name_expr(l) {
                        let Some((Value::Sym(id, _), _)) =
                            self.env_lookup_st(env, n)?
                        else {
                            return Err(self.syntax_err(format!(
                                "Private member {n} must be declared"
                            )));
                        };
                        let rv = self.eval_expr(r, env)?;
                        let Value::Obj(o) = &rv else {
                            return Err(self.type_err(
                                "Cannot use 'in' operator on non-object",
                            ));
                        };
                        let own = o.borrow().props.get(&PropKey::Sym(id)).is_some();
                        return Ok(Value::Bool(own));
                    }
                }
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
                computed,
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
                    // `o.#x` — resolve the PrivateName through the class
                    // env → OWN Sym-keyed slot: absent → TypeError, never
                    // a proto walk. `o["#x"]` is an ordinary string key
                    if !*computed && s.starts_with('#') {
                        let Some((Value::Sym(id, _), _)) =
                            self.env_lookup_st(env, s.as_str())?
                        else {
                            return Err(self.syntax_err(format!(
                                "Private member {s} must be declared"
                            )));
                        };
                        if let Value::Obj(o) = &ov {
                            if let Some(p) = o.borrow().props.get(&PropKey::Sym(id))
                            {
                                let (v, g, acc) =
                                    (p.val.clone(), p.get.clone(), p.is_accessor());
                                if let Some(gf) = g {
                                    return self.call(&gf, ov.clone(), vec![]);
                                }
                                if !acc {
                                    return Ok(v);
                                }
                            }
                        }
                        return Err(self.type_err(format!(
                            "Cannot read private member {s}"
                        )));
                    }
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
        if fscope.this_uninit.get() {
            return Err(self.ref_err(
                "Must call super constructor in derived class before accessing 'this'",
            ));
        }
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
                    // default derived ctor's `super(...\u{e000}args)`:
                    // forward the rest array's items directly — no
                    // observable @@iterator (ClassDefinitionEvaluation:
                    // the default ctor does not use argument evaluation)
                    if matches!(e, Expr::Ident(n) if n.starts_with('\u{e000}')) {
                        if let Value::Obj(o) = &v {
                            let b = o.borrow();
                            if let ObjKind::Arr(items) = &b.kind {
                                out.extend(items.iter().cloned());
                                continue;
                            }
                        }
                    }
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
        // `yield`/`yield*` intrinsics (parser emits calls to `$yield*`)
        if let Expr::Ident(n) = callee {
            if n == "$yield" {
                let v = match args.first() {
                    Some(Arg::Expr(e)) => self.eval_expr(e, env)?,
                    _ => Value::Und,
                };
                let Some(&y) = self.gen_yielders.last() else {
                    return Err(self.syntax_err("yield outside generator"));
                };
                // pop MY yielder while parked — the resumer's yielder
                // becomes current for its own $yield lookups — then
                // re-push on resume so nested coroutines see me again
                self.gen_yielders.pop();
                let input = unsafe { (*y).suspend(v) };
                self.gen_yielders.push(y);
                return match input {
                    GenIn::Next(sent) => Ok(sent),
                    GenIn::Throw(e) => Err(e),
                    GenIn::Return(_) => Err(gen_abort_v()),
                };
            }
            if n == "$yieldstar" {
                let v = match args.first() {
                    Some(Arg::Expr(e)) => self.eval_expr(e, env)?,
                    _ => Value::Und,
                };
                let Some(&y) = self.gen_yielders.last() else {
                    return Err(self.syntax_err("yield outside generator"));
                };
                let mut inner = crate::builtins::for_of_iter(self, &v)?;
                // 27.5.3.3 yield* delegation: for a protocol iterator we
                // drive its next/throw/return record directly so inner
                // results pass through verbatim and sent values round-trip.
                enum Deleg {
                    Next(Value),
                    Throw(Value),
                    Ret(Value),
                }
                if let IterSrc::Proto { iter, next, .. } = inner.src.clone() {
                    let mut act = Deleg::Next(Value::Und);
                    loop {
                        let ir = match &act {
                            Deleg::Next(s) => self.call(
                                &next,
                                iter.clone(),
                                vec![s.clone()],
                            )?,
                            Deleg::Throw(e) => {
                                let tm = self.get_prop_str(&iter, "throw")?;
                                if self.is_callable(&tm) {
                                    self.call(
                                        &tm,
                                        iter.clone(),
                                        vec![e.clone()],
                                    )?
                                } else if matches!(tm, Value::Und | Value::Null)
                                {
                                    // no throw method: IteratorClose runs
                                    // with a NORMAL completion — a throwing
                                    // return getter replaces the error;
                                    // then a fresh TypeError signals the
                                    // yield* protocol violation
                                    crate::builtins::iter_lazy_close(
                                        self, &mut inner,
                                    )?;
                                    return Err(self.type_err(
                                        "yield*: iterator has no throw method",
                                    ));
                                } else {
                                    return Err(self.type_err(
                                        "iterator throw is not callable",
                                    ));
                                }
                            }
                            Deleg::Ret(rv) => {
                                let rm = self.get_prop_str(&iter, "return")?;
                                if self.is_callable(&rm) {
                                    self.call(
                                        &rm,
                                        iter.clone(),
                                        vec![rv.clone()],
                                    )?
                                } else if matches!(rm, Value::Und | Value::Null)
                                {
                                    self.gen_ret_override = Some(rv.clone());
                                    return Err(gen_abort_v());
                                } else {
                                    return Err(self.type_err(
                                        "iterator return is not callable",
                                    ));
                                }
                            }
                        };
                        if !matches!(ir, Value::Obj(_)) {
                            return Err(self.type_err(
                                "iterator result is not an object",
                            ));
                        }
                        let done_v = self.get_prop_str(&ir, "done")?;
                        if self.to_bool(&done_v) {
                            let rv = self.get_prop_str(&ir, "value")?;
                            if matches!(act, Deleg::Ret(_)) {
                                self.gen_ret_override = Some(rv);
                                return Err(gen_abort_v());
                            }
                            return Ok(rv);
                        }
                        self.yieldstar_raw = true;
                        self.gen_yielders.pop();
                        let input = unsafe { (*y).suspend(ir) };
                        self.gen_yielders.push(y);
                        act = match input {
                            GenIn::Next(s) => Deleg::Next(s),
                            GenIn::Throw(e) => Deleg::Throw(e),
                            GenIn::Return(v) => Deleg::Ret(v),
                        };
                    }
                }
                // non-protocol sources (arrays/strings/lists): no
                // throw/return methods exist to delegate to, so synthesize
                // result objects from pulled values.
                loop {
                    match crate::builtins::iter_lazy_step(self, &mut inner) {
                        Ok(Some(x)) => {
                            let ir = self.new_obj();
                            self.set_data(&ir, "value", x);
                            self.set_data(&ir, "done", Value::Bool(false));
                            let irv = Value::Obj(ir);
                            self.yieldstar_raw = true;
                            self.gen_yielders.pop();
                            let input = unsafe { (*y).suspend(irv) };
                            self.gen_yielders.push(y);
                            match input {
                                GenIn::Next(_) => {}
                                GenIn::Throw(e) => {
                                    let _ = crate::builtins::iter_lazy_close(
                                        self, &mut inner,
                                    );
                                    return Err(e);
                                }
                                GenIn::Return(v) => {
                                    let _ = crate::builtins::iter_lazy_close(
                                        self, &mut inner,
                                    );
                                    self.gen_ret_override = Some(v);
                                    return Err(gen_abort_v());
                                }
                            }
                        }
                        Ok(None) => break,
                        Err(e) => return Err(e),
                    }
                }
                return Ok(Value::Und);
            }
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
            // spec order: args evaluate and the super constructor RUNS
            // before BindThisValue — a second super() calls the base
            // ctor again, then the already-bound `this` is a
            // ReferenceError
            let argv = self.eval_args(args, env)?;
            let nt = fscope.new_target.borrow().clone();
            let this = self.construct(&Value::Obj(parent), argv, Some(nt.clone()))?;
            if !fscope.this_uninit.get() {
                return Err(self.ref_err(
                    "Super constructor may only be called once",
                ));
            }
            *fscope.thisv.borrow_mut() = this.clone();
            fscope.this_uninit.set(false);
            // derived class fields init right after super() binds `this`
            self.init_class_fields(&this, ctor_obj, &nt)?;
            return Ok(this);
        }
        match callee {
            Expr::Member {
                obj,
                prop,
                computed,
                optional,
            } => {
                if matches!(&**obj, Expr::Super) {
                    let f = self.eval_super_member(prop, env)?;
                    let argv = self.eval_args(args, env)?;
                    let fscope = self.find_fn_scope(env);
                    if fscope.this_uninit.get() {
                        return Err(self.ref_err(
                            "Must call super constructor in derived class before accessing 'this'",
                        ));
                    }
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
                    // `o.#m(...)` — private call: resolve the
                    // PrivateName in the class env → own Sym-keyed slot
                    // or TypeError, never a proto walk; a getter's result
                    // becomes the callee. `o["#m"](...)` is a plain
                    // string-key call — ordinary lookup below
                    if !*computed && s.starts_with('#') {
                        let Some((Value::Sym(id, _), _)) =
                            self.env_lookup_st(env, s.as_str())?
                        else {
                            return Err(self.syntax_err(format!(
                                "Private member {s} must be declared"
                            )));
                        };
                        let slot = if let Value::Obj(o) = &ov {
                            o.borrow()
                                .props
                                .get(&PropKey::Sym(id))
                                .map(|p| (p.val.clone(), p.get.clone(), p.is_accessor()))
                        } else {
                            None
                        };
                        let Some((pv, pg, acc)) = slot else {
                            return Err(self.type_err(format!(
                                "Cannot read private member {s}"
                            )));
                        };
                        let f = if let Some(g) = pg {
                            self.call(&g, ov.clone(), vec![])?
                        } else if acc {
                            return Err(self.type_err(format!(
                                "Cannot read private member {s}"
                            )));
                        } else {
                            pv
                        };
                        let argv = self.eval_args(args, env)?;
                        if !self.is_callable(&f) {
                            return Err(self.type_err(format!(
                                "{s} is not a function"
                            )));
                        }
                        return self.call(&f, ov, argv);
                    }
                    let f = self.get_prop_str(&ov, s)?;
                    // spec: ArgumentListEvaluation runs before the
                    // IsCallable check — arg side effects still fire when
                    // the callee turns out not to be a function
                    let argv = self.eval_args(args, env)?;
                    if !self.is_callable(&f) {
                        let iv = self.inspect(&f, 0);
                        return Err(
                            self.type_err(format!("{s} is not a function (got {iv})"))
                        );
                    }
                    return self.call(&f, ov, argv);
                }
                let pv = self.eval_expr(prop, env)?;
                let key = self.to_propkey(&pv)?;
                let f = self.get_prop(&ov, &key)?;
                let argv = self.eval_args(args, env)?;
                if !self.is_callable(&f) {
                    let ks = self.key_str(&key);
                    let iv = self.inspect(&f, 0);
                    return Err(self.type_err(format!("{ks} is not a function (got {iv})")));
                }
                self.call(&f, ov, argv)
            }
            _ => {
                // direct eval: the callee resolved to THE installed eval fn —
                // run the source in the caller's own environment
                if let Expr::Ident(n) = callee {
                    if n == "eval" {
                        let f = self.eval_expr(callee, env)?;
                        if self.is_eval_fn(&f) {
                            let argv = self.eval_args(args, env)?;
                            let Some(first) = argv.into_iter().next() else {
                                return Ok(Value::Und);
                            };
                            let Value::Str(src) = &first else {
                                return Ok(first);
                            };
                            let src = src.clone();
                            return self.eval_source(&src, env.clone());
                        }
                    }
                }
                // idents keep their reference base — `with(obj){ method() }`
                // calls with this = the with-object (env record's
                // WithBaseObject); parens preserve the reference
                let (f, this_v) = match Self::unparen(callee) {
                    Expr::Ident(n) => {
                        let mut lr = self.eval_lref_ident(env, n)?;
                        let tv = match &lr {
                            // real `with` scopes → WithBaseObject; the
                            // global-object fallback keeps undefined
                            // (sloppy substitution lands there anyway,
                            // and passing global breaks ctor-vs-call
                            // natives that sniff `this`)
                            LRef::WithObj(o, _)
                                if !Rc::ptr_eq(o, &self.p.global) =>
                            {
                                Value::Obj(o.clone())
                            }
                            _ => Value::Und,
                        };
                        (self.get_lref(&mut lr)?, tv)
                    }
                    _ => (self.eval_expr(callee, env)?, Value::Und),
                };
                let argv = self.eval_args(args, env)?;
                if !self.is_callable(&f) {
                    let tn = self.type_name(&f);
                    return Err(self.type_err(format!("{tn} is not a function")));
                }
                self.call(&f, this_v, argv)
            }
        }
    }

    /// is `f` the installed global `eval` function object?
    fn is_eval_fn(&self, f: &Value) -> bool {
        match (f, &self.eval_fn) {
            (Value::Obj(a), Some(Value::Obj(b))) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }

    fn eval_unary(&mut self, op: UnaryOp, expr: &Expr, env: &EnvRef) -> ERes {
        match op {
            UnaryOp::Neg => {
                let v = self.eval_expr(expr, env)?;
                // ToNumeric unwraps objects — a wrapped bigint coerces
                // to the primitive stand-in
                let v = if matches!(v, Value::Obj(_)) {
                    self.to_primitive(&v)?
                } else {
                    v
                };
                if let Some((neg, mag)) = crate::binary::bigint_bits(&v) {
                    return Ok(self.bigint_obj_b(!neg, &mag));
                }
                Ok(Value::Num(-self.to_num(&v)?))
            }
            UnaryOp::Pos => {
                let v = self.eval_expr(expr, env)?;
                let v = if matches!(v, Value::Obj(_)) {
                    self.to_primitive(&v)?
                } else {
                    v
                };
                if crate::binary::bigint_magnitude(&v).is_some() {
                    return Err(self.type_err("Cannot convert a BigInt value to a number"));
                }
                Ok(Value::Num(self.to_num(&v)?))
            }
            UnaryOp::Not => {
                let v = self.eval_expr(expr, env)?;
                Ok(Value::Bool(!self.to_bool(&v)))
            }
            UnaryOp::BitNot => {
                let v = self.eval_expr(expr, env)?;
                let v = if matches!(v, Value::Obj(_)) {
                    self.to_primitive(&v)?
                } else {
                    v
                };
                if let Some((neg, mag)) = crate::binary::bigint_bits(&v) {
                    let (n, m) = crate::binary::bigint_not((neg, &mag));
                    return Ok(self.bigint_obj_b(n, &m));
                }
                Ok(Value::Num(!(to_i32(self.to_num(&v)?)) as f64))
            }
            UnaryOp::Typeof => {
                // `typeof undefinedIdent` must not throw — `typeof (x)`
                // keeps the reference through parens
                let expr = Self::unparen(expr);
                if let Expr::Ident(n) = expr {
                    match self.env_lookup_st(env, n)? {
                        Some((_, true)) => {
                            return Err(self.ref_err(format!(
                                "cannot access '{n}' before initialization"
                            )))
                        }
                        Some((v, false)) => {
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
            UnaryOp::Delete => match Self::unparen(expr) {
                Expr::Member { obj, prop, .. } => {
                    // IsSuperReference → ReferenceError, thrown BEFORE
                    // the key is evaluated
                    if matches!(&**obj, Expr::Super) {
                        return Err(
                            self.ref_err("unsupported reference to 'super'")
                        );
                    }
                    let ov = self.eval_expr(obj, env)?;
                    let ok = if let Expr::Str(s) = &**prop {
                        if matches!(ov, Value::Und | Value::Null) {
                            return Err(self.type_err(
                                "Cannot convert undefined or null to object",
                            ));
                        }
                        // route through delete_prop — proxies fire the
                        // deleteProperty trap (delete_prop_str can't)
                        self.delete_prop(&ov, &PropKey::from_str(s))?
                    } else {
                        let pv = self.eval_expr(prop, env)?;
                        let key = self.to_propkey(&pv)?;
                        if matches!(ov, Value::Und | Value::Null) {
                            return Err(self.type_err(
                                "Cannot convert undefined or null to object",
                            ));
                        }
                        self.delete_prop(&ov, &key)?
                    };
                    if !ok && self.cur_strict {
                        return Err(self.type_err(
                            "Cannot delete property (non-configurable)",
                        ));
                    }
                    Ok(Value::Bool(ok))
                }
                Expr::This => Ok(Value::Bool(true)),
                Expr::Ident(n) => {
                    // unresolvable reference → true; found binding → its
                    // `deletable` flag (eval-declared vars/fns only)
                    let k = self.intern(n);
                    let mut cur = Some(env);
                    while let Some(sc) = cur {
                        if let Some(o) = &sc.obj_env {
                            if self.with_has(o, n)? {
                                let ov = Value::Obj(o.clone());
                                let b = self.delete_prop(&ov, &PropKey::S(k))?;
                                return Ok(Value::Bool(b));
                            }
                        } else if sc.vars.borrow().contains_key(k.as_ref()) {
                            let deletable =
                                sc.vars.borrow().get(k.as_ref()).unwrap().deletable;
                            if deletable {
                                sc.vars.borrow_mut().remove(k.as_ref());
                                if Rc::ptr_eq(sc, &self.global) {
                                    self.p
                                        .global
                                        .borrow_mut()
                                        .props
                                        .remove(&PropKey::S(k.clone()));
                                }
                            }
                            return Ok(Value::Bool(deletable));
                        }
                        cur = sc.parent.as_ref();
                    }
                    // global env's object record: `delete x` where x is a
                    // global-object prop (e.g. `NaN`, or a sloppy
                    // undeclared write)
                    let g = self.p.global.clone();
                    if self.has_prop(
                        &Value::Obj(g.clone()),
                        &PropKey::S(k.clone()),
                    )? {
                        let ov = Value::Obj(g);
                        let b = self.delete_prop(&ov, &PropKey::S(k.clone()))?;
                        if b {
                            self.global.vars.borrow_mut().remove(k.as_ref());
                        }
                        return Ok(Value::Bool(b));
                    }
                    Ok(Value::Bool(true))
                }
                _ => {
                    self.eval_expr(expr, env)?;
                    Ok(Value::Bool(true))
                }
            },
        }
    }

    /// BigInt arithmetic — both operands (post-ToPrimitive) must be
    /// BigInt or it's a TypeError; `+` still concatenates when the
    /// other operand is a String
    fn bigint_arith(&mut self, op: BinOp, l: Value, r: Value) -> ERes {
        let lb = crate::binary::bigint_bits(&l);
        let rb = crate::binary::bigint_bits(&r);
        match (lb, rb) {
            (Some(a), Some(b)) => {
                match crate::binary::bigint_apply(op, (a.0, &a.1), (b.0, &b.1)) {
                    Ok((neg, mag)) => Ok(self.bigint_obj_b(neg, &mag)),
                    Err(m) => Err(self.range_err(m)),
                }
            }
            _ => {
                if op == BinOp::Add
                    && (matches!(l, Value::Str(_)) || matches!(r, Value::Str(_)))
                {
                    let a = self.to_str(&l)?;
                    let b = self.to_str(&r)?;
                    return Ok(Value::Str(Rc::from(format!("{a}{b}").as_str())));
                }
                Err(self.type_err(
                    "Cannot mix BigInt and other types, use explicit conversions",
                ))
            }
        }
    }

    fn binary(&mut self, op: BinOp, l: Value, r: Value) -> ERes {
        use BinOp::*;
        // Coercion order differs by operator:
        //   `+`        — ToPrimitive(lhs), ToPrimitive(rhs), then
        //                ToNumeric(lhs) / ToNumeric(rhs)
        //   numerics   — ToNumeric(lhs) fully resolves (ToPrimitive +
        //                conversion) BEFORE ToPrimitive(rhs)
        //   relational — both ToPrimitive (number hint), then compare
        // Computing lp/rp here keeps valueOf/getter side effects from
        // firing twice in the arms.
        let (lp, rp) = if matches!(
            op,
            Sub | Mul | Div | Mod | Pow | BitAnd | BitOr | BitXor | Shl | Shr | Ushr
        ) {
            let lp = self.to_primitive(&l)?;
            // ToNumeric(lhs) throws for Symbol before rhs coerces
            if matches!(lp, Value::Sym(..)) {
                let _ = self.to_num(&lp)?;
            }
            let lbig = crate::binary::bigint_magnitude(&lp).is_some();
            if !lbig {
                let _ = self.to_num(&lp)?;
            }
            let rp = self.to_primitive(&r)?;
            if lbig || crate::binary::bigint_magnitude(&rp).is_some() {
                return match op {
                    Ushr => Err(self.type_err(
                        "BigInts have no unsigned right shift",
                    )),
                    _ => self.bigint_arith(op, lp, rp),
                };
            }
            (lp, rp)
        } else if matches!(op, Add) {
            let lp = self.to_primitive(&l)?;
            let rp = self.to_primitive(&r)?;
            // spec `+` order: both ToPrimitives, then ToNumeric(lhs)
            if matches!(lp, Value::Sym(..)) {
                let _ = self.to_num(&lp)?;
            }
            if crate::binary::bigint_magnitude(&lp).is_some()
                || crate::binary::bigint_magnitude(&rp).is_some()
            {
                return self.bigint_arith(op, lp, rp);
            }
            (lp, rp)
        } else if matches!(op, Lt | Le | Gt | Ge) {
            (self.to_primitive(&l)?, self.to_primitive(&r)?)
        } else {
            (Value::Und, Value::Und)
        };
        Ok(match op {
            Add => {
                // numeric fast path — skips the to_num round-trip
                if let (Value::Num(a), Value::Num(b)) = (&l, &r) {
                    Value::Num(*a + *b)
                } else if matches!(lp, Value::Str(_)) || matches!(rp, Value::Str(_)) {
                    let a = self.to_str(&lp)?;
                    let b = self.to_str(&rp)?;
                    Value::Str(Rc::from(format!("{a}{b}").as_str()))
                } else {
                    Value::Num(self.to_num(&lp)? + self.to_num(&rp)?)
                }
            }
            Sub => Value::Num(self.to_num(&lp)? - self.to_num(&rp)?),
            Mul => Value::Num(self.to_num(&lp)? * self.to_num(&rp)?),
            Div => Value::Num(self.to_num(&lp)? / self.to_num(&rp)?),
            Mod => {
                let a = self.to_num(&lp)?;
                let b = self.to_num(&rp)?;
                Value::Num(if b == 0.0 { f64::NAN } else { a % b })
            }
            Pow => {
                let a = self.to_num(&lp)?;
                let b = self.to_num(&rp)?;
                // spec: |base| == 1 with an infinite exponent is NaN —
                // Rust's powf returns 1
                Value::Num(if b.is_infinite() && a.abs() == 1.0 {
                    f64::NAN
                } else {
                    a.powf(b)
                })
            }
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
                if let (Value::Str(a), Value::Str(b)) = (&lp, &rp) {
                    let c = match op {
                        Lt => a < b,
                        Le => a <= b,
                        Gt => a > b,
                        Ge => a >= b,
                        _ => unreachable!(),
                    };
                    Value::Bool(c)
                } else if crate::binary::bigint_magnitude(&lp).is_some()
                    || crate::binary::bigint_magnitude(&rp).is_some()
                {
                    // ToNumeric(Symbol) throws — the bigint arm bypasses
                    // the to_num fallback that would otherwise do it
                    if matches!(lp, Value::Sym(..)) || matches!(rp, Value::Sym(..)) {
                        return Err(self.type_err(
                            "Cannot convert a Symbol value to a number",
                        ));
                    }
                    // BigInt vs Number/String/Boolean — exact ordering;
                    // unconvertible operand → undefined → false
                    let c = crate::binary::bigint_order(&lp, &rp);
                    let r = match c {
                        None => false,
                        Some(o) => match op {
                            Lt => o == std::cmp::Ordering::Less,
                            Le => o != std::cmp::Ordering::Greater,
                            Gt => o == std::cmp::Ordering::Greater,
                            Ge => o != std::cmp::Ordering::Less,
                            _ => unreachable!(),
                        },
                    };
                    Value::Bool(r)
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
            BitAnd => Value::Num((to_i32(self.to_num(&lp)?) & to_i32(self.to_num(&rp)?)) as f64),
            BitOr => Value::Num((to_i32(self.to_num(&lp)?) | to_i32(self.to_num(&rp)?)) as f64),
            BitXor => Value::Num((to_i32(self.to_num(&lp)?) ^ to_i32(self.to_num(&rp)?)) as f64),
            Shl => Value::Num(
                (to_i32(self.to_num(&lp)?).wrapping_shl(to_u32(self.to_num(&rp)?) & 31)) as f64,
            ),
            Shr => Value::Num(
                (to_i32(self.to_num(&lp)?).wrapping_shr(to_u32(self.to_num(&rp)?) & 31)) as f64,
            ),
            Ushr => {
                Value::Num(
                    (to_u32(self.to_num(&lp)?).wrapping_shr(to_u32(self.to_num(&rp)?) & 31))
                        as f64,
                )
            }
            In => {
                if !matches!(r, Value::Obj(_)) {
                    return Err(self.type_err("Cannot use 'in' operator on non-object"));
                }
                let key = self.to_propkey(&l)?;
                Value::Bool(self.has_prop(&r, &key)?)
            }
            Instanceof => {
                // 13.10.2 InstanceofOperator(O, C)
                if !matches!(r, Value::Obj(_)) {
                    return Err(
                        self.type_err("Right-hand side of 'instanceof' is not an object")
                    );
                }
                let hm = self.get_prop(&r, &PropKey::Sym(SYM_HASINSTANCE))?;
                let hm = match hm {
                    Value::Und | Value::Null => None,
                    m if self.is_callable(&m) => Some(m),
                    _ => {
                        return Err(
                            self.type_err("Symbol.hasInstance is not callable")
                        )
                    }
                };
                if let Some(hm) = hm {
                    let res = self.call(&hm, r.clone(), vec![l.clone()])?;
                    Value::Bool(self.to_bool(&res))
                } else {
                    if !self.is_callable(&r) {
                        return Err(self.type_err(
                            "Right-hand side of 'instanceof' is not callable",
                        ));
                    }
                    Value::Bool(crate::builtins::ordinary_has_instance(
                        self, &r, &l,
                    )?)
                }
            }
        })
    }

    fn unparen(e: &Expr) -> &Expr {
        match e {
            Expr::Paren(inner) => Self::unparen(inner),
            _ => e,
        }
    }

    fn eval_assign(&mut self, op: AssignOp, target: &Expr, value: &Expr, env: &EnvRef) -> ERes {
        if op == AssignOp::Assign {
            // a parenthesized LHS is not an IdentifierRef — the
            // NamedEvaluation name check inspects the raw target
            let raw_target = target;
            let target = Self::unparen(target);
            // destructuring targets arrive as Array/Object literal exprs
            match target {
                Expr::Array(_) | Expr::Object(_) | Expr::ObjDup(_) => {
                    let v = self.eval_expr(value, env)?;
                    if let Some(pat) = expr_to_pattern(target) {
                        self.assign_pattern(env, &pat, v.clone())?;
                    } else {
                        return Err(self.syntax_err("invalid assignment target"));
                    }
                    return Ok(v);
                }
                _ => {
                    // LHS reference evaluates before the RHS
                    let lr = match target {
                        Expr::Member { .. } => Some(self.eval_lref(target, env)?),
                        Expr::Ident(n) => {
                            Some(self.eval_lref_ident(env, n)?)
                        }
                        _ => None,
                    };
                    let v = self.eval_expr(value, env)?;
                    if let Expr::Ident(n) = raw_target {
                        if Self::is_anon_fn_def(value) {
                            self.maybe_name(&v, n);
                        }
                    }
                    match lr {
                        Some(lr) => self.put_lref(lr, v.clone())?,
                        None => {
                            self.assign_target(env, target, v.clone())?;
                        }
                    }
                    return Ok(v);
                }
            }
        }
        // compound assignments — resolve the reference ONCE: spec runs
        // GetValue(lref) then PutValue(lref, v), so an object-env base
        // stays pinned even if its binding disappears mid-operation
        let mut lr = match Self::unparen(target) {
            Expr::Member { .. } => Some(self.eval_lref(target, env)?),
            Expr::Ident(n) => Some(self.eval_lref_ident(env, n)?),
            _ => None,
        };
        let put = |this: &mut Self, lr: Option<LRef>, v: Value| -> Result<(), Value> {
            match lr {
                Some(lr) => this.put_lref(lr, v),
                None => this.assign_target(env, target, v).map(|_| ()),
            }
        };
        let cur = match &mut lr {
            Some(lr) => self.get_lref(lr)?,
            None => self.eval_expr(target, env)?,
        };
        // NamedEvaluation: `x ||= function(){}` names the fn `x`
        let named = matches!(Self::unparen(target), Expr::Ident(_))
            && Self::is_anon_fn_def(value);
        let result = match op {
            AssignOp::LogAnd => {
                if !self.to_bool(&cur) {
                    return Ok(cur);
                }
                let v = self.eval_expr(value, env)?;
                if named {
                    if let Expr::Ident(n) = Self::unparen(target) {
                        self.maybe_name(&v, n);
                    }
                }
                put(self, lr, v.clone())?;
                return Ok(v);
            }
            AssignOp::LogOr => {
                if self.to_bool(&cur) {
                    return Ok(cur);
                }
                let v = self.eval_expr(value, env)?;
                if named {
                    if let Expr::Ident(n) = Self::unparen(target) {
                        self.maybe_name(&v, n);
                    }
                }
                put(self, lr, v.clone())?;
                return Ok(v);
            }
            AssignOp::Nullish => {
                if !matches!(cur, Value::Und | Value::Null) {
                    return Ok(cur);
                }
                let v = self.eval_expr(value, env)?;
                if named {
                    if let Expr::Ident(n) = Self::unparen(target) {
                        self.maybe_name(&v, n);
                    }
                }
                put(self, lr, v.clone())?;
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
        put(self, lr, result.clone())?;
        Ok(result)
    }

    /// assign `v` to an lvalue expression.
    fn assign_target(&mut self, env: &EnvRef, target: &Expr, v: Value) -> Result<Value, Value> {
        match target {
            Expr::Ident(name) => {
                if name == "$new_target" {
                    return Err(self.syntax_err("invalid assignment target"));
                }
                self.env_assign(env, name, v.clone())?;
                Ok(v)
            }
            Expr::Member { .. } => {
                let lr = self.eval_lref(target, env)?;
                self.put_lref(lr, v.clone())?;
                Ok(v)
            }
            Expr::Paren(inner) => self.assign_target(env, inner, v),
            Expr::Array(_) | Expr::Object(_) | Expr::ObjDup(_) => {
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

    /// Evaluate `src` as a program in `env` — the engine behind both direct
    /// and indirect `eval()`. Parse errors surface as thrown SyntaxError
    /// values so JS try/catch sees them.
    pub fn eval_source(&mut self, src: &str, env: EnvRef) -> ERes {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            return Err(self.range_err("Maximum call stack size exceeded"));
        }
        // direct eval inherits caller strictness + fn/method context
        let caller_strict = self.cur_strict;
        let (mut in_fn, mut in_method) = (false, false);
        let mut privs: Vec<String> = Vec::new();
        {
            let mut cur = Some(&env);
            while let Some(s) = cur {
                // the global scope is is_fn=true (var boundary) but is NOT
                // function code — `eval("new.target")` at top level must
                // still be a SyntaxError
                if s.is_fn && s.parent.is_some() {
                    in_fn = true;
                }
                if s.home.borrow().is_some() {
                    in_method = true;
                }
                // `#x` bindings = this class scope's declared private
                // names — direct eval inherits the PrivateEnvironment
                for n in s.vars.borrow().keys() {
                    if n.starts_with('#') {
                        privs.push(String::from(&**n));
                    }
                }
                cur = s.parent.as_ref();
            }
        }
        let r = (|| {
            let stmts = match parser::parse_eval(
                src,
                caller_strict,
                in_fn,
                in_method,
                std::mem::take(&mut privs),
            ) {
                Ok(s) => s,
                Err(e) => return Err(self.syntax_err(e.msg)),
            };
            // direct eval inside a class field initializer may not
            // contain `arguments` — early error, checked before running
            // (an `arguments` reference anywhere in the source aborts)
            if self.eval_args_ban > 0 && parser::stmts_have_arguments(&stmts) {
                return Err(self.syntax_err(
                    "`arguments` is not allowed in eval inside a class field initializer",
                ));
            }
            let strict_eval = caller_strict || parser::stmts_strict(&stmts);
            // eval code runs in a fresh declarative env over the caller's;
            // strict eval also gets a fresh variable env (var bindings must
            // not leak into the caller's var env)
            let lex = Scope::new(Some(env), false);
            let eenv = if strict_eval {
                // strict eval's separate VAR env — a var boundary, but NOT
                // a function scope (this/new.target/super must still walk
                // through to the caller's function env)
                let v = Scope::new(Some(lex), false);
                v.var_boundary.set(true);
                v
            } else {
                lex
            };
            let saved_strict = std::mem::replace(&mut self.cur_strict, strict_eval);
            let saved_decl = std::mem::replace(&mut self.eval_decl, true);
            let r = self.eval_source_in(&stmts, &eenv);
            self.eval_decl = saved_decl;
            self.cur_strict = saved_strict;
            r
        })();
        self.depth -= 1;
        r
    }

    /// Hoist + execute an already-parsed eval body in `eenv`.
    fn eval_source_in(&mut self, stmts: &[Stmt], env: &EnvRef) -> ERes {
        // EvalDeclarationInstantiation: a `var`/fn name that collides with
        // a lexical (let/const/class/catch) binding anywhere in the chain
        // above the eval's own env is a SyntaxError
        // EvalDeclarationInstantiation step 5: the lexical-collision rule
        // applies only when the eval is NOT strict — strict eval code gets
        // its own variable env, so `var x` colliding with a caller's
        // `let x` is legal
        if !self.cur_strict {
            let h = self.hoisted_slice(stmts);
            let mut names: Vec<Rc<str>> = h.vars.clone();
            for f in &h.fns {
                if let Some(n) = f.name.as_deref() {
                    names.push(self.intern(n));
                }
            }
            for name in &names {
                let mut cur = env.parent.clone();
                while let Some(sc) = cur {
                    let vars = sc.vars.borrow();
                    let hit = vars.get(name.as_ref()).is_some_and(|b| b.lexical)
                        // a parameter environment's bindings are its formals —
                        // `var <param>` inside eval is a SyntaxError too
                        || (sc.param_env.get() && vars.contains_key(name.as_ref()));
                    drop(vars);
                    if hit {
                        return Err(self.syntax_err(format!(
                            "Identifier `{name}` has already been declared"
                        )));
                    }
                    cur = sc.parent.clone();
                }
            }
        }
        {
            let stmts = stmts;
            // `var arguments`/`var eval` inside a parameter environment is an
            // early SyntaxError (the implicit binding can't be redeclared)
            {
                let h = self.hoisted_slice(&stmts);
                let mut bad = h.vars.iter().any(|n| &**n == "arguments" || &**n == "eval");
                if !bad {
                    bad = h.fns.iter().any(|f| {
                        f.name.as_deref() == Some("arguments") || f.name.as_deref() == Some("eval")
                    });
                }
                if bad {
                    // walk PAST the fresh eval envs to the caller's scopes
                    let mut cur = env.parent.as_ref();
                    while let Some(s) = cur {
                        if s.param_env.get() {
                            return Err(self.syntax_err(
                                "cannot declare `arguments`/`eval` in eval within a parameter environment",
                            ));
                        }
                        cur = s.parent.as_ref();
                    }
                }
            }
            self.hoist_body(stmts, env)?;
            // FunctionDeclaration instantiation already happened at hoist —
            // per spec its Evaluation is a no-op, so executing it must NOT
            // re-bind (a `delete f` between hoist and exec removes the
            // binding for good)
            let hf = self.hoisted_slice(stmts);
            let mut last = Value::Und;
            for s in stmts {
                if let Stmt::FnDecl(f) = s {
                    let hoisted = hf
                        .fns
                        .iter()
                        .any(|hf| hf.name.as_deref() == f.name.as_deref());
                    if hoisted {
                        continue;
                    }
                }
                match self.exec_stmt(s, env) {
                    Ok(Completion::Normal(v)) | Ok(Completion::Return(v)) => last = v,
                    // a break/continue escaping the eval'd program resolves
                    // to its carried value and stops the program
                    Ok(Completion::Break(_, v)) | Ok(Completion::Continue(_, v)) => {
                        last = v.unwrap_or(Value::Und);
                        break;
                    }
                    Ok(_) => {}
                    Err(v) => return Err(v),
                }
            }
            Ok(last)
        }
    }

    // -- top-level ---------------------------------------------------------------
    pub fn run(&mut self, src: &str) -> Result<Value, Error> {
        let stmts = parser::parse(src)?;
        self.cur_strict = parser::stmts_strict(&stmts);
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
                Ok(Completion::Break(_, v)) | Ok(Completion::Continue(_, v)) => {
                    last = v.unwrap_or(Value::Und);
                    break;
                }
                Ok(_) => {}
                Err(v) => return Err(self.throw_to_error(v)),
            }
        }
        Ok(last)
    }

    pub fn throw_to_error(&mut self, v: Value) -> Error {
        let msg = match &v {
            Value::Obj(_) => self
                .to_str(&v)
                .unwrap_or_else(|_| self.inspect(&v, 0)),
            other => self.to_str(other).unwrap_or_else(|_| "<exception>".into()),
        };
        Error::new(format!("Uncaught {msg}"), 0, 0)
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Canonical integer-index key for [[OwnPropertyKeys]] ordering —
/// array indices are ToString(ToUint32) keys below 2^32-1.
pub(crate) fn js_int_index(s: &str) -> Option<usize> {
    if s.is_empty() || s.len() > 10 {
        return None;
    }
    if !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if s.len() > 1 && s.starts_with('0') {
        return None;
    }
    s.parse::<usize>().ok().filter(|i| *i < 4294967295)
}

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

/// CanonicalNumericIndexString — any string that round-trips through
/// ToNumber/ToString ("NaN", "-0", "1.5", "20", ...) — the key class
/// TypedArray exotic methods intercept, wider than `arr_index`.
pub fn canonical_numeric_index(s: &str) -> Option<f64> {
    if s == "-0" {
        return Some(-0.0);
    }
    let n: f64 = s.parse().ok()?;
    if js_num_str(n) == s {
        Some(n)
    } else {
        None
    }
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
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    // Rust's f64 parse accepts "inf"/"INFINITY"/"infinity" — the JS
    // grammar only allows the exact "Infinity" spelling (handled above)
    let lower = t.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "inf" | "infinity" | "+inf" | "+infinity" | "-inf" | "-infinity"
    ) {
        return f64::NAN;
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
    // a trailing 'n' is the BigInt-literal suffix — not a Number
    // numeric-string production; leave it to fail the parse
    t.parse::<f64>().unwrap_or(f64::NAN)
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
        .take_while(|p| !matches!(p, Param::Rest(_) | Param::Default(..)))
        .count()
}

fn collect_hoisted<'a>(
    stmts: &'a [Stmt],
    fns: &mut Vec<&'a Rc<FnDef>>,
    block_fns: &mut Vec<&'a str>,
    vars: &mut Vec<&'a str>,
    lets: &mut Vec<&'a str>,
) {
    collect_hoisted_at(stmts, fns, block_fns, vars, lets, true)
}

fn collect_hoisted_at<'a>(
    stmts: &'a [Stmt],
    fns: &mut Vec<&'a Rc<FnDef>>,
    block_fns: &mut Vec<&'a str>,
    vars: &mut Vec<&'a str>,
    lets: &mut Vec<&'a str>,
    top: bool,
) {
    for s in stmts {
        collect_stmt(s, fns, block_fns, vars, lets, top);
    }
}

/// Owned (cacheable) form of `collect_hoisted`.
fn collect_hoisted_owned(stmts: &[Stmt]) -> Hoisted {
    let mut fns: Vec<&Rc<FnDef>> = Vec::new();
    let mut block_fns: Vec<&str> = Vec::new();
    let mut vars: Vec<&str> = Vec::new();
    let mut lets: Vec<&str> = Vec::new();
    collect_hoisted(stmts, &mut fns, &mut block_fns, &mut vars, &mut lets);
    Hoisted {
        fns: fns.into_iter().cloned().collect(),
        vars: vars.into_iter().map(|s| Rc::<str>::from(s)).collect(),
        lets: lets.into_iter().map(|s| Rc::<str>::from(s)).collect(),
        block_fns: block_fns.into_iter().map(|s| Rc::<str>::from(s)).collect(),
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
        Pattern::Target(e) => {
            matches!(&**e, Expr::Member { obj, prop, .. }
                if expr_uses_arguments(obj) || expr_uses_arguments(prop))
        }
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
        Expr::Paren(inner) => return expr_uses_arguments(inner),
        Expr::Ident(n) => n == "arguments",
        Expr::Num(_)
        | Expr::BigInt(_)
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
            ArrElem::Expr(e) | ArrElem::Spread(e) | ArrElem::SpreadTrail(e) => expr_uses_arguments(e),
            ArrElem::Hole => false,
        }),
        Expr::Object(props) | Expr::ObjDup(props) => props.iter().any(|p| {
            if matches!(&p.key, ast::PropKey::Computed(ex) if expr_uses_arguments(ex)) {
                return true;
            }
            match &p.val {
                PropVal::Expr(e) | PropVal::Spread(e) | PropVal::SpreadTrail(e) => expr_uses_arguments(e),
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
            // a direct `eval(...)` may read `arguments` — materialize the
            // binding so the eval'd source can see it
            let eval_call = matches!(
                &**callee,
                Expr::Ident(n) if n == "eval"
            ) || matches!(
                &**callee,
                Expr::Paren(inner) if matches!(&**inner, Expr::Ident(n) if n == "eval")
            );
            eval_call
                || expr_uses_arguments(callee)
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
            || m.init.as_ref().map_or(false, expr_uses_arguments)
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
        // Annex B.3.4: `if (x) function f(){}`, `label: function f(){}` get
        // an implicit block scope for the decl
        Stmt::FnDecl(_) => true,
        Stmt::If { t, e, .. } => {
            stmt_binds_lexically(t)
                || e.as_ref().map_or(false, |s| stmt_binds_lexically(s))
        }
        Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => {
            stmt_binds_lexically(body)
        }
        Stmt::Labeled(_, inner) => stmt_binds_lexically(inner),
        Stmt::With { body, .. } => stmt_binds_lexically(body),
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
        Stmt::With { obj, body } => {
            expr_uses_arguments(obj) || stmt_uses_arguments(body)
        }
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

fn collect_stmt<'a>(
    s: &'a Stmt,
    fns: &mut Vec<&'a Rc<FnDef>>,
    block_fns: &mut Vec<&'a str>,
    vars: &mut Vec<&'a str>,
    lets: &mut Vec<&'a str>,
    top: bool,
) {
    // Annex-B block-fn var bindings only cover plain `function` decls —
    // generators and async fns stay purely block-scoped
    let nested = |f: &'a Rc<FnDef>, block_fns: &mut Vec<&'a str>| {
        if !f.is_gen && !f.is_async {
            if let Some(n) = f.name.as_deref() {
                block_fns.push(n);
            }
        }
    };
    match s {
        Stmt::FnDecl(f) => {
            if top {
                fns.push(f)
            } else {
                nested(f, block_fns)
            }
        }
        Stmt::VarDecl { kind, decls } => {
            if *kind == VarKind::Var {
                for (p, _) in decls {
                    collect_pattern_names(p, vars);
                }
            } else if top {
                // lexical decls instantiate uninit at THIS scope —
                // decls inside nested scopes hoist on their own entry
                for (p, _) in decls {
                    collect_pattern_names(p, lets);
                }
            }
        }
        Stmt::ClassDecl(c) => {
            if top {
                if let Some(n) = c.name.as_deref() {
                    lets.push(n);
                }
            }
        }
        Stmt::Block(ss) => collect_hoisted_at(ss, fns, block_fns, vars, lets, false),
        Stmt::If { t, e, .. } => {
            collect_stmt(t, fns, block_fns, vars, lets, false);
            if let Some(e) = e {
                collect_stmt(e, fns, block_fns, vars, lets, false);
            }
        }
        Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => {
                collect_stmt(body, fns, block_fns, vars, lets, false)
            }
        Stmt::For { init, body, .. } => {
            if let Some(ForInit::Var(VarKind::Var, decls)) = init {
                for (p, _) in decls {
                    collect_pattern_names(p, vars);
                }
            }
            collect_stmt(body, fns, block_fns, vars, lets, false);
        }
        Stmt::ForIn { left, body, .. } | Stmt::ForOf { left, body, .. } => {
            if let ForTarget::Var(VarKind::Var, p) = left {
                collect_pattern_names(p, vars);
            }
            collect_stmt(body, fns, block_fns, vars, lets, false);
        }
        Stmt::Switch { cases, .. } => {
            for c in cases {
                collect_hoisted_at(&c.body, fns, block_fns, vars, lets, false);
            }
        }
        Stmt::Try {
            block,
            handler,
            finally,
            ..
        } => {
            collect_hoisted_at(block, fns, block_fns, vars, lets, false);
            if let Some(h) = handler {
                collect_hoisted_at(h, fns, block_fns, vars, lets, false);
            }
            if let Some(f) = finally {
                collect_hoisted_at(f, fns, block_fns, vars, lets, false);
            }
        }
        Stmt::Labeled(_, inner) => collect_stmt(inner, fns, block_fns, vars, lets, false),
        Stmt::With { body, .. } => collect_stmt(body, fns, block_fns, vars, lets, false),
        _ => {}
    }
}

pub(crate) fn collect_pattern_names<'a>(p: &'a Pattern, out: &mut Vec<&'a str>) {
    match p {
        Pattern::Target(_) => {}
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
        Expr::Paren(inner) => expr_to_pattern(inner),
        Expr::Ident(n) if n != "$new_target" => Some(Pattern::Ident(n.clone())),
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
                    ArrElem::SpreadTrail(_) => return None,
                    ArrElem::Spread(ex) => {
                        // rest must be the final element — a mid-list
                        // spread is not a valid assignment pattern
                        if !std::ptr::eq(el, elems.last().unwrap()) {
                            return None;
                        }
                        out.push(ArrayPatElem::Rest(expr_to_pattern(ex)?));
                    }
                }
            }
            Some(Pattern::Array(out))
        }
        Expr::Object(props) | Expr::ObjDup(props) => {
            let mut out = Vec::new();
            for p in props {
                match &p.val {
                    PropVal::SpreadTrail(_) => return None,
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
                            // `{a: [x, y]} = o` / `{a: {b}} = o` / `{a: x.y} = o`
                            Expr::Array(_) | Expr::Object(_) | Expr::ObjDup(_)
                            | Expr::Member { .. } => {
                                (expr_to_pattern(ex)?, None)
                            }
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
        Expr::Member { .. } => Some(Pattern::Target(Box::new(e.clone()))),
        _ => None,
    }
}

impl Default for Interp {
    fn default() -> Self {
        Self::new()
    }
}
